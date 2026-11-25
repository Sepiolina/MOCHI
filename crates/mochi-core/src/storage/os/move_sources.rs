//! "Delete files after archiving" on the local filesystem (spec Annex B.2.23
//! item 3): move what [`crate::sources::check_removable`] lets go to where
//! the user can get it back, through the caller's [`SetAside`] (the Recycle
//! Bin or the Trash in the desktop app and the CLI; a folder in tests).
//!
//! Shared by both clients so that the rules are the same everywhere:
//!
//! * files first, in import order, each checked once more without following
//!   a link (a file replaced by a link or a folder meanwhile is kept);
//! * then folders, deepest first, each only if it is a plain folder and
//!   empty at that moment (something added to it meanwhile keeps it);
//! * anything that cannot be moved is kept, with the reason;
//! * nothing is ever deleted outright, and nothing here fails: the commit is
//!   already published, so the caller reports what moved and what was kept.

use std::path::{Path, PathBuf};

use super::SetAside;
use crate::catalog::path::ArchivePath;
use crate::import::SourceKind;
use crate::sources::{Checked, Verdict};

/// What [`move_removable`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MovedSources {
    /// Files and folders moved.
    pub moved: u64,
    /// What was kept, and why, in the order it was decided.
    pub kept: Vec<(ArchivePath, String)>,
}

impl MovedSources {
    /// Every entry kept for one reason (the archive could not be read back,
    /// so nothing may be moved).
    pub fn all_kept<'a>(paths: impl IntoIterator<Item = &'a ArchivePath>, why: &str) -> Self {
        MovedSources {
            moved: 0,
            kept: paths
                .into_iter()
                .map(|p| (p.clone(), why.to_owned()))
                .collect(),
        }
    }
}

/// Move every source `checked` lets go through `set_aside` (see the module
/// rules).
pub fn move_removable(checked: &[Checked<PathBuf>], set_aside: &SetAside) -> MovedSources {
    let mut out = MovedSources::default();
    let mut keep = |path: &ArchivePath, why: String| out.kept.push((path.clone(), why));
    let mut moved = 0u64;
    let try_move = |node: &Path| set_aside(node);
    for c in checked.iter().filter(|c| c.kind == SourceKind::File) {
        match &c.verdict {
            Verdict::Keep(why) => keep(&c.path, why.clone()),
            Verdict::Remove => match std::fs::symlink_metadata(&c.node) {
                Ok(m) if m.is_file() => match try_move(&c.node) {
                    Ok(()) => moved += 1,
                    Err(e) => keep(&c.path, format!("it could not be moved to the Bin: {e}")),
                },
                Ok(_) => keep(&c.path, "it is no longer a plain file".to_owned()),
                Err(e) => keep(&c.path, format!("it could not be looked at again: {e}")),
            },
        }
    }
    for c in checked
        .iter()
        .rev()
        .filter(|c| c.kind == SourceKind::Directory)
    {
        match &c.verdict {
            Verdict::Keep(why) => keep(&c.path, why.clone()),
            Verdict::Remove => {
                let empty = std::fs::symlink_metadata(&c.node)
                    .ok()
                    .filter(std::fs::Metadata::is_dir)
                    .and_then(|_| std::fs::read_dir(&c.node).ok())
                    .map(|mut d| d.next().is_none());
                match empty {
                    Some(true) => match try_move(&c.node) {
                        Ok(()) => moved += 1,
                        Err(e) => keep(&c.path, format!("it could not be moved to the Bin: {e}")),
                    },
                    Some(false) => keep(&c.path, "it is not empty".to_owned()),
                    None => keep(&c.path, "it is no longer a plain folder".to_owned()),
                }
            }
        }
    }
    out.moved = moved;
    out
}
