//! Operating-system integration (plan D6, decision O10): what the app does
//! when the shell starts it with arguments, whether that is the first launch
//! or a second one that the single-instance plugin forwards here.
//!
//! # Rules
//!
//! * **Paths from the OS stay in Rust.** The webview is told that an archive
//!   was opened (an [`ArchiveSummary`], as `open_archive` returns) or what the
//!   problem was. It never receives, and never sends, a path.
//! * **Two kinds of verb (owner decision 2026-10-16, replacing "nothing runs
//!   silently"; amended 2026-10-20).** The *panel verb* "Open with MOCHI"
//!   opens an archive read-only in an archive window (the empty one, else a new one, `crate::windows`). *Quick verbs*
//!   run in a small compact window ([`crate::quick`]; progress, then a
//!   one-line result with "Show in folder" and Close) and never open the main
//!   window: "Extract here" and "Extract to new folder" (`--extract-here`,
//!   `--extract-to-folder`), "Add to archive" (`--add-now`, at once), and
//!   "Add to archive…" (`--add`, a small form first: name, Browse…, format,
//!   level, `.mochi` options, OK and Cancel; nothing is written until OK). A quick verb
//!   never overwrites: when anything in the way exists, nothing is written and
//!   the extract plan for the same archive opens in the main window instead
//!   (the panel verb's screen, with the folder chosen in Rust). Explorer starts
//!   one process per selected item, so `--add-now` and `--add` launches that
//!   arrive within [`ADD_BATCH_WINDOW`] of each other are collected into one
//!   archive; a later `--add` launch joins the form while it is still open.
//! * **No secret on a command line.** There is no option for a passphrase;
//!   a passphrase option is refused by name, and the refusal never repeats an
//!   option's value. Encrypted archives ask for the passphrase in the window.
//! * **Arguments are untrusted.** Every path is checked (empty, NUL, length,
//!   Windows device namespace, a verb's path count) before anything touches
//!   the file system, and a launch that does not parse changes nothing.
//! * **Foreign formats are not parsed here.** A ZIP, 7z, RAR, tar, tar.gz, or
//!   tar.zst path (by extension, [`foreign_kind`], unless the file starts with
//!   the MOCHI descriptor frame: `crate::route`) goes to the same open path as the
//!   in-app "Open foreign archive" button ([`Foreign::open`], plan D8): the
//!   isolated helper lists it and the window shows the read-only foreign
//!   browser. An archive that needs a password is remembered in Rust
//!   (`Foreign::pending`) and the window asks for it, once, exactly as it
//!   does for the in-app path. What D8 does not support (a split ZIP, an SFX,
//!   an unsupported method) comes back as the helper's typed refusal.
//! * **Extract verbs are for `.mochi`.** `--extract-here` and
//!   `--extract-to-folder` on a foreign archive extract nothing, choose no
//!   folder, and open no compact window: the archive is opened for browsing,
//!   with a plain sentence that extraction from the shell is for `.mochi`
//!   archives. The user extracts
//!   from the foreign browser's own buttons, which show the plan first.
//!
//! # Command line
//!
//! ```text
//! mochi-desktop [VERB] [--] PATH...
//!   (no verb)               open PATH read-only (what a file manager passes)
//!   --open                  the same, explicitly ("Open with MOCHI")
//!   --extract-here          quick: extract PATH into the archive's folder
//!   --extract-to-folder     quick: extract PATH into a new folder named for it
//!   --add                   quick: the Add form for PATH... (name, format, OK)
//!   --add-now               quick: add PATH... to a new archive next to it
//!   --find                  MOCHI Find: search the names inside every .mochi under
//!                           the folders PATH... (none: open Find as it was left)
//! ```
//!
//! `--` ends the options, so a path that starts with `-` is still a path.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::browse::Archives;
use crate::find::FoundEntry;
use crate::ipc::{
    ArchiveSummary, EntryKindView, ForeignArchiveSummary, IpcError, NameText, OpenOutcome,
};
use crate::quick::{MainPort, QuickAction, QuickJob, QuickPool};
use crate::windows::{Placement, WindowStates, MAIN_LABEL};
use crate::write::MAX_STAGED;

/// Longest path accepted from the command line, in bytes (Windows allows
/// 32,767 UTF-16 units; Linux 4,096).
pub const MAX_PATH_BYTES: usize = 32 * 1024;

/// Events kept for a window that is not listening yet.
const MAX_PENDING: usize = 64;

/// How long after the last `--add-now` launch the worker waits for another
/// before it adds what it has collected. Explorer starts one process per
/// selected item; the second and later ones reach this process through the
/// single-instance plugin, a few tens of milliseconds apart.
pub const ADD_BATCH_WINDOW: Duration = Duration::from_millis(500);

/// What the shell asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Open,
    ExtractHere,
    ExtractToFolder,
    Add,
    AddNow,
    /// MOCHI Find: search the file names inside every `.mochi` archive under
    /// the given folders. Opens the Find tool in an empty window (else a new
    /// one); not a quick verb.
    Find,
}

impl Verb {
    /// Quick verbs run in the compact window and never open the main one.
    pub fn is_quick(self) -> bool {
        matches!(
            self,
            Verb::ExtractHere | Verb::ExtractToFolder | Verb::Add | Verb::AddNow
        )
    }
}

impl Verb {
    fn from_option(name: &str) -> Option<Verb> {
        Some(match name {
            "--open" => Verb::Open,
            "--extract-here" => Verb::ExtractHere,
            "--extract-to-folder" => Verb::ExtractToFolder,
            "--add" => Verb::Add,
            "--add-now" => Verb::AddNow,
            "--find" => Verb::Find,
            _ => return None,
        })
    }
}

/// A parsed, validated launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub verb: Verb,
    /// Absolute when the launch carried a working directory; never empty.
    pub paths: Vec<PathBuf>,
}

/// Why a launch was refused: one sentence that is safe to show (it never
/// repeats an option's value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchError(pub String);

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn refuse<T>(what: impl Into<String>) -> Result<T, LaunchError> {
    Err(LaunchError(what.into()))
}

/// The name part of an option (`--x=value` → `--x`), so that a value, which
/// could be a secret, is never echoed.
fn option_name(arg: &str) -> &str {
    arg.split('=').next().unwrap_or(arg)
}

fn looks_like_secret_option(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["pass", "pwd", "secret", "key", "token"]
        .iter()
        .any(|w| lower.contains(w))
}

fn check_path(arg: &OsStr, cwd: &Path) -> Result<PathBuf, LaunchError> {
    let lossy = arg.to_string_lossy();
    if arg.is_empty() {
        return refuse("an empty path was given");
    }
    if arg.len() > MAX_PATH_BYTES {
        return refuse("a path is too long");
    }
    if lossy.contains('\0') {
        return refuse("a path contains a NUL character");
    }
    // `\\.\` names a device (a pipe, a disk): reading one can block forever.
    if lossy.starts_with(r"\\.\") {
        return refuse("a Windows device path was given; name a file instead");
    }
    let path = PathBuf::from(arg);
    if path.is_absolute() {
        return Ok(path);
    }
    if !cwd.is_absolute() {
        return refuse("a relative path was given without a working directory");
    }
    Ok(cwd.join(path))
}

/// Parse the arguments after the program name. `Ok(None)` when there are
/// none (a plain start). Relative paths are made absolute against `cwd`,
/// which for a second instance is the second process's directory.
pub fn parse<S: AsRef<OsStr>>(args: &[S], cwd: &Path) -> Result<Option<Launch>, LaunchError> {
    let mut verb: Option<Verb> = None;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut options_done = false;
    for arg in args {
        let arg = arg.as_ref();
        let text = arg.to_str();
        if !options_done {
            if text == Some("--") {
                options_done = true;
                continue;
            }
            if let Some(t) = text.filter(|t| t.starts_with('-') && t.len() > 1) {
                let name = option_name(t);
                if looks_like_secret_option(name) {
                    return refuse(
                        "a passphrase cannot be given on the command line; the window asks for it",
                    );
                }
                let Some(v) = Verb::from_option(name).filter(|_| name == t) else {
                    return refuse(format!("unknown option {name}"));
                };
                if verb.is_some() {
                    return refuse(
                        "only one of --open, --extract-here, --extract-to-folder, --add, --add-now, --find",
                    );
                }
                verb = Some(v);
                continue;
            }
        }
        if paths.len() >= MAX_STAGED {
            return refuse(format!("at most {MAX_STAGED} paths can be given at once"));
        }
        paths.push(check_path(arg, cwd)?);
    }
    let verb = verb.unwrap_or(Verb::Open);
    match (verb, paths.len()) {
        (_, 0) if args.is_empty() => Ok(None),
        // Find without a folder (the app-menu launcher) opens the Find tool.
        (Verb::Find, _) => Ok(Some(Launch { verb, paths })),
        (_, 0) => refuse("the command needs a path"),
        (Verb::Add | Verb::AddNow, _) | (_, 1) => Ok(Some(Launch { verb, paths })),
        _ => refuse("one archive at a time: give a single path"),
    }
}

/// The arguments of this process, without the program name.
pub fn from_environment() -> Result<Option<Launch>, LaunchError> {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() {
        args = crate::automation::launch_args();
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    parse(&args, &cwd)
}

/// The arguments a second instance forwards (`argv[0]` first, as the
/// plugin delivers them) and its working directory.
pub fn from_forwarded(argv: &[String], cwd: &str) -> Result<Option<Launch>, LaunchError> {
    parse(argv.get(1..).unwrap_or_default(), Path::new(cwd))
}

// ---- what the window hears ------------------------------------------------

/// What the window is told about a launch. No path crosses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "event", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum LaunchEvent {
    /// An archive was opened read-only; show it. With `extract`, the folder
    /// to extract into is already chosen in Rust and the window shows the
    /// extract plan for the whole snapshot (nothing is written until the
    /// user confirms it).
    Opened {
        archive: ArchiveSummary,
        extract: bool,
    },
    /// A ZIP, 7z, RAR, tar, or tar.gz was opened read-only (plan D8); show
    /// the foreign browser on it. Nothing here says it was verified.
    ForeignOpened { archive: ForeignArchiveSummary },
    /// An Encrypted archive was handed to this (new) window and waits in Rust
    /// for its passphrase: ask for it and answer with `unlock_archive`.
    NeedsPassphrase {
        pending: crate::ipc::PendingId,
        /// The file's name on this computer.
        file_name: String,
    },
    /// The foreign archive needs a password (or the one given was wrong). Rust
    /// remembers the file; the window asks and retries with the password, which
    /// crosses IPC once, in that call.
    ForeignPassword { wrong: bool },
    /// Show the entry `key` (a folder or a file, `path` is its escaped full
    /// path) of the open archive `handle`: a hit of MOCHI Find. The window
    /// goes to the entry's folder and selects it once it shows that archive.
    Reveal {
        handle: u32,
        key: String,
        path: NameText,
        kind: EntryKindView,
    },
    /// MOCHI Find was asked for (`--find`). Rust holds the folders (`folders`
    /// of them, possibly none) of this window; the window opens the Find tool and, when there
    /// are folders, starts the scan. No path crosses.
    Find { folders: u32 },
    /// The launch could not be carried out, in one sentence.
    Problem { message: String },
    /// An archive is being read for this window (or for a new window this one
    /// asked for), which can take a while on a network drive or for a very
    /// large archive: the window says so and takes no other action until
    /// `OpeningEnded`. `file_name` is the file's name on this computer.
    Opening { file_name: String },
    /// The read `Opening` announced ended, whatever its outcome (which comes
    /// separately, or as the answer to the window's own request).
    OpeningEnded,
}

/// The code of an error the shell's open already told a window about (no
/// second notice is needed).
const SAID: &str = "ALREADY_SAID";

/// Where launch events go (a Tauri `Channel` in the app).
pub trait LaunchSink: Send + Sync + 'static {
    /// `false` once nobody listens.
    fn send(&self, event: LaunchEvent) -> bool;
}

