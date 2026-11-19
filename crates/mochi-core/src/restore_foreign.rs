//! Restoring entries of a **foreign archive** (ZIP, 7z, RAR, tar; plan D8
//! rule 4) through the same rules as the C6 restore engine.
//!
//! The foreign-format helper (`mochi-foreign`, a separate process that this
//! crate knows nothing about) hands over entry names and entry bytes. This
//! module is the only thing that turns them into files, so every §10.4
//! protection applies to a foreign archive exactly as to a MOCHI one, and
//! none of them is re-implemented in the app or the helper:
//!
//! * **Names are judged here, never trusted.** [`parse_name`] refuses
//!   absolute paths (`/x`, `C:x`), `..` anywhere, empty components, NUL, and
//!   backslashes (an ambiguous separator) before anything is created. What it
//!   accepts becomes an [`ArchivePath`], whose own rules (no `.`, no empty
//!   component, bounded length and depth) then apply, and the destination's
//!   rules ([`RestoreDir::name_issue`]: Windows reserved names, alternate data
//!   stream syntax, trailing dots) apply last, per component.
//! * **Nothing is overwritten or merged.** The same [`preflight`] finds
//!   collisions and names the destination cannot hold before anything is
//!   written, and creation is exclusive: an entry that exists, or that the
//!   destination folds onto another's name, is a `NAME_COLLISION` and the
//!   earlier entry keeps its bytes. Two archive entries with one path: the
//!   first wins.
//! * **No link is ever created or followed.** Symbolic links, hard links,
//!   devices, FIFOs and anything else that is not a regular file or a
//!   directory are *skipped and reported* ([`RefusalReason::Link`],
//!   [`RefusalReason::Special`]). A later entry therefore cannot be written
//!   "through" an earlier link, and nothing can point outside the destination.
//! * **Only complete files appear under their own names.** Each file streams
//!   into a temporary name, and is published (without replacing) only after
//!   its source reported a clean result; otherwise it is discarded.
//! * **Entries arrive in archive order**, not path order (a solid archive is
//!   decoded once), so directories are created first, up front, and a file's
//!   parents are implied from its path when the archive has no entry for them.
//!
//! # Use
//!
//! [`plan_foreign`] (before anything is written) shows what a restore would
//! meet; [`ForeignRestorer::new`] runs the preflight again, creates the
//! directories, and says which entries the source must stream
//! ([`ForeignRestorer::wanted`]). The caller then feeds each streamed entry
//! through [`begin_file`](ForeignRestorer::begin_file),
//! [`write`](ForeignRestorer::write) and [`end_file`](ForeignRestorer::end_file),
//! and finally [`finish`](ForeignRestorer::finish). A failure to *parse* the
//! source is the caller's to report; this module only judges names and writes.
//!
//! This is the smallest entry point that keeps every check in core: the
//! existing [`crate::restore`] entry points read a verified MOCHI commit and
//! pull bytes from it, while a foreign source pushes bytes in an order of its
//! own choosing.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::catalog::namespace::{EntryKind, FileVersionId};
use crate::catalog::path::ArchivePath;
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::manifest::{Attributes, Mtime};
use crate::restore::{
    preflight, AttributeException, ExceptionKind, OnExisting, Preflight, RestoreException,
    RestoreReport, RESTORE_PHASE,
};
use crate::storage::{AttributeIssue, DirectoryDurability, RestoreDir, Storage, StorageError};

/// How many temporary names are tried in one directory before giving up.
const TEMP_ATTEMPTS: u32 = 64;

/// What a foreign archive says an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignKind {
    File,
    Directory,
    Symlink,
    HardLink,
    /// Devices, FIFOs, sockets, anti-items, anything else.
    Other,
}

/// One entry of a foreign archive as the helper listed it. `name` is the
/// exact bytes the archive stored, `/`-separated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignEntry {
    /// The entry's index in the source (what the source is asked to stream).
    pub index: u64,
    pub name: Vec<u8>,
    pub kind: ForeignKind,
    /// Declared size (a claim, shown in listings).
    pub size: Option<u64>,
    /// Seconds since the Unix epoch.
    pub mtime_unix: Option<i64>,
}

