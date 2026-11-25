//! The local head history and evidence log, shared with the command-line
//! tool (spec Annex B.2.22, owner decision 2026-11-23, which replaces B.3
//! item 5: "the desktop app keeps no head history").
//!
//! The store is `mochi_core::storage::os::history::HeadStore` at its default
//! location, the same one `mochi` uses without `--state-dir`: a head the app
//! records anchors the CLI's freshness check and the reverse, and `mochi
//! health` and the app's health summary read the same evidence. On by
//! default, as in the CLI; Settings, "Remember archives on this computer",
//! turns it off (`speed::Settings::local_history`), and the app then keeps
//! this session's evidence in memory only and freshness has no anchor.
//!
//! Everything here is best effort: failing to record is never a failure of
//! the job, because the anchor is then only older, never wrong (D8). Nothing
//! is written into any archive; verification stays read-only.
//!
//! Tests never touch the user's state directory: under `cfg(test)` the store
//! is only what [`use_dir_for_test`] set on the calling thread.

use mochi_core::health::{EvidenceHead, EvidenceRecord};
use mochi_core::object::ArchiveId;
use mochi_core::publish::{
    locate_head, read_commit, CommitOutcome, PublishDurability, ReadOptions,
};
use mochi_core::storage::os::history::{HeadStore, SeenHead};
use mochi_core::storage::ReadStorage;
use mochi_core::timestamp::Timestamp;
use mochi_core::verify::FreshnessAnchor;
use mochi_format::digest::CommitId;

#[cfg(test)]
thread_local! {
    static TEST_DIR: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Tests: use `dir` as the state directory on this thread (`None`: no
/// store, as with the setting off).
#[cfg(test)]
pub fn use_dir_for_test(dir: Option<&std::path::Path>) {
    TEST_DIR.with(|d| *d.borrow_mut() = dir.map(std::path::Path::to_path_buf));
}

/// The shared store, or `None` when the user turned local history off or
/// this machine has no state directory.
pub fn store() -> Option<HeadStore> {
    if !crate::speed::settings().local_history {
        return None;
    }
    #[cfg(test)]
    {
        TEST_DIR.with(|d| {
            d.borrow()
                .as_deref()
                .and_then(|p| HeadStore::locate(Some(p)))
        })
    }
    #[cfg(not(test))]
    {
        HeadStore::locate(None)
    }
}

/// The archive ID the head's commit record claims (readable without a key),
/// to look up its anchor before verifying.
pub fn claimed_archive_id(src: &dyn ReadStorage, read: &ReadOptions) -> Option<ArchiveId> {
    let loc = locate_head(src, &read.limits).ok()?;
    read_commit(src, &loc.footer, read)
        .ok()
        .map(|(c, _)| c.archive_id)
}

/// The freshness anchor for `archive`: the head last seen on this computer
/// by the app or the CLI (D8), or none.
pub fn anchor(store: Option<&HeadStore>, archive: Option<ArchiveId>) -> FreshnessAnchor {
    let (Some(store), Some(id)) = (store, archive) else {
        return FreshnessAnchor::None;
    };
    match store.get(&id) {
        Ok(Some(seen)) => FreshnessAnchor::LocalHistory {
            seq: seen.seq,
            commit_id: seen.commit_id,
        },
        // A damaged history file leaves freshness without an anchor, as in
        // the CLI (which warns); never a guess.
        Ok(None) | Err(_) => FreshnessAnchor::None,
    }
}

/// Remember a head this client saw (after a check in which nothing failed,
/// or a commit it published).
pub fn remember(store: Option<&HeadStore>, archive: ArchiveId, seq: u64, commit_id: CommitId) {
    if let Some(s) = store {
        let _ = s.record(&archive, SeenHead { seq, commit_id });
    }
}

/// Append an evidence record to the shared log.
pub fn record_evidence(store: Option<&HeadStore>, archive: &ArchiveId, record: &EvidenceRecord) {
    if let Some(s) = store {
        let _ = s.append_evidence(archive, record);
    }
}

/// A commit the app published: its head as the anchor, and a commit record
/// with what the publication knew of its durability (Annex B.2.22).
pub fn committed(archive: ArchiveId, outcome: &CommitOutcome) {
    committed_head(archive, outcome.seq, outcome.commit_id, &outcome.durability);
}

/// The same for a head the app published by other means (the new archive a
/// repair wrote).
pub fn committed_head(
    archive: ArchiveId,
    seq: u64,
    commit_id: CommitId,
    durability: &PublishDurability,
) {
    let store = store();
    let Some(store) = store.as_ref() else { return };
    remember(Some(store), archive, seq, commit_id);
    let exit_code = match durability {
        PublishDurability::Durable => 0,
        PublishDurability::DirectoryUnconfirmed(_) => 2,
    };
    if let Ok(now) = Timestamp::now() {
        let head = EvidenceHead {
            seq,
            commit_id: commit_id.to_hex(),
        };
        record_evidence(
            Some(store),
            &archive,
            &EvidenceRecord::from_commit(head, durability, &now, exit_code),
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn tests_never_reach_the_user_s_state_directory() {
        use_dir_for_test(None);
        assert!(store().is_none());
        let dir = tempfile::tempdir().unwrap();
        use_dir_for_test(Some(dir.path()));
        let s = store().unwrap();
        assert_eq!(s.path(), dir.path().join("heads.json"));
        use_dir_for_test(None);
    }

    #[test]
    fn anchors_come_from_the_store_and_a_missing_one_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let s = HeadStore::locate(Some(dir.path())).unwrap();
        let a = ArchiveId::from_bytes([7; 32]);
        assert!(matches!(anchor(Some(&s), Some(a)), FreshnessAnchor::None));
        assert!(matches!(anchor(None, Some(a)), FreshnessAnchor::None));
        let id = CommitId::from_bytes([3; 32]);
        remember(Some(&s), a, 3, id);
        match anchor(Some(&s), Some(a)) {
            FreshnessAnchor::LocalHistory { seq, commit_id } => {
                assert_eq!((seq, commit_id), (3, id));
            }
            other => panic!("{other:?}"),
        }
        // A damaged file is no anchor, never an error for the job.
        std::fs::write(s.path(), b"{oops").unwrap();
        assert!(matches!(anchor(Some(&s), Some(a)), FreshnessAnchor::None));
    }
}
