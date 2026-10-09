//! C10: the TAR-compatible profile writes a TAR stream per commit (spec 7.2,
//! Annex B.2.9 D19; plan C10).
//!
//! The oracle is independent of the writer: `zstd` (libzstd, the reference
//! decoder) decodes the whole file, skipping every skippable frame, and a
//! small TAR reader written here (not `mochi_core::tar`) lists what it
//! finds. What the stream must hold is computed from the transactions.

use mochi_core::catalog::path::ArchivePath;
use mochi_core::descriptor::Profile;
use mochi_core::manifest::{Attributes, Mtime, PosixAttributes};
use mochi_core::publish::{
    open_head, ArchiveWriter, CheckpointPolicy, Dedup, ReadOptions, TailPolicy, WriterOptions,
};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{read_state, test_options, Job};
use mochi_testkit::replay::Model;
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

const TAR: Profile = Profile {
    tar_compatible: true,
    encrypted: false,
};

fn tar_options() -> WriterOptions {
    WriterOptions {
        profile: Some(TAR),
        ..test_options()
    }
}

/// One member as the reference reader sees it: path, directory?, bytes.
type Seen = (Vec<u8>, bool, Vec<u8>);

fn octal(field: &[u8]) -> u64 {
    let digits: Vec<u8> = field
        .iter()
        .copied()
        .take_while(|b| *b != 0 && *b != b' ')
        .collect();
    u64::from_str_radix(std::str::from_utf8(&digits).unwrap(), 8).unwrap()
}

/// Read concatenated TAR streams the way `tar --ignore-zeros` does: zero
/// blocks are skipped, a pax `x` header's `path` overrides the next name.
fn read_tar(mut bytes: &[u8]) -> Vec<Seen> {
    let mut out = Vec::new();
    let mut pax_path: Option<Vec<u8>> = None;
    while !bytes.is_empty() {
        assert!(bytes.len() >= 512, "a partial block");
        let (head, rest) = bytes.split_at(512);
        bytes = rest;
        if head.iter().all(|b| *b == 0) {
            continue;
        }
        let size = usize::try_from(octal(&head[124..136])).unwrap();
        let padded = size.div_ceil(512) * 512;
        assert!(bytes.len() >= padded, "content past the end");
        let (content, rest) = bytes.split_at(padded);
        bytes = rest;
        assert!(content[size..].iter().all(|b| *b == 0), "padding is zero");
        let content = &content[..size];
        match head[156] {
            b'x' => {
                let mut rec = content;
                while !rec.is_empty() {
                    let sp = rec.iter().position(|b| *b == b' ').unwrap();
                    let len: usize = std::str::from_utf8(&rec[..sp]).unwrap().parse().unwrap();
                    let body = &rec[sp + 1..len - 1];
                    assert_eq!(rec[len - 1], b'\n');
                    if let Some(v) = body.strip_prefix(b"path=") {
                        pax_path = Some(v.to_vec());
                    }
                    rec = &rec[len..];
                }
            }
            flag @ (b'0' | b'5') => {
                let name = pax_path.take().unwrap_or_else(|| {
                    let n = &head[..100];
                    n[..n.iter().position(|b| *b == 0).unwrap_or(100)].to_vec()
                });
                out.push((name, flag == b'5', content.to_vec()));
            }
            other => panic!("unexpected typeflag {other}"),
        }
    }
    out
}

fn decode(s: &SimStorage) -> Vec<u8> {
    zstd::stream::decode_all(&s.contents()[..]).unwrap()
}

fn file(name: &str, bytes: &[u8]) -> Seen {
    (name.as_bytes().to_vec(), false, bytes.to_vec())
}

/// A directory member's name ends in `/` (D19 rule 4).
fn dir(name: &str) -> Seen {
    (format!("{name}/").into_bytes(), true, Vec::new())
}

fn a0() -> Attributes {
    Attributes::default()
}

/// A model that remembers, per commit, which members its puts should produce.
struct Script {
    model: Model,
    expect: Vec<Seen>,
}

