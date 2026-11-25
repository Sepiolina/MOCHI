//! This client's local evidence: the last-seen head of each archive, by
//! archive ID (spec Annex B.1 D8, plan O8: "the client keeps the last-seen
//! head per archive ID locally (opt-out available)"), and the evidence log
//! that [`crate::health`] reads.
//!
//! Shared by the command-line tool and the desktop app (owner decision
//! 2026-11-23, spec Annex B.2.22), so a head either one recorded anchors the
//! other's freshness check. It lives beside the OS storage because it is
//! plain file I/O outside every archive; verification stays read-only.
//!
//! The file is `heads.json` in the state directory: an explicit directory
//! (`--state-dir`), else `$MOCHI_STATE_DIR`, else `$XDG_STATE_HOME/mochi` or
//! `~/.local/state/mochi` on Linux and `%LOCALAPPDATA%\mochi` on Windows.
//!
//! ```json
//! {"version": 1, "heads": {"<archive id hex>": {"seq": 4, "commit_id": "<hex>"}}}
//! ```
//!
//! Rules:
//! * A head is recorded after this client commits it, or after a
//!   verification found nothing failing. The recorded sequence never
//!   decreases, so a rolled-back copy cannot lower the anchor.
//! * **Updates are serialized** by an exclusive lock on `heads.json.lock`
//!   beside it (plan K5's remedy): read, change, and replace happen under the
//!   lock, so two clients recording at once both land. The lock is waited
//!   for at most [`LOCK_WAIT`]; a client that cannot get it records nothing
//!   and says so (the anchor is then older, never wrong).
//! * Writes go to a temporary file in the same directory, then replace
//!   `heads.json` by rename; readers take no lock and see one version or the
//!   other.
//!
//! Beside it, `evidence/<archive id hex>.jsonl` is the **evidence log**: one
//! JSON object per completed `verify`, `fsck`, `restore-test`, `repair apply`,
//! or commit (create, append), appended and never rewritten. A torn or
//! damaged line is skipped and counted, never trusted; the next append starts
//! on a fresh line so one torn write cannot damage two records.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mochi_format::digest::CommitId;
use serde_json::{json, Value};

use crate::error::{ErrorCode, MochiError, Result};
use crate::health::EvidenceRecord;
use crate::object::ArchiveId;

pub const FILE_NAME: &str = "heads.json";
/// The lock file that serializes updates of [`FILE_NAME`].
pub const LOCK_FILE_NAME: &str = "heads.json.lock";
/// The directory of evidence logs, inside the state directory.
pub const EVIDENCE_DIR: &str = "evidence";
/// How long a recording client waits for another to finish.
pub const LOCK_WAIT: Duration = Duration::from_secs(5);
const VERSION: u64 = 1;

/// The local head history and evidence logs in one state directory.
#[derive(Debug, Clone)]
pub struct HeadStore {
    dir: PathBuf,
}

/// What an evidence log held: the records that could be read, and how many
/// lines could not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvidenceRead {
    pub records: Vec<EvidenceRecord>,
    pub unreadable: u64,
}

/// One recorded head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeenHead {
    pub seq: u64,
    pub commit_id: CommitId,
}

fn err(path: &Path, what: impl std::fmt::Display) -> MochiError {
    MochiError::new(
        ErrorCode::IoError,
        format!("local head history {}: {what}", path.display()),
    )
}

fn default_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("MOCHI_STATE_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA")
            .filter(|d| !d.is_empty())
            .map(|d| PathBuf::from(d).join("mochi"));
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("mochi"));
    }
    std::env::var_os("HOME")
        .filter(|d| !d.is_empty())
        .map(|h| PathBuf::from(h).join(".local").join("state").join("mochi"))
}

