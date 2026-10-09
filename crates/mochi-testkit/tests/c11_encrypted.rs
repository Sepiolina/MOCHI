//! C11: the Encrypted profile (spec Annex B.2.10, D20), end to end through
//! the writer and the readers.
//!
//! Oracles are independent of the code under test: the raw bytes of the file
//! (is there any plaintext in them), BLAKE3 over a byte range computed here,
//! the decoded commit record, and what a reader returns for the passphrase it
//! was given.

use mochi_core::commit::CommitRecord;
use mochi_core::keys::read_envelopes;
use mochi_core::publish::{
    open_head, ArchiveWriter, ReadOptions, TailPolicy, Transaction, WriterOptions,
};
use mochi_core::read::{list, read_file};
use mochi_core::ErrorCode;
use mochi_format::digest::stored_object_hash;
use mochi_format::repr::StoredObject;
use mochi_testkit::archive::{encrypted_options, keyed_read, path, read_state, Job};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

const PASS: &str = "correct horse battery staple";

/// 200 bytes: four chunks at the test chunk size of 64.
fn content(seed: u64) -> Vec<u8> {
    deterministic_bytes(seed, 200)
}

fn put(tx: &mut Transaction, p: &str, bytes: &[u8]) {
    tx.put_file(path(p), bytes.to_vec(), attrs(0o644, 0));
}

fn create(s: &SimStorage, opts: WriterOptions) -> ArchiveWriter<SimStorage> {
    ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(7)), opts).unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn reader(pass: &str) -> ReadOptions {
    keyed_read(&[pass])
}

/// The head commit record, read from the raw bytes without any key.
fn head_commit(s: &SimStorage, opts: &ReadOptions) -> (CommitRecord, u64) {
    let loc = mochi_core::publish::locate_head(s, &opts.limits).unwrap();
    let (rec, _) = mochi_core::publish::read_commit(s, &loc.footer, opts).unwrap();
    (rec, loc.footer.fields.commit_offset)
}

#[test]
fn an_encrypted_archive_hides_names_and_content_and_reads_back_with_the_passphrase() {
    let s = SimStorage::new();
    let secret_name = "very-secret-project/plans.txt";
    let data = content(1);
    let mut w = create(&s, encrypted_options(&[PASS]));
    let mut tx = Transaction::new();
    tx.put_dir(path("very-secret-project"), attrs(0o755, 0));
    put(&mut tx, secret_name, &data);
    let out = w.commit(tx, &Job::new().ctx()).unwrap();
    assert_eq!(out.seq, 0);
    drop(w);

    // No plaintext anywhere in the file: not the name, not any 16-byte
    // window of the content, not a Zstandard data frame.
    let raw = s.contents();
    assert!(!contains(&raw, b"very-secret-project"));
    assert!(!contains(&raw, b"plans.txt"));
    for w in data.windows(16).step_by(7) {
        assert!(!contains(&raw, w));
    }
    assert!(
        !contains(&raw, &[0x28, 0xB5, 0x2F, 0xFD]),
        "a bare zstd frame"
    );

    // Read it back with the passphrase.
    let o = reader(PASS);
    let head = open_head(&s, &o).unwrap();
    assert!(head.unlocked.is_some());
    let mut got = Vec::new();
    read_file(
        &s,
        &head,
        &path(secret_name),
        &mut got,
        &o,
        &Job::new().ctx(),
    )
    .unwrap();
    assert_eq!(got, data);
    let names: Vec<_> = list(&head, None)
        .unwrap()
        .into_iter()
        .map(|e| e.path.as_stored().to_vec())
        .collect();
    assert_eq!(
        names,
        vec![
            b"very-secret-project".to_vec(),
            secret_name.as_bytes().to_vec()
        ]
    );
    let state = read_state(&s, &head).unwrap();
    assert_eq!(state.len(), 2);
}

#[test]
fn a_wrong_or_missing_passphrase_is_key_unavailable_and_returns_nothing() {
    let s = SimStorage::new();
    let mut w = create(&s, encrypted_options(&[PASS]));
    let mut tx = Transaction::new();
    put(&mut tx, "a.txt", &content(2));
    w.commit(tx, &Job::new().ctx()).unwrap();
    drop(w);

    for o in [reader("not the passphrase"), ReadOptions::default()] {
        let e = open_head(&s, &o).unwrap_err();
        assert_eq!(e.code, ErrorCode::KeyUnavailable, "{e}");
        assert!(!e.message.contains(PASS) && !e.message.contains("not the passphrase"));
    }
    // Appending is refused the same way, before a byte is written.
    let before = s.contents();
    let err = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(9)),
        WriterOptions {
            read: reader("wrong"),
            ..encrypted_options(&["wrong"])
        },
        TailPolicy::Refuse,
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::KeyUnavailable);
    assert_eq!(s.contents(), before);
}