impl Script {
    fn new() -> Self {
        Self {
            model: Model::default(),
            expect: Vec::new(),
        }
    }
    fn file(&mut self, p: &str, bytes: &[u8]) {
        self.model.file(p, bytes.to_vec(), a0());
        self.expect.push(file(p, bytes));
    }
    fn dir(&mut self, p: &str) {
        self.model.dir(p, a0());
        self.expect.push(dir(p));
    }
    fn delete(&mut self, p: &str) {
        self.model.delete(p);
    }
    /// A rename re-writes the version's bytes at the new path (D19 rule 6).
    fn rename(&mut self, from: &str, to: &str, bytes: &[u8]) {
        self.model.rename(from, to);
        self.expect.push(file(to, bytes));
    }
    fn end(&mut self, t: i64) {
        self.model.end(t);
    }
}

fn create(s: &SimStorage, opts: WriterOptions) -> ArchiveWriter<SimStorage> {
    ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(7)), opts).unwrap()
}

fn commit_all(w: &mut ArchiveWriter<SimStorage>, steps: &[mochi_testkit::replay::Step]) {
    let job = Job::new();
    for st in steps {
        w.commit(st.tx.clone(), &job.ctx()).unwrap();
    }
}

fn history() -> (Script, Vec<u8>, Vec<u8>) {
    let a1 = deterministic_bytes(1, 200);
    let a2 = deterministic_bytes(3, 130);
    let b = deterministic_bytes(2, 70);
    let mut sc = Script::new();
    sc.dir("d");
    sc.file("d/a", &a1);
    sc.file("b", &b);
    sc.file("c", b"");
    sc.end(0);
    sc.file("d/a", &a2);
    sc.file("e", &deterministic_bytes(4, 512));
    sc.end(10);
    sc.rename("b", "d/b", &b);
    sc.delete("c");
    sc.end(20);
    sc.delete("e");
    sc.end(30);
    (sc, a1, a2)
}

/// The decoded file is the commits' streams in order, and each stream holds
/// exactly the commit's puts: a rename re-writes the version, a delete-only
/// commit writes nothing.
#[test]
fn c10_zstd_decodes_the_file_to_the_streams_of_the_puts() {
    let (sc, _, _) = history();
    let s = SimStorage::new();
    let mut w = create(&s, tar_options());
    commit_all(&mut w, &sc.model.steps);
    w.close().unwrap();
    assert_eq!(read_tar(&decode(&s)), sc.expect);
}

/// The profile changes the file, not what a MOCHI reader sees: the snapshot
/// of every commit equals the model's, as for a Core archive.
#[test]
fn c10_core_readers_see_the_same_snapshots_as_without_the_profile() {
    let (sc, _, _) = history();
    for policy in [CheckpointPolicy::EveryCommit, CheckpointPolicy::Every(3)] {
        let s = SimStorage::new();
        let mut w = create(&s, tar_options());
        w.set_checkpoint_policy(policy).unwrap();
        commit_all(&mut w, &sc.model.steps);
        w.close().unwrap();
        let head = open_head(&s, &ReadOptions::default()).unwrap();
        assert!(head.descriptor.tar_compatible);
        let want = &sc.model.steps.last().unwrap().after;
        assert_eq!(&read_state(&s, &head).unwrap(), want);
    }
}

