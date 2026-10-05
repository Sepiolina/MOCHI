//! Replay segments: the base rule and chain walk of plan T11 (spec Annex B.2
//! D10.6). A *segment* is a base checkpoint commit *b* and the delta commits
//! *b*+1 … *h* whose state is *b*'s checkpoint plus their delta manifests.
//! Replay itself is `catalog::SegmentApplier`; the open paths in
//! `crate::publish` (`open_head`, `open_at_footer`, `open_append`) use both.
//!
//! # Rules (review decisions 2026-10-02)
//!
//! * **The base rule, for every delta in the segment** (decision 16), not
//!   only the head: each commit *j* with *b* < *j* ≤ *h* is a delta whose
//!   base is (commit ID, sequence) = (*id_b*, *b*); commit *b*, reached by
//!   the authenticated parent walk, is a checkpoint with ID *id_b*. A
//!   checkpoint inside the segment means some delta's base skips it.
//! * **The base footer hint is not trusted and not required to match**
//!   (decision 17). The parent walk establishes commit *b* by ID and
//!   sequence; a mismatching base hint is reported in [`SegmentInfo`] and
//!   otherwise ignored. Parent hints keep the existing rule (a parent "not
//!   at its hint" is `RECORD_INVALID`); that asymmetry is recorded for spec
//!   clarification.
//! * **One descriptor across the segment** (Annex B.2 D10.6, "One
//!   descriptor per segment", 2026-10-04): every commit in the segment
//!   references the same descriptor object; a difference is
//!   `DESCRIPTOR_INVALID` (D12 "mismatched").
//! * **No search** (D10.6): nothing here, or in [`walk_segment`], looks for
//!   another checkpoint, and a failure never yields an earlier state.
//!
//! The delta-manifest parent link (review amendment 2, replaced 2026-10-03,
//! checklist Q22) is
//! [`check_delta_parent_link`].

use crate::commit::{CommitRecord, Metadata};
use crate::error::{ErrorCode, MochiError, Result};
use crate::manifest::{Manifest, ManifestKind};
use crate::publish::{walk_back, HistoryEntry, ReadOptions};
use crate::storage::ReadStorage;
use mochi_format::digest::CommitId;

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::RecordInvalid, msg)
}

/// What [`check_segment`] established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentInfo {
    pub base_seq: u64,
    pub base_commit_id: CommitId,
    /// Where the walk found commit *b*'s footer.
    pub base_footer_offset: u64,
    /// The head's recorded base hint, if it differs from
    /// `base_footer_offset`. Not an error (decision 17); a diagnostic.
    pub base_hint_mismatch: Option<u64>,
    /// Δ of the checkpoint trigger (Annex B.2.3) at the head: the stored
    /// bytes of the delta manifests, commit records, and footers of commits
    /// *b*+1 … *h*. Generated checkpoint bytes (the base's image and
    /// snapshot) and data objects are not counted. 0 for a checkpoint head.
    pub delta_bytes: u64,
    /// *B* of the checkpoint trigger: the base's image plus snapshot
    /// manifest, stored bytes.
    pub base_bytes: u64,
}

/// Checkpoint-trigger accounting (Annex B.2.3) for a segment given base
/// first: (Δ, *B*). Writer policy, not validation, so archive-derived sums
/// saturate rather than fail. A commit's record is the bytes from its
/// commit offset to its footer, plus the footer frame.
pub fn segment_accounting(entries: &[HistoryEntry]) -> (u64, u64) {
    use mochi_format::footer::FOOTER_FRAME_LEN;
    let base_bytes = match entries.first().map(|e| e.commit.metadata) {
        Some(Metadata::Checkpoint { image, snapshot }) => {
            image.stored_len.saturating_add(snapshot.stored_len)
        }
        _ => 0,
    };
    let delta_bytes = entries.iter().skip(1).fold(0u64, |acc, e| {
        let record = e
            .footer_offset
            .saturating_sub(e.commit_offset)
            .saturating_add(FOOTER_FRAME_LEN);
        acc.saturating_add(e.commit.delta_manifest.stored_len)
            .saturating_add(record)
    });
    (delta_bytes, base_bytes)
}

