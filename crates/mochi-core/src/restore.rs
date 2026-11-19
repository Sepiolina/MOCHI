//! Restore engine (plan C6; spec §10.4, §20.1 "restoration", §23.3 #7):
//! write an opened commit's namespace, or one subtree of it, into a
//! directory.
//!
//! # Rules
//!
//! * **Nothing is overwritten or merged unless the caller asks**
//!   ([`RestoreOptions::on_existing`], owner decision 2026-11-11; spec
//!   §10.4 forbids only *silent* overwriting). By default ([`OnExisting::Keep`])
//!   every directory is created exclusively and every file is published
//!   without replacing: an entry that already exists at the destination, or
//!   that the destination filesystem folds onto another entry's name (case,
//!   normalization), is a `NAME_COLLISION` exception, the earlier entry keeps
//!   its bytes, and a colliding directory's subtree is not restored.
//!   [`OnExisting::Merge`] enters a directory that already exists (only a
//!   plain one the backend allows, [`RestoreDir::open_existing_dir`]) and
//!   restores what it lacks; an existing file still keeps its bytes.
//!   [`OnExisting::Replace`] also moves an existing regular file to where the
//!   user can get it back ([`RestoreDir::set_aside`]: the Recycle Bin or the
//!   Trash) once the restored file has been verified, just before it is
//!   published, still without replacing. Nothing else is ever replaced: a
//!   file where the archive has a directory (or the reverse), a link, or a
//!   special file stays a `NAME_COLLISION`. A directory that was entered is
//!   not counted as restored and gets none of the archive's attributes; it
//!   was the user's.
//! * **Names are never altered.** A name the destination cannot hold
//!   ([`RestoreDir::name_issue`]) is a `NAME_UNSUPPORTED` exception, and
//!   the entry and its subtree are skipped.
//! * **Traversal cannot be expressed.** Archive paths have no `.`, `..`,
//!   empty, or separator-bearing components (`catalog::path`), and the
//!   engine only ever creates single components inside directories it
//!   created itself.
//! * **Only verified files appear under their own names.** Each file is
//!   streamed into a temporary name in its directory ([`crate::read`]).
//!   Every chunk and the whole file's content hash are checked, the file is
//!   synced, and only then is it published. On failure the temporary file
//!   is discarded, so a damaged file is absent, never partly present.
//! * **Collisions are found before anything is written** ([`preflight`]):
//!   the destination's case behaviour is detected on the root, sibling
//!   names that it would treat as one are grouped (the first in path order
//!   is restored, the others are `NAME_COLLISION`), names it cannot hold
//!   are `NAME_UNSUPPORTED`, and top-level names that already exist there
//!   are `NAME_COLLISION`. Exclusive creation remains the authority: a
//!   collision the preflight cannot foresee (Unicode normalization) is still
//!   caught when the entry is created, and nothing is overwritten. With
//!   [`RestoreOptions::refuse_on_preflight_exceptions`] any preflight
//!   exception ends the job before anything is written.
//! * **Exceptions do not stop the restore.** Each is recorded with its path
//!   and code ([`RestoreReport`]). Only cancellation and failures of the
//!   destination itself end the job early.
//!
//! * **Attributes** (plan O6, spec §10.4.1) come from the manifests
//!   ([`crate::publish::promised_attributes`]). They are applied after a
//!   file is published, and to directories at the end, deepest first, so
//!   creating children does not disturb a directory's time or need write
//!   permission it no longer has. Setuid and setgid are removed unless
//!   [`RestoreOptions::restore_setid`] asks for them. Whatever the
//!   destination cannot apply is an `ATTRIBUTE_NOT_RESTORED` exception
//!   ([`RestoreReport::attribute_exceptions`]). If the attributes cannot be
//!   reconstructed at all (a damaged snapshot manifest), content is still
//!   restored and the report says the attributes were unavailable.

use std::collections::BTreeMap;
use std::io::Write;

