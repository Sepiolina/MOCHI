//! The short list of archives opened lately (start screen). Rust keeps the
//! paths; the webview is shown only file and folder names and asks to open an
//! entry by its position, so it never names a path (README "Security
//! decisions"). The list lives in a small JSON file in the app's config
//! folder, can be cleared from the window, and is best-effort: a missing or
//! damaged file means an empty list.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

const FILE: &str = "recent-archives.json";

/// Entries kept, newest first.
pub const MAX_RECENT: usize = 6;

/// One entry as the window sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct RecentView {
    /// Position in the list: what `open_recent` takes.
    pub index: u32,
    /// The archive's file name.
    pub name: String,
    /// The folder it is in, as the user would read it.
    pub folder: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    paths: Vec<String>,
}

#[derive(Default)]
pub struct Recents {
    file: Mutex<Option<PathBuf>>,
    list: Mutex<Vec<PathBuf>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Recents {
    /// Keep the list in `dir` from now on, reading what is there.
    pub fn configure(&self, dir: &Path) {
        let file = dir.join(FILE);
        let loaded: Stored = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        *lock(&self.list) = loaded
            .paths
            .into_iter()
            .map(PathBuf::from)
            .take(MAX_RECENT)
            .collect();
        *lock(&self.file) = Some(file);
    }

    /// Note that `path` was opened: it moves to the front.
    pub fn record(&self, path: &Path) {
        {
            let mut list = lock(&self.list);
            list.retain(|p| p != path);
            list.insert(0, path.to_path_buf());
            list.truncate(MAX_RECENT);
        }
        self.save();
    }

    pub fn view(&self) -> Vec<RecentView> {
        lock(&self.list)
            .iter()
            .enumerate()
            .map(|(i, p)| RecentView {
                index: u32::try_from(i).unwrap_or(u32::MAX),
                name: p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                folder: p
                    .parent()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default(),
            })
            .collect()
    }

    pub fn get(&self, index: usize) -> Option<PathBuf> {
        lock(&self.list).get(index).cloned()
    }

    pub fn clear(&self) {
        lock(&self.list).clear();
        self.save();
    }

    /// Best effort: a list that cannot be saved is still right for this run.
    fn save(&self) {
        let Some(file) = lock(&self.file).clone() else {
            return;
        };
        let stored = Stored {
            paths: lock(&self.list)
                .iter()
                .filter_map(|p| p.to_str().map(str::to_owned))
                .collect(),
        };
        let Ok(json) = serde_json::to_vec(&stored) else {
            return;
        };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = file.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &file);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_deduplicated_and_capped() {
        let r = Recents::default();
        for i in 0..10 {
            r.record(Path::new(&format!("/a/{i}.mochi")));
        }
        r.record(Path::new("/a/7.mochi"));
        let v = r.view();
        assert_eq!(v.len(), MAX_RECENT);
        assert_eq!(v[0].name, "7.mochi");
        assert_eq!(v.iter().filter(|e| e.name == "7.mochi").count(), 1);
        assert_eq!(r.get(1), Some(PathBuf::from("/a/9.mochi")));
        assert_eq!(r.get(MAX_RECENT), None);
    }

    #[test]
    fn it_survives_a_restart_and_a_damaged_file_means_empty() {
        let dir = tempfile::tempdir().unwrap();
        let r = Recents::default();
        r.configure(dir.path());
        r.record(Path::new("/x/one.mochi"));
        r.record(Path::new("/x/two.mochi"));
        let again = Recents::default();
        again.configure(dir.path());
        assert_eq!(again.view()[0].name, "two.mochi");
        again.clear();
        let cleared = Recents::default();
        cleared.configure(dir.path());
        assert!(cleared.view().is_empty());
        std::fs::write(dir.path().join(FILE), b"{not json").unwrap();
        let damaged = Recents::default();
        damaged.configure(dir.path());
        assert!(damaged.view().is_empty());
    }
}