/// Why an entry will not be extracted, decided without touching the
/// destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusalReason {
    /// A NUL byte in the name.
    Nul,
    /// A leading `/`: an absolute path.
    Absolute,
    /// A leading drive (`C:`): an absolute path on Windows.
    DriveLetter,
    /// A backslash: on some systems a separator, on others a name byte; an
    /// archive that uses it is ambiguous, so the entry is not guessed at.
    Backslash,
    /// A `..` component: it would leave the destination.
    Traversal,
    /// `//` inside the name.
    EmptyComponent,
    /// The archive path rules refuse it (too long, too deep, ...).
    Unrepresentable(String),
    /// A symbolic link or hard link: never created, never followed.
    Link(ForeignKind),
    /// A device, FIFO, or other special entry: never created.
    Special,
    /// The entry has no name the engine can use (the helper did not carry it).
    NoName,
}

impl RefusalReason {
    /// The registered code this is reported under.
    pub fn code(&self) -> ErrorCode {
        match self {
            // The registry is frozen (R7), so no code is added for this:
            // "unsupported feature" is what skipping a link means.
            Self::Link(_) | Self::Special => ErrorCode::UnsupportedFeature,
            _ => ErrorCode::NameUnsupported,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Nul => "the name contains a NUL byte".into(),
            Self::Absolute => "the name is an absolute path".into(),
            Self::DriveLetter => "the name starts with a drive letter".into(),
            Self::Backslash => {
                "the name contains a backslash, which is ambiguous between systems".into()
            }
            Self::Traversal => "the name contains \"..\" and would leave the destination".into(),
            Self::EmptyComponent => "the name contains an empty component".into(),
            Self::Unrepresentable(why) => format!("the name cannot be used: {why}"),
            Self::Link(ForeignKind::Symlink) => {
                "a symbolic link; links are never created or followed".into()
            }
            Self::Link(_) => "a hard link; links are never created or followed".into(),
            Self::Special => "a device, pipe, or other special entry; not created".into(),
            Self::NoName => "the entry's name was not carried".into(),
        }
    }
}

/// An entry that was not extracted, and why, by the name the archive stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignRefusal {
    pub index: u64,
    pub name: Vec<u8>,
    pub reason: RefusalReason,
}

/// A foreign name as an archive path, after the rules above. `Ok(None)` is
/// the destination root itself (`.`, `./`), which needs nothing.
pub fn parse_name(raw: &[u8]) -> std::result::Result<Option<ArchivePath>, RefusalReason> {
    if raw.is_empty() {
        return Err(RefusalReason::NoName);
    }
    if raw.contains(&0) {
        return Err(RefusalReason::Nul);
    }
    if raw[0] == b'/' {
        return Err(RefusalReason::Absolute);
    }
    if raw.len() >= 2 && raw[0].is_ascii_alphabetic() && raw[1] == b':' {
        return Err(RefusalReason::DriveLetter);
    }
    if raw.contains(&b'\\') {
        return Err(RefusalReason::Backslash);
    }
    // One trailing separator marks a directory; it is not a component.
    let body = raw.strip_suffix(b"/").unwrap_or(raw);
    let mut comps: Vec<&[u8]> = Vec::new();
    for c in body.split(|b| *b == b'/') {
        match c {
            b".." => return Err(RefusalReason::Traversal),
            b"." => {} // `./a` is `a`: removing it cannot change where it lands
            b"" => return Err(RefusalReason::EmptyComponent),
            _ => comps.push(c),
        }
    }
    if comps.is_empty() {
        return Ok(None);
    }
    ArchivePath::from_components(comps)
        .map(Some)
        .map_err(|e| RefusalReason::Unrepresentable(e.message))
}

/// One thing to create: a file or a directory at a path.
#[derive(Debug, Clone)]
struct Item {
    path: ArchivePath,
    dir: bool,
    /// The source entry; `None` for a directory implied by a child's path.
    index: Option<u64>,
    mtime: Option<i64>,
}

/// The entries of an archive reduced to what can be created, and what cannot.
struct Prepared {
    /// In path order, parents before children, selection applied, one per
    /// path.
    items: Vec<Item>,
    /// Later entries at a path an earlier entry already holds: the first is
    /// the one restored, these are `NAME_COLLISION`.
    duplicates: Vec<Item>,
    refusals: Vec<ForeignRefusal>,
}