/// Check a segment, given its commits in ascending sequence order from the
/// base checkpoint to the head, as the parent walk produced them.
pub fn check_segment(entries: &[HistoryEntry]) -> Result<SegmentInfo> {
    let (Some(first), Some(head)) = (entries.first(), entries.last()) else {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            "a segment has at least one commit",
        ));
    };

    // The chain itself: contiguous, linked by ID, one archive. The walk
    // already guarantees this; re-checking keeps this function sound on its
    // own input.
    for pair in entries.windows(2) {
        let (prev, next) = (&pair[0], &pair[1]);
        let linked = next
            .commit
            .parent
            .is_some_and(|p| p.commit_id == prev.commit_id && p.seq == prev.commit.seq);
        if !linked || prev.commit.seq.checked_add(1) != Some(next.commit.seq) {
            return Err(invalid(format!(
                "commit {} does not follow commit {} in the parent chain",
                next.commit.seq, prev.commit.seq
            )));
        }
        if next.commit.archive_id != prev.commit.archive_id {
            return Err(invalid(format!(
                "commit {} belongs to a different archive than its parent",
                next.commit.seq
            )));
        }
    }

    let (id_b, b, hint_b) = match head.commit.metadata {
        Metadata::Checkpoint { .. } => {
            if entries.len() != 1 {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    "a checkpoint head is a segment of exactly itself",
                ));
            }
            (head.commit_id, head.commit.seq, head.footer_offset)
        }
        Metadata::Delta { base } => (base.commit_id, base.seq, base.footer_offset),
    };

    // Commit b: reached by the walk, so its ID is authenticated by the
    // chain. It must be the recorded base, and a checkpoint.
    if first.commit.seq != b {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            format!(
                "segment starts at commit {}, but the head's base is commit {b}",
                first.commit.seq
            ),
        ));
    }
    if first.commit_id != id_b {
        return Err(invalid(format!(
            "commit {} names base checkpoint {} at sequence {b}, but the commit at sequence {b} \
             in its ancestry has a different ID (D10.6)",
            head.commit.seq,
            id_b.to_hex()
        )));
    }
    if !first.commit.metadata.is_checkpoint() {
        return Err(invalid(format!(
            "commit {} names commit {b} as its base, which is a delta, not a checkpoint (D10.6)",
            head.commit.seq
        )));
    }

    // Every later commit: a delta on exactly this base.
    for e in &entries[1..] {
        match e.commit.metadata {
            Metadata::Checkpoint { .. } => {
                return Err(invalid(format!(
                    "commit {} is a checkpoint, so commit {} must not name the earlier commit {b} \
                     as its base (D10.6)",
                    e.commit.seq,
                    e.commit.seq + 1
                )))
            }
            Metadata::Delta { base } => {
                if base.commit_id != id_b || base.seq != b {
                    return Err(invalid(format!(
                        "commit {} names base {} at sequence {}, but its segment's base is \
                         commit {b} (D10.6)",
                        e.commit.seq,
                        base.commit_id.to_hex(),
                        base.seq
                    )));
                }
            }
        }
    }

    // One descriptor per segment (D10.6; see module docs).
    for e in &entries[1..] {
        if e.commit.descriptor != first.commit.descriptor {
            return Err(MochiError::new(
                ErrorCode::DescriptorInvalid,
                format!(
                    "commit {} references a different archive descriptor than its base \
                     checkpoint {b}",
                    e.commit.seq
                ),
            ));
        }
    }

    let (delta_bytes, base_bytes) = segment_accounting(entries);
    Ok(SegmentInfo {
        delta_bytes,
        base_bytes,
        base_seq: b,
        base_commit_id: id_b,
        base_footer_offset: first.footer_offset,
        base_hint_mismatch: (hint_b != first.footer_offset).then_some(hint_b),
    })
}