/// Dedup is off in the profile: identical files are written twice, each
/// where its member is. Asking for in-archive dedup there is refused.
#[test]
fn c10_identical_files_are_written_twice_and_dedup_is_refused() {
    let same = deterministic_bytes(9, 300);
    let mut sc = Script::new();
    sc.file("x", &same);
    sc.file("y", &same);
    sc.end(0);
    sc.file("z", &same);
    sc.end(1);
    let s = SimStorage::new();
    let mut w = create(&s, tar_options());
    commit_all(&mut w, &sc.model.steps);
    w.close().unwrap();
    assert_eq!(read_tar(&decode(&s)), sc.expect);
    let at = s.contents().len();
    // Three copies of 300 bytes at chunk size 64 compress to well over one.
    assert!(at > 300, "{at}");

    let e = ArchiveWriter::create(
        SimStorage::new(),
        Box::new(SeqIds::new(1)),
        WriterOptions {
            dedup: Dedup::InArchive,
            ..tar_options()
        },
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    // Explicitly off is the same as the profile's default.
    let t = SimStorage::new();
    let mut w = create(
        &t,
        WriterOptions {
            dedup: Dedup::Off,
            ..tar_options()
        },
    );
    commit_all(&mut w, &sc.model.steps);
    w.close().unwrap();
    assert_eq!(read_tar(&decode(&t)), sc.expect);
}

/// Without the profile the same history deduplicates, so the stream cannot
/// be recovered: the profile is what makes `zstd -dc` a TAR.
#[test]
fn c10_the_default_profile_writes_no_stream() {
    let same = deterministic_bytes(9, 300);
    let mut sc = Script::new();
    sc.file("x", &same);
    sc.file("y", &same);
    sc.end(0);
    let s = SimStorage::new();
    let mut w = create(&s, test_options());
    commit_all(&mut w, &sc.model.steps);
    w.close().unwrap();
    let decoded = decode(&s);
    assert!(decoded.len() < 2 * same.len() + 1024, "deduplicated");
    assert!(
        std::panic::catch_unwind(|| read_tar(&decoded)).is_err(),
        "a Core archive's chunks are not a TAR stream"
    );
}

/// The profile belongs to the archive: a later session appends streams
/// without being told, and may name the profile again.
#[test]
fn c10_appending_keeps_the_profile() {
    let (sc, _, _) = history();
    let s = SimStorage::new();
    let mut w = create(&s, tar_options());
    commit_all(&mut w, &sc.model.steps[..2]);
    w.close().unwrap();
    for (seed, asked) in [(100, None), (200, Some(TAR))] {
        let (mut w, _) = ArchiveWriter::open_append(
            s.clone(),
            Box::new(SeqIds::new(seed)),
            WriterOptions {
                profile: asked,
                ..test_options()
            },
            TailPolicy::Refuse,
        )
        .unwrap();
        let n = if asked.is_none() { 2 } else { 3 };
        commit_all(&mut w, &sc.model.steps[n..n + 1]);
        w.close().unwrap();
    }
    assert_eq!(read_tar(&decode(&s)), sc.expect);
}

/// Long paths, non-UTF-8 paths, large attributes, an empty file, and a file
/// that is not a multiple of the block or the chunk size.
#[test]
fn c10_pax_headers_and_odd_sizes_decode() {
    let long = format!("{}/{}", "d".repeat(90), "f".repeat(120));
    let mut m = Model::default();
    let mut expect = Vec::new();
    let big = Attributes {
        posix: Some(PosixAttributes {
            mode: 0o600,
            uid: 5_000_000,
            gid: 7,
        }),
        windows: None,
        mtime: Some(Mtime {
            secs: 99_999_999_999,
            nanos: 5,
        }),
    };
    m.dir(&"d".repeat(90), a0());
    expect.push(dir(&"d".repeat(90)));
    for (name, bytes) in [
        (long.as_str(), deterministic_bytes(1, 511)),
        ("empty", Vec::new()),
        ("block", deterministic_bytes(2, 512)),
        ("block+1", deterministic_bytes(3, 513)),
        ("one", vec![7]),
    ] {
        m.file(name, bytes.clone(), big);
        expect.push(file(name, &bytes));
    }
    // A name that is not UTF-8 (O24: bytes are the identity).
    let odd: &[u8] = b"caf\xe9/\xff\xfe";
    m.tx.put_dir(ArchivePath::from_stored(b"caf\xe9").unwrap(), a0());
    expect.push((b"caf\xe9/".to_vec(), true, Vec::new()));
    m.tx.put_file(
        ArchivePath::from_stored(odd).unwrap(),
        b"odd".to_vec(),
        a0(),
    );
    expect.push((odd.to_vec(), false, b"odd".to_vec()));
    m.end(0);
    let s = SimStorage::new();
    let mut w = create(&s, tar_options());
    commit_all(&mut w, &m.steps);
    w.close().unwrap();
    assert_eq!(read_tar(&decode(&s)), expect);
}

/// A commit with no put writes no stream, not even end blocks.
#[test]
fn c10_a_commit_without_puts_adds_no_stream() {
    let mut sc = Script::new();
    sc.file("x", b"hello");
    sc.end(0);
    sc.delete("x");
    sc.end(1);
    let s = SimStorage::new();
    let mut w = create(&s, tar_options());
    commit_all(&mut w, &sc.model.steps[..1]);
    w.close().unwrap();
    let one = decode(&s);
    let (mut w, _) = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(50)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap();
    commit_all(&mut w, &sc.model.steps[1..]);
    w.close().unwrap();
    assert_eq!(decode(&s), one);
    let head = open_head(&s, &ReadOptions::default()).unwrap();
    assert!(read_state(&s, &head).unwrap().is_empty());
}