fn prepare(entries: &[ForeignEntry], selection: &[ArchivePath]) -> Prepared {
    let mut refusals = Vec::new();
    let mut explicit: Vec<Item> = Vec::new();
    for e in entries {
        let path = match parse_name(&e.name) {
            Ok(Some(p)) => p,
            Ok(None) => continue, // the root itself
            Err(reason) => {
                refusals.push(ForeignRefusal {
                    index: e.index,
                    name: e.name.clone(),
                    reason,
                });
                continue;
            }
        };
        let dir = match e.kind {
            ForeignKind::File => false,
            ForeignKind::Directory => true,
            ForeignKind::Symlink | ForeignKind::HardLink => {
                refusals.push(ForeignRefusal {
                    index: e.index,
                    name: e.name.clone(),
                    reason: RefusalReason::Link(e.kind),
                });
                continue;
            }
            ForeignKind::Other => {
                refusals.push(ForeignRefusal {
                    index: e.index,
                    name: e.name.clone(),
                    reason: RefusalReason::Special,
                });
                continue;
            }
        };
        explicit.push(Item {
            path,
            dir,
            index: Some(e.index),
            mtime: e.mtime_unix,
        });
    }

    let in_selection = |p: &ArchivePath| {
        selection.is_empty()
            || selection
                .iter()
                .any(|s| p == s || p.is_descendant_of(s) || s.is_descendant_of(p))
    };
    // A selection keeps refusals only for entries it would have included;
    // with no selection every refusal is reported.
    if !selection.is_empty() {
        refusals.retain(|r| {
            parse_name(&r.name)
                .ok()
                .flatten()
                .is_some_and(|p| in_selection(&p))
        });
    }

    // Directories implied by the paths of what is selected.
    let taken: HashSet<ArchivePath> = explicit.iter().map(|i| i.path.clone()).collect();
    let mut implied: BTreeMap<ArchivePath, Item> = BTreeMap::new();
    for it in explicit.iter().filter(|i| in_selection(&i.path)) {
        let mut cur = it.path.parent();
        while let Some(p) = cur {
            if !taken.contains(&p) && !implied.contains_key(&p) {
                implied.insert(
                    p.clone(),
                    Item {
                        path: p.clone(),
                        dir: true,
                        index: None,
                        mtime: None,
                    },
                );
            }
            cur = p.parent();
        }
    }
    let mut items: Vec<Item> = explicit
        .into_iter()
        .filter(|i| in_selection(&i.path))
        .chain(implied.into_values())
        .collect();
    // Path order puts parents first; at one path, the earlier archive entry
    // sorts first, so it is the one that wins a collision.
    items.sort_by(|a, b| {
        a.path.cmp(&b.path).then(
            a.index
                .unwrap_or(u64::MAX)
                .cmp(&b.index.unwrap_or(u64::MAX)),
        )
    });
    let mut kept: Vec<Item> = Vec::with_capacity(items.len());
    let mut duplicates = Vec::new();
    for it in items {
        if kept.last().is_some_and(|k| k.path == it.path) {
            duplicates.push(it);
        } else {
            kept.push(it);
        }
    }
    Prepared {
        items: kept,
        duplicates,
        refusals,
    }
}

fn tuple(items: &[Item]) -> Vec<(ArchivePath, EntryKind, FileVersionId)> {
    items
        .iter()
        .map(|i| {
            (
                i.path.clone(),
                if i.dir {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                },
                FileVersionId::from_bytes([0; 32]),
            )
        })
        .collect()
}

/// What extracting a selection would meet, found before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignPlan {
    /// Items the extraction would handle: selected entries plus the folders
    /// above them.
    pub items: u64,
    /// Entries refused outright (names, links, special files).
    pub refusals: Vec<ForeignRefusal>,
    /// Collisions and names the destination cannot hold.
    pub preflight: Preflight,
    /// Paths the archive holds more than once (the later entries collide).
    pub duplicates: Vec<ArchivePath>,
}

/// The advice [`ForeignRestorer::new`] acts on, without writing anything. Only
/// the case probe touches `root`, and it removes what it creates. Exclusive
/// creation stays the authority when the restore runs.
pub fn plan_foreign<D: RestoreDir>(
    entries: &[ForeignEntry],
    selection: &[ArchivePath],
    root: &mut D,
    on_existing: OnExisting,
) -> Result<ForeignPlan> {
    let prepared = prepare(entries, selection);
    let pre = preflight(root, &tuple(&prepared.items), on_existing)?;
    Ok(ForeignPlan {
        items: (prepared.items.len() + prepared.duplicates.len()) as u64,
        refusals: prepared.refusals,
        preflight: pre,
        duplicates: prepared.duplicates.into_iter().map(|d| d.path).collect(),
    })
}