/// Review amendment 2 as replaced on 2026-10-03 (checklist Q22): for
/// replayed commit *j*, the delta manifest's parent sequence equals *j* − 1
/// and `parent.1` equals the stored hash in commit *j* − 1's **key-6**
/// manifest reference. This applies equally when commit *j* − 1 is a
/// checkpoint; the base snapshot (key 5) is not an alternative target. A
/// mismatch is `RECORD_INVALID`. The caller passes commit *j* − 1 as `prev`
/// (the open paths take it from the contiguous, ID-linked segment).
pub fn check_delta_parent_link(delta: &Manifest, prev: &CommitRecord) -> Result<()> {
    if delta.kind != ManifestKind::Delta {
        return Err(invalid("only a delta manifest has a parent link"));
    }
    let Some(link) = delta.parent else {
        return Err(invalid(format!(
            "delta manifest {} has no parent link",
            delta.commit_seq
        )));
    };
    if link.seq != prev.seq {
        return Err(invalid(format!(
            "delta manifest {} links to sequence {}, not to commit {}",
            delta.commit_seq, link.seq, prev.seq
        )));
    }
    if link.delta_manifest_hash != prev.delta_manifest.stored_hash {
        return Err(invalid(format!(
            "delta manifest {}'s parent link does not name commit {}'s delta manifest \
             (commit key 6)",
            delta.commit_seq, prev.seq
        )));
    }
    Ok(())
}

/// Walk from `head` back to its base along authenticated parent links and
/// check the segment. Reads only the footers and commit records of the
/// segment, plus the target of any parent hint it follows (a bad hint's
/// target is read to find out it is bad); it never scans for a checkpoint.
pub fn walk_segment(
    src: &dyn ReadStorage,
    head: HistoryEntry,
    opts: &ReadOptions,
) -> Result<(Vec<HistoryEntry>, SegmentInfo)> {
    let down_to = match head.commit.metadata {
        Metadata::Checkpoint { .. } => head.commit.seq,
        Metadata::Delta { base } => {
            // Structure already guarantees base.seq < seq; re-check before
            // using it to bound a walk.
            if base.seq >= head.commit.seq {
                return Err(invalid("the base checkpoint is not before this commit"));
            }
            base.seq
        }
    };
    let entries = walk_back(src, head, down_to, opts)?;
    let info = check_segment(&entries)?;
    Ok((entries, info))
}

#[cfg(test)]
mod tests {
    use mochi_format::digest::StoredObjectHash;

    use super::*;
    use crate::commit::{uuid_v4, CommitLink, ObjectRef};
    use crate::manifest::ParentLink;
    use crate::object::ArchiveId;

    fn r(offset: u64, len: u64, h: u8) -> ObjectRef {
        ObjectRef {
            offset,
            stored_len: len,
            stored_hash: StoredObjectHash::from_bytes([h; 32]),
        }
    }

    fn checkpoint_meta(seq: u64) -> Metadata {
        Metadata::Checkpoint {
            image: r(1000 * seq + 100, 64, 3),
            snapshot: r(1000 * seq + 200, 64, 5),
        }
    }

    /// The footer offset the fake archive puts commit `seq` at.
    fn footer_at(seq: u64) -> u64 {
        1000 * seq + 900
    }

    /// Build a chain from a list of forms: `true` = checkpoint, `false` =
    /// delta on the rule's base. Each commit's IDs are real.
    fn chain(forms: &[bool]) -> Vec<HistoryEntry> {
        let mut out: Vec<HistoryEntry> = Vec::new();
        let mut base: Option<CommitLink> = None;
        for (seq, &cp) in forms.iter().enumerate() {
            let seq = seq as u64;
            let parent = out.last().map(|p| CommitLink {
                commit_id: p.commit_id,
                seq: p.commit.seq,
                footer_offset: p.footer_offset,
            });
            let metadata = if cp {
                checkpoint_meta(seq)
            } else {
                Metadata::Delta {
                    base: base.expect("commit 0 is a checkpoint"),
                }
            };
            out.push(entry(seq, parent, metadata));
            if cp {
                let e = out.last().unwrap();
                base = Some(CommitLink {
                    commit_id: e.commit_id,
                    seq,
                    footer_offset: e.footer_offset,
                });
            }
        }
        out
    }

