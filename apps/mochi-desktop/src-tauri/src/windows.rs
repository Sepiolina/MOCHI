//! One window per archive, as WinRAR does it.
//!
//! Everything that used to be "the open archive" of the one main window is
//! kept here per window, keyed by the window's label:
//!
//! * the staged sources, the drop listener, the extract destination and the
//!   Test and Repair tool's state ([`WindowState`]);
//! * which open archives, foreign archives, waiting passphrases and running
//!   jobs belong to the window (`Archives::open_as`, `Foreign::open_as`,
//!   `JobRegistry::start_for` record the owner);
//! * where a launch event for the window goes (`launch::Launches`).
//!
//! Closing a window ([`WindowStates::release`]) closes exactly its archives
//! (which releases their readers), forgets its waiting passphrases and foreign
//! passwords and cancels its jobs. A writer lock is held by a job for the
//! length of the job, so cancelling the job releases it; no other window's
//! archive, job or lock is touched.
//!
//! The decision of *where* an archive opens is [`place`]: the archive is
//! already open in some window (focus that one), the asking window is empty
//! (reuse it), or a new window is made. It is a pure function so that it is
//! tested without a display.
//!
//! Window labels: `main` for the first archive window, `win-N` for the rest
//! ([`WINDOW_PREFIX`]), `quick` for the compact window (which has no state
//! here). `capabilities/main.json` grants `main` and `win-*` the same
//! commands.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::browse::Archives;
use crate::check::Checks;
use crate::extract::Destination;
use crate::find::{Cache, Finder};
use crate::folders::FolderHistory;
use crate::foreign::Foreign;
use crate::jobs::JobRegistry;
use crate::write::{DropWatch, Staging};

/// The first archive window's label, which `capabilities/main.json` grants.
pub const MAIN_LABEL: &str = "main";

/// The first compact window's label (`capabilities/quick.json` grants it and
/// `quick-*`; `quick::QuickPool` names them).
pub use crate::quick::QUICK_LABEL;

/// The prefix of every archive window after the first
/// (`capabilities/main.json` grants `win-*`).
pub const WINDOW_PREFIX: &str = "win-";

/// Archive windows open at once. Each holds an archive's index in memory, so
/// the count is bounded (opening another is refused with a plain sentence).
pub const MAX_WINDOWS: usize = 12;

/// Whether `label` names a window that shows archives.
pub fn is_archive_label(label: &str) -> bool {
    label == MAIN_LABEL
        || label
            .strip_prefix(WINDOW_PREFIX)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether two paths are the same file: the same device and inode on Unix,
/// the same canonical path elsewhere. A path that cannot be examined is the
/// same only if the two are equal.
pub fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(x), Ok(y)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return x.dev() == y.dev() && x.ino() == y.ino();
        }
    }
    #[cfg(not(unix))]
    {
        if let (Ok(x), Ok(y)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            return x == y;
        }
    }
    a == b
}

/// What belongs to one window besides its archives.
#[derive(Default)]
pub struct WindowState {
    /// What is staged to add, and where a new archive would go.
    pub staging: Arc<Staging>,
    /// The listener for this window's drag-and-drop events.
    pub drops: Arc<DropWatch>,
    /// The folder this window's user chose to extract into.
    pub destination: Arc<Destination>,
    /// The archive chosen in this window's Test and Repair tool, its reports
    /// and the repair's setup.
    pub checks: Arc<Checks>,
    /// This window's MOCHI Find: its folders and its latest scan. Only the
    /// cache of names (`WindowStates::find_cache`) is shared between windows.
    pub finder: Arc<Finder>,
}

/// Where an archive opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// The archive is already open in this window: bring it forward.
    Focus(String),
    /// This window is empty: open the archive in it.
    Reuse(String),
    /// Every candidate is in use: open a new window.
    New,
}

/// The routing decision. `open_in` is the window that already has the file
/// open (or is asking for its passphrase); `empty` is a window that may take
/// the archive because it shows nothing.
pub fn place(open_in: Option<String>, empty: Option<String>) -> Placement {
    match (open_in, empty) {
        (Some(window), _) => Placement::Focus(window),
        (None, Some(window)) => Placement::Reuse(window),
        (None, None) => Placement::New,
    }
}

/// Where a new window goes: down and to the right of its opener by `step`,
/// wrapping to the monitor's corner (plus `step`) when it would run off. All
/// in physical pixels. `window` is the size the new window will have.
pub fn cascade(
    opener: (i32, i32),
    step: i32,
    monitor: ((i32, i32), (u32, u32)),
    window: (u32, u32),
) -> (i32, i32) {
    let ((mx, my), (mw, mh)) = monitor;
    let fits = |pos: i32, origin: i32, room: u32, size: u32| {
        let room = i64::from(room);
        let size = i64::from(size).min(room);
        i64::from(pos) + size <= i64::from(origin) + room
    };
    let (x, y) = (opener.0.saturating_add(step), opener.1.saturating_add(step));
    if fits(x, mx, mw, window.0) && fits(y, my, mh, window.1) {
        (x, y)
    } else {
        (mx.saturating_add(step), my.saturating_add(step))
    }
}

#[derive(Default)]
struct Inner {
    windows: HashMap<String, Arc<WindowState>>,
    /// Archive windows, most recently used first.
    recent: Vec<String>,
    last_number: u32,
}

/// What closing a window released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Released {
    pub archives: usize,
    pub foreign: usize,
    pub jobs: usize,
}