/// Options for [`ForeignRestorer`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForeignRestoreOptions {
    /// End the job before writing anything if the preflight finds any
    /// collision or unsupported name (or any entry refused outright).
    pub refuse_on_exceptions: bool,
    /// Publish each checked file without first flushing its data ("fast
    /// extract"); as [`crate::restore::RestoreOptions::skip_file_sync`].
    pub skip_file_sync: bool,
    /// What to do with a name that already exists at the destination; as
    /// [`crate::restore::RestoreOptions::on_existing`].
    pub on_existing: OnExisting,
}

/// The result: the shared restore report, plus the entries refused outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignRestoreReport {
    pub report: RestoreReport,
    pub refusals: Vec<ForeignRefusal>,
}

/// Why a streamed file is not published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileFailure {
    /// The format's own check ran and did not match (the message says which).
    CheckFailed(String),
    /// The entry could not be read, was refused by a limit, needed a password,
    /// or its source stopped.
    Unreadable(String),
}

struct Slot {
    path: ArchivePath,
    parent: Option<ArchivePath>,
    mtime: Option<i64>,
}

struct Current<F> {
    file: F,
    temp: Vec<u8>,
    slot: Slot,
    bytes: u64,
}

/// Creates a foreign archive's directories and files in a destination as a
/// source streams them (see the module documentation).
pub struct ForeignRestorer<'c, 'j, D: RestoreDir> {
    ctx: &'c JobContext<'j>,
    dirs: HashMap<Option<ArchivePath>, D>,
    /// Files still to be delivered, by source index.
    pending: HashMap<u64, Slot>,
    current: Option<Current<D::File>>,
    report: RestoreReport,
    refusals: Vec<ForeignRefusal>,
    /// Directories created, with the mtime to set once their children exist.
    restored_dirs: Vec<(ArchivePath, Option<i64>)>,
    /// Existing files to move to the Recycle Bin or the Trash before the
    /// restored file takes the name (`OnExisting::Replace`).
    replaced: std::collections::BTreeSet<ArchivePath>,
    temp_seq: u64,
    total: u64,
    done: u64,
}

fn last_component(p: &ArchivePath) -> &[u8] {
    p.components().last().unwrap_or(p.as_stored())
}

fn failed(code: ErrorCode, message: impl Into<String>) -> ExceptionKind {
    ExceptionKind::Failed {
        code,
        message: message.into(),
    }
}

fn mtime_of(secs: Option<i64>) -> Attributes {
    Attributes {
        posix: None,
        windows: None,
        mtime: secs.map(|secs| Mtime { secs, nanos: 0 }),
    }
}