    fn entry(seq: u64, parent: Option<CommitLink>, metadata: Metadata) -> HistoryEntry {
        let commit = CommitRecord {
            archive_id: ArchiveId::from_bytes([1; 32]),
            seq,
            transaction_id: uuid_v4([seq as u8; 16]),
            parent,
            metadata,
            delta_manifest: r(1000 * seq + 50, 40, 0x40 + seq as u8),
            required_features: vec![],
            time: None,
            descriptor: r(0, 56, 6),
        };
        let commit_id = commit.commit_id().unwrap();
        HistoryEntry {
            footer_offset: footer_at(seq),
            commit_offset: footer_at(seq) - 100,
            commit,
            commit_id,
        }
    }

    /// Replace commit `seq` (re-linking every later commit so the chain stays
    /// authentic), keeping everything else.
    fn rebuild(
        mut es: Vec<HistoryEntry>,
        seq: usize,
        f: impl FnOnce(&mut CommitRecord),
    ) -> Vec<HistoryEntry> {
        f(&mut es[seq].commit);
        es[seq].commit_id = es[seq].commit.commit_id().unwrap();
        for i in seq + 1..es.len() {
            let prev = es[i - 1].commit_id;
            if let Some(p) = es[i].commit.parent.as_mut() {
                p.commit_id = prev;
            }
            es[i].commit_id = es[i].commit.commit_id().unwrap();
        }
        es
    }

    fn base_of(es: &[HistoryEntry]) -> CommitLink {
        match es.last().unwrap().commit.metadata {
            Metadata::Delta { base } => base,
            Metadata::Checkpoint { .. } => panic!("head is a checkpoint"),
        }
    }

    #[test]
    fn valid_segments() {
        // C D D D: segment 0..=3.
        let es = chain(&[true, false, false, false]);
        let info = check_segment(&es).unwrap();
        assert_eq!(info.base_seq, 0);
        assert_eq!(info.base_footer_offset, footer_at(0));
        assert_eq!(info.base_hint_mismatch, None);
        // C D C D D: segment 2..=4.
        let es = chain(&[true, false, true, false, false]);
        assert_eq!(check_segment(&es[2..]).unwrap().base_seq, 2);
        // Checkpoint head: itself.
        assert_eq!(check_segment(&es[2..3]).unwrap().base_seq, 2);
    }

    #[test]
    fn tampered_base_id_is_rejected() {
        let es = chain(&[true, false, false]);
        let es = rebuild(es, 2, |c| {
            if let Metadata::Delta { base } = &mut c.metadata {
                base.commit_id = CommitId::from_bytes([0xBB; 32]);
            }
        });
        let e = check_segment(&es).unwrap_err();
        assert_eq!(e.code, ErrorCode::RecordInvalid, "{e}");
    }

    #[test]
    fn base_off_the_ancestry_is_rejected() {
        // A valid checkpoint at sequence 0 of *another* archive.
        let other = {
            let mut e = entry(0, None, checkpoint_meta(0));
            e.commit.archive_id = ArchiveId::from_bytes([9; 32]);
            e.commit_id = e.commit.commit_id().unwrap();
            e
        };
        let es = chain(&[true, false, false]);
        let es = rebuild(es, 1, |c| {
            c.metadata = Metadata::Delta {
                base: CommitLink {
                    commit_id: other.commit_id,
                    seq: 0,
                    footer_offset: footer_at(0),
                },
            }
        });
        let es = rebuild(es, 2, |c| {
            c.metadata = Metadata::Delta {
                base: CommitLink {
                    commit_id: other.commit_id,
                    seq: 0,
                    footer_offset: footer_at(0),
                },
            }
        });
        assert_eq!(
            check_segment(&es).unwrap_err().code,
            ErrorCode::RecordInvalid
        );
    }

    #[test]
    fn base_that_is_a_delta_is_rejected() {
        // C D D, with commit 2 naming delta commit 1 as its base.
        let es = chain(&[true, false, false]);
        let d1 = CommitLink {
            commit_id: es[1].commit_id,
            seq: 1,
            footer_offset: es[1].footer_offset,
        };
        let es = rebuild(es, 2, |c| c.metadata = Metadata::Delta { base: d1 });
        let e = check_segment(&es[1..]).unwrap_err();
        assert_eq!(e.code, ErrorCode::RecordInvalid);
        assert!(e.message.contains("not a checkpoint"), "{e}");
    }