use crate::catalog::namespace::{EntryKind, FileVersionId};
use crate::catalog::path::ArchivePath;
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::{JobContext, NullProgress};
use crate::manifest::Attributes;
use crate::publish::{promised_attributes, OpenedHead, ReadOptions};
use crate::read::read_entry;
use crate::report::{Finding, Severity};
use crate::storage::{
    case_fold_key, AttributeIssue, AttributeKind, CaseBehavior, DirectoryDurability, Existing,
    NameIssue, RestoreDir, Storage, StorageError,
};

/// Progress phase of [`restore`]: `completed` is entries handled.
pub const RESTORE_PHASE: &str = "restore";

/// How many temporary names are tried in one directory before giving up
/// (each collides only with an archive name of the same form).
const TEMP_ATTEMPTS: u32 = 64;

/// What the caller asks of a restoration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreOptions {
    /// Restore setuid and setgid bits (plan O6: only on explicit request;
    /// from an untrusted archive they are a privilege-escalation risk).
    pub restore_setid: bool,
    /// End the job before writing anything if the [`preflight`] finds any
    /// collision or unsupported name (`NAME_COLLISION` or
    /// `NAME_UNSUPPORTED`), instead of restoring everything else.
    pub refuse_on_preflight_exceptions: bool,
    /// Publish each verified file without first flushing its data to the
    /// device ("fast extract"). Off by default. Every file is still verified
    /// before it gets its name, and directories are still flushed at the
    /// end; what is lost is the guarantee that, after a power failure, a
    /// published name holds the file's complete content. A process crash or
    /// cancellation is unaffected. The report says which way it ran
    /// ([`RestoreReport::file_data_synced`]).
    pub skip_file_sync: bool,
    /// What to do with a name that already exists at the destination
    /// ([`OnExisting`]). `Keep` by default.
    pub on_existing: OnExisting,
}

/// What a restoration does with a name that already exists at the
/// destination (owner decision 2026-11-11). Names the archive itself holds
/// twice (or that the destination folds together), names the destination
/// cannot hold, and an existing entry of the other kind are exceptions in
/// every mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OnExisting {
    /// Leave it alone: the entry, and a directory's subtree, is a
    /// `NAME_COLLISION` and is not restored.
    #[default]
    Keep,
    /// Restore into a directory that already exists, adding what it lacks.
    /// An existing file keeps its bytes (`NAME_COLLISION`).
    Merge,
    /// As `Merge`, and an existing regular file is moved to the Recycle Bin
    /// or the Trash ([`RestoreDir::set_aside`]) once the restored file has
    /// been verified, which then takes its name.
    Replace,
}

/// The setuid and setgid bits.
const SETID_BITS: u32 = 0o6000;

/// Why one entry was not restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExceptionKind {
    /// The name exists at the destination, or folds onto one that does.
    Collision,
    /// The destination cannot hold the name.
    UnsupportedName(NameIssue),
    /// An ancestor was not restored, so this entry was skipped. `cause` is
    /// the code of the ancestor's own exception.
    ParentNotRestored { cause: ErrorCode },
    /// The file's content failed verification (stored or content
    /// integrity); it was not published.
    Integrity { code: ErrorCode, message: String },
    /// Any other per-entry failure (the catalog, or the destination refusing
    /// one entry).
    Failed { code: ErrorCode, message: String },
}

impl ExceptionKind {
    /// The stable code this exception is reported under.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Collision => ErrorCode::NameCollision,
            Self::UnsupportedName(_) => ErrorCode::NameUnsupported,
            Self::ParentNotRestored { cause } => *cause,
            Self::Integrity { code, .. } | Self::Failed { code, .. } => *code,
        }
    }
}

/// One entry that was not restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreException {
    pub path: ArchivePath,
    pub kind: ExceptionKind,
}

/// One attribute that was not applied to a restored entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeException {
    pub path: ArchivePath,
    pub issue: AttributeIssue,
}