impl<'c, 'j, D: RestoreDir> ForeignRestorer<'c, 'j, D> {
    /// Judge every name, run the preflight, create the directories, and note
    /// the files the source must stream. Honours cancellation.
    pub fn new(
        entries: &[ForeignEntry],
        selection: &[ArchivePath],
        mut root: D,
        options: ForeignRestoreOptions,
        ctx: &'c JobContext<'j>,
    ) -> Result<Self> {
        ctx.check_cancelled()?;
        let prepared = prepare(entries, selection);
        let pre = preflight(&mut root, &tuple(&prepared.items), options.on_existing)?;
        if options.refuse_on_exceptions {
            let first = pre
                .exceptions
                .iter()
                .next()
                .map(|(p, k)| {
                    (
                        String::from_utf8_lossy(p.as_stored()).into_owned(),
                        k.code(),
                    )
                })
                .or_else(|| {
                    prepared.duplicates.first().map(|d| {
                        (
                            String::from_utf8_lossy(d.path.as_stored()).into_owned(),
                            ErrorCode::NameCollision,
                        )
                    })
                })
                .or_else(|| {
                    // Links and special files are skipped by design in every
                    // restore; only a refused *name* is an exception here.
                    prepared
                        .refusals
                        .iter()
                        .find(|r| r.reason.code() == ErrorCode::NameUnsupported)
                        .map(|r| {
                            (
                                String::from_utf8_lossy(&r.name).into_owned(),
                                r.reason.code(),
                            )
                        })
                });
            if let Some((name, code)) = first {
                return Err(MochiError::new(
                    code,
                    format!(
                        "{} entr{} cannot be extracted here, first {name:?} ({code}); nothing was written",
                        pre.exceptions.len() + prepared.refusals.len(),
                        if pre.exceptions.len() + prepared.refusals.len() == 1 {
                            "y"
                        } else {
                            "ies"
                        }
                    ),
                ));
            }
        }
        let mut me = ForeignRestorer {
            ctx,
            dirs: HashMap::new(),
            pending: HashMap::new(),
            current: None,
            report: RestoreReport::empty(pre.case_behavior, !options.skip_file_sync),
            refusals: prepared.refusals,
            restored_dirs: Vec::new(),
            replaced: pre.replaced.clone(),
            temp_seq: 0,
            total: prepared.items.len() as u64,
            done: 0,
        };
        me.dirs.insert(None, root);
        for d in &prepared.duplicates {
            me.report.exceptions.push(RestoreException {
                path: d.path.clone(),
                kind: ExceptionKind::Collision,
            });
        }
        me.total += prepared.duplicates.len() as u64;
        me.done += prepared.duplicates.len() as u64;
        ctx.report(RESTORE_PHASE, 0, Some(me.total));
        // Directories first (path order: parents before children), and the
        // parent check for files, so what the source must stream is known.
        let mut not_restored: HashMap<ArchivePath, ErrorCode> = HashMap::new();
        for it in &prepared.items {
            ctx.check_cancelled()?;
            let parent_path = it.path.parent();
            let parent_cause = parent_path
                .as_ref()
                .and_then(|p| not_restored.get(p).copied())
                .unwrap_or(ErrorCode::CatalogInvalid);
            let mut except = |me: &mut Self, ek: ExceptionKind| {
                if it.dir {
                    not_restored.insert(it.path.clone(), ek.code());
                }
                me.report.exceptions.push(RestoreException {
                    path: it.path.clone(),
                    kind: ek,
                });
                me.done += 1;
            };
            if !me.dirs.contains_key(&parent_path) {
                except(
                    &mut me,
                    ExceptionKind::ParentNotRestored {
                        cause: parent_cause,
                    },
                );
                continue;
            }
            if let Some(kind) = pre.exceptions.get(&it.path) {
                except(&mut me, kind.clone());
                continue;
            }
            let name = last_component(&it.path).to_vec();
            let issue = me.dirs.get(&parent_path).and_then(|p| p.name_issue(&name));
            if let Some(issue) = issue {
                except(&mut me, ExceptionKind::UnsupportedName(issue));
                continue;
            }
            if it.dir && pre.merged.contains(&it.path) {
                // An existing directory the preflight chose to enter, opened
                // again now; no longer enterable is a collision as before.
                let opened = me
                    .dirs
                    .get_mut(&parent_path)
                    .map(|p| p.open_existing_dir(&name))
                    .unwrap_or(Err(StorageError::Unsupported("parent vanished")));
                match opened {
                    Ok(Some(child)) => {
                        me.report.merged_directories += 1;
                        me.dirs.insert(Some(it.path.clone()), child);
                        me.done += 1;
                        ctx.report(RESTORE_PHASE, me.done, Some(me.total));
                    }
                    Ok(None) => except(&mut me, ExceptionKind::Collision),
                    Err(e) => {
                        let e = MochiError::from(e);
                        except(&mut me, failed(e.code, e.message));
                    }
                }
            } else if it.dir {
                let made = me
                    .dirs
                    .get_mut(&parent_path)
                    .map(|p| p.create_dir(&name))
                    .unwrap_or(Err(StorageError::Unsupported("parent vanished")));
                match made {
                    Ok(child) => {
                        me.report.directories += 1;
                        me.dirs.insert(Some(it.path.clone()), child);
                        me.restored_dirs.push((it.path.clone(), it.mtime));
                        me.done += 1;
                        ctx.report(RESTORE_PHASE, me.done, Some(me.total));
                    }
                    Err(StorageError::Exists { .. }) => except(&mut me, ExceptionKind::Collision),
                    Err(e) => {
                        let e = MochiError::from(e);
                        except(&mut me, failed(e.code, e.message));
                    }
                }
            } else if let Some(index) = it.index {
                me.pending.insert(
                    index,
                    Slot {
                        path: it.path.clone(),
                        parent: parent_path,
                        mtime: it.mtime,
                    },
                );
            }
        }
        Ok(me)
    }

