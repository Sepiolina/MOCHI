//! R8 recipes: how each case's bytes are made (plan W16, K21).
//!
//! Every recipe is deterministic given the library versions (fixed ID
//! source, no wall clock) except the Encrypted profile, whose nonces, salts
//! and data key come from the OS (the writer has no injection point for the
//! random stream, and adding one for tests would change production code).
//! That is why the set is **frozen when written**: a case derived from
//! another reads the other from disk (`Ctx::base`), never rebuilds it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::catalog::path::ArchivePath;
use mochi_core::commit::ObjectRef;
use mochi_core::compact::{compact, CompactOptions, Keep};
use mochi_core::descriptor::Profile;
use mochi_core::gc::plan as gc_plan;
use mochi_core::manifest::Mtime;
use mochi_core::publish::{
    commit_history, open_at_footer, ArchiveWriter, CheckpointPolicy, ReadOptions, TailPolicy,
    Transaction, WriterOptions,
};
use mochi_format::frame::Frames;
use mochi_format::kdf::KdfParams;
use mochi_format::registry::FrameKind;
use mochi_testkit::archive::{keyed_read, path, test_options, Job};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, CrashMode, Fault, Op, SeqIds, SimDir, SimStorage};

pub const PASS: &str = "r8 golden passphrase";
pub const PASS2: &str = "r8 second passphrase";
pub const WRONG: &str = "r8 wrong passphrase";

/// Argon2id as `c11` does it: 64 KiB, one pass, one lane.
pub const KDF: KdfParams = KdfParams {
    memory_kib: 64,
    iterations: 1,
    lanes: 1,
};

pub fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/r8")
}

// ---- the index ---------------------------------------------------------------------

/// One row of `cases.json` before the reference run fills `expect`.
#[derive(Debug, Clone)]
pub struct Spec {
    pub name: &'static str,
    pub profile: &'static str,
    pub class: &'static str,
    pub fault: &'static str,
    pub spec: &'static str,
    pub row: Option<&'static str>,
    pub passphrase: Option<&'static str>,
    /// The case whose head commit ID the verifier is told to expect.
    pub expected_head_of: Option<&'static str>,
    pub copies: usize,
    /// `repair plan` is run with the salvage scan.
    pub salvage: bool,
}

const fn s(
    name: &'static str,
    profile: &'static str,
    class: &'static str,
    spec: &'static str,
    fault: &'static str,
) -> Spec {
    Spec {
        name,
        profile,
        class,
        fault,
        spec,
        row: None,
        passphrase: None,
        expected_head_of: None,
        copies: 0,
        salvage: false,
    }
}

impl Spec {
    const fn row(mut self, r: &'static str) -> Self {
        self.row = Some(r);
        self
    }
    const fn pass(mut self, p: &'static str) -> Self {
        self.passphrase = Some(p);
        self
    }
    const fn head_of(mut self, n: &'static str) -> Self {
        self.expected_head_of = Some(n);
        self
    }
    const fn copies(mut self, n: usize) -> Self {
        self.copies = n;
        self
    }
    const fn salvage(mut self) -> Self {
        self.salvage = true;
        self
    }
}

// The §24.2 rows, verbatim keys (the coverage test lists them again).
pub const ROW_TERMINATION: &str = "Termination at each publication stage";
pub const ROW_TRUNCATION: &str = "Truncation at every byte of small fixtures";
pub const ROW_TORN: &str = "Torn, reordered, or lost writes";
pub const ROW_FOOTER: &str = "Corrupted latest footer";
pub const ROW_MAGIC: &str = "False magic inside payload";
pub const ROW_OVERSIZED: &str = "Oversized or malicious frame metadata";
pub const ROW_CATALOG: &str = "Destroyed SQLite catalogs";
pub const ROW_DICT: &str = "Missing shared dictionary";
pub const ROW_CONTENT: &str = "Corrupted content object";
pub const ROW_PARITY: &str = "Damage beyond parity capacity";
pub const ROW_OLDER: &str = "Older valid archive substituted";
pub const ROW_KEY: &str = "Primary key service unavailable";
pub const ROW_COMPACTION: &str = "Compaction interrupted";
pub const ROW_HOLD: &str = "Retention expires under legal hold";
pub const ROW_PATH: &str = "Path traversal or naming collision";

