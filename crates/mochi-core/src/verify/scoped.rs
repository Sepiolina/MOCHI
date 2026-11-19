//! Verification scoped to the commits after one already-verified head
//! (desktop feedback: knowing quickly that a commit just made is sound must
//! not cost a pass over the whole archive).
//!
//! # What it checks
//!
//! Given the last head a client verified ([`VerifiedHead`]: archive ID,
//! sequence, commit ID), [`verify_commits_since`] reports on exactly the
//! commits after it:
//!
//! * the head opens: its commit record, descriptor, delta manifest and
//!   catalog image or replayed segment are hash-verified **before** they are
//!   parsed, exactly as [`crate::publish::open_head`] does for any reader;
//! * **chain link:** the previous verified head is in the head's history, at
//!   its sequence, with its commit ID (so the new commits really extend what
//!   was verified, and an older or different archive is `FAIL`);
//! * **new objects:** every data object the new commits' delta manifests
//!   introduce is located (one data frame inside the committed prefix, before
//!   the head's commit frame), its stored-object hash is checked, and it is
//!   decoded against its recorded lengths and chunk hash (the same checks as
//!   `verify` at the content-integrity level).
//!
//! # What it does not check
//!
//! Everything older than the new commits: no old object is read, so nothing
//! about them is claimed. The result carries a
//! [`ScopedVerification::whole_archive`] status that is **always**
//! `UNKNOWN`, and a [`ScopedVerification::not_checked`] list that says so in
//! words. Files are not reassembled (no restoration evidence), nothing is
//! compared with a freshness anchor beyond the chain link, and the
//! durability, searchability and retention dimensions are not assessed.
//! A scoped `PASS` is therefore never a statement about the archive's health;
//! run [`crate::verify::verify`] for that.
//!
//! Read-only, like all verification (spec §20.2).

use std::collections::BTreeSet;

use crate::catalog::Catalog;
use crate::error::{ErrorCode, MochiError};
use crate::job::JobContext;
use crate::manifest::ManifestKind;
use crate::object::{decode_verified, load_stored, ObjectId};
use crate::publish::{commit_history, open_head, read_bound_manifest, ReadOptions};
use crate::report::{Finding, SeqRange, Severity, SkippedItem};
use crate::status::Status;
use crate::storage::{ReadStorage, StorageReader};

use super::{classify, phase, referential, ErrorClass, VerifiedHead};

/// The outcome of [`verify_commits_since`]. Its statuses describe only the
/// scope in [`ScopedVerification::from_seq`] (exclusive) to
/// [`ScopedVerification::to_seq`] (inclusive).
#[derive(Debug, Clone)]
pub struct ScopedVerification {
    /// The previously verified head's sequence; not itself checked again.
    pub from_seq: u64,
    /// The head's sequence, when it could be opened.
    pub to_seq: Option<u64>,
    /// The head that was opened, when one was.
    pub head: Option<VerifiedHead>,
    /// The previous head is in the head's history with its commit ID.
    pub chain: Status,
    /// The data objects the new commits introduce: located, hash-checked and
    /// decoded. `PASS` only when every one of them was.
    pub new_objects: Status,
    /// Always `UNKNOWN`: nothing here is a claim about the whole archive.
    pub whole_archive: Status,
    /// Data objects introduced by the new commits.
    pub objects_expected: u64,
    pub objects_checked: u64,
    /// Stored bytes of those objects.
    pub bytes_expected: u64,
    pub bytes_checked: u64,
    pub findings: Vec<Finding>,
    /// What this run did not examine, in words.
    pub not_checked: Vec<SkippedItem>,
    /// The run could not finish (I/O, limits, cancellation, a key it lacks):
    /// nothing is concluded from the unfinished part.
    pub operational_error: bool,
}