/// The per-window state of every archive window.
pub struct WindowStates {
    archives: Arc<Archives>,
    foreign: Arc<Foreign>,
    jobs: Arc<JobRegistry>,
    /// The folders extracted into lately, one list for every window.
    history: Arc<FolderHistory>,
    /// The names MOCHI Find read from archives, for every window.
    find_cache: Arc<Cache>,
    inner: Mutex<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl WindowStates {
    pub fn new(archives: Arc<Archives>, foreign: Arc<Foreign>, jobs: Arc<JobRegistry>) -> Self {
        WindowStates {
            archives,
            foreign,
            jobs,
            history: Arc::default(),
            find_cache: Arc::default(),
            inner: Mutex::default(),
        }
    }

    fn new_state(&self) -> Arc<WindowState> {
        Arc::new(WindowState {
            destination: Arc::new(Destination::with_history(Arc::clone(&self.history))),
            finder: Arc::new(Finder::new(Arc::clone(&self.find_cache))),
            ..WindowState::default()
        })
    }

    /// The recent extract folders every window's `Destination` shares.
    pub fn history(&self) -> &Arc<FolderHistory> {
        &self.history
    }

    /// The MOCHI archives of every window.
    pub fn archives(&self) -> &Archives {
        &self.archives
    }

    /// The foreign archives of every window.
    pub fn foreign(&self) -> &Foreign {
        &self.foreign
    }

    /// The state of `label`, made on first use.
    pub fn state(&self, label: &str) -> Arc<WindowState> {
        Arc::clone(
            lock(&self.inner)
                .windows
                .entry(label.to_owned())
                .or_insert_with(|| self.new_state()),
        )
    }

    /// A new archive window exists (or is about to): remember it as the most
    /// recently used.
    pub fn register(&self, label: &str) {
        let fresh = self.new_state();
        let mut g = lock(&self.inner);
        g.windows.entry(label.to_owned()).or_insert(fresh);
        g.recent.retain(|l| l != label);
        g.recent.insert(0, label.to_owned());
    }

    /// The window was used (it got the focus).
    pub fn touch(&self, label: &str) {
        let mut g = lock(&self.inner);
        if g.windows.contains_key(label) {
            g.recent.retain(|l| l != label);
            g.recent.insert(0, label.to_owned());
        }
    }

    /// Whether `label` is a live archive window.
    pub fn is_registered(&self, label: &str) -> bool {
        lock(&self.inner).recent.iter().any(|l| l == label)
    }

    /// Live archive windows, most recently used first.
    pub fn labels(&self) -> Vec<String> {
        lock(&self.inner).recent.clone()
    }

    /// The label for the next archive window: `main` if it is free, else the
    /// next `win-N` (numbers are not reused within a run).
    pub fn next_label(&self) -> String {
        let mut g = lock(&self.inner);
        if !g.recent.iter().any(|l| l == MAIN_LABEL) {
            return MAIN_LABEL.to_owned();
        }
        g.last_number = g.last_number.saturating_add(1);
        format!("{WINDOW_PREFIX}{}", g.last_number)
    }

    /// Whether the window shows nothing: no archive open or waiting for a
    /// passphrase, no foreign archive, no job running, nothing staged. Such a
    /// window is reused for the next archive instead of making a new one.
    pub fn is_empty(&self, label: &str) -> bool {
        let staged = lock(&self.inner)
            .windows
            .get(label)
            .map_or(0, |w| w.staging.view().items.len());
        staged == 0
            && !self.archives.holds_any(label)
            && !self.foreign.holds_any(label)
            && self.jobs.running_for(label) == 0
    }

    /// The most recently used live window that is empty.
    pub fn empty_window(&self) -> Option<String> {
        self.labels().into_iter().find(|l| self.is_empty(l))
    }

    /// The live window that has `path` open (as a MOCHI or a foreign archive,
    /// or while asking for its passphrase or password).
    pub fn window_with_file(&self, path: &Path) -> Option<String> {
        let found = self
            .archives
            .owner_of_file(path)
            .or_else(|| self.foreign.owner_of_file(path))?;
        self.is_registered(&found).then_some(found)
    }

    /// Where `path` opens when `requester` (a window the user acted in, or
    /// `None` for the shell) asks for it. A user's open reuses only the window
    /// it was asked in; the shell's reuses any empty window.
    pub fn decide(&self, path: &Path, requester: Option<&str>) -> Placement {
        let empty = match requester {
            Some(r) => (self.is_registered(r) && self.is_empty(r)).then(|| r.to_owned()),
            None => self.empty_window(),
        };
        place(self.window_with_file(path), empty)
    }

    /// Whether another archive window may be made.
    pub fn has_room(&self) -> bool {
        lock(&self.inner).recent.len() < MAX_WINDOWS
    }

    /// The title of a window: the archive it shows, then the product name. An
    /// archive's name is the user's own file name (untrusted text, shown as
    /// plain text by the operating system).
    pub fn title(&self, label: &str) -> String {
        let mut names = self.archives.file_names_of(label);
        names.extend(self.foreign.file_names_of(label));
        match names.last() {
            Some(name) if !name.is_empty() => format!("{name} - MOCHI"),
            _ => "MOCHI".to_owned(),
        }
    }

    /// The window is gone: release everything it owned. Nothing of any other
    /// window is touched.
    pub fn release(&self, label: &str) -> Released {
        let state = {
            let mut g = lock(&self.inner);
            g.recent.retain(|l| l != label);
            g.windows.remove(label)
        };
        // Jobs first: a write job holds its archive's writer lock until it
        // stops, and its reader goes with the archive.
        let jobs = self.jobs.cancel_owned(label);
        let archives = self.archives.close_owned_by(label);
        let foreign = self.foreign.close_owned_by(label);
        if let Some(state) = state {
            // An extract folder the window made and never used.
            state.destination.discard_made();
            // A scan still running for this window stops with it.
            state.finder.cancel();
        }
        Released {
            archives,
            foreign,
            jobs,
        }
    }
}
