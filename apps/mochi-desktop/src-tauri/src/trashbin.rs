//! Where "Replace existing files" puts the file it replaces (owner decision
//! 2026-11-11): the Recycle Bin on Windows, the Trash on Ubuntu
//! (freedesktop.org), never a deletion, so the user can get it back from
//! there. The restore engine calls this only for a regular file it has looked
//! at without following a link, only once the extracted file has passed its
//! checks, and checks afterwards that the name is free
//! (`mochi_core::storage::RestoreDir::set_aside`).
//!
//! Tests and E2E builds must not fill the real Bin of the machine they run
//! on: unit tests move the file into a folder of their own, and an E2E build
//! does so when `MOCHI_E2E_TRASH_DIR` names one (the spec then reads it).

use std::path::Path;
use std::sync::Arc;

use mochi_core::storage::os::SetAside;

/// The function the restore engine moves an existing file with.
pub fn set_aside() -> Arc<SetAside> {
    #[cfg(test)]
    {
        Arc::new(|path: &Path| into_folder(&test_bin(), path))
    }
    #[cfg(not(test))]
    {
        #[cfg(feature = "e2e")]
        if let Some(dir) = std::env::var_os("MOCHI_E2E_TRASH_DIR") {
            let dir = std::path::PathBuf::from(dir);
            return Arc::new(move |path: &Path| into_folder(&dir, path));
        }
        Arc::new(|path: &Path| trash::delete(path).map_err(std::io::Error::other))
    }
}

/// Move `path` into `dir` under its own name, numbered if that is taken there,
/// never replacing anything (a stand-in for the Bin in tests).
#[cfg(any(test, feature = "e2e"))]
fn into_folder(dir: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("no file name"))?
        .to_string_lossy()
        .into_owned();
    for n in 1..1000u32 {
        let target = if n == 1 {
            dir.join(&name)
        } else {
            dir.join(format!("{name} ({n})"))
        };
        match mochi_core::storage::os::rename_no_replace(path, &target) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free name in the stand-in bin"))
}

/// The folder unit tests use in place of the Bin: one per test process.
#[cfg(test)]
pub fn test_bin() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("mochi-test-bin-{}", std::process::id()))
}