pub fn specs() -> Vec<Spec> {
    let mut v = vec![
        // ---- valid ----------------------------------------------------------------
        s("core-valid-small", "core", "valid", "§12, §24.2", "Two commits; small enough to truncate at every byte."),
        s("core-valid-history", "core", "valid", "§10, §11, §12", "Seven commits: mkdir, put, overwrite, delete, rename, an empty file, a multi-chunk file; checkpoints and deltas."),
        s("core-valid-hostile-names", "core", "valid", "§10.4, §23.3 #9", "Hostile names: markup, a newline, U+202E, a non-UTF-8 byte, an unpaired surrogate (WTF-8), CON, a trailing dot, a trailing space, and the case pair a/A.")
            .row(ROW_PATH),
        s("core-valid-retention", "core", "valid", "§16.3, §18.3", "A forced checkpoint between deltas, a legal hold, and expiry of four snapshots.")
            .row(ROW_HOLD),
        s("core-valid-compacted", "core", "valid", "§18.2", "A compaction output: every snapshot kept, provenance recorded."),
        s("core-valid-gc", "core", "valid", "§18.3", "A GC output: only the retained roots; the held snapshot survives its expiry."),
        s("core-valid-self-containing", "core", "valid", "§8.5, §22.1", "A file whose content is a raw copy of the archive's own earlier state."),
        s("core-valid-false-magic", "core", "valid", "§8.5, §8.4", "A file whose content embeds a real footer frame and frame headers: nothing inside a payload is a frame.")
            .row(ROW_MAGIC),
        s("tar-valid-history", "tar", "valid", "Annex B.2.9 D19", "The seven-commit history under the TAR-compatible profile; each commit's stream reassembles with tarfile."),
        s("tar-valid-hostile-names", "tar", "valid", "Annex B.2.9 D19, §10.4", "Hostile names in a TAR-compatible archive (pax records)."),
        s("enc-valid-history", "enc", "valid", "Annex B.2.10 D20", "The seven-commit history under the Encrypted profile, summarized with the passphrase.").pass(PASS),
        s("enc-valid-hostile-names", "enc", "valid", "Annex B.2.10 D20, §10.4", "Hostile names in an Encrypted archive.").pass(PASS),
        s("enc-valid-rekeyed", "enc", "valid", "Annex B.2.10 D20", "A rewrap adds a second passphrase and removes none; either opens it.").pass(PASS2),
        s("enc-valid-reencrypted", "enc", "valid", "Annex B.2.10 D20 item 10", "A re-encrypted rewrite: new data key, provenance reason re-encryption.").pass(PASS2),
        s("enc-valid-keyless", "enc", "valid", "Annex B.2.10 D20 item 9", "The Encrypted archive verified without a passphrase: stored integrity only, everything else unknown; content fields null.")
            .row(ROW_KEY),
        s("red-valid-50pct", "red", "valid", "Annex B.2.11 D21", "A 50 % recovery record (the base of the damage cases)."),
        s("red-valid-5pct", "red", "valid", "Annex B.2.11 D21", "A 5 % recovery record."),
        s("red-valid-100pct", "red", "valid", "Annex B.2.11 D21", "A 100 % recovery record."),
        s("red-valid-multigroup", "red", "valid", "Annex B.2.11 D21, R6", "About 3 MiB of incompressible data: a commit with more than one parity group."),
        s("tar-red-valid-history", "tar+red", "valid", "Annex B.2.9, B.2.11", "TAR-compatible with a 50 % recovery record."),
        s("enc-red-valid-history", "enc+red", "valid", "Annex B.2.10, B.2.11", "Encrypted with a 50 % recovery record.").pass(PASS),
        // ---- corrupt: Core ------------------------------------------------------------
        s("core-corrupt-chunk-head", "core", "corrupt", "§9, §20", "One byte flipped in a chunk only the head commit uses.").row(ROW_CONTENT),
        s("core-corrupt-chunk-older", "core", "corrupt", "§9, §20", "One byte flipped in a chunk only an older snapshot uses.").row(ROW_CONTENT),
        s("core-corrupt-all-images", "core", "corrupt", "Annex B.2 D10.9", "Every catalog image destroyed: the catalog is rebuilt from manifests.").row(ROW_CATALOG),
        s("core-corrupt-delta-manifest", "core", "corrupt", "Annex B.2 D10", "A middle commit's delta manifest damaged."),
        s("core-corrupt-latest-footer", "core", "corrupt", "§8.4, §12.2", "The head footer's digest damaged: the previous commit is the head, the tail is unresolved.").row(ROW_FOOTER),
        s("core-corrupt-descriptor", "core", "corrupt", "Annex B.2 D12", "The descriptor frame damaged.").row(ROW_CATALOG),
        s("core-corrupt-middle-commit", "core", "corrupt", "§8.4, §12", "A middle commit frame damaged: the chain cannot be walked past it."),
        s("core-corrupt-oversized-frame", "core", "corrupt", "§8.5", "A referenced manifest frame whose header declares a size beyond every limit."),
        s("core-corrupt-noncanonical-commit", "core", "corrupt", "Annex B.1 D2", "A head commit whose CBOR is not canonical, with a footer that covers it."),
        s("core-corrupt-dictionary-dependency", "core", "corrupt", "§24.2, plan K17", "A head chunk that depends on a dictionary this build cannot use.").row(ROW_DICT),
        s("core-corrupt-missing-dictionary", "core", "corrupt", "§24.2, §5.2", "A head chunk that names a dictionary the catalog does not hold.").row(ROW_DICT),
        s("core-corrupt-content-hash", "core", "corrupt", "§9", "A head chunk whose stored hash holds but whose content hash does not."),
        s("core-corrupt-traversal-path", "core", "corrupt", "§10.4, §22.1", "A head manifest whose path is a traversal (..).").row(ROW_PATH),
        s("core-corrupt-snapshot-bound-elsewhere", "core", "corrupt", "Annex B.2 D11", "A forged head whose manifest reference names another commit's manifest, correctly hashed."),
        s("core-corrupt-orphan-put", "core", "corrupt", "§10.2", "A head whose put has no parent directory in the snapshot."),
        s("core-corrupt-short-extents", "core", "corrupt", "§10.3", "A head file version that claims ten more bytes than its extents cover."),
        s("core-corrupt-undefined-version", "core", "corrupt", "§10.5, R4", "A head put that names a file version no manifest defines."),
        s("core-corrupt-reused-chunk-id", "core", "corrupt", "§10.2, Annex B D10.4", "A head chunk that reuses an existing object ID with different content."),
        s("core-corrupt-image-pages", "core", "corrupt", "§10.5, R4", "A head checkpoint whose catalog image has a zeroed page but is correctly enveloped and hashed."),
        s("core-corrupt-oversized-tail-frame", "core", "corrupt", "§8.5", "After the head, a frame header declaring a size beyond every limit and the file.").row(ROW_OVERSIZED),
        s("core-corrupt-manifest-not-a-frame", "core", "corrupt", "§8.5", "A head whose manifest reference hashes correctly but names bytes that are not a frame."),
        s("core-corrupt-manifest-cut-short", "core", "corrupt", "§8.5", "A head whose manifest reference names a skippable frame cut short."),
        s("core-corrupt-older-archive", "core", "corrupt", "§5.7", "An older valid state of the archive substituted for the current one.").row(ROW_OLDER).head_of("core-valid-history"),
        s("core-corrupt-truncated-mid-history", "core", "corrupt", "§12.2", "Cut inside an older commit: the later commits are gone, the earlier ones still open.").row(ROW_TRUNCATION),
        // ---- corrupt: other profiles ----------------------------------------------------
        s("tar-corrupt-chunk-head", "tar", "corrupt", "§9, D19", "A head chunk flipped in a TAR-compatible archive.").row(ROW_CONTENT),
        s("tar-corrupt-stream", "tar", "corrupt", "Annex B.2.9 D19 rule 8", "A stream-only chunk altered and re-hashed: every hash holds, the stream no longer matches the commit."),
        s("enc-corrupt-chunk-head", "enc", "corrupt", "Annex B.2.10 D20", "A ciphertext byte flipped in a head chunk.").pass(PASS).row(ROW_CONTENT),
        s("enc-corrupt-wrong-passphrase", "enc", "corrupt", "Annex B.2.10 D20 item 9", "A wrong passphrase: KEY_UNAVAILABLE, never a verification result.").pass(WRONG).row(ROW_KEY),
        s("enc-corrupt-aead-tag", "enc", "corrupt", "Annex B.2.10 D20", "A head chunk sealed under another key: every hash holds, the AEAD tag does not.").pass(PASS),
        s("enc-corrupt-envelopes-unordered", "enc", "corrupt", "Annex B.2.10 D20 item 10, R3 §7.1", "A head commit listing its key envelopes in decreasing envelope-ID order.").pass(PASS),
        s("enc-corrupt-envelope-duplicate", "enc", "corrupt", "Annex B.2.10 D20 item 10, R3 §7.1", "A head commit listing the same key envelope twice.").pass(PASS),
        s("enc-corrupt-data-region-kind", "enc", "corrupt", "Annex B.2.10 D20 item 7, R3 §7.1", "A head commit whose data region is a sealed frame that is not a data object.").pass(PASS),
        s("red-corrupt-within-capacity", "red", "corrupt", "Annex B.2.11 D21", "Damage within the recovery record: rebuilt in memory, bit-identically."),
        s("red-corrupt-beyond-capacity", "red", "corrupt", "Annex B.2.11 D21", "One damaged shard more than the group's parity: reported, never rebuilt.").row(ROW_PARITY),
        s("red-corrupt-parity-frame", "red", "corrupt", "Annex B.2.11 D21", "A parity frame damaged: integrity unaffected, recoverability degraded."),
        s("red-corrupt-forged-parity", "red", "corrupt", "Annex B.2.11 D21", "A parity frame that verifies but disagrees with its commit."),
        s("tar-red-corrupt-within-capacity", "tar+red", "corrupt", "B.2.9, B.2.11", "Damage within capacity in a TAR-compatible archive."),
        s("enc-red-corrupt-beyond-capacity", "enc+red", "corrupt", "B.2.10, B.2.11", "Damage beyond capacity in an Encrypted archive.").pass(PASS).row(ROW_PARITY),
        // ---- interrupted -----------------------------------------------------------------
        s("core-interrupted-after-data", "core", "interrupted", "§12.2 steps 1-2", "Halted after the data objects: no manifest, no commit.").row(ROW_TERMINATION),
        s("core-interrupted-after-commit-frame", "core", "interrupted", "§12.2 steps 4-5", "Halted after the commit frame, before its footer.").row(ROW_TERMINATION),
        s("core-interrupted-torn-footer", "core", "interrupted", "§12.2 step 5", "A footer torn mid-write.").row(ROW_TORN),
        s("core-interrupted-lost-write", "core", "interrupted", "§12.2 step 3", "The second half of a referenced data object reads back as zeros under a complete footer.").row(ROW_TORN),
        s("tar-interrupted-after-commit-frame", "tar", "interrupted", "§12.2", "Halted after the commit frame of a TAR-compatible archive.").row(ROW_TERMINATION),
        s("enc-interrupted-after-data", "enc", "interrupted", "§12.2, D20", "Halted after the sealed data objects.").pass(PASS).row(ROW_TERMINATION),
        s("red-interrupted-after-parity", "red", "interrupted", "§12.2, D21", "Halted after the parity frames, before the footer.").row(ROW_TERMINATION),
        s("tar-red-interrupted-after-parity", "tar+red", "interrupted", "§12.2, D19, D21", "Halted after the parity frames of a TAR-compatible archive.").row(ROW_TERMINATION),
        s("enc-red-interrupted-after-commit-frame", "enc+red", "interrupted", "§12.2, D20, D21", "Halted after the commit frame of an Encrypted archive with a recovery record.").pass(PASS).row(ROW_TERMINATION),
        s("core-interrupted-compaction", "core", "interrupted", "§18.2", "A compaction output cut off before its last footer; the source is untouched.").row(ROW_COMPACTION),
        // ---- copy / salvage / legacy -----------------------------------------------------
        s("copy-damaged-chunk", "core", "copy", "Annex B.2.12 D22", "A damaged chunk with an intact copy: the copy donates it.").copies(1),
        s("copy-head-destroyed", "core", "copy", "Annex B.2.12 D22", "The head's commit frame and footer destroyed; the copy confirms the commit.").copies(1),
        s("copy-diverged", "core", "copy", "Annex B.2.12 D22", "A copy that diverged after commit 2: donations only below the divergence.").copies(1),
        s("salvage-destroyed-footers", "core", "salvage", "Annex B.2.13 D23", "Every footer destroyed: the scan confirms commits by parent links and the repair is partial.").salvage(),
        s("salvage-destroyed-descriptor", "core", "salvage", "Annex B.2.13 D23", "The descriptor destroyed: re-encoded from the hash the commits record.").salvage(),
        s("salvage-archive-inside", "core", "salvage", "Annex B.2.13 D23", "A byte copy of an earlier state stored after a damaged head: the archive inside the archive is not scanned as the archive.").salvage(),
        // ---- legacy ----------------------------------------------------------------------
        s("legacy-draft-1", "core", "legacy", "Annex B.2 D24, plan W15", "A draft-1 archive (pre-freeze): refused, UNSUPPORTED_FEATURE."),
    ];
    // Stable order: valid, corrupt, interrupted, copy, salvage, legacy as listed.
    v.shrink_to_fit();
    v
}