/// What a restoration did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub files: u64,
    pub directories: u64,
    /// Logical bytes written to restored files.
    pub bytes: u64,
    pub exceptions: Vec<RestoreException>,
    /// The weakest directory flush: `Unconfirmed` if any flush was.
    pub directory_durability: DirectoryDurability,
    /// Each file's data was flushed before it was published; `false` when
    /// [`RestoreOptions::skip_file_sync`] asked not to.
    pub file_data_synced: bool,
    /// `None` if the promised attributes were reconstructed and applied;
    /// otherwise why they were unavailable (nothing was applied).
    pub attributes_unavailable: Option<String>,
    /// Attributes that were not applied to restored entries.
    pub attribute_exceptions: Vec<AttributeException>,
    /// The destination's case behaviour, detected before writing.
    pub case_behavior: CaseBehavior,
    /// Directories that already existed and were restored into
    /// ([`OnExisting::Merge`] and `Replace`); not in `directories`.
    pub merged_directories: u64,
    /// Existing files moved to the Recycle Bin or the Trash and replaced by
    /// the restored file ([`OnExisting::Replace`]); counted in `files` too.
    pub replaced: u64,
}

impl RestoreReport {
    /// An empty report for a restoration into a destination of
    /// `case_behavior`.
    pub(crate) fn empty(case_behavior: CaseBehavior, file_data_synced: bool) -> Self {
        RestoreReport {
            files: 0,
            directories: 0,
            bytes: 0,
            exceptions: Vec::new(),
            directory_durability: DirectoryDurability::Confirmed,
            file_data_synced,
            attributes_unavailable: None,
            attribute_exceptions: Vec::new(),
            case_behavior,
            merged_directories: 0,
            replaced: 0,
        }
    }

    /// Every selected entry was restored and verified, with no exception.
    /// Attributes are reported separately ([`Self::attributes_complete`]).
    pub fn complete(&self) -> bool {
        self.exceptions.is_empty()
    }

    /// Every restored entry received all its promised attributes.
    pub fn attributes_complete(&self) -> bool {
        self.attributes_unavailable.is_none() && self.attribute_exceptions.is_empty()
    }

    /// The report findings: one per entry exception; one per kind of
    /// attribute that was not applied, with a count and the first path; and
    /// one if attributes were unavailable.
    pub fn findings(&self) -> Vec<Finding> {
        let mut out: Vec<Finding> = self
            .exceptions
            .iter()
            .map(|e| {
                let path = String::from_utf8_lossy(e.path.as_stored()).into_owned();
                let (severity, message) = match &e.kind {
                    ExceptionKind::Collision => (
                        Severity::Warning,
                        format!("{path:?} collides with an existing name; nothing was overwritten"),
                    ),
                    ExceptionKind::UnsupportedName(issue) => (
                        Severity::Warning,
                        format!("{path:?} was not restored: the name is {issue}"),
                    ),
                    ExceptionKind::ParentNotRestored { .. } => (
                        Severity::Warning,
                        format!("{path:?} was skipped because a parent directory was not restored"),
                    ),
                    ExceptionKind::Integrity { message, .. }
                    | ExceptionKind::Failed { message, .. } => (
                        Severity::Error,
                        format!("{path:?} was not restored: {message}"),
                    ),
                };
                Finding {
                    code: e.kind.code(),
                    severity,
                    message: Some(message),
                    expected: None,
                    observed: None,
                    affected: None,
                }
            })
            .collect();
        let warn = |message: String| Finding {
            code: ErrorCode::AttributeNotRestored,
            severity: Severity::Warning,
            message: Some(message),
            expected: None,
            observed: None,
            affected: None,
        };
        if let Some(why) = &self.attributes_unavailable {
            out.push(warn(format!(
                "no attributes were restored: they could not be reconstructed ({why})"
            )));
        }
        let mut by_kind: BTreeMap<AttributeKind, (u64, &AttributeException)> = BTreeMap::new();
        for e in &self.attribute_exceptions {
            by_kind.entry(e.issue.attribute).or_insert((0, e)).0 += 1;
        }
        for (kind, (count, first)) in by_kind {
            out.push(warn(format!(
                "{kind:?} was not restored on {count} entr{}, first {:?}: {}",
                if count == 1 { "y" } else { "ies" },
                String::from_utf8_lossy(first.path.as_stored()),
                first.issue.reason
            )));
        }
        out
    }
}