    /// Source indices to stream: the files that passed every check so far,
    /// ascending.
    pub fn wanted(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.pending.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// Entries refused outright, so far.
    pub fn refusals(&self) -> &[ForeignRefusal] {
        &self.refusals
    }

    /// The source begins streaming file `index`. A source entry that was not
    /// asked for (or is streamed twice) is refused. Honours cancellation.
    pub fn begin_file(&mut self, index: u64) -> Result<()> {
        self.ctx.check_cancelled()?;
        if self.current.is_some() {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "a file began while another was still open",
            ));
        }
        let Some(slot) = self.pending.remove(&index) else {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                format!("entry {index} was not asked for, or was already delivered"),
            ));
        };
        let name = last_component(&slot.path).to_vec();
        let Some(parent) = self.dirs.get_mut(&slot.parent) else {
            self.except(
                &slot.path,
                ExceptionKind::ParentNotRestored {
                    cause: ErrorCode::CatalogInvalid,
                },
            );
            return Ok(());
        };
        let mut created = None;
        for _ in 0..TEMP_ATTEMPTS {
            self.temp_seq += 1;
            let temp = format!(".mochi-restore.{}.tmp", self.temp_seq).into_bytes();
            if temp == name {
                continue;
            }
            match parent.create_file(&temp) {
                Ok(f) => {
                    created = Some((f, temp));
                    break;
                }
                Err(StorageError::Exists { .. }) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let Some((file, temp)) = created else {
            return Err(MochiError::new(
                ErrorCode::IoError,
                "no free temporary name in the destination directory",
            ));
        };
        self.current = Some(Current {
            file,
            temp,
            slot,
            bytes: 0,
        });
        Ok(())
    }

    /// Bytes of the open file. Without an open file (its creation was
    /// refused) they are dropped.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(cur) = self.current.as_mut() else {
            return Ok(());
        };
        cur.file.append(bytes).map_err(MochiError::from)?;
        cur.bytes += bytes.len() as u64;
        Ok(())
    }

    /// The open file ended. `Ok(())` from the source means every check the
    /// format defines passed: the file is synced and published without
    /// replacing anything. `Err(reason)` discards it and records why.
    pub fn end_file(&mut self, outcome: std::result::Result<(), FileFailure>) -> Result<()> {
        let Some(cur) = self.current.take() else {
            return Ok(());
        };
        let Current {
            mut file,
            temp,
            slot,
            bytes,
        } = cur;
        let name = last_component(&slot.path).to_vec();
        let Some(parent) = self.dirs.get_mut(&slot.parent) else {
            return Ok(());
        };
        let result = match outcome {
            Err(why) => {
                parent.discard(file, &temp)?;
                let kind = match why {
                    // The format's own check ran and did not match: reported
                    // under the integrity code, with the source's wording.
                    FileFailure::CheckFailed(message) => ExceptionKind::Integrity {
                        code: ErrorCode::ContentIntegrityFailed,
                        message,
                    },
                    FileFailure::Unreadable(message) => failed(ErrorCode::IoError, message),
                };
                self.except(&slot.path, kind);
                self.step();
                return Ok(());
            }
            Ok(()) if !self.report.file_data_synced => Ok(()),
            Ok(()) => file.sync_data().map_err(MochiError::from),
        };
        if let Err(e) = result {
            parent.discard(file, &temp)?;
            self.except(&slot.path, failed(e.code, e.message));
            self.step();
            return Ok(());
        }
        // Replace: the file passed its checks; only now is the existing one
        // moved to the Recycle Bin or the Trash. If that fails it keeps its
        // name and its bytes, and this file is not published.
        let set_aside = self.replaced.contains(&slot.path);
        if set_aside {
            if let Err(e) = parent.set_aside(&name) {
                parent.discard(file, &temp)?;
                self.except(
                    &slot.path,
                    failed(
                        ErrorCode::IoError,
                        format!(
                            "the existing file could not be moved to the Recycle Bin or the \
                             Trash, so it was kept: {e}"
                        ),
                    ),
                );
                self.step();
                return Ok(());
            }
        }
        let Some(parent) = self.dirs.get_mut(&slot.parent) else {
            return Ok(());
        };
        match parent.publish_no_replace(&temp, &name) {
            Ok(()) => {
                drop(file);
                self.report.files += 1;
                self.report.bytes += bytes;
                if set_aside {
                    self.report.replaced += 1;
                }
                if slot.mtime.is_some() {
                    let issues =
                        parent.apply_attributes(&name, EntryKind::File, &mtime_of(slot.mtime));
                    self.attr_issues(&slot.path, issues);
                }
            }
            Err(StorageError::Exists { .. }) => {
                parent.discard(file, &temp)?;
                self.except(&slot.path, ExceptionKind::Collision);
            }
            Err(e) => {
                parent.discard(file, &temp)?;
                return Err(e.into());
            }
        }
        self.step();
        Ok(())
    }

    /// Stop: discard the open file, if any (cancellation, a failed source).
    pub fn abandon(&mut self, why: &str) {
        if let Some(cur) = self.current.take() {
            if let Some(parent) = self.dirs.get_mut(&cur.slot.parent) {
                let _ = parent.discard(cur.file, &cur.temp);
            }
            self.except(&cur.slot.path, failed(ErrorCode::IoError, why.to_string()));
        }
    }

    /// Everything the source delivered has been fed in. Files it never
    /// delivered are reported with `why_missing`; directory times are applied
    /// deepest first; the directories are synced.
    pub fn finish(mut self, why_missing: &str) -> Result<ForeignRestoreReport> {
        self.abandon("the source stopped while this file was being written");
        let mut missing: Vec<(u64, ArchivePath)> =
            self.pending.drain().map(|(i, s)| (i, s.path)).collect();
        missing.sort();
        for (_, path) in missing {
            self.except(
                &path,
                failed(ErrorCode::IoError, format!("not extracted: {why_missing}")),
            );
        }
        // Reverse path order puts every descendant before its ancestors.
        let dirs = std::mem::take(&mut self.restored_dirs);
        for (path, mtime) in dirs.iter().rev() {
            if mtime.is_none() {
                continue;
            }
            let issues = match self.dirs.get_mut(&path.parent()) {
                Some(parent) => parent.apply_attributes(
                    last_component(path),
                    EntryKind::Directory,
                    &mtime_of(*mtime),
                ),
                None => continue,
            };
            self.attr_issues(path, issues);
        }
        for d in self.dirs.values_mut() {
            if let DirectoryDurability::Unconfirmed(why) = d.sync_directory()? {
                self.report.directory_durability = DirectoryDurability::Unconfirmed(why);
            }
        }
        Ok(ForeignRestoreReport {
            report: self.report,
            refusals: self.refusals,
        })
    }

    fn except(&mut self, path: &ArchivePath, kind: ExceptionKind) {
        self.report.exceptions.push(RestoreException {
            path: path.clone(),
            kind,
        });
    }

    fn attr_issues(&mut self, path: &ArchivePath, issues: Vec<AttributeIssue>) {
        self.report
            .attribute_exceptions
            .extend(issues.into_iter().map(|issue| AttributeException {
                path: path.clone(),
                issue,
            }));
    }

    fn step(&mut self) {
        self.done += 1;
        self.ctx.report(RESTORE_PHASE, self.done, Some(self.total));
    }
}