#[derive(Default)]
struct Delivery {
    sink: Option<Arc<dyn LaunchSink>>,
    pending: VecDeque<LaunchEvent>,
}

/// What a launch needs besides its arguments.
struct Inner {
    /// The windows, and through them the archives each one owns.
    windows: Arc<WindowStates>,
    quick: Arc<QuickPool>,
    /// Where each archive window's launch events go, by label.
    delivery: Mutex<HashMap<String, Delivery>>,
    /// Held while an open decides which window it goes to (see `open_routed`).
    placing: Mutex<()>,
    /// The windows reading an archive now, and the file each is reading.
    opening: Mutex<HashMap<String, PathBuf>>,
}

/// Where an archive was opened (see [`Inner::open_routed`]).
struct Landing {
    /// The window that now has it (or had it).
    window: String,
    /// What opening produced; `None` when `window` was only brought forward
    /// (it has the file as a foreign archive, or waiting for a passphrase).
    outcome: Option<OpenOutcome>,
    /// `window` is the window the user asked in: it shows the outcome itself.
    in_place: bool,
}

/// The launch event that shows `outcome` in a window that did not ask for it
/// itself (`None`: there is nothing to show).
fn launch_event(outcome: OpenOutcome, extract: bool) -> Option<LaunchEvent> {
    Some(match outcome {
        OpenOutcome::Opened { archive } => LaunchEvent::Opened { archive, extract },
        OpenOutcome::NeedsPassphrase { pending, file_name } => {
            LaunchEvent::NeedsPassphrase { pending, file_name }
        }
        OpenOutcome::Foreign { archive } => LaunchEvent::ForeignOpened { archive },
        OpenOutcome::ForeignPassword { wrong, .. } => LaunchEvent::ForeignPassword { wrong },
        OpenOutcome::Elsewhere => return None,
    })
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Only queues and a channel handle live behind these locks.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// What the launch worker is asked to do.
enum Item {
    Launch(Result<Launch, LaunchError>),
    /// A second start with no arguments: just bring the window forward.
    ShowMain,
}

/// Handles launches in the order they arrive, on one worker thread, so that
/// opening an archive never blocks the async runtime or the single-instance
/// callback, and two quick launches cannot overtake each other. Quick jobs
/// (extract, add now, the Add form) each run in a compact window of their own,
/// beside each other ([`QuickPool`]), and an archive the shell asked to open is
/// read on a thread of its own: a slow one (on a network drive, or very large)
/// holds up neither the next launch nor any window (owner report 2026-11-12).
pub struct Launches {
    inner: Arc<Inner>,
    queue: Mutex<Sender<Item>>,
}

impl Launches {
    pub fn new(windows: Arc<WindowStates>, quick: Arc<QuickPool>) -> Launches {
        Self::with_batch_window(windows, quick, ADD_BATCH_WINDOW)
    }

    /// [`Launches::new`] with another collection window for `--add-now` and `--add`.
    pub fn with_batch_window(
        windows: Arc<WindowStates>,
        quick: Arc<QuickPool>,
        batch_window: Duration,
    ) -> Launches {
        let inner = Arc::new(Inner {
            windows,
            quick,
            delivery: Mutex::default(),
            placing: Mutex::default(),
            opening: Mutex::default(),
        });
        let quick_worker = Arc::clone(&inner);
        let run_quick = move |job: QuickJob| {
            let main: Arc<dyn MainPort + Send + Sync> = Arc::clone(&quick_worker) as _;
            quick_worker.quick.spawn(job, main);
        };
        let (tx, rx) = channel::<Item>();
        let worker = Arc::clone(&inner);
        std::thread::spawn(move || {
            let mut later: VecDeque<Item> = VecDeque::new();
            loop {
                let item = match later.pop_front() {
                    Some(item) => item,
                    None => match rx.recv() {
                        Ok(item) => item,
                        Err(_) => return,
                    },
                };
                match item {
                    Item::ShowMain => worker.show_main(),
                    Item::Launch(Ok(mut launch))
                        if matches!(launch.verb, Verb::AddNow | Verb::Add) =>
                    {
                        // Collect the launches of one selection into one
                        // archive: wait until none has arrived for a window.
                        // Only launches of the same verb are collected;
                        // anything else that arrives meanwhile keeps its
                        // place, after this one.
                        loop {
                            match rx.recv_timeout(batch_window) {
                                Ok(Item::Launch(Ok(more))) if more.verb == launch.verb => {
                                    for p in more.paths {
                                        if !launch.paths.contains(&p) {
                                            launch.paths.push(p);
                                        }
                                    }
                                }
                                Ok(other) => later.push_back(other),
                                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                                    break
                                }
                            }
                        }
                        worker.handle(Ok(launch), &run_quick, true);
                    }
                    Item::Launch(launch) => worker.handle(launch, &run_quick, true),
                }
            }
        });
        Launches {
            inner,
            queue: Mutex::new(tx),
        }
    }

    /// Queue a launch (or its refusal) to be handled. No arguments: nothing
    /// to do.
    pub fn submit(&self, item: Result<Option<Launch>, LaunchError>) {
        let item = match item {
            Ok(None) => return,
            Ok(Some(launch)) => Ok(launch),
            Err(e) => Err(e),
        };
        // A send fails only if the worker is gone (a panic); nothing to do.
        let _ = lock(&self.queue).send(Item::Launch(item));
    }

    /// A second start with no arguments: bring the window used last forward
    /// (creating the first archive window if only a compact window is open).
    pub fn focus(&self) {
        let _ = lock(&self.queue).send(Item::ShowMain);
    }

    /// Listen for the window `label` with `sink` and hand it what arrived
    /// before there was one.
    pub fn watch(&self, label: &str, sink: Arc<dyn LaunchSink>) {
        let mut all = lock(&self.inner.delivery);
        let d = all.entry(label.to_owned()).or_default();
        while let Some(event) = d.pending.pop_front() {
            if !sink.send(event.clone()) {
                d.pending.push_front(event);
                return;
            }
        }
        d.sink = Some(sink);
    }

    /// The user in the window `requester` asked to open `path`. Opens it
    /// there when that window is empty; otherwise in a new window (the answer
    /// is then [`OpenOutcome::Elsewhere`]); or, when the archive is already
    /// open in some window, brings that window forward (also `Elsewhere`).
    /// A dialog and a window are made on the calling thread, which is never
    /// the event loop.
    pub fn open_in_window(&self, requester: &str, path: &Path) -> Result<OpenOutcome, IpcError> {
        let landing = self.inner.open_routed(Some(requester), path)?;
        self.inner.windows_titled(&landing.window);
        if landing.in_place {
            if let Some(outcome) = landing.outcome.clone() {
                return Ok(outcome);
            }
        }
        self.inner.deliver(landing, false);
        Ok(OpenOutcome::Elsewhere)
    }

    /// A hit of MOCHI Find: open its archive as `open_in_window` does (here if
    /// this window is empty, the window that has it, else a new window), then
    /// tell the window that shows it to go to `entry`. `Opened` is for the
    /// asking window to show; `Elsewhere` means another window has it.
    pub fn open_hit_in_window(
        &self,
        requester: &str,
        path: &Path,
        entry: FoundEntry,
    ) -> Result<OpenOutcome, IpcError> {
        let landing = self.inner.open_routed(Some(requester), path)?;
        self.inner.windows_titled(&landing.window);
        let handle = match &landing.outcome {
            Some(OpenOutcome::Opened { archive }) => Some(archive.handle),
            _ => self.inner.windows.archives().handle_of_file(path),
        };
        let window = landing.window.clone();
        let in_place = landing.in_place;
        let shown = match (&landing.outcome, in_place) {
            (Some(outcome), true) => outcome.clone(),
            _ => {
                self.inner.deliver(landing, false);
                OpenOutcome::Elsewhere
            }
        };
        if let Some(handle) = handle {
            self.inner.emit(
                &window,
                LaunchEvent::Reveal {
                    handle,
                    key: entry.key,
                    path: entry.path,
                    kind: entry.kind,
                },
            );
        }
        Ok(shown)
    }

    /// Run `read`, an open of `path` for the window `label` that does not go
    /// through [`Launches::open_in_window`] (a foreign archive chosen in the
    /// foreign browser, or its password retry), with the window told it is busy
    /// meanwhile.
    pub fn while_opening<T>(&self, label: &str, path: &Path, read: impl FnOnce() -> T) -> T {
        self.inner.announce_opening(label, path);
        let out = read();
        self.inner.emit_quiet(label, LaunchEvent::OpeningEnded);
        out
    }

    /// Set the title of one window to the archive it shows.
    pub fn refresh_title(&self, label: &str) {
        self.inner.windows_titled(label);
    }

    /// Handle one launch now, on the calling thread (tests). Quick verbs run
    /// to their end here too.
    #[cfg(test)]
    fn handle_now(&self, item: Result<Launch, LaunchError>) {
        let jobs = Mutex::new(Vec::new());
        self.inner.handle(item, &|job| lock(&jobs).push(job), false);
        for job in lock(&jobs).drain(..) {
            self.inner.quick.run_now(&job, &*self.inner);
        }
    }
}

impl MainPort for Inner {
    fn archives(&self) -> &Archives {
        self.windows.archives()
    }

    fn hand_over_extract(&self, archive: ArchiveSummary, folder: PathBuf) {
        // The plan goes to the window that has this archive, else an empty
        // one, else a new one, like any other open.
        let handle = archive.handle;
        let window = self.window_for_handover(handle);
        self.windows.archives().assign(handle, &window);
        self.windows.state(&window).destination.set(folder);
        self.windows_titled(&window);
        self.emit(
            &window,
            LaunchEvent::Opened {
                archive,
                extract: true,
            },
        );
    }
}

impl Inner {
    /// Bring the window used last forward, making the first archive window if
    /// none is open.
    fn show_main(&self) {
        let Some(host) = self.quick.host() else {
            return;
        };
        match self.windows.labels().into_iter().next() {
            Some(label) if host.focus_window(&label) => {}
            _ => host.show_main(),
        }
    }

    /// Bring one window forward.
    fn show(&self, label: &str) {
        if let Some(host) = self.quick.host() {
            host.focus_window(label);
        }
    }

    /// The window a notice (a launch that failed) is shown in: the one used
    /// last, or the first archive window, made if none is open.
    fn notice_window(&self) -> String {
        match self.windows.labels().into_iter().next() {
            Some(label) => label,
            None => {
                if let Some(host) = self.quick.host() {
                    host.show_main();
                }
                MAIN_LABEL.to_owned()
            }
        }
    }

    /// Put the archive a window shows in its title.
    fn windows_titled(&self, label: &str) {
        if let Some(host) = self.quick.host() {
            host.set_title(label, &self.windows.title(label));
        }
    }

    /// Where the extract plan of an already open archive goes.
    fn window_for_handover(&self, handle: crate::ipc::ArchiveHandle) -> String {
        let path = self.windows.archives().path(handle).ok();
        let placement = path
            .as_deref()
            .map_or(Placement::New, |p| self.windows.decide(p, None));
        match placement {
            Placement::Focus(w) | Placement::Reuse(w) => w,
            Placement::New => self.fresh_window(),
        }
    }

    /// A new archive window, registered; if none can be made, the window used
    /// last.
    fn fresh_window(&self) -> String {
        let opener = self.windows.labels().into_iter().next();
        let label = self.windows.next_label();
        let made = self.windows.has_room()
            && self
                .quick
                .host()
                .is_some_and(|h| h.create_window(&label, opener.as_deref()));
        if made {
            self.windows.register(&label);
            label
        } else {
            self.notice_window()
        }
    }

    /// Run `open` for a launch: on a thread of its own with `concurrent`, so that
    /// a slow archive holds up no other launch, else here.
    fn run_open(self: &Arc<Self>, concurrent: bool, open: impl FnOnce(&Inner) + Send + 'static) {
        if !concurrent {
            return open(self);
        }
        let inner = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("mochi-open".to_owned())
            .spawn(move || open(&inner));
        if spawned.is_err() {
            self.problem("MOCHI could not start opening the archive; try again.");
        }
    }