// ---- profiles ----------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prof {
    pub tar: bool,
    pub enc: bool,
    pub red: Option<u16>,
}

pub const CORE: Prof = Prof {
    tar: false,
    enc: false,
    red: None,
};
pub const TAR: Prof = Prof { tar: true, ..CORE };
pub const ENC: Prof = Prof { enc: true, ..CORE };
pub const fn red(p: u16) -> Prof {
    Prof {
        red: Some(p),
        ..CORE
    }
}
pub const TAR_RED: Prof = Prof {
    tar: true,
    red: Some(50),
    enc: false,
};
pub const ENC_RED: Prof = Prof {
    enc: true,
    red: Some(50),
    tar: false,
};

pub fn writer_options(p: Prof, chunk: Option<u64>) -> WriterOptions {
    let mut o = if p.enc {
        WriterOptions {
            read: keyed_read(&[PASS]),
            kdf: Some(KDF),
            ..test_options()
        }
    } else {
        test_options()
    };
    o.profile = Some(Profile {
        tar_compatible: p.tar,
        encrypted: p.enc,
        redundancy: p.red,
    });
    if let Some(c) = chunk {
        o.chunk_size = Some(c);
    }
    o
}

/// Options for opening an existing archive for appending (the profile is the
/// archive's own).
pub fn append_options(enc: bool) -> WriterOptions {
    let mut o = test_options();
    if enc {
        o.read = keyed_read(&[PASS]);
        o.kdf = Some(KDF);
    }
    o
}