impl ScopedVerification {
    fn new(from_seq: u64) -> Self {
        ScopedVerification {
            from_seq,
            to_seq: None,
            head: None,
            chain: Status::Unknown,
            new_objects: Status::Unknown,
            whole_archive: Status::Unknown,
            objects_expected: 0,
            objects_checked: 0,
            bytes_expected: 0,
            bytes_checked: 0,
            findings: Vec::new(),
            not_checked: vec![
                SkippedItem {
                    item: format!("commits 0 through {}", from_seq),
                    reason: "outside the scope of this check; their objects were not read"
                        .to_string(),
                },
                SkippedItem {
                    item: "file reassembly".to_string(),
                    reason: "this check reads the new objects only; it is not a restoration test"
                        .to_string(),
                },
            ],
            operational_error: false,
        }
    }

    /// The status of the scope only: `FAIL` if any part failed, `PASS` only
    /// if the chain link and every new object checked out, otherwise
    /// `UNKNOWN`. Never a statement about the whole archive.
    pub fn scope_status(&self) -> Status {
        let parts = [self.chain, self.new_objects];
        if parts.contains(&Status::Fail) {
            Status::Fail
        } else if parts.contains(&Status::Unsupported) {
            Status::Unsupported
        } else if self.operational_error || parts.iter().any(|s| *s != Status::Pass) {
            Status::Unknown
        } else {
            Status::Pass
        }
    }

    /// Whether the new head may be recorded as verified-through: nothing
    /// failed and the run finished.
    pub fn head_is_anchorable(&self) -> bool {
        self.head.is_some() && self.scope_status() == Status::Pass
    }

    fn finding(&mut self, code: ErrorCode, message: String) {
        self.findings.push(Finding {
            code,
            severity: Severity::Error,
            message: Some(message),
            expected: None,
            observed: None,
            affected: self.to_seq.map(|last| SeqRange {
                first: self.from_seq.saturating_add(1),
                last,
            }),
        });
    }

    /// Record an error the way `verify` classifies it. Returns the class.
    fn error(&mut self, what: &str, e: &MochiError) -> ErrorClass {
        let class = classify(e.code);
        match class {
            ErrorClass::Violation | ErrorClass::Unsupported => {
                self.finding(e.code, format!("{what}: {}", e.message));
            }
            ErrorClass::Operational => {
                self.operational_error = true;
                self.not_checked.push(SkippedItem {
                    item: what.to_string(),
                    reason: format!("the run was interrupted ({}: {})", e.code, e.message),
                });
            }
        }
        class
    }
}

/// Verify the commits after `since` (see the module docs for exactly what
/// that covers). `since` is the head a client last verified, for example the
/// one a full [`crate::verify::verify`] returned or the writer reported at
/// its previous commit. Does not return `Err`: failures become findings.
pub fn verify_commits_since(
    src: &dyn ReadStorage,
    opts: &ReadOptions,
    since: &VerifiedHead,
    ctx: &JobContext<'_>,
) -> ScopedVerification {
    let mut out = ScopedVerification::new(since.seq);
    ctx.report(phase::SCOPED, 0, None);

    let head = match open_head(src, opts) {
        Ok(h) => h,
        Err(e) => {
            if out.error("opening the head", &e) == ErrorClass::Violation {
                out.chain = Status::Fail;
                out.new_objects = Status::Fail;
            }
            return out;
        }
    };
    out.to_seq = Some(head.seq());
    out.head = Some(VerifiedHead {
        archive_id: head.commit.archive_id,
        seq: head.seq(),
        commit_id: head.commit_id,
    });

    let history = match commit_history(src, opts) {
        Ok(h) => h,
        Err(e) => {
            if out.error("walking the commit chain", &e) == ErrorClass::Violation {
                out.chain = Status::Fail;
            }
            return out;
        }
    };

    // The chain link: the new commits extend what was verified.
    let linked = head.commit.archive_id == since.archive_id
        && history
            .iter()
            .any(|e| e.commit.seq == since.seq && e.commit_id == since.commit_id);
    if linked {
        out.chain = Status::Pass;
    } else {
        out.chain = Status::Fail;
        out.finding(
            ErrorCode::FreshnessFailed,
            format!(
                "the archive's history does not contain the previously verified commit {} \
                 ({}) at sequence {}: this is not a continuation of what was verified",
                since.commit_id.to_hex(),
                if head.commit.archive_id == since.archive_id {
                    "same archive"
                } else {
                    "a different archive"
                },
                since.seq
            ),
        );
        out.not_checked.push(SkippedItem {
            item: "new objects".to_string(),
            reason: "the new commits cannot be tied to the previously verified head".to_string(),
        });
        return out;
    }

    // The objects the new commits introduce: from their own delta manifests,
    // each hash-verified against its commit before it is decoded.
    let mut ids: BTreeSet<ObjectId> = BTreeSet::new();
    for e in history.iter().filter(|e| e.commit.seq > since.seq) {
        match read_bound_manifest(
            src,
            &e.commit,
            &e.commit.delta_manifest,
            e.commit_offset,
            ManifestKind::Delta,
            opts,
        ) {
            Ok(m) => ids.extend(m.chunks.iter().map(|c| c.record.id)),
            Err(err) => {
                let what = format!("the delta manifest of commit {}", e.commit.seq);
                if out.error(&what, &err) == ErrorClass::Violation {
                    out.new_objects = Status::Fail;
                }
                return out;
            }
        }
    }
    check_new_objects(src, opts, &head.catalog, &head, &ids, ctx, &mut out);
    out
}

