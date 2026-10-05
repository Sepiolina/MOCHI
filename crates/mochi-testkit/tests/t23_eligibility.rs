//! T23: tail eligibility (spec Annex B.2 D14; gate G8).
//!
//! A tail is *eligible* for explicit truncation only if it walks as complete
//! frames that are neither footers nor descriptors (optionally ending in one
//! frame cut short by EOF), no footer marker occurs where a complete footer
//! could fit, it contains no descriptor frame, and no unrecognised bytes.
//! Eligibility is a conservative screen, not proof. A tail that is not
//! eligible is `Unresolved`, and a writer refuses to truncate it.

use mochi_core::publish::{
    locate_head, open_head, ArchiveWriter, HeadSource, ReadOptions, TailPolicy, TailState,
};
use mochi_core::ErrorCode;
use mochi_format::footer::FOOTER_FRAME_LEN;
use mochi_format::registry::{self, FOOTER_MAGIC, SKIPPABLE_HEADER_LEN};
use mochi_testkit::archive::{build, scripted_history, test_options};
use mochi_testkit::{SeqIds, SimStorage};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

/// A three-commit archive and the offset of its latest footer.
fn archive() -> (Vec<u8>, usize) {
    let s = SimStorage::new();
    build(s.clone(), 7, &scripted_history()).unwrap();
    let bytes = s.contents();
    let footer = bytes.len() - FOOTER_FRAME_LEN as usize;
    (bytes, footer)
}

fn tail_of(bytes: &[u8]) -> TailState {
    locate_head(&SimStorage::from_bytes(bytes.to_vec()), &opts().limits)
        .unwrap()
        .tail
}

/// Asserts `bytes` has an `Unresolved` tail naming `reason`, that readers
/// fall back to commit 1, and that a truncating append is refused without
/// changing a byte.
fn assert_not_eligible(bytes: &[u8], reason: &str) {
    match tail_of(bytes) {
        TailState::Unresolved { reason: r, .. } => {
            assert!(r.contains(reason), "reason {r:?} should name {reason:?}")
        }
        other => panic!("expected Unresolved, got {other:?}"),
    }
    let s = SimStorage::from_bytes(bytes.to_vec());
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!((head.seq(), head.location.source), (1, HeadSource::Scan));
    let e = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(99)),
        test_options(),
        TailPolicy::TruncateWithoutQuarantine,
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::TailUnresolved);
    assert_eq!(s.contents(), bytes, "nothing truncated");
}

/// **Checklist DoD.** The latest footer's skippable header flipped to the
/// descriptor kind (`0x57`): readers fall back, and the tail is not eligible.
#[test]
fn t23_a_footer_flipped_to_0x57_is_not_eligible() {
    let (mut bytes, footer) = archive();
    assert_eq!(
        u32::from_le_bytes(bytes[footer..footer + 4].try_into().unwrap()),
        registry::COMMIT_FOOTER
    );
    bytes[footer] = 0x57;
    assert_eq!(
        u32::from_le_bytes(bytes[footer..footer + 4].try_into().unwrap()),
        registry::ARCHIVE_DESCRIPTOR
    );
    // The payload magic is intact, so the footer-marker scan already sees it.
    assert_not_eligible(&bytes, "footer payload magic");
}

/// The D14 single-event case: one corruption destroys **both** footer
/// markers (the header, flipped to `0x57`, and the payload magic; both lie
/// in the footer's first 16 bytes). No footer marker remains, so only the
/// descriptor rule keeps the tail from looking like an interrupted write.
#[test]
fn t23_a_footer_with_both_markers_destroyed_is_caught_by_the_descriptor_rule() {
    let (mut bytes, footer) = archive();
    bytes[footer] = 0x57;
    let magic = footer + SKIPPABLE_HEADER_LEN;
    assert_eq!(bytes[magic..magic + 8], FOOTER_MAGIC);
    bytes[magic] ^= 0xFF;
    assert_not_eligible(&bytes, "a descriptor frame at offset");
}

/// **Checklist DoD.** A complete descriptor frame in the tail (a copy of the
/// real one, appended after the head) makes the tail ineligible.
#[test]
fn t23_a_descriptor_frame_in_the_tail_is_not_eligible() {
    let (bytes, _) = archive();
    let committed = bytes.len();
    let desc_len = 8 + u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let mut with_tail = bytes.clone();
    with_tail.extend_from_slice(&bytes[..desc_len]);
    match tail_of(&with_tail) {
        TailState::Unresolved { reason, len } => {
            assert_eq!(len as usize, desc_len);
            assert!(
                reason.contains(&format!("descriptor frame at offset {committed}")),
                "{reason}"
            );
            assert!(reason.contains("D14"), "{reason}");
        }
        other => panic!("expected Unresolved, got {other:?}"),
    }
    let s = SimStorage::from_bytes(with_tail.clone());
    let e = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(99)),
        test_options(),
        TailPolicy::TruncateWithoutQuarantine,
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::TailUnresolved);
    assert_eq!(s.contents(), with_tail);
}

/// A descriptor frame cut short by EOF is not the torn final write of a
/// commit (no commit writes a descriptor after offset 0): not eligible.
#[test]
fn t23_a_cut_short_descriptor_at_eof_is_not_eligible() {
    let (bytes, _) = archive();
    let mut with_tail = bytes.clone();
    with_tail.extend_from_slice(&bytes[..12]);
    match tail_of(&with_tail) {
        TailState::Unresolved { reason, .. } => assert!(reason.contains("descriptor"), "{reason}"),
        other => panic!("expected Unresolved, got {other:?}"),
    }
}

/// Control: ordinary complete frames after the head (a copy of a commit's
/// delta manifest) and a frame cut short by EOF stay eligible.
#[test]
fn t23_ordinary_frames_stay_eligible() {
    let (bytes, footer) = archive();
    let s = SimStorage::from_bytes(bytes.clone());
    let head = open_head(&s, &opts()).unwrap();
    let r = head.commit.delta_manifest;
    let frame = bytes[r.offset as usize..(r.offset + r.stored_len) as usize].to_vec();
    assert!(footer > r.offset as usize);

    let mut complete = bytes.clone();
    complete.extend_from_slice(&frame);
    assert!(matches!(
        tail_of(&complete),
        TailState::Uncommitted {
            incomplete_final_frame: false,
            ..
        }
    ));
    let mut cut = bytes.clone();
    cut.extend_from_slice(&frame[..frame.len() / 2]);
    assert!(matches!(
        tail_of(&cut),
        TailState::Uncommitted {
            incomplete_final_frame: true,
            ..
        }
    ));
}
