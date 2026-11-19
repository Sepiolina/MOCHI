//! Shared helpers for the integration tests. Every test goes through the real
//! helper binary and the real client, as the app will.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use mochi_foreign::*;

pub fn helper() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mochi-foreign-helper"))
}

pub fn client() -> ForeignClient {
    ForeignClient::new(helper())
}

pub fn client_with(limits: Limits) -> ForeignClient {
    client().with_limits(limits)
}

pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/foreign")
}

pub fn fixture(name: &str) -> PathBuf {
    fixture_dir().join(name)
}

pub fn names(l: &ListReport) -> Vec<Vec<u8>> {
    l.entries.iter().map(|e| e.name.clone()).collect()
}

pub fn b(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

/// Reads one entry fully, returning the report and the bytes delivered.
pub fn read_all(
    c: &ForeignClient,
    archive: &Path,
    index: u64,
    pw: Option<&Password>,
) -> (ReadReport, Vec<u8>) {
    let mut got = Vec::new();
    let r = c
        .read_entry(archive, index, pw, &mut |chunk| {
            got.extend_from_slice(chunk);
            Ok(())
        })
        .expect("read_entry");
    (r, got)
}

pub fn only_kinds(t: &TestReport, f: impl Fn(&EntryOutcome) -> bool) -> bool {
    t.entries.iter().all(|e| f(&e.outcome))
}

/// An AES-256 ZIP built in memory with the `zip` crate (no installed tool can
/// write one). Because the writer is the same library family as the reader
/// under test, these tests prove the helper's *handling* (labels, fail-closed,
/// tamper detection), not interoperability with WinZip/7-Zip; that gap is
/// listed under Not done.
pub fn aes_zip(password: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in files {
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .with_aes_encryption(zip::AesMode::Aes256, password);
        w.start_file(*name, opts).expect("start_file");
        w.write_all(data).expect("write");
    }
    w.finish().expect("finish").into_inner()
}

/// One entry as `read_many` delivered it.
pub struct Got {
    pub info: EntryInfo,
    pub outcome: EntryOutcome,
    pub bytes: Vec<u8>,
}

struct Collect {
    done: Vec<Got>,
    cur: Option<(EntryInfo, Vec<u8>)>,
}

impl ReadManySink for Collect {
    fn begin(&mut self, info: &EntryInfo) -> std::io::Result<()> {
        assert!(self.cur.is_none(), "begin inside an entry");
        self.cur = Some((info.clone(), Vec::new()));
        Ok(())
    }
    fn data(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.cur
            .as_mut()
            .expect("data outside an entry")
            .1
            .extend_from_slice(bytes);
        Ok(())
    }
    fn finish(&mut self, info: &EntryInfo, outcome: &EntryOutcome) -> std::io::Result<()> {
        let (i, bytes) = self.cur.take().expect("finish outside an entry");
        assert_eq!(i.index, info.index);
        self.done.push(Got {
            info: i,
            outcome: outcome.clone(),
            bytes,
        });
        Ok(())
    }
}

/// Streams `indices` in one helper run; entries come back in the order the
/// helper delivered them, keyed by index through `Got::info`.
pub fn read_many(
    c: &ForeignClient,
    archive: &Path,
    indices: &[u64],
    pw: Option<&Password>,
) -> Result<(ReadManyReport, Vec<Got>), ForeignError> {
    let mut sink = Collect {
        done: Vec::new(),
        cur: None,
    };
    let r = c.read_many(archive, indices, pw, &mut sink)?;
    Ok((r, sink.done))
}

pub fn pw(s: &str) -> Password {
    Password::new(s.as_bytes().to_vec())
}

pub fn crc() -> EntryOutcome {
    EntryOutcome::Matched(CheckSet::of(&[CheckKind::Crc32]))
}

pub fn got(all: &[Got], index: u64) -> &Got {
    all.iter()
        .find(|g| g.info.index == index)
        .expect("entry delivered")
}