    #[test]
    fn base_that_skips_a_later_checkpoint_is_rejected() {
        // C D C D, with commit 3 naming commit 0 (skipping checkpoint 2).
        let es = chain(&[true, false, true, false]);
        let b0 = base_of(&es[..2]);
        let es = rebuild(es, 3, |c| c.metadata = Metadata::Delta { base: b0 });
        let e = check_segment(&es).unwrap_err();
        assert_eq!(e.code, ErrorCode::RecordInvalid);
        assert!(e.message.contains("is a checkpoint"), "{e}");
    }

    #[test]
    fn inconsistent_segment_is_rejected() {
        // C D C D D: commit 4 correctly copies its parent's base (2), but
        // commit 3 names base 0. Checking only the head would pass.
        let es = chain(&[true, false, true, false, false]);
        let b0 = base_of(&es[..2]);
        let es = rebuild(es, 3, |c| c.metadata = Metadata::Delta { base: b0 });
        let e = check_segment(&es[2..]).unwrap_err();
        assert_eq!(e.code, ErrorCode::RecordInvalid, "{e}");
        assert!(e.message.contains("commit 3"), "{e}");
    }

    #[test]
    fn wrong_base_hint_is_reported_not_rejected() {
        // Decision 17: identity, ancestry, sequence, and checkpoint status
        // are mandatory; the hint is not.
        let es = chain(&[true, false, false]);
        let es = rebuild(es, 2, |c| {
            if let Metadata::Delta { base } = &mut c.metadata {
                base.footer_offset = 12_345;
            }
        });
        let info = check_segment(&es).unwrap();
        assert_eq!(info.base_footer_offset, footer_at(0));
        assert_eq!(info.base_hint_mismatch, Some(12_345));
    }

    #[test]
    fn descriptor_mismatch_in_segment_is_descriptor_invalid() {
        let es = chain(&[true, false, false]);
        let es = rebuild(es, 1, |c| c.descriptor = r(0, 56, 0x77));
        assert_eq!(
            check_segment(&es).unwrap_err().code,
            ErrorCode::DescriptorInvalid
        );
    }

    #[test]
    fn broken_chain_input_is_rejected() {
        let es = chain(&[true, false, false]);
        // A gap.
        let gapped = vec![es[0].clone(), es[2].clone()];
        assert_eq!(
            check_segment(&gapped).unwrap_err().code,
            ErrorCode::RecordInvalid
        );
        // A segment that does not start at the base.
        assert_eq!(
            check_segment(&es[1..]).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }

    fn delta_manifest(seq: u64, link: Option<ParentLink>) -> Manifest {
        Manifest {
            archive_id: ArchiveId::from_bytes([1; 32]),
            commit_seq: seq,
            transaction_id: [0; 16],
            parent: link,
            kind: ManifestKind::Delta,
            chunks: vec![],
            file_versions: vec![],
            ops: vec![],
            entries: vec![],
            required_features: vec![],
        }
    }

    #[test]
    fn parent_link_names_the_previous_key_6() {
        let es = chain(&[true, false]);
        let base = &es[0].commit;
        let key6 = base.delta_manifest.stored_hash;
        let ok = delta_manifest(
            1,
            Some(ParentLink {
                seq: 0,
                delta_manifest_hash: key6,
            }),
        );
        check_delta_parent_link(&ok, base).unwrap();

        // The base checkpoint's snapshot manifest (key 5): never key 6.
        let Metadata::Checkpoint { snapshot, .. } = base.metadata else {
            unreachable!()
        };
        assert_ne!(snapshot.stored_hash, key6);
        for bad in [
            Some(ParentLink {
                seq: 0,
                delta_manifest_hash: snapshot.stored_hash,
            }),
            Some(ParentLink {
                seq: 0,
                delta_manifest_hash: es[1].commit.delta_manifest.stored_hash,
            }),
            Some(ParentLink {
                seq: 7,
                delta_manifest_hash: key6,
            }),
            None,
        ] {
            let e = check_delta_parent_link(&delta_manifest(1, bad), base).unwrap_err();
            assert_eq!(e.code, ErrorCode::RecordInvalid, "{bad:?}");
        }
    }
}
