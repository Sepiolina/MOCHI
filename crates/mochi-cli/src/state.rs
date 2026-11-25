//! This client's local evidence: the last-seen head of each archive and the
//! evidence log `mochi health` reads (spec Annex B.1 D8, plan K5).
//!
//! The store is `mochi_core::storage::os::history`, shared with the desktop
//! app (spec Annex B.2.22): a head either one records anchors the other's
//! freshness check. Its location, file format, and rules are documented
//! there and in `docs/cli.md`, rule 3 and rule 13.

pub use mochi_core::storage::os::history::{
    EvidenceRead, HeadStore, SeenHead, EVIDENCE_DIR, FILE_NAME,
};