#[test]
fn append_keeps_the_data_key_and_the_profile() {
    let s = SimStorage::new();
    let mut w = create(&s, encrypted_options(&[PASS]));
    let mut tx = Transaction::new();
    put(&mut tx, "one.txt", &content(3));
    w.commit(tx, &Job::new().ctx()).unwrap();
    drop(w);

    let (mut w, _) = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(11)),
        encrypted_options(&[PASS]),
        TailPolicy::Refuse,
    )
    .unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "two.txt", &content(4));
    let out = w.commit(tx, &Job::new().ctx()).unwrap();
    assert_eq!(out.seq, 1);
    drop(w);

    let o = reader(PASS);
    let head = open_head(&s, &o).unwrap();
    let state = read_state(&s, &head).unwrap();
    assert_eq!(state.len(), 2);
    // Both commits list the same key envelope and the same data key.
    let (c1, off1) = head_commit(&s, &o);
    let envs = read_envelopes(&s, &c1, off1, &o).unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].1.sequence, 0,
        "the envelope was written by commit 0"
    );
}

#[test]
fn the_commit_record_is_schema_2_with_no_time_and_a_hash_checked_data_region() {
    let s = SimStorage::new();
    let mut opts = encrypted_options(&[PASS]);
    opts.record_time = true; // forced off in the Encrypted profile
    let mut w = create(&s, opts);
    let mut tx = Transaction::new();
    put(&mut tx, "x.txt", &content(5));
    tx.at(mochi_core::manifest::Mtime { secs: 1, nanos: 0 }); // an explicit time is dropped too
    w.commit(tx, &Job::new().ctx()).unwrap();
    drop(w);

    let o = ReadOptions::default(); // keyless: the commit needs no key
    let (rec, commit_offset) = head_commit(&s, &o);
    assert!(rec.encrypted());
    assert_eq!(rec.schema_version(), 2);
    assert_eq!(rec.time, None);
    assert_eq!(rec.required_features, vec![1]);
    assert_eq!(rec.key_envelopes.len(), 1);

    // The data region: whole frames of kind 0x184D2A59, ending before the
    // delta manifest, and the hash is BLAKE3 over exactly those bytes.
    let region = rec.data_region.expect("the commit adds data");
    assert!(region.offset + region.stored_len <= rec.delta_manifest.offset);
    assert!(rec.delta_manifest.offset + rec.delta_manifest.stored_len <= commit_offset);
    let raw = s.contents();
    let bytes = &raw[region.offset as usize..(region.offset + region.stored_len) as usize];
    assert_eq!(
        stored_object_hash(StoredObject::from_loaded(bytes.to_vec()).view()),
        region.stored_hash
    );
    // Walk it as consecutive sealed frames, and count them: 200 bytes at a
    // chunk size of 64 is four data objects.
    let (mut at, mut frames) = (0usize, 0);
    while at < bytes.len() {
        assert_eq!(&bytes[at..at + 4], &0x184D_2A59u32.to_le_bytes());
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        // sealed header: version 0, suite 1, kind 0 (a data chunk)
        assert_eq!(&bytes[at + 8..at + 16], &[0, 0, 1, 0, 0, 0, 0, 0]);
        at += 8 + len;
        frames += 1;
    }
    assert_eq!((at, frames), (bytes.len(), 4));
}

#[test]
fn the_unencrypted_descriptor_cannot_be_combined_with_encryption_flags() {
    // Encrypted + TAR is refused at creation (D20 item 4).
    let mut opts = encrypted_options(&[PASS]);
    opts.profile = Some(mochi_core::descriptor::Profile {
        tar_compatible: true,
        encrypted: true,
    });
    let e = ArchiveWriter::create(SimStorage::new(), Box::new(SeqIds::new(1)), opts).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    // An Encrypted archive without a passphrase cannot be created.
    let mut opts = encrypted_options(&[PASS]);
    opts.read = ReadOptions::default();
    let e = ArchiveWriter::create(SimStorage::new(), Box::new(SeqIds::new(1)), opts).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
}