/// Writes go to a [`Storage`] by appending.
struct Appender<'a, S: Storage>(&'a mut S);

impl<S: Storage> Write for Appender<'_, S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .append(buf)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn last_component(path: &ArchivePath) -> &[u8] {
    path.components().last().unwrap_or(path.as_stored())
}

/// `true` for errors that mean the file's content is bad (not the
/// destination or the request).
fn is_integrity(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::StoredIntegrityFailed
            | ErrorCode::ContentIntegrityFailed
            | ErrorCode::ExtentInvalid
            | ErrorCode::MalformedFrame
            | ErrorCode::Truncated
            | ErrorCode::OutOfBounds
    )
}

/// One selected entry: its path, kind, and version.
type Selected = (ArchivePath, EntryKind, FileVersionId);

/// What restoring would meet, found before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preflight {
    pub case_behavior: CaseBehavior,
    /// Entries that will not be restored, and why. Their subtrees are
    /// skipped too (reported as `ParentNotRestored` during the restore).
    pub exceptions: BTreeMap<ArchivePath, ExceptionKind>,
    /// Directories that already exist and will be restored into
    /// ([`OnExisting::Merge`] and `Replace`).
    pub merged: std::collections::BTreeSet<ArchivePath>,
    /// Files that already exist as regular files: with
    /// [`OnExisting::Replace`] they will be moved to the Recycle Bin or the
    /// Trash and replaced. (With `Keep` and `Merge` they are collisions in
    /// `exceptions` instead.)
    pub replaced: std::collections::BTreeSet<ArchivePath>,
    /// Of the `exceptions`: directories that exist but that the backend will
    /// not enter ([`RestoreDir::open_existing_dir`] said no: a link, or on
    /// Linux one that other users can modify), so a caller can say why.
    pub not_entered: std::collections::BTreeSet<ArchivePath>,
    /// Of the `exceptions`: entries the destination would treat as having the
    /// same name as another of the archive's own (case, normalization).
    pub same_name_in_archive: std::collections::BTreeSet<ArchivePath>,
}

/// The restore preflight (plan C6): detect the destination's case
/// behaviour, then find the collisions and unsupported names among
/// `selected` (in path order) before anything is written. With
/// [`OnExisting::Merge`] or `Replace` it looks inside the directories that
/// already exist (opened as [`RestoreDir::open_existing_dir`] allows, never
/// through a link) for the names that are there. Only the case probe writes
/// to the destination, and it removes what it creates.
pub fn preflight<D: RestoreDir>(
    root: &mut D,
    selected: &[Selected],
    on_existing: OnExisting,
) -> Result<Preflight> {
    let case_behavior = root.case_behavior()?;
    let mut exceptions = BTreeMap::new();
    let mut merged = std::collections::BTreeSet::new();
    let mut replaced = std::collections::BTreeSet::new();
    let mut not_entered = std::collections::BTreeSet::new();
    let mut same_name_in_archive = std::collections::BTreeSet::new();
    // Existing directories opened to look inside, by archive path.
    let mut open: std::collections::HashMap<ArchivePath, D> = std::collections::HashMap::new();
    let mut taken: std::collections::HashSet<(Option<ArchivePath>, Vec<u8>)> =
        std::collections::HashSet::new();
    for (path, kind, _) in selected {
        let name = last_component(path);
        if let Some(issue) = root.name_issue(name) {
            exceptions.insert(path.clone(), ExceptionKind::UnsupportedName(issue));
            continue;
        }
        let key = match case_behavior {
            CaseBehavior::Sensitive => name.to_vec(),
            CaseBehavior::Insensitive => case_fold_key(name),
        };
        let parent = path.parent();
        if !taken.insert((parent.clone(), key)) {
            exceptions.insert(path.clone(), ExceptionKind::Collision);
            same_name_in_archive.insert(path.clone());
            continue;
        }
        // The destination is looked at only where the parent exists there:
        // the root, or a directory that will be entered.
        let dir: Option<&mut D> = match &parent {
            None => Some(&mut *root),
            Some(p) if merged.contains(p) => open.get_mut(p),
            Some(_) => None,
        };
        let Some(dir) = dir else { continue };
        let is_dir = *kind == EntryKind::Directory;
        match (dir.existing(name)?, is_dir, on_existing) {
            (Existing::None, _, _) => {}
            (Existing::Directory, true, OnExisting::Merge | OnExisting::Replace) => {
                match dir.open_existing_dir(name)? {
                    Some(d) => {
                        merged.insert(path.clone());
                        open.insert(path.clone(), d);
                    }
                    None => {
                        exceptions.insert(path.clone(), ExceptionKind::Collision);
                        not_entered.insert(path.clone());
                    }
                }
            }
            (Existing::File, false, OnExisting::Replace) => {
                replaced.insert(path.clone());
            }
            _ => {
                exceptions.insert(path.clone(), ExceptionKind::Collision);
            }
        }
    }
    Ok(Preflight {
        case_behavior,
        exceptions,
        merged,
        replaced,
        not_entered,
        same_name_in_archive,
    })
}