pub fn read_for(enc: bool) -> ReadOptions {
    if enc {
        keyed_read(&[PASS])
    } else {
        ReadOptions::default()
    }
}

// ---- scripts -----------------------------------------------------------------------

fn p(b: &[u8]) -> ArchivePath {
    ArchivePath::from_stored(b).expect("valid path")
}

fn put(tx: &mut Transaction, name: &str, seed: u64, len: usize, mode: u32) {
    tx.put_file(
        path(name),
        deterministic_bytes(seed, len),
        attrs(mode, seed as i64),
    );
}

fn timed(mut tx: Transaction, t: i64) -> Transaction {
    tx.at(Mtime {
        secs: 1_700_000_000 + t,
        nanos: 0,
    });
    tx
}

/// Seven commits: mkdir, put, overwrite, delete, rename, an empty file, files
/// spanning several 64-byte chunks.
pub fn history() -> Vec<Transaction> {
    let mut out = Vec::new();
    let mut t = Transaction::new();
    t.put_dir(path("d"), attrs(0o750, 0));
    put(&mut t, "d/a", 1, 200, 0o640);
    put(&mut t, "b", 2, 70, 0o600);
    t.put_file(path("c"), Vec::new(), attrs(0o444, 3));
    t.put_dir(path("e"), attrs(0o755, 4));
    out.push(timed(t, 0));
    let mut t = Transaction::new();
    put(&mut t, "d/a", 3, 130, 0o641);
    put(&mut t, "f", 4, 300, 0o644);
    out.push(timed(t, 10));
    let mut t = Transaction::new();
    t.rename(path("b"), path("d/b"));
    t.delete(path("c"));
    out.push(timed(t, 20));
    let mut t = Transaction::new();
    put(&mut t, "d/g", 5, 90, 0o606);
    out.push(timed(t, 30));
    let mut t = Transaction::new();
    t.delete(path("f"));
    put(&mut t, "e/n", 6, 150, 0o600);
    out.push(timed(t, 40));
    let mut t = Transaction::new();
    t.rename(path("d/a"), path("e/a"));
    out.push(timed(t, 50));
    let mut t = Transaction::new();
    put(&mut t, "h", 7, 65, 0o655);
    out.push(timed(t, 60));
    out
}

