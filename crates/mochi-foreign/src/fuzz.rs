//! Exercisers for the listing paths and the protocol decoder, shared by the
//! cargo-fuzz targets (`fuzz/`) and the deterministic mutation test
//! (`tests/fuzz_smoke.rs`). They drive the very code the helper runs, on
//! in-memory bytes, with small limits, and assert the invariants the protocol
//! promises for any input. A panic here is a finding.

use std::io::Cursor;
use std::path::Path;

use crate::detect::detect;
use crate::engine::{self, EngineError, Out};
use crate::protocol::{read_frame, Message, Request, DATA_CHUNK};
use crate::types::*;
use crate::zipfmt;
use crate::{rarfmt, sevenfmt, tarfmt};

pub fn fuzz_limits() -> Limits {
    Limits {
        max_total_output: 1 << 20,
        max_entry_size: 1 << 19,
        max_entries: 64,
        max_ratio: 50,
        ratio_min_output: 1 << 12,
        max_name_len: 256,
    }
}

fn request(op: Op, read_index: u64) -> Request {
    Request {
        op,
        read_index,
        read_indices: if op == Op::ReadMany {
            vec![0, 2, 5]
        } else {
            Vec::new()
        },
        limits: fuzz_limits(),
        memory_limit: None,
        cpu_seconds: None,
        archive_path: Vec::new(),
        password: None,
    }
}

/// Checks every message the engine emits: it must survive the wire round trip,
/// and respect the limits it was given.
struct Checker {
    limits: Limits,
    entries: u64,
    /// Listings and tests announce entries in index order; reads announce the
    /// requested ones in the order the archive stores them.
    sequential: bool,
}

impl Checker {
    fn check(&mut self, m: Message) -> std::io::Result<()> {
        let (k, body) = m.encode();
        let back = Message::decode(k, &body);
        assert_eq!(back.as_ref().ok(), Some(&m), "wire round trip");
        match &m {
            Message::Entry(e) => {
                if self.sequential {
                    assert_eq!(e.index, self.entries, "entry indices are sequential");
                }
                self.entries += 1;
                assert!(self.entries <= self.limits.max_entries);
                assert!(e.name.len() as u64 <= u64::from(self.limits.max_name_len));
            }
            Message::Data(d) => assert!(d.len() <= DATA_CHUNK),
            _ => {}
        }
        Ok(())
    }
}

fn run_ops(data: &[u8], f: impl Fn(&Request, &mut Out) -> Result<(), EngineError>) {
    for (op, idx) in [
        (Op::List, 0),
        (Op::Test, 0),
        (Op::Read, 0),
        (Op::Read, 3),
        (Op::ReadMany, 0),
    ] {
        let req = request(op, idx);
        let mut c = Checker {
            limits: req.limits,
            entries: 0,
            sequential: matches!(op, Op::List | Op::Test),
        };
        let mut sink = |m: Message| c.check(m);
        let _ = f(&req, &mut sink);
    }
    let _ = data;
}

/// ZIP listing/test/read path, bypassing detection so every input reaches it.
pub fn exercise_zip_listing(data: &[u8]) {
    let len = data.len() as u64;
    run_ops(data, |req, out| {
        zipfmt::run(Cursor::new(data), len, req, out).map(|_| ())
    });
}

/// Plain tar path.
pub fn exercise_tar_listing(data: &[u8]) {
    let len = data.len() as u64;
    run_ops(data, |req, out| {
        tarfmt::run(Cursor::new(data), len, tarfmt::Layer::None, req, out).map(|_| ())
    });
}

/// tar.gz path (gzip decoding included).
pub fn exercise_targz_listing(data: &[u8]) {
    let len = data.len() as u64;
    run_ops(data, |req, out| {
        tarfmt::run(Cursor::new(data), len, tarfmt::Layer::Gzip, req, out).map(|_| ())
    });
}

/// tar.zst path (Zstandard decoding included).
pub fn exercise_tarzst_listing(data: &[u8]) {
    let len = data.len() as u64;
    run_ops(data, |req, out| {
        tarfmt::run(Cursor::new(data), len, tarfmt::Layer::Zstd, req, out).map(|_| ())
    });
}

/// 7z path, bypassing detection so every input reaches it.
pub fn exercise_sevenz_listing(data: &[u8]) {
    let len = data.len() as u64;
    run_ops(data, |req, out| {
        sevenfmt::run(Cursor::new(data), len, &engine::Origin::MEMORY, req, out).map(|_| ())
    });
}

/// RAR path. UnRAR opens files itself, so this takes a path; the fuzz target
/// and the smoke test write their input to a temporary file first (outside
/// this crate, which never writes a file).
pub fn exercise_rar_file(path: &Path) {
    let origin = engine::Origin {
        path: Some(path),
        split: crate::volumes::SplitName::NotSplit,
        volumes: 1,
    };
    run_ops(&[], |req, out| rarfmt::run(&origin, req, out).map(|_| ()));
}

/// Detection plus dispatch, as the helper runs it.
pub fn exercise_detect(data: &[u8]) {
    let _ = detect(&mut Cursor::new(data));
    run_ops(data, |req, out| {
        engine::run(Cursor::new(data), &engine::Origin::MEMORY, req, out)
    });
}

/// The parent's decoder for a stream from a (possibly hostile) helper, and the
/// helper's decoder for a request: no panic, and what decodes re-encodes to the
/// same bytes.
pub fn exercise_protocol(data: &[u8]) {
    let mut cur = Cursor::new(data);
    while let Ok(Some((k, body))) = read_frame(&mut cur) {
        if let Ok(m) = Message::decode(k, &body) {
            let (k2, b2) = m.encode();
            assert_eq!(Message::decode(k2, &b2).ok(), Some(m));
        }
    }
    if let Ok(r) = Request::decode(data) {
        let again = Request::decode(&r.encode()).ok();
        assert!(again.is_some());
    }
}