/// The entries a restoration of `selection` would handle, in path order:
/// every entry at or under any selected path, with the directories above
/// each. An empty `selection` is the whole commit. A path the commit lacks
/// is `INVALID_ARGUMENT`, before anything is written.
/// Which entries a selection takes: a selected path, anything under one, and
/// the directories above one. Looked up by path bytes, so the cost grows with
/// the entries and their depth, not with entries times selected paths (a
/// retrieve of every match of a large search names many thousands).
struct Selector<'a> {
    chosen: std::collections::HashSet<&'a [u8]>,
    above: std::collections::HashSet<&'a [u8]>,
}

/// The proper prefixes of `b` that end just before a `/`: its ancestors' paths.
fn ancestors(b: &[u8]) -> impl Iterator<Item = &[u8]> {
    b.iter()
        .enumerate()
        .filter(|(_, c)| **c == b'/')
        .filter_map(move |(i, _)| b.get(..i))
}

impl<'a> Selector<'a> {
    fn new(selection: &'a [ArchivePath]) -> Self {
        Selector {
            chosen: selection.iter().map(ArchivePath::as_stored).collect(),
            above: selection
                .iter()
                .flat_map(|u| ancestors(u.as_stored()))
                .collect(),
        }
    }

    fn admits(&self, p: &ArchivePath) -> bool {
        let b = p.as_stored();
        self.chosen.contains(b)
            || self.above.contains(b)
            || ancestors(b).any(|a| self.chosen.contains(a))
    }
}

fn select(head: &OpenedHead, selection: &[ArchivePath]) -> Result<Vec<Selected>> {
    let snapshot = head.catalog.replay(None)?;
    for u in selection {
        if snapshot.get(u).is_none() {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "no entry {:?} in commit {}",
                    String::from_utf8_lossy(u.as_stored()),
                    head.seq()
                ),
            ));
        }
    }
    let wanted = Selector::new(selection);
    Ok(snapshot
        .iter()
        .filter(|(p, _)| selection.is_empty() || wanted.admits(p))
        .map(|(p, e)| (p.clone(), e.kind, e.version))
        .collect())
}

/// What [`restore_selected`] of the same `selection` would meet in `root`,
/// found without restoring anything (the same [`preflight`] the job runs
/// first): collisions and names the destination cannot hold, for a caller
/// that wants to ask the user before it starts. Only the case probe touches
/// `root`, and it removes what it creates. The answer is advice: exclusive
/// creation stays the authority when the job runs.
pub fn preflight_selected<D: RestoreDir>(
    head: &OpenedHead,
    selection: &[ArchivePath],
    root: &mut D,
    on_existing: OnExisting,
) -> Result<(Preflight, u64)> {
    let selected = select(head, selection)?;
    let total = selected.len() as u64;
    Ok((preflight(root, &selected, on_existing)?, total))
}