/// [`history`] with 4,000 more incompressible bytes in the first commit. The
/// catalog image is compressed (B.2.2 encoding 2), so the first commit of
/// `history()` fits one parity shard; the damage cases of the Redundancy
/// profile need a commit with several (more shards than its parity can cover).
pub fn history_big() -> Vec<Transaction> {
    let mut out = history();
    put(&mut out[0], "pad", 8, 4000, 0o644);
    out
}

pub const HOSTILE: [&[u8]; 10] = [
    b"<img src=x onerror=alert(1)>",
    b"line\nbreak",
    "\u{202e}gnp.exe".as_bytes(),
    &[0xff, 0xfe, b'x'],
    &[b'w', 0xed, 0xa0, 0x80],
    b"CON",
    b"trailing.",
    b"trailing ",
    b"a",
    b"A",
];

pub fn hostile_history() -> Vec<Transaction> {
    let mut t = Transaction::new();
    for (i, n) in HOSTILE.iter().enumerate() {
        t.put_file(
            p(n),
            deterministic_bytes(100 + i as u64, 40 + i * 30),
            attrs(0o644, i as i64),
        );
    }
    let mut u = Transaction::new();
    u.put_dir(path("dir"), attrs(0o755, 20));
    let mut nested = b"dir/".to_vec();
    nested.extend_from_slice(HOSTILE[0]);
    u.put_file(p(&nested), b"nested hostile".to_vec(), attrs(0o600, 21));
    vec![timed(t, 0), timed(u, 10)]
}

pub fn small_history() -> Vec<Transaction> {
    let mut t = Transaction::new();
    put(&mut t, "a", 1, 40, 0o644);
    let mut u = Transaction::new();
    put(&mut t.clone(), "a", 1, 40, 0o644);
    put(&mut u, "b", 2, 20, 0o644);
    vec![timed(t, 0), timed(u, 1)]
}

// ---- writing -----------------------------------------------------------------------

pub struct Plan {
    pub prof: Prof,
    pub chunk: Option<u64>,
    pub policy: CheckpointPolicy,
    pub seed: u64,
}

pub fn write_with(
    plan: &Plan,
    txs: &[Transaction],
    mut between: impl FnMut(usize, &mut ArchiveWriter<SimStorage>),
) -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(plan.seed)),
        writer_options(plan.prof, plan.chunk),
    )
    .unwrap();
    w.set_checkpoint_policy(plan.policy).unwrap();
    let job = Job::new();
    for (i, tx) in txs.iter().enumerate() {
        between(i, &mut w);
        w.commit(tx.clone(), &job.ctx()).unwrap();
    }
    w.close().unwrap();
    s
}

