//! Volume sets. 7z split volumes (`name.7z.001`, `.002`, ...) are plain byte
//! splits of one archive (plan D8 rule 8), read here as one concatenated
//! stream. RAR volumes are found by UnRAR itself (`rarfmt`).
//!
//! Runs inside the helper only. Volume names are derived from the archive
//! path the user chose by changing the numeric suffix; nothing is written.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Most volumes one set may have; a set longer than this is not followed.
pub const MAX_VOLUMES: usize = 10_000;

/// The numeric suffix of a `.7z.NNN` name, with its width, when `name` has
/// that form.
pub fn split_suffix(name: &str) -> Option<(usize, u32)> {
    let lower = name.to_ascii_lowercase();
    let dot = lower.rfind('.')?;
    let digits = &lower[dot + 1..];
    if digits.len() < 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !lower[..dot].ends_with(".7z") {
        return None;
    }
    Some((digits.len(), digits.parse().ok()?))
}

/// What the file name says about a 7z split set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitName {
    /// Not a `.7z.NNN` name.
    NotSplit,
    /// `.7z.001`: the first volume.
    First,
    /// `.7z.002` and later: not the start of the set.
    Later(u32),
}

pub fn classify(path: &Path) -> SplitName {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return SplitName::NotSplit;
    };
    match split_suffix(name) {
        Some((_, 1)) => SplitName::First,
        Some((_, n)) => SplitName::Later(n),
        None => SplitName::NotSplit,
    }
}

/// The paths of the volumes `.001`, `.002`, ... that exist next to `first`,
/// stopping at the first gap.
pub fn find_volumes(first: &Path) -> Vec<PathBuf> {
    let mut out = vec![first.to_path_buf()];
    let Some(name) = first.file_name().and_then(|n| n.to_str()) else {
        return out;
    };
    let Some((width, _)) = split_suffix(name) else {
        return out;
    };
    let stem = &name[..name.len() - width];
    for n in 2..=MAX_VOLUMES as u32 {
        let candidate = first.with_file_name(format!("{stem}{n:0width$}"));
        match std::fs::symlink_metadata(&candidate) {
            // A regular file only: a link could point anywhere.
            Ok(m) if m.is_file() => out.push(candidate),
            _ => break,
        }
    }
    out
}

/// Several files read as one: `Read + Seek` over their concatenation.
pub struct MultiFile {
    parts: Vec<(File, u64)>,
    total: u64,
    pos: u64,
}

impl MultiFile {
    pub fn open(paths: &[PathBuf]) -> io::Result<Self> {
        let mut parts = Vec::with_capacity(paths.len());
        let mut total: u64 = 0;
        for p in paths {
            let f = File::open(p)?;
            let len = f.metadata()?.len();
            total = total
                .checked_add(len)
                .ok_or_else(|| io::Error::other("volume set too large"))?;
            parts.push((f, len));
        }
        Ok(MultiFile {
            parts,
            total,
            pos: 0,
        })
    }

    pub fn len(&self) -> u64 {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub fn volume_count(&self) -> usize {
        self.parts.len()
    }
}

impl Read for MultiFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.total {
            return Ok(0);
        }
        let mut base: u64 = 0;
        for (file, len) in &mut self.parts {
            if self.pos < base + *len {
                let within = self.pos - base;
                let left = usize::try_from(*len - within).unwrap_or(usize::MAX);
                let take = buf.len().min(left);
                file.seek(SeekFrom::Start(within))?;
                let n = file.read(&mut buf[..take])?;
                self.pos += n as u64;
                return Ok(n);
            }
            base += *len;
        }
        Ok(0)
    }
}

impl Seek for MultiFile {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let new = match from {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.total.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        match new {
            Some(p) => {
                self.pos = p;
                Ok(p)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start",
            )),
        }
    }
}