/// Restore the opened commit's entries, or only `under` and its subtree
/// (with the directories above it), into `root`, which must exist. Entries
/// keep their full archive paths below `root`.
///
/// A long operation: it reports [`RESTORE_PHASE`] progress and honours
/// cancellation between entries and inside each file. `Err` means the job
/// stopped (cancellation, or `root` itself failing). Files restored before
/// that stay restored; a file being written is discarded. Everything else
/// is an exception in the `Ok` report.
pub fn restore<D: RestoreDir>(
    src: &dyn crate::storage::ReadStorage,
    head: &OpenedHead,
    under: Option<&ArchivePath>,
    root: D,
    options: &RestoreOptions,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<RestoreReport> {
    let selection: Vec<ArchivePath> = under.into_iter().cloned().collect();
    restore_selected(src, head, &selection, root, options, opts, ctx)
}

/// [`restore`] of several subtrees in one job: every entry at or under any
/// path in `selection`, with the directories above each, each restored
/// once. An empty `selection` restores the whole commit. Every selected
/// path must exist in the commit (`INVALID_ARGUMENT` otherwise, before
/// anything is written).
pub fn restore_selected<D: RestoreDir>(
    src: &dyn crate::storage::ReadStorage,
    head: &OpenedHead,
    selection: &[ArchivePath],
    mut root: D,
    options: &RestoreOptions,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<RestoreReport> {
    let cat = &head.catalog;
    let selected = select(head, selection)?;
    let total = selected.len() as u64;

    ctx.check_cancelled()?;
    let pre = preflight(&mut root, &selected, options.on_existing)?;
    if options.refuse_on_preflight_exceptions {
        if let Some((p, kind)) = pre.exceptions.iter().next() {
            return Err(MochiError::new(
                kind.code(),
                format!(
                    "{} entr{} cannot be restored here, first {:?} ({}); nothing was written",
                    pre.exceptions.len(),
                    if pre.exceptions.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                    String::from_utf8_lossy(p.as_stored()),
                    kind.code()
                ),
            ));
        }
    }

    let mut report = RestoreReport::empty(pre.case_behavior, !options.skip_file_sync);
    let attributes: Option<BTreeMap<FileVersionId, Attributes>> =
        match promised_attributes(src, head, opts) {
            Ok(a) => Some(a),
            Err(e) => {
                report.attributes_unavailable = Some(e.to_string());
                None
            }
        };
    // The attributes to apply, setuid and setgid removed unless requested.
    let effective = |version: &FileVersionId, path: &ArchivePath| {
        let mut a = *attributes.as_ref()?.get(version)?;
        let mut issue = None;
        if let Some(p) = a.posix.as_mut() {
            if p.mode & SETID_BITS != 0 && !options.restore_setid {
                p.mode &= !SETID_BITS;
                issue = Some(AttributeException {
                    path: path.clone(),
                    issue: AttributeIssue {
                        attribute: AttributeKind::SetId,
                        reason: "setuid/setgid are restored only on request".into(),
                    },
                });
            }
        }
        Some((a, issue))
    };
    // Restored directories, for their attributes at the end.
    let mut restored_dirs: Vec<(ArchivePath, FileVersionId)> = Vec::new();
    // Directories this restoration created, by archive path. `None` key is
    // the root.
    let mut dirs: std::collections::HashMap<Option<ArchivePath>, D> =
        std::collections::HashMap::new();
    dirs.insert(None, root);
    // Directories that were not restored, with their exception's code.
    let mut not_restored: std::collections::HashMap<ArchivePath, ErrorCode> =
        std::collections::HashMap::new();
    let mut temp_seq = 0u64;

    // Each file's read reports its own byte progress; inside a restoration
    // that would interleave a `read` phase with every `restore` event, which
    // restarts a display's rate estimate per file and multiplies the events.
    // The restoration reports entries only; cancellation still reaches every
    // read.
    let quiet = JobContext {
        progress: &NullProgress,
        cancel: ctx.cancel,
    };

    ctx.report(RESTORE_PHASE, 0, Some(total));
    for (done, (path, kind, version)) in selected.iter().enumerate() {
        ctx.check_cancelled()?;
        let is_dir = *kind == EntryKind::Directory;
        let parent_path = path.parent();
        let parent_cause = parent_path
            .as_ref()
            .and_then(|p| not_restored.get(p).copied())
            .unwrap_or(ErrorCode::CatalogInvalid);
        let mut except = |ek: ExceptionKind| {
            if is_dir {
                not_restored.insert(path.clone(), ek.code());
            }
            report.exceptions.push(RestoreException {
                path: path.clone(),
                kind: ek,
            })
        };
        let Some(parent) = dirs.get_mut(&parent_path) else {
            except(ExceptionKind::ParentNotRestored {
                cause: parent_cause,
            });
            continue;
        };
        if let Some(kind) = pre.exceptions.get(path) {
            except(kind.clone());
            continue;
        }
        let name = last_component(path);
        if let Some(issue) = parent.name_issue(name) {
            except(ExceptionKind::UnsupportedName(issue));
            continue;
        }
        match kind {
            // An existing directory the preflight chose to enter: opened again
            // now (never through a link); if it is no longer one this
            // restoration may enter, it is a collision as before.
            EntryKind::Directory if pre.merged.contains(path) => {
                match parent.open_existing_dir(name) {
                    Ok(Some(child)) => {
                        report.merged_directories += 1;
                        dirs.insert(Some(path.clone()), child);
                    }
                    Ok(None) => except(ExceptionKind::Collision),
                    Err(e) => {
                        let e = MochiError::from(e);
                        except(ExceptionKind::Failed {
                            code: e.code,
                            message: e.message,
                        })
                    }
                }
            }
            EntryKind::Directory => match parent.create_dir(name) {
                Ok(child) => {
                    report.directories += 1;
                    dirs.insert(Some(path.clone()), child);
                    restored_dirs.push((path.clone(), *version));
                }
                Err(StorageError::Exists { .. }) => except(ExceptionKind::Collision),
                Err(e) => {
                    let e = MochiError::from(e);
                    except(ExceptionKind::Failed {
                        code: e.code,
                        message: e.message,
                    })
                }
            },
            EntryKind::File => {
                // A temporary name of a form archive entries rarely take; a
                // clash just moves on to the next number.
                let mut created = None;
                for _ in 0..TEMP_ATTEMPTS {
                    temp_seq += 1;
                    let temp = format!(".mochi-restore.{temp_seq}.tmp").into_bytes();
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
                let Some((mut file, temp)) = created else {
                    return Err(MochiError::new(
                        ErrorCode::IoError,
                        "no free temporary name in the destination directory",
                    ));
                };
                let read = read_entry(
                    src,
                    cat,
                    path,
                    version,
                    &mut Appender(&mut file),
                    opts,
                    &quiet,
                )
                .and_then(|r| {
                    if options.skip_file_sync {
                        return Ok(r);
                    }
                    file.sync_data().map(|()| r).map_err(MochiError::from)
                });
                // Replace: the restored file is verified; only now is the
                // existing one moved to the Recycle Bin or the Trash. If that
                // fails it keeps its name and its bytes, and this file is not
                // published.
                let read = match read {
                    Ok(r) if pre.replaced.contains(path) => match parent.set_aside(name) {
                        Ok(()) => Ok((r, true)),
                        Err(e) => {
                            parent.discard(file, &temp)?;
                            except(ExceptionKind::Failed {
                                code: ErrorCode::IoError,
                                message: format!(
                                    "the existing file could not be moved to the Recycle Bin or \
                                     the Trash, so it was kept: {e}"
                                ),
                            });
                            ctx.report(RESTORE_PHASE, done as u64 + 1, Some(total));
                            continue;
                        }
                    },
                    Ok(r) => Ok((r, false)),
                    Err(e) => Err(e),
                };
                match read {
                    Ok((r, set_aside)) => match parent.publish_no_replace(&temp, name) {
                        Ok(()) => {
                            drop(file);
                            report.files += 1;
                            report.bytes += r.logical_len;
                            if set_aside {
                                report.replaced += 1;
                            }
                            if let Some((a, setid)) = effective(version, path) {
                                report.attribute_exceptions.extend(setid);
                                let issues = parent.apply_attributes(name, EntryKind::File, &a);
                                report.attribute_exceptions.extend(issues.into_iter().map(
                                    |issue| AttributeException {
                                        path: path.clone(),
                                        issue,
                                    },
                                ));
                            }
                        }
                        Err(StorageError::Exists { .. }) => {
                            parent.discard(file, &temp)?;
                            except(ExceptionKind::Collision);
                        }
                        Err(e) => {
                            parent.discard(file, &temp)?;
                            return Err(e.into());
                        }
                    },
                    Err(e) => {
                        parent.discard(file, &temp)?;
                        if e.code == ErrorCode::Cancelled {
                            return Err(e);
                        }
                        let kind = if is_integrity(e.code) {
                            ExceptionKind::Integrity {
                                code: e.code,
                                message: e.message,
                            }
                        } else {
                            ExceptionKind::Failed {
                                code: e.code,
                                message: e.message,
                            }
                        };
                        except(kind);
                    }
                }
            }
        }
        ctx.report(RESTORE_PHASE, done as u64 + 1, Some(total));
    }

    // Directory attributes last, deepest first (reverse byte order puts
    // every descendant before its ancestors).
    for (path, version) in restored_dirs.iter().rev() {
        let Some((a, setid)) = effective(version, path) else {
            continue;
        };
        report.attribute_exceptions.extend(setid);
        if let Some(parent) = dirs.get_mut(&path.parent()) {
            let issues = parent.apply_attributes(last_component(path), EntryKind::Directory, &a);
            report
                .attribute_exceptions
                .extend(issues.into_iter().map(|issue| AttributeException {
                    path: path.clone(),
                    issue,
                }));
        }
    }

    // Persist every directory's entries, the root's included.
    for d in dirs.values_mut() {
        if let DirectoryDurability::Unconfirmed(why) = d.sync_directory()? {
            report.directory_durability = DirectoryDurability::Unconfirmed(why);
        }
    }
    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod selector_tests {
    use super::*;

    #[test]
    fn the_selector_takes_what_the_pairwise_rule_takes() {
        let p = |s: &str| ArchivePath::from_stored(s.as_bytes()).unwrap();
        let entries: Vec<ArchivePath> = [
            "a",
            "a/b",
            "a/b/c.txt",
            "a/bc",
            "a/bc/d",
            "ab",
            "ab/c",
            "x",
            "x/y",
            "x/y/z",
            "x/y/z/w.log",
            "logs",
            "logs/2026",
            "logs/2026/app.log",
            "logs/2026/app.log.1",
        ]
        .iter()
        .map(|s| p(s))
        .collect();
        let selections: Vec<Vec<ArchivePath>> = vec![
            vec![p("a/b")],
            vec![p("a")],
            vec![p("x/y/z/w.log"), p("ab")],
            vec![p("logs/2026/app.log")],
            vec![p("a/bc/d"), p("x"), p("logs/2026")],
        ];
        for sel in &selections {
            let s = Selector::new(sel);
            for e in &entries {
                let pairwise = sel
                    .iter()
                    .any(|u| e.is_descendant_of(u) || e == u || u.is_descendant_of(e));
                assert_eq!(s.admits(e), pairwise, "{e:?} in {sel:?}");
            }
        }
    }
}