// ---- browsing --------------------------------------------------------------

/// One row of a foreign archive's directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// The path to select or descend into; `None` for an entry whose name was
    /// refused (it is listed under the root so it is visible, but cannot be
    /// selected).
    pub path: Option<ArchivePath>,
    /// What to show: the last component, or the whole raw name for a refused
    /// entry.
    pub name: Vec<u8>,
    pub kind: ForeignKind,
    pub size: Option<u64>,
    pub index: Option<u64>,
    pub refused: Option<RefusalReason>,
    pub child_dirs: u32,
    pub child_files: u32,
}

/// The directory structure of a foreign archive's listing, for browsing.
/// Directories the archive implies (no entry of their own) are included.
#[derive(Debug, Clone, Default)]
pub struct ForeignTree {
    nodes: BTreeMap<ArchivePath, TreeRow>,
    children: BTreeMap<Option<ArchivePath>, Vec<ArchivePath>>,
    refused: Vec<TreeRow>,
    /// Entries in the listing, refused ones included.
    pub entries: u64,
}

impl ForeignTree {
    pub fn build(entries: &[ForeignEntry]) -> Self {
        let mut t = ForeignTree {
            entries: entries.len() as u64,
            ..Self::default()
        };
        for e in entries {
            match parse_name(&e.name) {
                Ok(None) => {}
                Err(reason) => t.refused.push(TreeRow {
                    path: None,
                    name: e.name.clone(),
                    kind: e.kind,
                    size: e.size,
                    index: Some(e.index),
                    refused: Some(reason),
                    child_dirs: 0,
                    child_files: 0,
                }),
                Ok(Some(path)) => {
                    // Links and special entries are listed (as what they are)
                    // and refused at extraction time.
                    let reason = match e.kind {
                        ForeignKind::Symlink | ForeignKind::HardLink => {
                            Some(RefusalReason::Link(e.kind))
                        }
                        ForeignKind::Other => Some(RefusalReason::Special),
                        _ => None,
                    };
                    // The first entry at a path is the one a restore keeps.
                    if t.nodes.get(&path).is_some_and(|n| n.index.is_some()) {
                        continue;
                    }
                    t.nodes.insert(
                        path.clone(),
                        TreeRow {
                            path: Some(path.clone()),
                            name: last_component(&path).to_vec(),
                            kind: e.kind,
                            size: e.size,
                            index: Some(e.index),
                            refused: reason,
                            child_dirs: 0,
                            child_files: 0,
                        },
                    );
                }
            }
        }
        // Implied directories.
        let paths: Vec<ArchivePath> = t.nodes.keys().cloned().collect();
        for p in &paths {
            let mut cur = p.parent();
            while let Some(d) = cur {
                t.nodes.entry(d.clone()).or_insert_with(|| TreeRow {
                    path: Some(d.clone()),
                    name: last_component(&d).to_vec(),
                    kind: ForeignKind::Directory,
                    size: None,
                    index: None,
                    refused: None,
                    child_dirs: 0,
                    child_files: 0,
                });
                cur = d.parent();
            }
        }
        // A path that is a file but also has children is listed as the file;
        // its children are reachable only if it is a directory.
        let all: Vec<ArchivePath> = t.nodes.keys().cloned().collect();
        for p in all {
            t.children.entry(p.parent()).or_default().push(p);
        }
        let dirs: HashSet<ArchivePath> = t
            .nodes
            .iter()
            .filter(|(_, n)| n.kind == ForeignKind::Directory)
            .map(|(p, _)| p.clone())
            .collect();
        let counts: HashMap<ArchivePath, (u32, u32)> = dirs
            .iter()
            .map(|d| {
                let kids = t
                    .children
                    .get(&Some(d.clone()))
                    .map_or(&[][..], Vec::as_slice);
                let dd = kids.iter().filter(|k| dirs.contains(*k)).count();
                (d.clone(), (dd as u32, (kids.len() - dd) as u32))
            })
            .collect();
        for (p, (dd, ff)) in counts {
            if let Some(n) = t.nodes.get_mut(&p) {
                n.child_dirs = dd;
                n.child_files = ff;
            }
        }
        t
    }