fn check_new_objects(
    src: &dyn ReadStorage,
    opts: &ReadOptions,
    cat: &Catalog,
    head: &crate::publish::OpenedHead,
    ids: &BTreeSet<ObjectId>,
    ctx: &JobContext<'_>,
    out: &mut ScopedVerification,
) {
    let known: BTreeSet<ObjectId> = match cat.object_ids() {
        Ok(v) => v.into_iter().collect(),
        Err(e) => {
            out.error("listing the catalog's objects", &e);
            return;
        }
    };
    let commit_offset = head.location.footer.fields.commit_offset;
    let prefix = match StorageReader::prefix(src, head.location.committed_len) {
        Ok(r) => r,
        Err(e) => {
            out.error("reading the committed prefix", &MochiError::from(e));
            return;
        }
    };
    let key = crate::keys::catalog_key(opts, cat);
    let seal = key.as_deref().map(crate::keys::Unlocked::context);

    let total = ids.len() as u64;
    out.objects_expected = total;
    let mut failed = false;
    let mut cancelled = false;
    ctx.report(phase::SCOPED, 0, Some(total));
    for (i, id) in ids.iter().enumerate() {
        if ctx.check_cancelled().is_err() {
            cancelled = true;
            out.operational_error = true;
            out.not_checked.push(SkippedItem {
                item: "new objects".to_string(),
                reason: "the check was cancelled".to_string(),
            });
            break;
        }
        let record = match cat.object(id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                failed = true;
                out.finding(
                    ErrorCode::ReferenceInvalid,
                    format!(
                        "object {}: a new commit introduces it but the catalog has no record",
                        id.to_hex()
                    ),
                );
                continue;
            }
            Err(e) => {
                failed |=
                    out.error(&format!("object {}", id.to_hex()), &e) == ErrorClass::Violation;
                continue;
            }
        };
        out.bytes_expected = out.bytes_expected.saturating_add(record.stored_len);
        let result =
            referential(cat, &record, &known, commit_offset, &prefix, opts).and_then(|at| {
                let stored = load_stored(src, at, &record, &opts.limits)?;
                decode_verified(&record, &stored, &opts.limits, seal.as_ref())
                    .map(|_| ())
                    .map_err(|e| MochiError::new(e.code, format!("at offset {at}: {}", e.message)))
            });
        match result {
            Ok(()) => {
                out.objects_checked += 1;
                out.bytes_checked = out.bytes_checked.saturating_add(record.stored_len);
            }
            Err(e) => {
                failed |=
                    out.error(&format!("object {}", id.to_hex()), &e) == ErrorClass::Violation;
            }
        }
        ctx.report(phase::SCOPED, i as u64 + 1, Some(total));
    }
    out.new_objects = if failed {
        Status::Fail
    } else if cancelled || out.operational_error || out.objects_checked != total {
        Status::Unknown
    } else {
        Status::Pass
    };
}
