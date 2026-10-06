//! This client's local evidence: the last-seen head of each archive, by
//! archive ID (spec Annex B.1 D8, plan O8: "the client keeps the last-seen
//! head per archive ID locally (opt-out available)").
//!
//! The file is `heads.json` in the state directory: `--state-dir`, else
//! `$MOCHI_STATE_DIR`, else `$XDG_STATE_HOME/mochi` or
//! `~/.local/state/mochi` on Linux and `%LOCALAPPDATA%\mochi` on Windows.
//!
//! ```json
//! {"version": 1, "heads": {"<archive id hex>": {"seq": 4, "commit_id": "<hex>"}}}
//! ```
//!
//! Rules:
//! * A head is recorded after this client commits it (`create`, `append`)
//!   or after `verify`/`fsck` found nothing failing. The recorded sequence
//!   never decreases, so a rolled-back copy cannot lower the anchor.
//! * Writes go to a temporary file in the same directory, then replace
//!   `heads.json` by rename. Two clients writing at once may lose one
//!   update (concession: the anchor is then older, never wrong; noted in
//!   `docs/c14-cli.md`).
//! * The file is outside every archive: verification stays read-only.

use std::path::{Path, PathBuf};

use mochi_core::object::ArchiveId;
use mochi_core::{ErrorCode, MochiError, Result};
use mochi_format::digest::CommitId;
use serde_json::{json, Value};

pub const FILE_NAME: &str = "heads.json";
const VERSION: u64 = 1;

/// The local head history.
#[derive(Debug, Clone)]
pub struct HeadStore {
    dir: PathBuf,
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
        let commit_id =
            mochi_core::verify::parse_commit_id(id).map_err(|e| err(&path, e.message))?;
        Ok(Some(SeenHead { seq, commit_id }))
    }

    /// Record `head` for `archive`, unless a later sequence is recorded.
    pub fn record(&self, archive: &ArchiveId, head: SeenHead) -> Result<()> {
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
        std::fs::create_dir_all(&self.dir).map_err(|e| err(&path, e))?;
        let body = serde_json::to_vec_pretty(&json!({"version": VERSION, "heads": heads}))
            .map_err(|e| err(&path, e))?;
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
mod tests {
    use super::*;

    fn id(n: u8) -> CommitId {
        CommitId::from_bytes([n; 32])
    }

    #[test]
    fn heads_round_trip_and_never_move_back() {
        let dir = tempfile::tempdir().unwrap();
        let s = HeadStore::locate(Some(&dir.path().join("nested"))).unwrap();
        let a = ArchiveId::from_bytes([1; 32]);
        assert_eq!(s.get(&a).unwrap(), None);
        s.record(
            &a,
            SeenHead {
                seq: 3,
                commit_id: id(3),
            },
        )
        .unwrap();
        s.record(
            &a,
            SeenHead {
                seq: 1,
                commit_id: id(1),
            },
        )
        .unwrap();
        assert_eq!(
            s.get(&a).unwrap(),
            Some(SeenHead {
                seq: 3,
                commit_id: id(3)
            })
        );
        s.record(
            &a,
            SeenHead {
                seq: 4,
                commit_id: id(4),
            },
        )
        .unwrap();
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
}