pub fn write(prof: Prof, seed: u64, txs: &[Transaction]) -> SimStorage {
    write_with(
        &Plan {
            prof,
            chunk: None,
            policy: CheckpointPolicy::Never,
            seed,
        },
        txs,
        |_, _| {},
    )
}

/// Append one commit to `bytes` (the archive's own profile).
pub fn append_one(bytes: &[u8], enc: bool, seed: u64, tx: Transaction) -> SimStorage {
    let s = SimStorage::from_bytes(bytes.to_vec());
    let (mut w, _) = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(seed)),
        append_options(enc),
        TailPolicy::Refuse,
    )
    .unwrap();
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    s
}

/// The retention script of `c9_compact`, with a forced checkpoint between
/// deltas: nine commits; snapshots 1–4 expired, 2 also held.
pub fn retention_source() -> SimStorage {
    let s = SimStorage::new();
    let mut w =
        ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(81)), test_options()).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    let mut n = 0i64;
    let mut c = |w: &mut ArchiveWriter<SimStorage>, f: &dyn Fn(&mut Transaction)| {
        let mut tx = Transaction::new();
        f(&mut tx);
        n += 1;
        tx.at(Mtime {
            secs: 1_700_000_000 + n,
            nanos: 7,
        });
        w.commit(tx, &job.ctx()).unwrap();
    };
    let f150 =
        |t: &mut Transaction, name: &str, seed: u64, mode: u32| put(t, name, seed, 150, mode);
    c(&mut w, &|t| {
        t.put_dir(path("d"), attrs(0o755, 1));
        f150(t, "d/a", 1, 0o644);
        f150(t, "b", 2, 0o600);
    }); // 0
    c(&mut w, &|t| f150(t, "d/a", 3, 0o640)); // 1
    c(&mut w, &|t| {
        t.rename(path("b"), path("d/b"));
    }); // 2
    w.request_checkpoint();
    c(&mut w, &|t| f150(t, "c", 4, 0o644)); // 3: checkpoint
    c(&mut w, &|t| {
        t.delete(path("c"));
    }); // 4
    c(&mut w, &|t| f150(t, "e", 5, 0o644)); // 5
    c(&mut w, &|t| f150(t, "e", 6, 0o644)); // 6
    c(&mut w, &|t| {
        t.expire(1).expire(2).expire(3).expire(4).hold(b"audit", 2);
    }); // 7
    c(&mut w, &|t| f150(t, "g", 7, 0o644)); // 8
    w.close().unwrap();
    s
}

// ---- reading an archive's layout -----------------------------------------------------

pub struct Spans(pub Vec<(u64, u64, FrameKind)>);

pub fn spans(bytes: &[u8]) -> Spans {
    let s = SimStorage::from_bytes(bytes.to_vec());
    let r = mochi_core::storage::StorageReader::new(&s).unwrap();
    Spans(
        Frames::new(&r, 0, mochi_format::Limits::default())
            .map_while(|f| f.ok())
            .map(|f| (f.offset, f.len, f.kind))
            .collect(),
    )
}

pub fn history_of(bytes: &[u8], read: &ReadOptions) -> Vec<mochi_core::publish::HistoryEntry> {
    commit_history(&SimStorage::from_bytes(bytes.to_vec()), read).unwrap()
}

/// Where the first chunk of `name` in commit `seq` lies: (offset, stored length).
pub fn chunk_range(bytes: &[u8], read: &ReadOptions, seq: u64, name: &str) -> (u64, u64) {
    let s = SimStorage::from_bytes(bytes.to_vec());
    let h = history_of(bytes, read);
    let o = open_at_footer(&s, h[seq as usize].footer_offset, read).unwrap();
    let snap = o.catalog.replay(None).unwrap();
    let entry = snap.get(&path(name)).unwrap();
    let (_, extents) = o.catalog.file_version(&entry.version).unwrap().unwrap();
    let chunk = extents
        .iter()
        .find_map(|e| match e.source {
            ExtentSource::Chunk { chunk, .. } => Some(chunk),
            ExtentSource::Hole => None,
        })
        .unwrap();
    let r = o.catalog.object(&chunk).unwrap().unwrap();
    (
        o.catalog.object_location(&chunk).unwrap().unwrap(),
        r.stored_len,
    )
}

pub fn flip(b: &mut [u8], at: u64) {
    b[at as usize] ^= 0x40;
}

pub fn damage_ref(b: &mut [u8], r: ObjectRef) {
    flip(b, r.offset + r.stored_len / 2);
}

// ---- interrupted publication -----------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum Stage {
    /// After the data objects, before the delta manifest.
    AfterData,
    /// After the commit frame, before whatever follows it.
    AfterCommitFrame,
    /// After the parity frames, before the footer.
    AfterParity,
    /// Inside the footer.
    TornFooter,
}

