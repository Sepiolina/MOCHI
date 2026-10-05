//! Restore engine (plan C6; spec §10.4, §20.1 "restoration", §23.3 #7):
//! write an opened commit's namespace, or one subtree of it, into a
//! directory.
//!
//! # Rules
//!
//! * **Nothing is ever overwritten or merged.** Every directory is created
//!   exclusively and every file is published without replacing. An entry
//!   that already exists at the destination, or that the destination
//!   filesystem folds onto another entry's name (case, normalization), is
//!   a `NAME_COLLISION` exception. The earlier entry keeps its bytes, and a
//!   colliding directory's subtree is not restored.
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
//! * **Exceptions do not stop the restore.** Each is recorded with its path
//!   and code ([`RestoreReport`]). Only cancellation and failures of the
//!   destination itself end the job early.
//!
//! **Not restored yet:** attributes (mode, ownership, times, Windows
//! attribute bits: plan O6). The report says so (`attributes_restored:
//! false`), and its findings include a warning that names them.

use std::io::Write;

use crate::catalog::namespace::EntryKind;
use crate::catalog::path::ArchivePath;
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::publish::{OpenedHead, ReadOptions};
use crate::read::read_file_in;
use crate::report::{Finding, Severity};
use crate::storage::{DirectoryDurability, NameIssue, RestoreDir, Storage, StorageError};

/// Progress phase of [`restore`]: `completed` is entries handled.
pub const RESTORE_PHASE: &str = "restore";

/// How many temporary names are tried in one directory before giving up
/// (each collides only with an archive name of the same form).
const TEMP_ATTEMPTS: u32 = 64;

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
    /// Always `false` until attribute restoration lands (plan O6).
    pub attributes_restored: bool,
}

impl RestoreReport {
    /// Every selected entry was restored, verified, with no exception.
    /// Attributes are reported separately ([`Self::attributes_restored`]).
    pub fn complete(&self) -> bool {
        self.exceptions.is_empty()
    }

    /// The report findings: one per exception, plus a warning that
    /// attributes were not restored.
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
        if !self.attributes_restored {
            out.push(Finding {
                code: ErrorCode::UnsupportedFeature,
                severity: Severity::Warning,
                message: Some(
                    "attributes (permissions, ownership, times, Windows attribute bits) were \
                     not restored: not implemented in this build"
                        .into(),
                ),
                expected: None,
                observed: None,
                affected: None,
            });
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
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<RestoreReport> {
    let cat = &head.catalog;
    let snapshot = cat.replay(None)?;
    if let Some(u) = under {
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
    let selected: Vec<(ArchivePath, EntryKind)> = snapshot
        .iter()
        .filter(|(p, _)| match under {
            None => true,
            Some(u) => p.is_descendant_of(u) || *p == u || u.is_descendant_of(p),
        })
        .map(|(p, e)| (p.clone(), e.kind))
        .collect();
    let total = selected.len() as u64;

    let mut report = RestoreReport {
        files: 0,
        directories: 0,
        bytes: 0,
        exceptions: Vec::new(),
        directory_durability: DirectoryDurability::Confirmed,
        attributes_restored: false,
    };
    // Directories this restoration created, by archive path. `None` key is
    // the root.
    let mut dirs: std::collections::HashMap<Option<ArchivePath>, D> =
        std::collections::HashMap::new();
    dirs.insert(None, root);
    // Directories that were not restored, with their exception's code.
    let mut not_restored: std::collections::HashMap<ArchivePath, ErrorCode> =
        std::collections::HashMap::new();
    let mut temp_seq = 0u64;

    ctx.report(RESTORE_PHASE, 0, Some(total));
    for (done, (path, kind)) in selected.iter().enumerate() {
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
        let name = last_component(path);
        if let Some(issue) = parent.name_issue(name) {
            except(ExceptionKind::UnsupportedName(issue));
            continue;
        }
        match kind {
            EntryKind::Directory => match parent.create_dir(name) {
                Ok(child) => {
                    report.directories += 1;
                    dirs.insert(Some(path.clone()), child);
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
                let read = read_file_in(src, cat, path, &mut Appender(&mut file), opts, ctx)
                    .and_then(|r| file.sync_data().map(|()| r).map_err(MochiError::from));
                match read {
                    Ok(r) => match parent.publish_no_replace(&temp, name) {
                        Ok(()) => {
                            drop(file);
                            report.files += 1;
                            report.bytes += r.logical_len;
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

    // Persist every directory's entries, the root's included.
    for d in dirs.values_mut() {
        if let DirectoryDurability::Unconfirmed(why) = d.sync_directory()? {
            report.directory_durability = DirectoryDurability::Unconfirmed(why);
        }
    }
    Ok(report)
}