    /// The window that is opening `path` now, if one is.
    fn opening_window(&self, path: &Path) -> Option<String> {
        lock(&self.opening)
            .iter()
            .find(|(_, p)| crate::windows::same_file(p, path))
            .map(|(l, _)| l.clone())
    }

    /// Say in `window` that `path` is being read (and, after, that it no longer
    /// is), so the window shows that it is busy and takes no other action while
    /// a slow archive is read (owner report 2026-11-12: on a network drive the
    /// window seemed to do nothing).
    fn announce_opening(&self, window: &str, path: &Path) {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.emit(window, LaunchEvent::Opening { file_name });
    }

    /// Open `path` where it belongs ([`WindowStates::decide`]), making a new
    /// window when needed. `requester` is the window the user acted in, or
    /// `None` for the shell. Delivery of the outcome is the caller's.
    ///
    /// Opens may run at once (the shell's each on a thread of its own, and the
    /// windows' own on the blocking pool): where each goes is decided one at a
    /// time (`placing`), and a window that is reading an archive (`opening`) is
    /// not empty, so two archives never land in one window and one file being
    /// read is not read again beside itself. The read itself runs unlocked.
    fn open_routed(&self, requester: Option<&str>, path: &Path) -> Result<Landing, IpcError> {
        let archives = self.windows.archives();
        let foreign = self.windows.foreign();
        let placing = lock(&self.placing);
        // The same file is being read for a window already: that window will show it.
        if let Some(window) = self.opening_window(path) {
            return Ok(Landing {
                window,
                outcome: None,
                in_place: false,
            });
        }
        let placement = match self.windows.decide(path, requester) {
            Placement::Reuse(w) if lock(&self.opening).contains_key(&w) => Placement::New,
            other => other,
        };
        match placement {
            // Already open there: read it again under the same handle and tell
            // that window to show it. The window's page may no longer show it
            // (it went back to the start screen, or reloaded), and bringing the
            // window forward alone then did nothing at all. A foreign archive, or
            // one still waiting for its passphrase, is only brought forward.
            Placement::Focus(window) => {
                // A window that is gone without its release having run still
                // holds the file: release it, then decide again (it no longer
                // holds anything, so this happens at most once per window).
                if requester != Some(window.as_str())
                    && self.quick.host().is_some_and(|h| !h.window_exists(&window))
                {
                    self.windows.release(&window);
                    drop(placing);
                    return self.open_routed(requester, path);
                }
                drop(placing);
                let outcome = archives
                    .handle_of_file(path)
                    .filter(|&h| archives.owner_of(h).as_deref() == Some(window.as_str()))
                    .and_then(|h| archives.reopen(h).ok())
                    .map(|archive| OpenOutcome::Opened { archive });
                let in_place = outcome.is_some() && requester == Some(window.as_str());
                Ok(Landing {
                    window,
                    outcome,
                    in_place,
                })
            }
            Placement::Reuse(window) => {
                lock(&self.opening).insert(window.clone(), path.to_path_buf());
                drop(placing);
                let told = requester.unwrap_or(&window).to_owned();
                self.announce_opening(&told, path);
                let outcome = crate::route::open_path_as(archives, foreign, &window, path);
                lock(&self.opening).remove(&window);
                self.emit_quiet(&told, LaunchEvent::OpeningEnded);
                let in_place = requester == Some(window.as_str());
                Ok(Landing {
                    window,
                    outcome: Some(outcome?),
                    in_place,
                })
            }
            Placement::New => {
                if !self.windows.has_room() {
                    return Err(IpcError {
                        code: mochi_core::ErrorCode::LimitExceeded.as_str().to_owned(),
                        message: format!(
                            "at most {} windows can be open; close one first",
                            crate::windows::MAX_WINDOWS
                        ),
                    });
                }
                let opener = requester
                    .map(str::to_owned)
                    .or_else(|| self.windows.labels().into_iter().next());
                let window = self.windows.next_label();
                // The label is taken now, under `placing`, so a second open
                // decided meanwhile gets another.
                self.windows.register(&window);
                lock(&self.opening).insert(window.clone(), path.to_path_buf());
                drop(placing);
                let host = self.quick.host();
                let made_first = requester.is_none();
                if made_first {
                    // The shell: the window comes first and says it is reading the
                    // file, which may take a while; a file that cannot be opened is
                    // then said there.
                    if !host
                        .as_ref()
                        .is_some_and(|h| h.create_window(&window, opener.as_deref()))
                    {
                        lock(&self.opening).remove(&window);
                        self.windows.release(&window);
                        return Err(IpcError {
                            code: mochi_core::ErrorCode::IoError.as_str().to_owned(),
                            message: "a new window could not be opened".to_owned(),
                        });
                    }
                }
                // A window's own open: it says it is busy itself, and the new
                // window is made only for a file that opened.
                let told = requester.unwrap_or(&window).to_owned();
                self.announce_opening(&told, path);
                let outcome = crate::route::open_path_as(archives, foreign, &window, path);
                lock(&self.opening).remove(&window);
                self.emit_quiet(&told, LaunchEvent::OpeningEnded);
                let outcome = match outcome {
                    Ok(o) => o,
                    Err(e) if made_first => {
                        // The window stays, empty, and says why.
                        self.emit(
                            &window,
                            LaunchEvent::Problem {
                                message: format!(
                                    "{} could not be opened ({}): {}",
                                    path.display(),
                                    e.code,
                                    e.message
                                ),
                            },
                        );
                        return Err(IpcError {
                            code: SAID.to_owned(),
                            message: e.message,
                        });
                    }
                    Err(e) => {
                        self.windows.release(&window);
                        return Err(e);
                    }
                };
                if !made_first && !host.is_some_and(|h| h.create_window(&window, opener.as_deref()))
                {
                    self.windows.release(&window);
                    return Err(IpcError {
                        code: mochi_core::ErrorCode::IoError.as_str().to_owned(),
                        message: "a new window could not be opened".to_owned(),
                    });
                }
                Ok(Landing {
                    window,
                    outcome: Some(outcome),
                    in_place: false,
                })
            }
        }
    }

    /// Tell the landing window what it now shows, and bring it forward.
    fn deliver(&self, landing: Landing, extract: bool) {
        self.windows_titled(&landing.window);
        match landing.outcome.and_then(|o| launch_event(o, extract)) {
            Some(event) => self.emit(&landing.window, event),
            None => self.show(&landing.window),
        }
    }

    /// Queue an event for the window `label` and bring it forward: whatever
    /// reached here needs the full window.
    fn emit(&self, label: &str, event: LaunchEvent) {
        self.emit_quiet(label, event);
        self.show(label);
    }

    /// Queue an event for the window `label`, leaving the focus where it is.
    fn emit_quiet(&self, label: &str, event: LaunchEvent) {
        {
            let mut all = lock(&self.delivery);
            let d = all.entry(label.to_owned()).or_default();
            let sent = match d.sink.clone() {
                Some(sink) if sink.send(event.clone()) => true,
                Some(_) => {
                    d.sink = None;
                    false
                }
                None => false,
            };
            if !sent {
                if d.pending.len() >= MAX_PENDING {
                    d.pending.pop_front();
                }
                d.pending.push_back(event);
            }
        }
    }

    fn problem(&self, message: impl Into<String>) {
        let window = self.notice_window();
        self.emit(
            &window,
            LaunchEvent::Problem {
                message: message.into(),
            },
        );
    }

    /// Carry out one launch. Quick jobs go to `quick`; with `concurrent`, an
    /// archive to open is read on a thread of its own (the tests read it here).
    fn handle(
        self: &Arc<Self>,
        item: Result<Launch, LaunchError>,
        quick: &dyn Fn(QuickJob),
        concurrent: bool,
    ) {
        let launch = match item {
            Ok(l) => l,
            Err(e) => {
                return self.problem(format!(
                    "MOCHI was started with arguments it cannot use: {e}."
                ))
            }
        };
        match launch.verb {
            Verb::Add => {
                // A form that is still open takes the items; otherwise a form
                // of its own is queued behind whatever the quick worker is
                // doing.
                if !self.quick.extend_prompt(&launch.paths) {
                    quick(QuickJob::AddDialog {
                        paths: launch.paths,
                    });
                }
            }
            Verb::AddNow => {
                quick(QuickJob::Add {
                    paths: launch.paths,
                });
            }
            Verb::Find => self.find(&launch.paths),
            Verb::Open => {
                // `parse` guarantees exactly one path for these verbs.
                if let Some(path) = launch.paths.first().cloned() {
                    self.run_open(concurrent, move |inner| inner.open(&path));
                }
            }
            Verb::ExtractHere | Verb::ExtractToFolder => {
                let Some(path) = launch.paths.first() else {
                    return;
                };
                if crate::route::route(path) == crate::route::Route::Foreign {
                    let path = path.clone();
                    return self.run_open(concurrent, move |inner| inner.open_foreign(true, &path));
                }
                let action = if launch.verb == Verb::ExtractHere {
                    QuickAction::ExtractHere
                } else {
                    QuickAction::ExtractToFolder
                };
                quick(QuickJob::Extract {
                    action,
                    path: path.clone(),
                });
            }
        }
    }

    /// `--find`: Find opens in an empty window, else in a new one, and never
    /// in a window that shows an archive. The folders (when any were given)
    /// become that window's own. A path that is not an existing folder is left
    /// out; if none is left the window's folders are kept and the problem is said.
    fn find(&self, paths: &[PathBuf]) {
        let window = self
            .windows
            .empty_window()
            .unwrap_or_else(|| self.fresh_window());
        let finder = Arc::clone(&self.windows.state(&window).finder);
        if !paths.is_empty() {
            if let Err(e) = finder.set_scopes(paths.to_vec()) {
                return self.problem(format!(
                    "MOCHI Find needs a folder to search: {}.",
                    e.message
                ));
            }
        }
        self.emit(
            &window,
            LaunchEvent::Find {
                folders: u32::try_from(finder.scopes().len()).unwrap_or(u32::MAX),
            },
        );
    }

    fn open(&self, path: &Path) {
        let shown = path.display();
        if crate::route::route(path) == crate::route::Route::Foreign {
            return self.open_foreign(false, path);
        }
        match std::fs::metadata(path) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return self.problem(format!("{shown} is not a file.")),
            Err(e) => return self.problem(format!("{shown} cannot be read: {e}.")),
        }
        // An Encrypted archive needs its passphrase (D5), which the shell verbs
        // cannot carry: say so instead of failing with a misleading code.
        if crate::browse::head_encrypted(path) {
            return self.problem(format!(
                "{shown} is encrypted. Use Open archive… in MOCHI and enter its passphrase; \
                 the shell's open verb cannot ask for one."
            ));
        }
        match self.open_routed(None, path) {
            Ok(landing) => self.deliver(landing, false),
            Err(e) if e.code == SAID => {}
            Err(e) => self.problem(format!(
                "{shown} could not be opened ({}): {}",
                e.code, e.message
            )),
        }
    }
}

impl Inner {
    /// Open a foreign archive for browsing through the in-app path
    /// ([`Foreign::open`]); the password, if any, is asked for in the window.
    /// `extract_verb`: the shell asked to extract, which is for `.mochi` only.
    fn open_foreign(&self, extract_verb: bool, path: &Path) {
        let shown = path.display();
        match std::fs::metadata(path) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return self.problem(format!("{shown} is not a file.")),
            Err(e) => return self.problem(format!("{shown} cannot be read: {e}.")),
        }
        match self.open_routed(None, path) {
            Ok(landing) => {
                if extract_verb && matches!(landing.outcome, Some(OpenOutcome::Foreign { .. })) {
                    self.emit(
                        &landing.window,
                        LaunchEvent::Problem {
                            message: format!(
                                "{shown}: extracting from the shell is for .mochi archives. \
                                 It is open for browsing; use Extract in the foreign archive section."
                            ),
                        },
                    );
                }
                self.deliver(landing, false);
            }
            Err(e) if e.code == SAID => {}
            Err(e) => self.problem(format!(
                "{shown} could not be opened ({}): {}",
                e.code, e.message
            )),
        }
    }
}