fn is_mutation(op: &Op) -> bool {
    matches!(
        op,
        Op::Append { .. } | Op::SyncData | Op::SyncDirectory | Op::Truncate { .. }
    )
}

/// Append `tx` to `base` and cut the write off at `stage`. The result is
/// what a fresh process finds after the kill (`KeepAll`).
pub fn interrupt(base: &[u8], enc: bool, tx: &Transaction, stage: Stage) -> Vec<u8> {
    let mk = |s: &SimStorage| {
        let (mut w, _) = ArchiveWriter::open_append(
            s.clone(),
            Box::new(SeqIds::new(990)),
            append_options(enc),
            TailPolicy::Refuse,
        )?;
        w.commit(tx.clone(), &Job::new().ctx())
    };
    let probe = SimStorage::from_bytes(base.to_vec());
    mk(&probe).unwrap();
    let full = probe.contents();
    let sp = spans(&full);
    let in_new: Vec<_> = sp.0.iter().filter(|f| f.0 >= base.len() as u64).collect();
    let footer = in_new
        .iter()
        .find(|f| f.2 == FrameKind::CommitFooter)
        .unwrap();
    let commit = in_new
        .iter()
        .find(|f| f.2 == FrameKind::CommitRecord)
        .unwrap();
    let delta_manifest = {
        let h = history_of(&full, &read_for(enc));
        h.last().unwrap().commit.delta_manifest.offset
    };
    let cut = match stage {
        Stage::AfterData => delta_manifest,
        Stage::AfterCommitFrame => commit.0 + commit.1,
        Stage::AfterParity => footer.0,
        Stage::TornFooter => footer.0 + 30,
    };
    // Which append covers `cut`?
    let trace = probe.trace();
    let mut mutation = 0usize;
    let mut append = 0usize;
    let mut fault = None;
    for op in &trace {
        if let Op::Append { offset, len } = *op {
            if cut >= offset && cut < offset + len {
                fault = Some(if cut == offset {
                    Fault::HaltBeforeMutation {
                        mutation_index: mutation,
                    }
                } else {
                    Fault::TearAppend {
                        append_index: append,
                        keep: (cut - offset) as usize,
                    }
                });
                break;
            }
            append += 1;
        }
        if is_mutation(op) {
            mutation += 1;
        }
    }
    let s = SimStorage::from_bytes(base.to_vec());
    s.add_fault(fault.expect("the cut lies in an append"));
    mk(&s).unwrap_err();
    let img = s.crash_image(CrashMode::KeepAll).contents();
    assert!(img.len() as u64 >= cut.min(full.len() as u64) || matches!(stage, Stage::AfterData));
    img
}

// ---- the recipe table --------------------------------------------------------------------

pub struct Built {
    pub main: Vec<u8>,
    pub copies: Vec<Vec<u8>>,
}

fn one(main: Vec<u8>) -> Built {
    Built {
        main,
        copies: Vec::new(),
    }
}

/// Frozen bases: the bytes of an earlier case, from disk if written.
pub struct Ctx {
    cache: BTreeMap<String, Vec<u8>>,
}

impl Ctx {
    pub fn new() -> Self {
        Ctx {
            cache: BTreeMap::new(),
        }
    }

    pub fn base(&mut self, name: &str) -> Vec<u8> {
        if let Some(b) = self.cache.get(name) {
            return b.clone();
        }
        let b = match std::fs::read(dir().join(format!("{name}.mochi"))) {
            Ok(b) => b,
            Err(_) => build(self, name).main,
        };
        self.cache.insert(name.to_owned(), b.clone());
        b
    }
}