/// The update lock, held until dropped.
struct UpdateLock(File);

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl HeadStore {
    /// The store in `explicit`, or the default location; `None` if no
    /// location can be determined.
    pub fn locate(explicit: Option<&Path>) -> Option<HeadStore> {
        explicit
            .map(Path::to_path_buf)
            .or_else(default_dir)
            .map(|dir| HeadStore { dir })
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    fn lock_update(&self, wait: Duration) -> Result<UpdateLock> {
        let path = self.dir.join(LOCK_FILE_NAME);
        std::fs::create_dir_all(&self.dir).map_err(|e| err(&path, e))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| err(&path, e))?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(UpdateLock(file)),
                Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < wait => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(err(
                        &path,
                        format!(
                            "another MOCHI program kept it locked for {} s, so nothing was \
                             recorded",
                            wait.as_secs()
                        ),
                    ))
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(err(&path, e)),
            }
        }
    }

    fn load(&self) -> Result<serde_json::Map<String, Value>> {
        let path = self.path();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(serde_json::Map::new()),
            Err(e) => return Err(err(&path, e)),
        };
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| err(&path, e))?;
        if v["version"] != json!(VERSION) {
            return Err(err(&path, "unknown version"));
        }
        match v.get("heads") {
            Some(Value::Object(m)) => Ok(m.clone()),
            _ => Err(err(&path, "no \"heads\" object")),
        }
    }

    /// The head last seen for `archive`.
    pub fn get(&self, archive: &ArchiveId) -> Result<Option<SeenHead>> {
        let heads = self.load()?;
        let Some(e) = heads.get(&archive.to_hex()) else {
            return Ok(None);
        };
        let path = self.path();
        let seq = e["seq"]
            .as_u64()
            .ok_or_else(|| err(&path, "a head has no sequence"))?;
        let id = e["commit_id"]
            .as_str()
            .ok_or_else(|| err(&path, "a head has no commit ID"))?;
        let commit_id = crate::verify::parse_commit_id(id).map_err(|e| err(&path, e.message))?;
        Ok(Some(SeenHead { seq, commit_id }))
    }

    /// Where `archive`'s evidence log is (it may not exist yet).
    pub fn evidence_path(&self, archive: &ArchiveId) -> PathBuf {
        self.dir
            .join(EVIDENCE_DIR)
            .join(format!("{}.jsonl", archive.to_hex()))
    }

    /// Append one record to `archive`'s evidence log.
    pub fn append_evidence(&self, archive: &ArchiveId, record: &EvidenceRecord) -> Result<()> {
        let path = self.evidence_path(archive);
        let mut line = serde_json::to_vec(record).map_err(|e| err(&path, e))?;
        line.push(b'\n');
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| err(&path, e))?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .map_err(|e| err(&path, e))?;
        // An earlier write that was cut short leaves no final newline; start
        // on a fresh line, so that one torn record never swallows the next.
        let len = f.metadata().map_err(|e| err(&path, e))?.len();
        if len > 0 {
            let mut last = [0u8; 1];
            f.seek(SeekFrom::End(-1)).map_err(|e| err(&path, e))?;
            f.read_exact(&mut last).map_err(|e| err(&path, e))?;
            if last[0] != b'\n' {
                line.insert(0, b'\n');
            }
        }
        // One write, in append mode: the line lands whole at the end.
        f.write_all(&line).map_err(|e| err(&path, e))
    }

    /// Every readable record of `archive`'s evidence log. A missing log is
    /// empty. A line that is not a valid record (damaged, torn, from another
    /// schema) is counted in `unreadable` and otherwise ignored.
    pub fn read_evidence(&self, archive: &ArchiveId) -> Result<EvidenceRead> {
        let path = self.evidence_path(archive);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(EvidenceRead::default())
            }
            Err(e) => return Err(err(&path, e)),
        };
        let mut out = EvidenceRead::default();
        for line in bytes.split(|b| *b == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<EvidenceRecord>(line) {
                Ok(r) if r.validate().is_ok() => out.records.push(r),
                _ => out.unreadable += 1,
            }
        }
        Ok(out)
    }

    /// Record `head` for `archive`, unless a later sequence is recorded.
    pub fn record(&self, archive: &ArchiveId, head: SeenHead) -> Result<()> {
        self.record_waiting(archive, head, LOCK_WAIT)
    }

    fn record_waiting(&self, archive: &ArchiveId, head: SeenHead, wait: Duration) -> Result<()> {
        let _lock = self.lock_update(wait)?;
        let mut heads = self.load()?;
        let key = archive.to_hex();
        if let Some(prev) = heads.get(&key).and_then(|e| e["seq"].as_u64()) {
            if prev > head.seq {
                return Ok(());
            }
        }
        heads.insert(
            key,
            json!({"seq": head.seq, "commit_id": head.commit_id.to_hex()}),
        );
        let path = self.path();
        let body = serde_json::to_vec_pretty(&json!({"version": VERSION, "heads": heads}))
            .map_err(|e| err(&path, e))?;
        // The lock excludes every other writer, in this process or another
        // (each open of the lock file is its own lock), so the name is free.
        let tmp = self
            .dir
            .join(format!("{FILE_NAME}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, body).map_err(|e| err(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            err(&path, e)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::health::{EvidenceCommand, EvidenceHead, EVIDENCE_SCHEMA};

    fn id(n: u8) -> CommitId {
        CommitId::from_bytes([n; 32])
    }

    fn seen(seq: u64) -> SeenHead {
        SeenHead {
            seq,
            commit_id: id(u8::try_from(seq % 250).unwrap_or(0)),
        }
    }

    #[test]
    fn heads_round_trip_and_never_move_back() {
        let dir = tempfile::tempdir().unwrap();
        let s = HeadStore::locate(Some(&dir.path().join("nested"))).unwrap();
        let a = ArchiveId::from_bytes([1; 32]);
        assert_eq!(s.get(&a).unwrap(), None);
        s.record(&a, seen(3)).unwrap();
        s.record(&a, seen(1)).unwrap();
        assert_eq!(s.get(&a).unwrap(), Some(seen(3)));
        s.record(&a, seen(4)).unwrap();
        assert_eq!(s.get(&a).unwrap().unwrap().seq, 4);
        assert_eq!(s.get(&ArchiveId::from_bytes([2; 32])).unwrap(), None);
    }

    #[test]
    fn a_damaged_history_file_is_an_error_not_an_empty_history() {
        let dir = tempfile::tempdir().unwrap();
        let s = HeadStore::locate(Some(dir.path())).unwrap();
        std::fs::write(s.path(), b"{not json").unwrap();
        let e = s.get(&ArchiveId::from_bytes([1; 32])).unwrap_err();
        assert_eq!(e.code, ErrorCode::IoError);
    }

    /// K5: two clients recording different archives at once both land
    /// (before the lock, the last rename won and one head was lost).
    #[test]
    fn concurrent_recorders_lose_no_update() {
        let dir = tempfile::tempdir().unwrap();
        let s = HeadStore::locate(Some(dir.path())).unwrap();
        let threads: Vec<_> = (0u8..8)
            .map(|n| {
                let s = s.clone();
                std::thread::spawn(move || {
                    for seq in 0..20 {
                        s.record(&ArchiveId::from_bytes([n; 32]), seen(seq))
                            .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        for n in 0u8..8 {
            assert_eq!(
                s.get(&ArchiveId::from_bytes([n; 32])).unwrap(),
                Some(seen(19)),
                "archive {n}"
            );
        }
    }

    #[test]
    fn a_lock_held_too_long_records_nothing_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let s = HeadStore::locate(Some(dir.path())).unwrap();
        let a = ArchiveId::from_bytes([1; 32]);
        s.record(&a, seen(1)).unwrap();
        let held = s.lock_update(LOCK_WAIT).unwrap();
        let e = s
            .record_waiting(&a, seen(2), Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::IoError);
        assert!(e.message.contains("locked"), "{}", e.message);
        drop(held);
        assert_eq!(s.get(&a).unwrap(), Some(seen(1)));
        s.record(&a, seen(2)).unwrap();
        assert_eq!(s.get(&a).unwrap(), Some(seen(2)));
    }

    fn record(seq: u64) -> EvidenceRecord {
        EvidenceRecord {
            schema: EVIDENCE_SCHEMA,
            command: EvidenceCommand::Verify,
            level: None,
            completed_at: "2026-10-08T00:00:00.000000000Z".into(),
            head: EvidenceHead {
                seq,
                commit_id: "ab".repeat(32),
            },
            dimensions: Default::default(),
            exit_code: 0,
            scope: None,
            finding_codes: Vec::new(),
        }
    }

    #[test]
    fn evidence_is_appended_per_archive_and_read_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let s = HeadStore::locate(Some(dir.path())).unwrap();
        let (a, b) = (
            ArchiveId::from_bytes([1; 32]),
            ArchiveId::from_bytes([2; 32]),
        );
        assert_eq!(s.read_evidence(&a).unwrap(), EvidenceRead::default());
        for seq in 0..3 {
            s.append_evidence(&a, &record(seq)).unwrap();
        }
        s.append_evidence(&b, &record(9)).unwrap();
        let got = s.read_evidence(&a).unwrap();
        assert_eq!(got.unreadable, 0);
        assert_eq!(
            got.records.iter().map(|r| r.head.seq).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(s.read_evidence(&b).unwrap().records.len(), 1);
        // One JSON object per line, in a file named by the archive ID.
        let text = std::fs::read_to_string(s.evidence_path(&a)).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(s
            .evidence_path(&a)
            .ends_with(format!("evidence/{}.jsonl", a.to_hex())));
    }

    #[test]
    fn a_damaged_line_is_counted_and_a_torn_one_cannot_swallow_the_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let s = HeadStore::locate(Some(dir.path())).unwrap();
        let a = ArchiveId::from_bytes([1; 32]);
        s.append_evidence(&a, &record(0)).unwrap();
        // Garbage, a valid-JSON line of another schema, a blank line, and a
        // torn write (no final newline).
        let mut f = OpenOptions::new()
            .append(true)
            .open(s.evidence_path(&a))
            .unwrap();
        f.write_all(b"{not json\n{\"schema\":7}\n\n").unwrap();
        let mut torn = serde_json::to_vec(&record(1)).unwrap();
        torn.truncate(torn.len() / 2);
        f.write_all(&torn).unwrap();
        drop(f);
        let got = s.read_evidence(&a).unwrap();
        assert_eq!(got.records.len(), 1);
        assert_eq!(got.unreadable, 3);
        // The next append starts on a fresh line: it is read, and the torn
        // line stays exactly one unreadable line.
        s.append_evidence(&a, &record(2)).unwrap();
        let got = s.read_evidence(&a).unwrap();
        assert_eq!(
            got.records.iter().map(|r| r.head.seq).collect::<Vec<_>>(),
            [0, 2]
        );
        assert_eq!(got.unreadable, 3);
    }
}