/// The foreign format a name suggests, by extension only. A name is a hint
/// for routing, never proof: the helper decides what the bytes are.
pub fn foreign_kind(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    const SUFFIXES: [(&str, &str); 9] = [
        (".zip", "ZIP"),
        (".7z", "7z"),
        (".rar", "RAR"),
        (".tar.gz", "tar.gz"),
        (".tgz", "tar.gz"),
        (".tar.zst", "tar.zst"),
        (".tzst", "tar.zst"),
        (".tar", "tar"),
        (".gz", "gzip"),
    ];
    SUFFIXES
        .iter()
        .find(|(suffix, _)| name.ends_with(suffix))
        .map(|(_, kind)| *kind)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::extract::Destination;
    use crate::foreign::Foreign;
    use crate::jobs::JobRegistry;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn cwd() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\work")
        } else {
            PathBuf::from("/work")
        }
    }

    fn abs(p: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\{p}"))
        } else {
            PathBuf::from(format!("/{p}"))
        }
    }

    fn parsed(args: &[&str]) -> Result<Option<Launch>, LaunchError> {
        parse(args, &cwd())
    }

    #[test]
    fn no_arguments_is_a_plain_start() {
        assert_eq!(parsed(&[]), Ok(None));
    }

    #[test]
    fn a_bare_path_opens_it_and_each_verb_is_recognised() {
        let a = abs("a.mochi").to_string_lossy().into_owned();
        let l = parsed(&[&a]).unwrap().unwrap();
        assert_eq!(l.verb, Verb::Open);
        assert_eq!(l.paths, vec![abs("a.mochi")]);
        for (opt, verb) in [
            ("--open", Verb::Open),
            ("--extract-here", Verb::ExtractHere),
            ("--extract-to-folder", Verb::ExtractToFolder),
            ("--add", Verb::Add),
            ("--add-now", Verb::AddNow),
        ] {
            let l = parsed(&[opt, "--", &a]).unwrap().unwrap();
            assert_eq!(l.verb, verb);
            assert_eq!(l.paths, vec![abs("a.mochi")]);
        }
    }

    #[test]
    fn a_relative_path_is_resolved_against_the_launchers_directory() {
        let l = parsed(&["--open", "x/y.mochi"]).unwrap().unwrap();
        assert_eq!(l.paths, vec![cwd().join("x/y.mochi")]);
        // A relative path with no usable directory is refused.
        assert!(parse(&["y.mochi"], Path::new("")).is_err());
    }

    #[test]
    fn unknown_verbs_and_options_are_refused_by_name() {
        for bad in ["--extract", "--ADD", "--add=1", "-x", "--verb", "--help"] {
            let e = parsed(&[bad, "a.mochi"]).unwrap_err();
            assert!(e.0.contains("unknown option"), "{bad}: {e}");
        }
        // A lone dash is a path, as it is everywhere.
        assert!(parsed(&["-"]).unwrap().is_some());
    }

    #[test]
    fn a_passphrase_option_is_refused_and_its_value_never_echoed() {
        for bad in [
            "--passphrase=hunter2",
            "--password=hunter2",
            "--passphrase-env=PW",
            "--key=hunter2",
            "--Passphrase=hunter2",
        ] {
            let e = parsed(&[bad, "a.mochi"]).unwrap_err();
            assert!(e.0.contains("passphrase"), "{bad}: {e}");
            assert!(
                !e.0.contains("hunter2") && !e.0.contains("PW"),
                "{bad}: {e}"
            );
        }
        // The next argument is not read as a path once an option failed.
        assert!(parsed(&["--passphrase", "hunter2"]).is_err());
    }

    #[test]
    fn unknown_option_errors_do_not_echo_values() {
        let e = parsed(&["--frobnicate=hunter2"]).unwrap_err();
        assert!(
            e.0.contains("--frobnicate") && !e.0.contains("hunter2"),
            "{e}"
        );
    }

    #[test]
    fn hostile_paths_are_refused_or_kept_as_plain_data() {
        assert!(parsed(&[""]).is_err(), "empty");
        assert!(parsed(&["a\0b"]).is_err(), "NUL");
        assert!(parsed(&[r"\\.\pipe\mochi"]).is_err(), "device path");
        let long = "a".repeat(MAX_PATH_BYTES + 1);
        assert!(parsed(&[long.as_str()]).is_err(), "too long");
        let many: Vec<String> = (0..=MAX_STAGED).map(|i| format!("f{i}")).collect();
        let mut args = vec!["--add".to_owned()];
        args.extend(many);
        assert!(parse(&args, &cwd()).is_err(), "too many");
        // After `--`, a path that looks like an option is a path.
        let l = parsed(&["--open", "--", "--passphrase=x"])
            .unwrap()
            .unwrap();
        assert_eq!(l.paths, vec![cwd().join("--passphrase=x")]);
        // Shell metacharacters, newlines, bidi controls, and `..` are bytes
        // of a name, not syntax: kept exactly, never expanded or normalised.
        let odd = "a; rm -rf ~ | b\n\u{202e}gpj.mochi";
        let l = parsed(&["--open", "--", odd]).unwrap().unwrap();
        assert_eq!(l.paths, vec![cwd().join(odd)]);
        let l = parsed(&["--open", "--", "../up/../x.mochi"])
            .unwrap()
            .unwrap();
        assert_eq!(l.paths, vec![cwd().join("../up/../x.mochi")]);
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_survives() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(b"/tmp/bad\xffname.mochi".to_vec());
        let l = parse(std::slice::from_ref(&raw), &cwd()).unwrap().unwrap();
        assert_eq!(l.paths, vec![PathBuf::from(raw)]);
    }

    #[test]
    fn verbs_check_their_path_counts_and_do_not_combine() {
        assert!(parsed(&["--open"]).is_err(), "a verb needs a path");
        assert!(parsed(&["--add"]).is_err());
        assert!(parsed(&["--add-now"]).is_err());
        assert!(
            parsed(&["--extract-here", "a", "b"]).is_err(),
            "one archive"
        );
        assert!(parsed(&["a.mochi", "b.mochi"]).is_err(), "one archive");
        assert!(parsed(&["--open", "--add", "a"]).is_err(), "one verb");
        let l = parsed(&["--add", "a", "b", "c"]).unwrap().unwrap();
        assert_eq!(l.paths.len(), 3);
        let l = parsed(&["--add-now", "a", "b"]).unwrap().unwrap();
        assert_eq!((l.verb, l.paths.len()), (Verb::AddNow, 2));
        assert!(!Verb::Open.is_quick());
        assert!(
            Verb::Add.is_quick(),
            "Add to archive... is the compact form"
        );
        assert!(Verb::ExtractHere.is_quick() && Verb::ExtractToFolder.is_quick());
        assert!(Verb::AddNow.is_quick());
    }

    #[test]
    fn find_takes_any_number_of_folders_and_none_opens_the_tool() {
        let a = abs("music").to_string_lossy().into_owned();
        let b = abs("photos").to_string_lossy().into_owned();
        let l = parsed(&["--find", "--", &a, &b]).unwrap().unwrap();
        assert_eq!(l.verb, Verb::Find);
        assert_eq!(l.paths, vec![abs("music"), abs("photos")]);
        // The Linux launcher's `%F` can expand to nothing, with or without `--`.
        for args in [&["--find"][..], &["--find", "--"][..]] {
            let l = parsed(args).unwrap().unwrap();
            assert_eq!((l.verb, l.paths.len()), (Verb::Find, 0));
        }
        // A relative folder is resolved against the launcher's directory.
        let l = parsed(&["--find", "--", "docs"]).unwrap().unwrap();
        assert_eq!(l.paths, vec![cwd().join("docs")]);
        // A folder named like an option stays a path after `--`.
        let l = parsed(&["--find", "--", "-rf"]).unwrap().unwrap();
        assert_eq!(l.paths, vec![cwd().join("-rf")]);
        assert!(!Verb::Find.is_quick(), "Find uses the main window");
        // It does not combine with another verb, and the strict checks still apply.
        assert!(parsed(&["--find", "--add", &a]).is_err());
        assert!(parsed(&["--open", "--find", &a]).is_err());
        assert!(parsed(&["--find", "--passphrase=hunter2"]).is_err());
        assert!(parsed(&["--find=1", &a]).is_err());
        assert!(parsed(&["--FIND", &a]).is_err());
        assert!(parsed(&["--find", ""]).is_err(), "an empty path");
        assert!(parsed(&["--find", "a\0b"]).is_err(), "a NUL");
        let device = r"\\.\pipe\x";
        assert!(parsed(&["--find", "--", device]).is_err(), "a device path");
        let long = "a".repeat(MAX_PATH_BYTES + 1);
        assert!(parsed(&["--find", "--", &long]).is_err());
        assert!(parse(&["--find", "--", "rel"], Path::new("")).is_err());
        let many: Vec<String> = (0..=MAX_STAGED)
            .map(|i| abs(&format!("d{i}")).to_string_lossy().into_owned())
            .collect();
        let mut args = vec!["--find", "--"];
        args.extend(many.iter().map(String::as_str));
        assert!(parse(&args, &cwd()).is_err(), "too many folders");
    }

    #[test]
    fn a_second_instances_payload_is_parsed_with_its_own_directory() {
        let argv = vec![
            "mochi-desktop".to_owned(),
            "--extract-here".to_owned(),
            "--".to_owned(),
            "one.mochi".to_owned(),
        ];
        let l = from_forwarded(&argv, &cwd().to_string_lossy())
            .unwrap()
            .unwrap();
        assert_eq!(l.verb, Verb::ExtractHere);
        assert_eq!(l.paths, vec![cwd().join("one.mochi")]);
        // Only the program name: nothing to do (the window is just focused).
        assert_eq!(from_forwarded(&["mochi-desktop".to_owned()], "/"), Ok(None));
        assert_eq!(from_forwarded(&[], "/"), Ok(None));
        // An unparsable payload is an error the window reports.
        assert!(from_forwarded(&["m".into(), "--passphrase=x".into()], "/").is_err());
    }

    #[test]
    fn foreign_formats_are_recognised_by_name_only() {
        for (name, kind) in [
            ("a.ZIP", "ZIP"),
            ("a.7z", "7z"),
            ("a.rar", "RAR"),
            ("a.tar.gz", "tar.gz"),
            ("a.tgz", "tar.gz"),
            ("a.tar.zst", "tar.zst"),
            ("a.TZST", "tar.zst"),
            ("a.tar", "tar"),
        ] {
            assert_eq!(foreign_kind(Path::new(name)), Some(kind), "{name}");
        }
        assert_eq!(foreign_kind(Path::new("a.mochi")), None);
        assert_eq!(foreign_kind(Path::new("zip")), None);
        // A bare .zst is a compressed single file, not a tar archive.
        assert_eq!(foreign_kind(Path::new("a.zst")), None);
    }

    // ---- handling --------------------------------------------------------

    struct Sink(Mutex<Vec<LaunchEvent>>, AtomicBool);

    impl Sink {
        fn live() -> Arc<Sink> {
            Arc::new(Sink(Mutex::default(), AtomicBool::new(true)))
        }
        /// What the window was told, without the busy notices around a read
        /// (`opening_is_announced_around_the_read` checks those).
        fn events(&self) -> Vec<LaunchEvent> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|e| !matches!(e, LaunchEvent::Opening { .. } | LaunchEvent::OpeningEnded))
                .cloned()
                .collect()
        }
        fn all_events(&self) -> Vec<LaunchEvent> {
            self.0.lock().unwrap().clone()
        }
    }

    impl LaunchSink for Sink {
        fn send(&self, event: LaunchEvent) -> bool {
            if !self.1.load(Ordering::SeqCst) {
                return false;
            }
            self.0.lock().unwrap().push(event);
            true
        }
    }

    /// Collects what the compact window would hear.
    struct QSink(Mutex<Vec<crate::quick::QuickEvent>>);

    impl QSink {
        fn live() -> Arc<QSink> {
            Arc::new(QSink(Mutex::default()))
        }
        fn events(&self) -> Vec<crate::quick::QuickEvent> {
            self.0.lock().unwrap().clone()
        }
    }

    impl crate::quick::QuickSink for QSink {
        fn send(&self, event: crate::quick::QuickEvent) -> bool {
            self.0.lock().unwrap().push(event);
            true
        }
    }

    /// The helper binary next to the test executable, built on demand when
    /// only this package's tests were asked for.
    fn helper() -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let name = if cfg!(windows) {
            "mochi-foreign-helper.exe"
        } else {
            "mochi-foreign-helper"
        };
        let path = exe.parent().unwrap().parent().unwrap().join(name);
        if !path.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let ok = std::process::Command::new(cargo)
                .args(["build", "-p", "mochi-foreign"])
                .status()
                .unwrap()
                .success();
            assert!(
                ok && path.exists(),
                "could not build the helper at {path:?}"
            );
        }
        path
    }

    fn fx(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../fixtures/foreign")
            .join(name)
    }

    fn launches() -> Launches {
        launches_with().0
    }

    fn launches_with() -> (Launches, Arc<Foreign>) {
        let (l, foreign, _) = launches_full();
        (l, foreign)
    }

    fn launches_full() -> (Launches, Arc<Foreign>, Arc<Destination>) {
        let foreign = Arc::new(Foreign::default());
        foreign.set_helper(helper());
        let windows = windows_for(Arc::new(Archives::default()), Arc::clone(&foreign));
        let destination = Arc::clone(&windows.state(MAIN_LABEL).destination);
        let l = Launches::new(windows, crate::quick::QuickPool::new());
        (l, foreign, destination)
    }

    /// Windows the app would have made, recorded.
    #[derive(Default)]
    struct FakeHost {
        created: Mutex<Vec<(String, Option<String>)>>,
        focused: Mutex<Vec<String>>,
        titles: Mutex<Vec<(String, String)>>,
        /// Make `create_window` fail.
        refuse: AtomicBool,
        /// Windows that no longer exist.
        gone: Mutex<Vec<String>>,
    }

    impl crate::quick::WindowHost for FakeHost {
        fn show_quick(&self, _: &str, _: crate::quick::QuickSize) {}
        fn close_quick(&self, _: &str) {}
        fn show_main(&self) {}
        fn open_folder(&self, _: &Path) -> std::io::Result<()> {
            Ok(())
        }
        fn create_window(&self, label: &str, opener: Option<&str>) -> bool {
            if self.refuse.load(Ordering::SeqCst) {
                return false;
            }
            self.created
                .lock()
                .unwrap()
                .push((label.to_owned(), opener.map(str::to_owned)));
            true
        }
        fn focus_window(&self, label: &str) -> bool {
            self.focused.lock().unwrap().push(label.to_owned());
            true
        }
        fn window_exists(&self, label: &str) -> bool {
            !self.gone.lock().unwrap().iter().any(|g| g == label)
        }
        fn set_title(&self, label: &str, title: &str) {
            self.titles
                .lock()
                .unwrap()
                .push((label.to_owned(), title.to_owned()));
        }
    }

    fn with_host(l: &Launches) -> Arc<FakeHost> {
        let host = Arc::new(FakeHost::default());
        l.inner.quick.set_host(host.clone());
        host
    }

    /// What one window has been told so far.
    fn heard(l: &Launches, label: &str) -> Vec<LaunchEvent> {
        let sink = Sink::live();
        l.watch(label, sink.clone());
        sink.events()
    }

    /// The windows of a test: one empty archive window, `main`.
    fn windows_for(archives: Arc<Archives>, foreign: Arc<Foreign>) -> Arc<WindowStates> {
        let windows = Arc::new(WindowStates::new(
            archives,
            foreign,
            Arc::new(JobRegistry::default()),
        ));
        windows.register(MAIN_LABEL);
        windows
    }

    fn write_archive(dir: &Path, name: &str) -> PathBuf {
        use mochi_core::catalog::path::ArchivePath;
        use mochi_core::manifest::Attributes;
        use mochi_core::publish::{ArchiveWriter, Transaction};
        use mochi_core::storage::os::OsStorage;
        use mochi_testkit::archive::{test_options, Job};
        use mochi_testkit::SeqIds;
        let path = dir.join(name);
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(&path).unwrap(),
            Box::new(SeqIds::new(9)),
            test_options(),
        )
        .unwrap();
        let mut tx = Transaction::new();
        tx.put_file(
            ArchivePath::from_stored(b"a.txt").unwrap(),
            b"alpha".to_vec(),
            Attributes::default(),
        );
        w.commit(tx, &Job::new().ctx()).unwrap();
        w.close().unwrap();
        path
    }

    #[test]
    fn events_before_the_window_listens_wait_for_it_in_order() {
        let l = launches();
        l.handle_now(Err(LaunchError("first".into())));
        l.handle_now(Err(LaunchError("second".into())));
        let sink = Sink::live();
        l.watch(MAIN_LABEL, sink.clone());
        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], LaunchEvent::Problem { message } if message.contains("first"))
        );
        assert!(
            matches!(&events[1], LaunchEvent::Problem { message } if message.contains("second"))
        );
        // Later events go straight through.
        l.handle_now(Err(LaunchError("third".into())));
        assert_eq!(sink.events().len(), 3);
    }

    #[test]
    fn a_gone_listener_is_forgotten_and_the_event_kept() {
        let l = launches();
        let sink = Sink::live();
        l.watch(MAIN_LABEL, sink.clone());
        sink.1.store(false, Ordering::SeqCst);
        l.handle_now(Err(LaunchError("kept".into())));
        let next = Sink::live();
        l.watch(MAIN_LABEL, next.clone());
        assert_eq!(next.events().len(), 1);
    }

    #[test]
    fn a_foreign_archive_opens_in_the_foreign_browser_not_the_mochi_one() {
        let (l, foreign) = launches_with();
        let host = with_host(&l);
        for name in [
            "basic_infozip.zip",
            "basic.tar",
            "basic.tar.gz",
            "7z/basic.7z",
        ] {
            l.handle_now(Ok(Launch {
                verb: Verb::Open,
                paths: vec![fx(name)],
            }));
        }
        // The first is shown in the empty window; each next one gets a window
        // of its own, opened from the one used last.
        assert_eq!(
            *host.created.lock().unwrap(),
            [
                ("win-1".to_owned(), Some("main".to_owned())),
                ("win-2".to_owned(), Some("win-1".to_owned())),
                ("win-3".to_owned(), Some("win-2".to_owned())),
            ]
        );
        let events: Vec<LaunchEvent> = ["main", "win-1", "win-2", "win-3"]
            .iter()
            .flat_map(|w| {
                let heard = heard(&l, w);
                assert_eq!(heard.len(), 1, "{w}: {heard:?}");
                heard
            })
            .collect();
        let formats: Vec<&str> = events
            .iter()
            .map(|e| match e {
                LaunchEvent::ForeignOpened { archive } => archive.format.as_str(),
                other => panic!("expected ForeignOpened, got {other:?}"),
            })
            .collect();
        assert_eq!(formats.len(), 4);
        assert_eq!(formats[0], "ZIP");
        assert_eq!(formats[3], "7z");
        assert_eq!(foreign.open_count(), 4, "opened by the in-app service");
        // The foreign wording: no MOCHI health dimension is claimed.
        if let LaunchEvent::ForeignOpened { archive } = &events[0] {
            assert_eq!(archive.dimensions.recoverability, "UNSUPPORTED");
        }
    }

    #[test]
    fn extract_verbs_on_a_foreign_archive_extract_nothing_and_open_it_for_browsing() {
        let (l, foreign, dest) = launches_full();
        with_host(&l);
        let dir = tempfile::tempdir().unwrap();
        for (verb, name) in [
            (Verb::ExtractHere, "photos.zip"),
            (Verb::ExtractToFolder, "pictures.zip"),
        ] {
            let zip = dir.path().join(name);
            std::fs::copy(fx("basic_infozip.zip"), &zip).unwrap();
            l.handle_now(Ok(Launch {
                verb,
                paths: vec![zip],
            }));
        }
        let mut events = heard(&l, "main");
        events.extend(heard(&l, "win-1"));
        assert_eq!(events.len(), 4, "{events:?}");
        for pair in events.chunks(2) {
            assert!(
                matches!(&pair[0], LaunchEvent::Problem { message }
                    if message.contains("extracting from the shell is for .mochi")),
                "{pair:?}"
            );
            assert!(
                matches!(&pair[1], LaunchEvent::ForeignOpened { .. }),
                "{pair:?}"
            );
        }
        assert_eq!(foreign.open_count(), 2);
        // No folder was made and none chosen.
        assert!(!dir.path().join("photos").exists());
        assert!(dest.get().is_none());
    }

    #[test]
    fn an_encrypted_foreign_archive_asks_for_its_password_in_the_window() {
        let (l, foreign) = launches_with();
        let sink = Sink::live();
        l.watch(MAIN_LABEL, sink.clone());
        let p = fx("7z/enc_names.7z");
        l.handle_now(Ok(Launch {
            verb: Verb::Open,
            paths: vec![p.clone()],
        }));
        assert_eq!(
            sink.events(),
            vec![LaunchEvent::ForeignPassword { wrong: false }]
        );
        assert_eq!(foreign.open_count(), 0);
        // The retry is the in-app one: the file is remembered in Rust.
        assert_eq!(
            foreign.pending_for(MAIN_LABEL).as_deref(),
            Some(p.as_path())
        );
        let s = foreign.open(&p, Some(b"SECRET".to_vec())).unwrap();
        assert!(s.opened_with_password);
    }

    #[test]
    fn foreign_variants_d8_does_not_support_keep_their_typed_refusal() {
        let (l, foreign) = launches_with();
        let sink = Sink::live();
        l.watch(MAIN_LABEL, sink.clone());
        let dir = tempfile::tempdir().unwrap();
        // A `.zip` that is not one, and a missing file.
        let fake = dir.path().join("fake.zip");
        std::fs::copy(fx("garbage.bin"), &fake).unwrap();
        let split = dir.path().join("split.zip");
        std::fs::copy(fx("split_last.zip"), &split).unwrap();
        for path in [fake, split, dir.path().join("missing.zip")] {
            l.handle_now(Ok(Launch {
                verb: Verb::Open,
                paths: vec![path],
            }));
        }
        let events = sink.events();
        assert_eq!(events.len(), 3);
        assert!(
            matches!(&events[0], LaunchEvent::Problem { message }
                if message.contains("could not be opened (NOT_AN_ARCHIVE)")),
            "{events:?}"
        );
        assert!(
            matches!(&events[1], LaunchEvent::Problem { message }
                if message.contains("could not be opened (UNSUPPORTED")),
            "{events:?}"
        );
        assert!(
            matches!(&events[2], LaunchEvent::Problem { message } if message.contains("cannot be read"))
        );
        assert_eq!(foreign.open_count(), 0);
    }

    #[test]
    fn a_missing_file_or_a_folder_is_a_problem_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let l = launches();
        let sink = Sink::live();
        l.watch(MAIN_LABEL, sink.clone());
        l.handle_now(Ok(Launch {
            verb: Verb::Open,
            paths: vec![dir.path().join("missing.mochi")],
        }));
        l.handle_now(Ok(Launch {
            verb: Verb::Open,
            paths: vec![dir.path().to_path_buf()],
        }));
        // A file that is not an archive.
        let junk = dir.path().join("junk.mochi");
        std::fs::write(&junk, b"this is not an archive").unwrap();
        l.handle_now(Ok(Launch {
            verb: Verb::Open,
            paths: vec![junk],
        }));
        let events = sink.events();
        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .all(|e| matches!(e, LaunchEvent::Problem { .. })),
            "{events:?}"
        );
    }

    #[test]
    fn open_shows_the_archive_read_only_in_the_main_window() {
        let dir = tempfile::tempdir().unwrap();
        let archive = write_archive(dir.path(), "photos.mochi");
        let l = launches();
        let sink = Sink::live();
        l.watch(MAIN_LABEL, sink.clone());
        l.handle_now(Ok(Launch {
            verb: Verb::Open,
            paths: vec![archive.clone()],
        }));
        let events = sink.events();
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            LaunchEvent::Opened { archive, extract } => {
                assert_eq!(archive.file_name, "photos.mochi");
                assert!(!extract);
            }
            other => panic!("{other:?}"),
        }
        assert!(l
            .inner
            .windows
            .state(MAIN_LABEL)
            .destination
            .get()
            .is_none());
        // Opening is read-only: nothing was extracted.
        assert!(!dir.path().join("a.txt").exists());
    }

    fn dirs(root: &Path, names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| {
                let d = root.join(n);
                std::fs::create_dir_all(&d).unwrap();
                d
            })
            .collect()
    }

    #[test]
    fn find_sets_the_folders_in_an_empty_window_and_tells_it_only_how_many() {
        let dir = tempfile::tempdir().unwrap();
        let two = dirs(dir.path(), &["one", "two"]);
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let l = launches();
        let host = with_host(&l);
        let sink = Sink::live();
        l.watch(MAIN_LABEL, sink.clone());
        let finder = Arc::clone(&l.inner.windows.state(MAIN_LABEL).finder);

        let mut paths = two.clone();
        paths.push(file.clone());
        l.handle_now(Ok(Launch {
            verb: Verb::Find,
            paths,
        }));
        assert_eq!(sink.events(), vec![LaunchEvent::Find { folders: 2 }]);
        assert!(
            host.created.lock().unwrap().is_empty(),
            "main was empty: reused"
        );
        let shown: Vec<String> = finder.scopes().into_iter().map(|s| s.path).collect();
        assert_eq!(
            shown,
            two.iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
            "a file among the paths is left out"
        );

        // No folder: the folders stay as they were and the tool opens.
        l.handle_now(Ok(Launch {
            verb: Verb::Find,
            paths: Vec::new(),
        }));
        assert_eq!(
            sink.events().last(),
            Some(&LaunchEvent::Find { folders: 2 })
        );

        // Nothing usable: said as a problem, the folders are kept.
        l.handle_now(Ok(Launch {
            verb: Verb::Find,
            paths: vec![file, dir.path().join("missing")],
        }));
        assert!(matches!(
            sink.events().last(),
            Some(LaunchEvent::Problem { message }) if message.contains("needs a folder")
        ));
        assert_eq!(finder.scopes().len(), 2);
    }

    #[test]
    fn find_never_takes_over_a_window_that_shows_an_archive() {
        let dir = tempfile::tempdir().unwrap();
        let archive = write_archive(dir.path(), "busy.mochi");
        let folders = dirs(dir.path(), &["f"]);
        let l = launches();
        let host = with_host(&l);
        l.inner
            .windows
            .archives()
            .open_as(
                MAIN_LABEL,
                &archive,
                mochi_core::publish::ReadOptions::default(),
            )
            .unwrap();
        l.handle_now(Ok(Launch {
            verb: Verb::Find,
            paths: folders,
        }));
        let created = host.created.lock().unwrap().clone();
        assert_eq!(created.len(), 1, "a new window");
        let new = created[0].0.clone();
        assert_ne!(new, MAIN_LABEL);
        assert_eq!(heard(&l, &new), vec![LaunchEvent::Find { folders: 1 }]);
        assert!(
            heard(&l, MAIN_LABEL).is_empty(),
            "the busy window is not told"
        );
        assert!(l.inner.windows.state(MAIN_LABEL).finder.scopes().is_empty());
        assert_eq!(l.inner.windows.state(&new).finder.scopes().len(), 1);
    }

    fn entry(path: &str) -> FoundEntry {
        FoundEntry {
            key: crate::browse::hex(path.as_bytes()),
            path: NameText::of(path.as_bytes()),
            kind: EntryKindView::File,
        }
    }

    #[test]
    fn a_hit_opens_in_the_empty_window_that_asked_and_that_window_is_told_to_reveal_it() {
        let dir = tempfile::tempdir().unwrap();
        let archive = write_archive(dir.path(), "hit.mochi");
        let l = launches();
        let host = with_host(&l);
        let sink = Sink::live();
        l.watch(MAIN_LABEL, sink.clone());
        let outcome = l
            .open_hit_in_window(MAIN_LABEL, &archive, entry("a.txt"))
            .unwrap();
        let OpenOutcome::Opened { archive: summary } = outcome else {
            panic!("expected the archive to open in place: {outcome:?}");
        };
        assert!(host.created.lock().unwrap().is_empty());
        let events = sink.events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(
            &events[0],
            LaunchEvent::Reveal { handle, key, kind: EntryKindView::File, .. }
                if *handle == summary.handle && *key == crate::browse::hex(b"a.txt")
        ));
    }

    #[test]
    fn a_hit_focuses_the_window_that_already_has_the_archive_and_reveals_there() {
        let dir = tempfile::tempdir().unwrap();
        let archive = write_archive(dir.path(), "has.mochi");
        let l = launches();
        let host = with_host(&l);
        l.inner.windows.register("win-1");
        let opened = l
            .inner
            .windows
            .archives()
            .open_as(
                "win-1",
                &archive,
                mochi_core::publish::ReadOptions::default(),
            )
            .unwrap();
        let outcome = l
            .open_hit_in_window(MAIN_LABEL, &archive, entry("a.txt"))
            .unwrap();
        assert_eq!(outcome, OpenOutcome::Elsewhere);
        assert!(host.created.lock().unwrap().is_empty(), "no second copy");
        assert!(host.focused.lock().unwrap().contains(&"win-1".to_owned()));
        // The window is told to show the archive again (under the same
        // handle: its page may have gone back to the start screen), then where.
        let told = heard(&l, "win-1");
        assert!(
            matches!(
                told.as_slice(),
                [
                    LaunchEvent::Opened { archive, extract: false },
                    LaunchEvent::Reveal { handle, .. },
                ] if archive.handle == opened.handle && *handle == opened.handle
            ),
            "{told:?}"
        );
        assert!(heard(&l, MAIN_LABEL).is_empty());
    }

    #[test]
    fn a_hit_asked_in_a_busy_window_opens_in_a_new_window_and_reveals_there() {
        let dir = tempfile::tempdir().unwrap();
        let busy = write_archive(dir.path(), "busy.mochi");
        let hit = write_archive(dir.path(), "hit.mochi");
        let l = launches();
        let host = with_host(&l);
        l.inner
            .windows
            .archives()
            .open_as(
                MAIN_LABEL,
                &busy,
                mochi_core::publish::ReadOptions::default(),
            )
            .unwrap();
        let outcome = l
            .open_hit_in_window(MAIN_LABEL, &hit, entry("a.txt"))
            .unwrap();
        assert_eq!(outcome, OpenOutcome::Elsewhere);
        let created = host.created.lock().unwrap().clone();
        assert_eq!(created.len(), 1);
        let new = created[0].0.clone();
        let told = heard(&l, &new);
        assert!(
            matches!(
                told.as_slice(),
                [LaunchEvent::Opened { .. }, LaunchEvent::Reveal { .. }]
            ),
            "{told:?}"
        );
        assert!(
            heard(&l, MAIN_LABEL).is_empty(),
            "the busy window keeps its archive"
        );
    }

    #[test]
    fn extract_verbs_run_in_the_compact_window_and_leave_the_main_one_alone() {
        let dir = tempfile::tempdir().unwrap();
        let archive = write_archive(dir.path(), "photos.mochi");
        let l = launches();
        let main = Sink::live();
        l.watch(MAIN_LABEL, main.clone());
        let compact = QSink::live();
        l.inner.quick.watch(compact.clone());
        for verb in [Verb::ExtractHere, Verb::ExtractToFolder] {
            l.handle_now(Ok(Launch {
                verb,
                paths: vec![archive.clone()],
            }));
        }
        assert!(main.events().is_empty(), "{:?}", main.events());
        let done = compact
            .events()
            .into_iter()
            .filter(|e| matches!(e, crate::quick::QuickEvent::Done { .. }))
            .count();
        assert_eq!(done, 2);
        // Written at once: here, and into a new folder named for the archive.
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"alpha");
        assert_eq!(
            std::fs::read(dir.path().join("photos").join("a.txt")).unwrap(),
            b"alpha"
        );
        assert!(l
            .inner
            .windows
            .state(MAIN_LABEL)
            .destination
            .get()
            .is_none());
    }

    #[test]
    fn a_collision_hands_the_extract_plan_to_the_main_window() {
        let dir = tempfile::tempdir().unwrap();
        let archive = write_archive(dir.path(), "photos.mochi");
        std::fs::write(dir.path().join("a.txt"), b"mine").unwrap();
        let l = launches();
        let main = Sink::live();
        l.watch(MAIN_LABEL, main.clone());
        let compact = QSink::live();
        l.inner.quick.watch(compact.clone());
        l.handle_now(Ok(Launch {
            verb: Verb::ExtractHere,
            paths: vec![archive],
        }));
        assert!(matches!(
            compact.events().last(),
            Some(crate::quick::QuickEvent::Fallback { conflicts: 1, .. })
        ));
        let events = main.events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(
            &events[0],
            LaunchEvent::Opened { extract: true, archive } if archive.file_name == "photos.mochi"
        ));
        assert_eq!(
            l.inner.windows.state(MAIN_LABEL).destination.get(),
            Some(dir.path().to_path_buf())
        );
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"mine");
    }

    #[test]
    fn add_now_launches_in_one_selection_make_one_archive() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("trip");
        std::fs::create_dir(&folder).unwrap();
        let files: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|n| {
                let p = folder.join(n);
                std::fs::write(&p, n.as_bytes()).unwrap();
                p
            })
            .collect();
        let l = Launches::with_batch_window(
            windows_for(Arc::new(Archives::default()), Arc::new(Foreign::default())),
            crate::quick::QuickPool::new(),
            Duration::from_millis(300),
        );
        let compact = QSink::live();
        l.inner.quick.watch(compact.clone());
        // One process per item, a few milliseconds apart, as Explorer does.
        for f in &files {
            l.submit(parse(
                &[OsStr::new("--add-now"), OsStr::new("--"), f.as_os_str()],
                &cwd(),
            ));
            std::thread::sleep(Duration::from_millis(20));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let done = loop {
            let events = compact.events();
            if let Some(e) = events
                .into_iter()
                .find(|e| matches!(e, crate::quick::QuickEvent::Done { .. }))
            {
                break e;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{:?}",
                compact.events()
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let crate::quick::QuickEvent::Done { name, files, .. } = done else {
            unreachable!()
        };
        assert_eq!(name.text, "trip.mochi");
        assert_eq!(files, 3);
        // One archive, not three.
        let archives: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".mochi"))
            .collect();
        assert_eq!(archives, ["trip.mochi"]);
    }

    #[test]
    fn a_launch_that_is_not_an_add_keeps_its_place_during_collection() {
        let dir = tempfile::tempdir().unwrap();
        let archive = write_archive(dir.path(), "a.mochi");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, b"x").unwrap();
        let l = Launches::with_batch_window(
            windows_for(Arc::new(Archives::default()), Arc::new(Foreign::default())),
            crate::quick::QuickPool::new(),
            Duration::from_millis(200),
        );
        let main = Sink::live();
        l.watch(MAIN_LABEL, main.clone());
        let compact = QSink::live();
        l.inner.quick.watch(compact.clone());
        l.submit(parse(&[OsStr::new("--add-now"), file.as_os_str()], &cwd()));
        l.submit(parse(&[archive.as_os_str()], &cwd()));
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while (main.events().is_empty()
            || !compact
                .events()
                .iter()
                .any(|e| matches!(e, crate::quick::QuickEvent::Done { .. })))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(&main.events()[0], LaunchEvent::Opened { .. }));
        assert!(dir.path().join("note.mochi").is_file());
    }

    /// Opens from the shell run beside each other (owner report 2026-11-12: a slow
    /// archive held up the next), so which one takes the empty window is not fixed; each
    /// still gets a window of its own, and neither is opened twice.
    #[test]
    fn two_launches_each_get_a_window_of_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_archive(dir.path(), "a.mochi");
        let b = write_archive(dir.path(), "b.mochi");
        let l = launches();
        with_host(&l);
        let first = Sink::live();
        l.watch(MAIN_LABEL, first.clone());
        let second = Sink::live();
        l.watch("win-1", second.clone());
        for p in [&a, &b] {
            l.submit(parse(&[p.as_os_str()], &cwd()));
        }
        l.submit(Ok(None));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while (first.events().is_empty() || second.events().is_empty())
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // One takes the empty window, the other a new one.
        let mut names: Vec<String> = first
            .events()
            .into_iter()
            .chain(second.events())
            .map(|e| match e {
                LaunchEvent::Opened { archive, .. } => archive.file_name,
                other => panic!("{other:?}"),
            })
            .collect();
        names.sort();
        assert_eq!(names, ["a.mochi", "b.mochi"]);
    }

    /// Wait until `want` finds an event in the compact window's stream.
    fn wait_quick(
        sink: &QSink,
        want: impl Fn(&crate::quick::QuickEvent) -> bool,
    ) -> crate::quick::QuickEvent {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(e) = sink.events().into_iter().find(|e| want(e)) {
                return e;
            }
            assert!(std::time::Instant::now() < deadline, "{:?}", sink.events());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn add_form_launches(batch: Duration) -> (Launches, Arc<QSink>) {
        let l = Launches::with_batch_window(
            windows_for(Arc::new(Archives::default()), Arc::new(Foreign::default())),
            crate::quick::QuickPool::new(),
            batch,
        );
        let compact = QSink::live();
        l.inner.quick.watch(compact.clone());
        (l, compact)
    }

    fn zip_request(name: &str) -> crate::quick::QuickAddRequest {
        crate::quick::QuickAddRequest {
            name: name.to_owned(),
            format: crate::ipc::ArchiveFormat::Zip,
            level: crate::ipc::CompressionPreset::Balanced,
            passphrase: None,
            recovery_percent: None,
        }
    }

    #[test]
    fn add_is_a_quick_verb_and_never_opens_the_main_window() {
        assert!(Verb::Add.is_quick());
    }

    #[test]
    fn add_launches_of_one_selection_make_one_form_and_nothing_is_written_until_ok() {
        use crate::quick::QuickEvent;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("trip");
        std::fs::create_dir(&folder).unwrap();
        let files: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|n| {
                let p = folder.join(n);
                std::fs::write(&p, n.as_bytes()).unwrap();
                p
            })
            .collect();
        let (l, compact) = add_form_launches(Duration::from_millis(300));
        // One process per item, a few milliseconds apart, as Explorer does.
        for f in &files {
            l.submit(parse(
                &[OsStr::new("--add"), OsStr::new("--"), f.as_os_str()],
                &cwd(),
            ));
            std::thread::sleep(Duration::from_millis(20));
        }
        let QuickEvent::AddPrompt {
            items,
            name,
            folder: shown,
            ..
        } = wait_quick(&compact, |e| matches!(e, QuickEvent::AddPrompt { .. }))
        else {
            unreachable!()
        };
        assert_eq!(items, 3, "one form for the whole selection");
        assert_eq!(name, "trip.mochi", "named like --add-now");
        assert!(shown.text.ends_with("trip"), "{}", shown.text);
        let prompts = compact
            .events()
            .iter()
            .filter(|e| matches!(e, QuickEvent::AddPrompt { .. }))
            .count();
        assert_eq!(prompts, 1);
        // Nothing runs and nothing is written before OK.
        std::thread::sleep(Duration::from_millis(150));
        assert!(!compact
            .events()
            .iter()
            .any(|e| matches!(e, QuickEvent::Started { .. } | QuickEvent::Done { .. })));
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 3);
        // OK writes the archive the form describes, as a ZIP.
        l.inner.quick.submit_add(zip_request("trip")).unwrap();
        let QuickEvent::Done {
            name,
            files,
            format,
            status,
            ..
        } = wait_quick(&compact, |e| matches!(e, QuickEvent::Done { .. }))
        else {
            unreachable!()
        };
        assert_eq!((name.text.as_str(), files), ("trip.zip", 3));
        assert_eq!(format, crate::ipc::ArchiveFormat::Zip);
        assert_eq!(status, None, "a ZIP has no commit status");
        assert!(folder.join("trip.zip").is_file());
        assert!(!folder.join("trip.mochi").exists());
    }

    #[test]
    fn a_later_add_launch_joins_the_form_while_it_is_open() {
        use crate::quick::QuickEvent;
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one.txt");
        let two = dir.path().join("two.txt");
        std::fs::write(&one, b"1").unwrap();
        std::fs::write(&two, b"22").unwrap();
        let (l, compact) = add_form_launches(Duration::from_millis(50));
        l.submit(parse(&[OsStr::new("--add"), one.as_os_str()], &cwd()));
        wait_quick(&compact, |e| {
            matches!(e, QuickEvent::AddPrompt { items: 1, .. })
        });
        // Well after the collection window: the form is open.
        std::thread::sleep(Duration::from_millis(200));
        l.submit(parse(&[OsStr::new("--add"), two.as_os_str()], &cwd()));
        wait_quick(&compact, |e| {
            matches!(e, QuickEvent::AddPrompt { items: 2, .. })
        });
        l.inner.quick.submit_add(zip_request("both")).unwrap();
        let QuickEvent::Done { files, .. } =
            wait_quick(&compact, |e| matches!(e, QuickEvent::Done { .. }))
        else {
            unreachable!()
        };
        assert_eq!(files, 2);
        assert!(dir.path().join("both.zip").is_file());
    }

    #[test]
    fn an_add_launch_after_ok_gets_a_form_of_its_own() {
        use crate::quick::QuickEvent;
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one.txt");
        let two = dir.path().join("two.txt");
        std::fs::write(&one, b"1").unwrap();
        std::fs::write(&two, b"22").unwrap();
        let (l, compact) = add_form_launches(Duration::from_millis(50));
        l.submit(parse(&[OsStr::new("--add"), one.as_os_str()], &cwd()));
        wait_quick(&compact, |e| matches!(e, QuickEvent::AddPrompt { .. }));
        l.inner.quick.submit_add(zip_request("first")).unwrap();
        wait_quick(&compact, |e| matches!(e, QuickEvent::Done { .. }));
        l.submit(parse(&[OsStr::new("--add"), two.as_os_str()], &cwd()));
        // The second form carries only the second item, not the first.
        let QuickEvent::AddPrompt { items, subject, .. } = wait_quick(
            &compact,
            |e| matches!(e, QuickEvent::AddPrompt { subject, .. } if subject.text == "two.txt"),
        ) else {
            unreachable!()
        };
        assert_eq!(items, 1);
        assert_eq!(subject.text, "two.txt");
        l.inner.quick.close_all();
        assert!(dir.path().join("first.zip").is_file());
        assert!(!dir.path().join("two.mochi").exists() && !dir.path().join("two.zip").exists());
    }

    #[test]
    fn closing_the_form_writes_nothing_and_frees_the_worker_for_the_next_form() {
        use crate::quick::QuickEvent;
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one.txt");
        std::fs::write(&one, b"1").unwrap();
        let (l, compact) = add_form_launches(Duration::from_millis(50));
        l.submit(parse(&[OsStr::new("--add"), one.as_os_str()], &cwd()));
        wait_quick(&compact, |e| matches!(e, QuickEvent::AddPrompt { .. }));
        l.inner.quick.close_all();
        // The worker ends the cancelled job (its event has no window to go to).
        std::thread::sleep(Duration::from_millis(150));
        let second = QSink::live();
        l.inner.quick.watch(second.clone());
        l.submit(parse(&[OsStr::new("--add"), one.as_os_str()], &cwd()));
        wait_quick(&second, |e| matches!(e, QuickEvent::AddPrompt { .. }));
        // What the first window never heard is not replayed to the next one.
        assert!(
            matches!(second.events()[0], QuickEvent::AddPrompt { .. }),
            "{:?}",
            second.events()
        );
        l.inner.quick.close_all();
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "only the item itself is in the folder"
        );
    }

    #[test]
    fn add_on_something_that_does_not_exist_fails_in_the_compact_window() {
        use crate::quick::QuickEvent;
        let dir = tempfile::tempdir().unwrap();
        let (l, compact) = add_form_launches(Duration::from_millis(50));
        l.submit(parse(
            &[OsStr::new("--add"), dir.path().join("gone").as_os_str()],
            &cwd(),
        ));
        let failed = wait_quick(&compact, |e| matches!(e, QuickEvent::Failed { .. }));
        assert!(matches!(failed, QuickEvent::Failed { .. }));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    // ---- one window per archive -------------------------------------------

    fn opened_name(e: &LaunchEvent) -> &str {
        match e {
            LaunchEvent::Opened { archive, .. } => &archive.file_name,
            other => panic!("expected Opened, got {other:?}"),
        }
    }

    fn open_verb(l: &Launches, path: &Path) {
        l.handle_now(Ok(Launch {
            verb: Verb::Open,
            paths: vec![path.to_path_buf()],
        }));
    }

    #[test]
    fn the_shell_opens_the_empty_window_first_and_a_new_window_for_each_next_archive() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (
            write_archive(dir.path(), "a.mochi"),
            write_archive(dir.path(), "b.mochi"),
        );
        let l = launches();
        let host = with_host(&l);
        open_verb(&l, &a);
        assert!(host.created.lock().unwrap().is_empty(), "main was empty");
        open_verb(&l, &b);
        assert_eq!(
            *host.created.lock().unwrap(),
            [("win-1".to_owned(), Some("main".to_owned()))]
        );
        let main = heard(&l, "main");
        let second = heard(&l, "win-1");
        assert_eq!((main.len(), second.len()), (1, 1));
        assert_eq!(opened_name(&main[0]), "a.mochi");
        assert_eq!(opened_name(&second[0]), "b.mochi");
        // Each window is titled with its archive.
        let titles = host.titles.lock().unwrap().clone();
        assert!(titles.contains(&("main".into(), "a.mochi - MOCHI".into())));
        assert!(titles.contains(&("win-1".into(), "b.mochi - MOCHI".into())));
        // The archives are the windows': closing one window leaves the other.
        let w = &l.inner.windows;
        assert_eq!(w.archives().open_count(), 2);
        w.release("main");
        assert_eq!(w.archives().open_count(), 1);
        assert_eq!(w.archives().file_names_of("win-1"), ["b.mochi"]);
    }

    #[test]
    fn opening_an_archive_that_is_already_open_shows_it_in_its_window() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (
            write_archive(dir.path(), "a.mochi"),
            write_archive(dir.path(), "b.mochi"),
        );
        let l = launches();
        let host = with_host(&l);
        open_verb(&l, &a);
        open_verb(&l, &b);
        host.focused.lock().unwrap().clear();
        // Through the shell, by another spelling of the path.
        open_verb(&l, &dir.path().join(".").join("a.mochi"));
        assert_eq!(host.created.lock().unwrap().len(), 1, "no third window");
        assert_eq!(*host.focused.lock().unwrap(), ["main"]);
        assert_eq!(
            l.inner.windows.archives().open_count(),
            2,
            "not opened twice"
        );
        // Through the user's Open in the other window.
        let outcome = l.open_in_window("win-1", &a).unwrap();
        assert_eq!(outcome, OpenOutcome::Elsewhere);
        assert_eq!(host.focused.lock().unwrap().last().unwrap(), "main");
        assert_eq!(l.inner.windows.archives().open_count(), 2);
        // The window that has it is told each time to show it, under the same
        // handle: its page may have lost it (it reloaded, or went back to the
        // start screen), and only bringing it forward then did nothing at all.
        let main = heard(&l, "main");
        assert_eq!(main.len(), 3, "{main:?}");
        let handles: Vec<_> = main
            .iter()
            .map(|e| match e {
                LaunchEvent::Opened { archive, .. } => archive.handle,
                other => panic!("{other:?}"),
            })
            .collect();
        assert!(handles.iter().all(|h| *h == handles[0]), "{handles:?}");
        // In the window that has it, it is shown there (reopened, not a copy).
        let again = l.open_in_window("main", &a).unwrap();
        let OpenOutcome::Opened { archive } = again else {
            panic!("{again:?}");
        };
        assert_eq!(archive.handle, handles[0]);
        assert_eq!(l.inner.windows.archives().open_count(), 2);
        assert!(heard(&l, "main").is_empty(), "no event for itself");
        assert_eq!(heard(&l, "win-1").len(), 1);
    }

    #[test]
    fn a_window_that_is_gone_does_not_keep_its_archive_from_opening() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_archive(dir.path(), "a.mochi");
        let l = launches();
        let host = with_host(&l);
        l.inner.windows.register("win-1");
        l.inner
            .windows
            .archives()
            .open_as("win-1", &a, mochi_core::publish::ReadOptions::default())
            .unwrap();
        // win-1 went away without its release having run.
        host.gone.lock().unwrap().push("win-1".to_owned());
        let outcome = l.open_in_window(MAIN_LABEL, &a).unwrap();
        let OpenOutcome::Opened { archive } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(archive.file_name, "a.mochi");
        assert!(!l.inner.windows.is_registered("win-1"));
        assert_eq!(
            l.inner.windows.archives().open_count(),
            1,
            "the old copy is closed"
        );
        assert_eq!(
            l.inner.windows.archives().file_names_of(MAIN_LABEL),
            ["a.mochi"]
        );
    }

    #[test]
    fn the_user_s_open_fills_an_empty_window_and_leaves_a_busy_one_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (
            write_archive(dir.path(), "a.mochi"),
            write_archive(dir.path(), "b.mochi"),
        );
        let l = launches();
        let host = with_host(&l);
        // The empty window shows what it opened itself.
        let first = l.open_in_window("main", &a).unwrap();
        let OpenOutcome::Opened { archive } = first else {
            panic!("{first:?}");
        };
        assert_eq!(archive.file_name, "a.mochi");
        assert!(host.created.lock().unwrap().is_empty());
        assert!(heard(&l, "main").is_empty(), "no launch event for itself");
        // Now it is busy: the next archive opens beside it, in a new window.
        let second = l.open_in_window("main", &b).unwrap();
        assert_eq!(second, OpenOutcome::Elsewhere);
        assert_eq!(
            *host.created.lock().unwrap(),
            [("win-1".to_owned(), Some("main".to_owned()))]
        );
        let events = heard(&l, "win-1");
        assert_eq!(events.len(), 1);
        assert_eq!(opened_name(&events[0]), "b.mochi");
        // The first window still has its archive.
        assert_eq!(
            l.inner.windows.archives().file_names_of("main"),
            ["a.mochi"]
        );
        // Closing it (the start screen again) makes the window usable again.
        let handle = archive.handle;
        assert!(l.inner.windows.archives().close(handle));
        let third = l.open_in_window("main", &b);
        assert_eq!(third.unwrap(), OpenOutcome::Elsewhere, "b is open in win-1");
        assert!(matches!(
            l.open_in_window("main", &a).unwrap(),
            OpenOutcome::Opened { .. }
        ));
    }

    #[test]
    fn a_file_that_cannot_be_opened_makes_no_window() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_archive(dir.path(), "a.mochi");
        let l = launches();
        let host = with_host(&l);
        l.open_in_window("main", &a).unwrap();
        let bad = dir.path().join("not-an-archive.mochi");
        std::fs::write(&bad, b"this is not an archive").unwrap();
        assert!(l.open_in_window("main", &bad).is_err());
        assert!(l
            .open_in_window("main", &dir.path().join("missing.mochi"))
            .is_err());
        assert!(host.created.lock().unwrap().is_empty());
        assert_eq!(l.inner.windows.labels(), ["main"]);
        assert_eq!(l.inner.windows.archives().open_count(), 1);
    }

    #[test]
    fn a_window_that_cannot_be_made_leaves_nothing_open() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (
            write_archive(dir.path(), "a.mochi"),
            write_archive(dir.path(), "b.mochi"),
        );
        let l = launches();
        let host = with_host(&l);
        l.open_in_window("main", &a).unwrap();
        host.refuse.store(true, Ordering::SeqCst);
        let e = l.open_in_window("main", &b).unwrap_err();
        assert!(e.message.contains("window"), "{e:?}");
        assert_eq!(l.inner.windows.archives().open_count(), 1);
        assert_eq!(l.inner.windows.labels(), ["main"]);
    }

    #[test]
    fn the_number_of_windows_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let l = launches();
        with_host(&l);
        for n in 0..crate::windows::MAX_WINDOWS {
            let p = write_archive(dir.path(), &format!("{n}.mochi"));
            l.open_in_window("main", &p).unwrap();
        }
        let extra = write_archive(dir.path(), "extra.mochi");
        let e = l.open_in_window("main", &extra).unwrap_err();
        assert_eq!(e.code, "LIMIT_EXCEEDED");
    }

    #[test]
    fn an_encrypted_archive_waits_for_its_passphrase_in_the_window_that_shows_it() {
        use mochi_core::manifest::Attributes;
        use mochi_core::publish::{ArchiveWriter, Transaction};
        use mochi_core::storage::os::OsStorage;
        use mochi_testkit::archive::{encrypted_options, Job};
        use mochi_testkit::SeqIds;
        let dir = tempfile::tempdir().unwrap();
        let a = write_archive(dir.path(), "a.mochi");
        let enc = dir.path().join("enc.mochi");
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(&enc).unwrap(),
            Box::new(SeqIds::new(3)),
            encrypted_options(&["correct horse"]),
        )
        .unwrap();
        let mut tx = Transaction::new();
        tx.put_file(
            mochi_testkit::archive::path("s.txt"),
            b"x".to_vec(),
            Attributes::default(),
        );
        w.commit(tx, &Job::new().ctx()).unwrap();
        w.close().unwrap();
        let l = launches();
        with_host(&l);
        l.open_in_window("main", &a).unwrap();
        assert_eq!(
            l.open_in_window("main", &enc).unwrap(),
            OpenOutcome::Elsewhere
        );
        let events = heard(&l, "win-1");
        let [LaunchEvent::NeedsPassphrase { pending, file_name }] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert_eq!(file_name, "enc.mochi");
        // The new window, not main, can unlock it; it then belongs to win-1.
        let archive = l
            .inner
            .windows
            .archives()
            .unlock(*pending, &crate::ipc::Secret::new("correct horse"))
            .unwrap();
        assert_eq!(
            l.inner
                .windows
                .archives()
                .owner_of(archive.handle)
                .as_deref(),
            Some("win-1")
        );
    }

    #[test]
    fn an_extract_plan_handed_over_goes_to_a_window_of_its_own_when_main_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (
            write_archive(dir.path(), "a.mochi"),
            write_archive(dir.path(), "b.mochi"),
        );
        let l = launches();
        let host = with_host(&l);
        l.open_in_window("main", &a).unwrap();
        // The quick worker opened b itself, unowned, and hands the plan over.
        let summary = l
            .inner
            .windows
            .archives()
            .open(&b, mochi_core::publish::ReadOptions::default())
            .unwrap();
        let folder = dir.path().join("out");
        crate::quick::MainPort::hand_over_extract(&*l.inner, summary.clone(), folder.clone());
        assert_eq!(host.created.lock().unwrap().len(), 1);
        let events = heard(&l, "win-1");
        assert!(
            matches!(
                events.as_slice(),
                [LaunchEvent::Opened { extract: true, .. }]
            ),
            "{events:?}"
        );
        assert_eq!(
            l.inner.windows.state("win-1").destination.get(),
            Some(folder)
        );
        assert!(l.inner.windows.state("main").destination.get().is_none());
        assert_eq!(
            l.inner
                .windows
                .archives()
                .owner_of(summary.handle)
                .as_deref(),
            Some("win-1")
        );
    }

    /// Owner report 2026-11-12: opening an archive on a network drive showed nothing at
    /// all. The window is told the read began and ended, around the outcome.
    #[test]
    fn opening_is_announced_around_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let archive = write_archive(dir.path(), "slow.mochi");
        let l = launches();
        with_host(&l);
        let main = Sink::live();
        l.watch(MAIN_LABEL, main.clone());
        l.handle_now(parse(&[archive.as_os_str()], &cwd()).map(Option::unwrap));
        let all = main.all_events();
        assert!(
            matches!(
                all.as_slice(),
                [
                    LaunchEvent::Opening { file_name },
                    LaunchEvent::OpeningEnded,
                    LaunchEvent::Opened { .. }
                ] if file_name == "slow.mochi"
            ),
            "{all:?}"
        );
        // A window's own open is announced to that window too, whatever the outcome.
        let other = write_archive(dir.path(), "other.mochi");
        let asked = Sink::live();
        l.watch("win-9", asked.clone());
        l.inner.windows.register("win-9");
        let outcome = l.open_in_window("win-9", &other).unwrap();
        assert!(matches!(outcome, OpenOutcome::Opened { .. }));
        assert!(matches!(
            asked.all_events().as_slice(),
            [LaunchEvent::Opening { .. }, LaunchEvent::OpeningEnded]
        ));
        let missing = dir.path().join("missing.mochi");
        assert!(l.open_in_window("win-9", &missing).is_err());
        assert_eq!(
            asked
                .all_events()
                .iter()
                .filter(|e| matches!(e, LaunchEvent::OpeningEnded))
                .count(),
            2,
            "a failed read ends its notice too"
        );
    }

    /// Opens run at once now: a window that is reading an archive is not empty, so a
    /// second open goes to a new window; and a file already being read for a window is
    /// not read again beside itself.
    #[test]
    fn a_window_that_is_reading_an_archive_is_not_reused_and_the_file_is_not_read_twice() {
        let dir = tempfile::tempdir().unwrap();
        let slow = write_archive(dir.path(), "slow.mochi");
        let next = write_archive(dir.path(), "next.mochi");
        let l = launches();
        let host = with_host(&l);
        // `main` is empty, but reading `slow`.
        lock(&l.inner.opening).insert(MAIN_LABEL.to_owned(), slow.clone());
        l.handle_now(parse(&[next.as_os_str()], &cwd()).map(Option::unwrap));
        assert!(
            heard(&l, MAIN_LABEL).is_empty(),
            "the busy window was not given the second archive"
        );
        let created: Vec<String> = host
            .created
            .lock()
            .unwrap()
            .iter()
            .map(|(label, _)| label.clone())
            .collect();
        assert_eq!(created, ["win-1"]);
        assert!(matches!(
            heard(&l, "win-1").as_slice(),
            [LaunchEvent::Opened { archive, .. }] if archive.file_name == "next.mochi"
        ));
        // The same file again while `main` still reads it: no second read, no new window.
        l.handle_now(parse(&[slow.as_os_str()], &cwd()).map(Option::unwrap));
        assert_eq!(host.created.lock().unwrap().len(), 1);
        assert!(l.inner.windows.archives().handle_of_file(&slow).is_none());
        assert!(host.focused.lock().unwrap().iter().any(|w| w == MAIN_LABEL));
    }
}