pub fn build(cx: &mut Ctx, name: &str) -> Built {
    match name {
        // ---- valid ----
        "core-valid-small" => one(write(CORE, 11, &small_history()).contents()),
        "core-valid-history" => one(write(CORE, 12, &history()).contents()),
        "core-valid-hostile-names" => one(write(CORE, 13, &hostile_history()).contents()),
        "core-valid-retention" => one(retention_source().contents()),
        "core-valid-compacted" => {
            let src = SimStorage::from_bytes(cx.base("core-valid-history"));
            let (mut w, _) = ArchiveWriter::open_append(
                src.clone(),
                Box::new(SeqIds::new(14)),
                test_options(),
                TailPolicy::Refuse,
            )
            .unwrap();
            let _ = &mut w;
            let mut d = SimDir::new();
            compact(
                &w,
                &mut d,
                "out.mochi",
                Box::new(SeqIds::new(15)),
                &Keep::Every,
                &CompactOptions::default(),
                &Job::new().ctx(),
            )
            .unwrap();
            one(d.file("out.mochi").unwrap().contents())
        }
        "core-valid-gc" => {
            let src = SimStorage::from_bytes(cx.base("core-valid-retention"));
            let plan = gc_plan(&src, &ReadOptions::default(), &Job::new().ctx()).unwrap();
            let (w, _) = ArchiveWriter::open_append(
                src.clone(),
                Box::new(SeqIds::new(16)),
                test_options(),
                TailPolicy::Refuse,
            )
            .unwrap();
            let mut d = SimDir::new();
            compact(
                &w,
                &mut d,
                "out.mochi",
                Box::new(SeqIds::new(17)),
                &Keep::Roots(plan.head.clone()),
                &CompactOptions::default(),
                &Job::new().ctx(),
            )
            .unwrap();
            one(d.file("out.mochi").unwrap().contents())
        }
        "core-valid-self-containing" => {
            let inner = cx.base("core-valid-small");
            let mut t = Transaction::new();
            t.put_dir(path("self"), attrs(0o755, 0));
            t.put_file(path("self/copy.mochi"), inner, attrs(0o644, 1));
            let plan = Plan {
                prof: CORE,
                chunk: Some(16 * 1024),
                policy: CheckpointPolicy::Never,
                seed: 18,
            };
            one(write_with(&plan, &[timed(t, 0)], |_, _| {}).contents())
        }
        "core-valid-false-magic" => {
            // A real footer frame (from the small archive) and a skippable header
            // with a plausible size, inside an uncompressible payload.
            let small = cx.base("core-valid-small");
            let h = history_of(&small, &ReadOptions::default());
            let f = &small[h[0].footer_offset as usize..][..72];
            let mut payload = Vec::new();
            payload.extend_from_slice(f);
            payload.extend_from_slice(&mochi_format::registry::FOOTER_HISTORY.to_le_bytes());
            payload.extend_from_slice(&64u32.to_le_bytes());
            payload.extend_from_slice(f);
            let mut t = Transaction::new();
            t.put_file(path("payload.bin"), payload, attrs(0o644, 1));
            let mut a = small.clone();
            let _ = &mut a;
            // Appended to the small archive's history by a writer, then the head
            // footer is damaged so the forward scan runs over the payload.
            let s = append_one(&small, false, 19, timed(t, 5));
            one(s.contents())
        }
        "tar-valid-history" => one(write(TAR, 21, &history()).contents()),
        "tar-valid-hostile-names" => one(write(TAR, 22, &hostile_history()).contents()),
        "enc-valid-history" => one(write(ENC, 23, &history()).contents()),
        "enc-valid-hostile-names" => one(write(ENC, 24, &hostile_history()).contents()),
        "enc-valid-rekeyed" | "enc-valid-reencrypted" | "enc-valid-keyless" => {
            enc_variants(cx, name)
        }
        "red-valid-5pct" => one(write(red(5), 31, &history()).contents()),
        "red-valid-100pct" => one(write(red(100), 32, &history()).contents()),
        "red-valid-multigroup" => {
            let mut t = Transaction::new();
            t.put_file(
                path("big.bin"),
                deterministic_bytes(33, 3 * 1024 * 1024),
                attrs(0o644, 1),
            );
            let plan = Plan {
                prof: red(10),
                chunk: Some(256 * 1024),
                policy: CheckpointPolicy::Every(3),
                seed: 33,
            };
            let s = write_with(&plan, &[timed(t, 0)], |_, _| {});
            one(s.contents())
        }
        "tar-red-valid-history" => one(write(TAR_RED, 34, &history_big()).contents()),
        "enc-red-valid-history" => one(write(ENC_RED, 35, &history_big()).contents()),
        _ => crate::recipes2::build(cx, name),
    }
}

fn enc_variants(cx: &mut Ctx, name: &str) -> Built {
    use mochi_core::rekey::{reencrypt, rewrap};
    use mochi_format::secret::Passphrase;
    match name {
        "enc-valid-keyless" => one(cx.base("enc-valid-history")),
        "enc-valid-rekeyed" => {
            let src = SimStorage::from_bytes(cx.base("enc-valid-history"));
            let (mut w, _) = ArchiveWriter::open_append(
                src.clone(),
                Box::new(SeqIds::new(25)),
                append_options(true),
                TailPolicy::Refuse,
            )
            .unwrap();
            rewrap(
                &mut w,
                vec![Passphrase::new(PASS2).unwrap()],
                vec![],
                &Job::new().ctx(),
            )
            .unwrap();
            w.close().unwrap();
            one(src.contents())
        }
        _ => {
            let src = SimStorage::from_bytes(cx.base("enc-valid-history"));
            let (w, _) = ArchiveWriter::open_append(
                src.clone(),
                Box::new(SeqIds::new(26)),
                append_options(true),
                TailPolicy::Refuse,
            )
            .unwrap();
            let mut d = SimDir::new();
            reencrypt(
                &w,
                &mut d,
                "out.mochi",
                Box::new(SeqIds::new(27)),
                keyed_read(&[PASS]),
                vec![Passphrase::new(PASS2).unwrap()],
                Some(KDF),
                &Job::new().ctx(),
            )
            .unwrap();
            one(d.file("out.mochi").unwrap().contents())
        }
    }
}