    /// Children of `dir` (`None` = the root): directories first, then the
    /// rest, each in name byte order. At the root, entries whose names were
    /// refused follow.
    pub fn children(&self, dir: Option<&ArchivePath>) -> Vec<TreeRow> {
        let mut rows: Vec<TreeRow> = self
            .children
            .get(&dir.cloned())
            .into_iter()
            .flatten()
            .filter_map(|p| self.nodes.get(p).cloned())
            .collect();
        rows.sort_by(|a, b| {
            let da = a.kind != ForeignKind::Directory;
            let db = b.kind != ForeignKind::Directory;
            da.cmp(&db).then(a.name.cmp(&b.name))
        });
        if dir.is_none() {
            rows.extend(self.refused.iter().cloned());
        }
        rows
    }

    pub fn get(&self, path: &ArchivePath) -> Option<&TreeRow> {
        self.nodes.get(path)
    }

    /// Every row the tree holds, for a search over the whole listing: the
    /// entries and the folders they imply in path byte order, then the rows
    /// whose names were refused (they have no path, only the raw name).
    pub fn rows(&self) -> impl Iterator<Item = &TreeRow> {
        self.nodes.values().chain(self.refused.iter())
    }

    /// The row for a refused entry by its source index.
    pub fn refused_by_index(&self, index: u64) -> Option<&TreeRow> {
        self.refused.iter().find(|r| r.index == Some(index))
    }
}
