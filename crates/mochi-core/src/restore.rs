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
use crate::job::JobContext;
use crate::manifest::Attributes;
use crate::publish::{promised_attributes, OpenedHead, ReadOptions};
use crate::read::read_file_in;
use crate::report::{Finding, Severity};
use crate::storage::{
    AttributeIssue, AttributeKind, DirectoryDurability, NameIssue, RestoreDir, Storage,
    StorageError,
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
    /// `None` if the promised attributes were reconstructed and applied;
    /// otherwise why they were unavailable (nothing was applied).
    pub attributes_unavailable: Option<String>,
    /// Attributes that were not applied to restored entries.
    pub attribute_exceptions: Vec<AttributeException>,
}

impl RestoreReport {
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
    let selected: Vec<(ArchivePath, EntryKind, FileVersionId)> = snapshot
        .iter()
        .filter(|(p, _)| match under {
            None => true,
            Some(u) => p.is_descendant_of(u) || *p == u || u.is_descendant_of(p),
        })
        .map(|(p, e)| (p.clone(), e.kind, e.version))
        .collect();
    let total = selected.len() as u64;

    let mut report = RestoreReport {
        files: 0,
        directories: 0,
        bytes: 0,
        exceptions: Vec::new(),
        directory_durability: DirectoryDurability::Confirmed,
        attributes_unavailable: None,
        attribute_exceptions: Vec::new(),
    };
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
                let read = read_file_in(src, cat, path, &mut Appender(&mut file), opts, ctx)
                    .and_then(|r| file.sync_data().map(|()| r).map_err(MochiError::from));
                match read {
                    Ok(r) => match parent.publish_no_replace(&temp, name) {
                        Ok(()) => {
                            drop(file);
                            report.files += 1;
                            report.bytes += r.logical_len;
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
