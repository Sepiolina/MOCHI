//! Writing ZIP, tar, tar.gz and tar.zst archives from the user's local files
//! (plan O31, owner decision 2026-10-20).
//!
//! # Why a crate of its own
//!
//! * Not `mochi-format`: that crate is the pure `.mochi` wire format.
//! * Not `mochi-core`: that crate owns `.mochi` archive operations, and its
//!   I/O goes through `Storage`; this one writes foreign formats and reads
//!   plain files.
//! * Not `mochi-foreign`: that crate is the isolated helper that *parses*
//!   untrusted archives. Nothing here parses an archive; the only input is
//!   files the user chose, so no helper process is needed.
//!
//! # Rules
//!
//! * **Never a partial archive under the final name.** Bytes go to a
//!   temporary file in the destination folder (`.<name>.<random>.part`), which
//!   is published with the same no-replace rename the `.mochi` writer uses
//!   ([`StorageDir::publish_no_replace`]). A failure, a cancellation, or an
//!   unwind removes the temporary file.
//! * **Never overwrite.** An existing destination is `DESTINATION_EXISTS`
//!   before any work, and again at publication if the name appeared
//!   meanwhile. There is no replace option.
//! * **Names.** Each input becomes a top-level entry named by its last
//!   component; children keep their names. Paths are relative and joined by
//!   `/`. A name that is not valid Unicode, or (ZIP) contains a backslash, is
//!   skipped and listed, never altered.
//! * **Kinds.** Regular files and directories (empty ones too). Symbolic
//!   links are not followed and not stored; devices, FIFOs, and sockets are
//!   not stored; each is listed in [`Report::skipped`], as is a file that
//!   cannot be opened. A file that is shorter when read than when it was
//!   measured fails the job (the archive would not match what was read).
//! * **Privacy.** tar entries carry no user or group name and uid/gid 0.
//! * **A job.** Progress goes to the `mochi-core` progress sink
//!   ([`SCAN_PHASE`], then [`WRITE_PHASE`] in content bytes with a known
//!   total) and the cancellation token is checked between entries and every
//!   read; no second mechanism.
//! * **No encryption.** ZIP is written without any; callers that are asked
//!   for it answer `UNSUPPORTED`.
//!
//! Wording belongs to the caller: a report says what was written ("Wrote 12
//! files, 3.4 MB"), never a MOCHI status or a health dimension.

#![forbid(unsafe_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use mochi_core::catalog::path::ArchivePath;
use mochi_core::import::{SourceKind, SourceTree};
use mochi_core::job::JobContext;
use mochi_core::storage::os::OsDir;
use mochi_core::storage::{DirectoryDurability, StorageDir};
use mochi_core::{ErrorCode, MochiError, Result};

/// Progress phase of the walk that measures the input; `completed` is the
/// number of entries found.
pub const SCAN_PHASE: &str = "export_scan";

/// Progress phase of the writing; `completed` and `total` are content bytes.
pub const WRITE_PHASE: &str = "export";

/// Content bytes between two progress reports.
const REPORT_EVERY: u64 = 256 * 1024;

/// Read buffer for file content.
const READ_BUFFER: usize = 256 * 1024;

/// ZIP entries at or above this size are written with ZIP64 headers.
const ZIP64_FROM: u64 = 1 << 31;

/// The archive formats this crate writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Zip,
    Tar,
    TarGz,
    TarZst,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::Zip, Format::Tar, Format::TarGz, Format::TarZst];

    /// The file extension, with its dot.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Zip => ".zip",
            Format::Tar => ".tar",
            Format::TarGz => ".tar.gz",
            Format::TarZst => ".tar.zst",
        }
    }

    /// Whether the format has a compression level to choose.
    pub fn has_level(self) -> bool {
        !matches!(self, Format::Tar)
    }

    /// The levels worth offering for this format, fastest first. Deflate has
    /// three that differ (1, 6, 9), so ZIP and `.tar.gz` offer three;
    /// Zstandard offers all seven. Plain tar has none.
    pub fn levels(self) -> &'static [Level] {
        match self {
            Format::Zip | Format::TarGz => &[Level::Fast, Level::Balanced, Level::Maximum],
            Format::TarZst => &Level::ALL,
            Format::Tar => &[],
        }
    }
}

/// The compression effort. Meaningless for plain tar.
///
/// The names follow the desktop app's presets. Zstandard (`.tar.zst`) uses
/// the level each name stands for. Deflate (ZIP, `.tar.gz`) has no negative
/// or ultra levels, so it offers only Fast, Balanced and Maximum
/// ([`Format::levels`]); the others still map (to 1 and 9) so that the
/// mapping is total, but the desktop app refuses them for those formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Zstandard `--fast=4` (level -4).
    Turbo,
    /// Zstandard `--fast=1` (level -1).
    VeryFast,
    /// Level 1.
    Fast,
    /// Zstandard 3, Deflate 6.
    Balanced,
    /// Zstandard 19.
    Maximum,
    /// Zstandard 20.
    Ultra,
    /// Zstandard 22, the highest; a window of up to 128 MiB.
    Extreme,
}

impl Level {
    /// Every level, fastest first.
    pub const ALL: [Level; 7] = [
        Level::Turbo,
        Level::VeryFast,
        Level::Fast,
        Level::Balanced,
        Level::Maximum,
        Level::Ultra,
        Level::Extreme,
    ];

    fn deflate(self) -> u32 {
        match self {
            Level::Turbo | Level::VeryFast | Level::Fast => 1,
            Level::Balanced => 6,
            Level::Maximum | Level::Ultra | Level::Extreme => 9,
        }
    }

    fn zstd(self) -> i32 {
        match self {
            Level::Turbo => -4,
            Level::VeryFast => -1,
            Level::Fast => 1,
            Level::Balanced => 3,
            Level::Maximum => 19,
            Level::Ultra => 20,
            Level::Extreme => 22,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub format: Format,
    pub level: Level,
}

/// An input entry that was not written, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The entry's path inside the archive (lossy text for display).
    pub path: String,
    pub reason: String,
}

/// What a finished export wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub format: Format,
    pub files: u64,
    pub directories: u64,
    /// Content bytes read from the files.
    pub bytes: u64,
    /// Size of the archive file.
    pub output_bytes: u64,
    pub skipped: Vec<Skipped>,
    /// Set when the new file's directory entry is not confirmed durable.
    pub durability_note: Option<String>,
}

fn invalid(what: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, what)
}

fn io_error(what: &str, e: &io::Error) -> MochiError {
    MochiError::new(ErrorCode::IoError, format!("{what}: {e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
}

#[derive(Debug)]
struct Entry {
    name: String,
    path: PathBuf,
    kind: Kind,
    mode: Option<u32>,
    mtime: Option<i64>,
}

struct Plan {
    entries: Vec<Entry>,
    skipped: Vec<Skipped>,
    total_bytes: u64,
}

fn display(name: &ArchivePath) -> String {
    String::from_utf8_lossy(name.as_stored()).into_owned()
}

fn skip_reason(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Symlink => "a symbolic link is not stored; it was not followed",
        _ => "only regular files and folders are stored (this is a device, pipe, or socket)",
    }
}

/// Walk `inputs` depth first, parents before children, siblings in name
/// order, and keep what can be written. Nothing is read or written.
fn walk<T: SourceTree<Node = PathBuf>>(
    tree: &T,
    inputs: &[(ArchivePath, PathBuf)],
    format: Format,
    ctx: &JobContext<'_>,
) -> Result<Plan> {
    let mut plan = Plan {
        entries: Vec::new(),
        skipped: Vec::new(),
        total_bytes: 0,
    };
    let mut stack: Vec<(ArchivePath, PathBuf)> = inputs.iter().rev().cloned().collect();
    ctx.report(SCAN_PHASE, 0, None);
    while let Some((name, node)) = stack.pop() {
        ctx.check_cancelled()?;
        let meta = tree.meta(&node)?;
        // The stored name must be text every reader can show unchanged.
        let text = match std::str::from_utf8(name.as_stored()) {
            Ok(t) => t,
            Err(_) => {
                plan.skipped.push(Skipped {
                    path: display(&name),
                    reason: "the name is not valid Unicode, which this format needs".into(),
                });
                continue;
            }
        };
        if format == Format::Zip && text.contains('\\') {
            plan.skipped.push(Skipped {
                path: text.to_owned(),
                reason: "a backslash in a name would be read as a separator by ZIP tools".into(),
            });
            continue;
        }
        let mode = meta.attributes.posix.map(|p| p.mode);
        let mtime = meta.attributes.mtime.map(|m| m.secs);
        match meta.kind {
            SourceKind::File => {
                plan.total_bytes = plan.total_bytes.saturating_add(meta.len);
                plan.entries.push(Entry {
                    name: text.to_owned(),
                    path: node,
                    kind: Kind::File,
                    mode,
                    mtime,
                });
            }
            SourceKind::Directory => {
                let mut children = tree.children(&node)?;
                children.sort_by(|a, b| a.0.cmp(&b.0));
                plan.entries.push(Entry {
                    name: text.to_owned(),
                    path: node,
                    kind: Kind::Dir,
                    mode,
                    mtime,
                });
                for (child, child_node) in children.into_iter().rev() {
                    let mut components: Vec<Vec<u8>> =
                        name.components().map(<[u8]>::to_vec).collect();
                    components.push(child);
                    match ArchivePath::from_components(components) {
                        Ok(p) => stack.push((p, child_node)),
                        Err(e) => plan.skipped.push(Skipped {
                            path: format!("{text}/…"),
                            reason: format!("the name cannot be stored: {}", e.message),
                        }),
                    }
                }
            }
            kind @ (SourceKind::Symlink | SourceKind::Other) => plan.skipped.push(Skipped {
                path: text.to_owned(),
                reason: skip_reason(kind).into(),
            }),
        }
        ctx.report(SCAN_PHASE, plan.entries.len() as u64, None);
    }
    Ok(plan)
}

/// Removes the temporary file unless the archive was published.
struct Temp {
    path: PathBuf,
    armed: bool,
}

impl Drop for Temp {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Counts content bytes and reports them to the job, at most every
/// [`REPORT_EVERY`] bytes.
struct Counter<'a, 'b> {
    ctx: &'a JobContext<'b>,
    done: u64,
    reported: u64,
    total: u64,
}

impl Counter<'_, '_> {
    fn add(&mut self, n: u64) {
        self.done += n;
        if self.done - self.reported >= REPORT_EVERY {
            self.flush();
        }
    }

    fn flush(&mut self) {
        self.reported = self.done;
        self.ctx.report(WRITE_PHASE, self.done, Some(self.total));
    }
}

/// Why a copy of one file stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Cancelled,
    Shrank,
}

/// The content of one file: at most `left` bytes, a cancellation check and a
/// progress count on every read. A file that ends early is an error so that a
/// header's size can never disagree with the bytes behind it.
struct Content<'c, 'a, 'b, R: Read> {
    from: R,
    left: u64,
    counter: &'c mut Counter<'a, 'b>,
    stop: Option<Stop>,
}

impl<R: Read> Read for Content<'_, '_, '_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        if self.counter.ctx.check_cancelled().is_err() {
            self.stop = Some(Stop::Cancelled);
            return Err(io::Error::other("cancelled"));
        }
        let want = usize::try_from(self.left).map_or(buf.len(), |l| l.min(buf.len()));
        let n = loop {
            match self.from.read(&mut buf[..want]) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        };
        if n == 0 {
            self.stop = Some(Stop::Shrank);
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the file was shorter when it was read than when it was measured",
            ));
        }
        self.left -= n as u64;
        self.counter.add(n as u64);
        Ok(n)
    }
}

/// An error from the encoders, with a cancellation or a shrunk file told
/// apart from a plain I/O failure.
fn write_error(name: &str, stop: Option<Stop>, e: &dyn std::fmt::Display) -> MochiError {
    match stop {
        Some(Stop::Cancelled) => MochiError::new(ErrorCode::Cancelled, "operation cancelled"),
        Some(Stop::Shrank) => MochiError::new(
            ErrorCode::IoError,
            format!("{name} changed while it was being read; nothing was written"),
        ),
        None => MochiError::new(
            ErrorCode::IoError,
            format!("writing {name} to the archive failed: {e}"),
        ),
    }
}

/// An opened regular file and its size now.
fn open_file(path: &Path) -> std::result::Result<(File, u64), String> {
    let file = File::open(path).map_err(|e| format!("it could not be read: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("it could not be read: {e}"))?;
    if !meta.is_file() {
        return Err("it is no longer a regular file".into());
    }
    Ok((file, meta.len()))
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A ZIP timestamp (UTC; ZIP has no zone), or `None` outside 1980 to 2107.
fn zip_time(secs: i64) -> Option<zip::DateTime> {
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let rest = secs.rem_euclid(86_400);
    zip::DateTime::from_date_and_time(
        u16::try_from(year).ok()?,
        month,
        day,
        u8::try_from(rest / 3600).ok()?,
        u8::try_from(rest % 3600 / 60).ok()?,
        u8::try_from(rest % 60).ok()?,
    )
    .ok()
}

struct Written {
    files: u64,
    directories: u64,
    skipped: Vec<Skipped>,
}

fn write_zip<W: Write + io::Seek>(
    out: W,
    plan: &Plan,
    level: Level,
    counter: &mut Counter<'_, '_>,
) -> Result<(W, Written)> {
    use zip::write::SimpleFileOptions;
    let mut zip = zip::ZipWriter::new(out);
    let mut written = Written {
        files: 0,
        directories: 0,
        skipped: Vec::new(),
    };
    for entry in &plan.entries {
        counter.ctx.check_cancelled()?;
        let mut options = SimpleFileOptions::default();
        if let Some(t) = entry.mtime.and_then(zip_time) {
            options = options.last_modified_time(t);
        }
        if let Some(mode) = entry.mode {
            options = options.unix_permissions(mode & 0o7777);
        }
        match entry.kind {
            Kind::Dir => {
                zip.add_directory(entry.name.as_str(), options)
                    .map_err(|e| write_error(&entry.name, None, &e))?;
                written.directories += 1;
            }
            Kind::File => {
                let (file, len) = match open_file(&entry.path) {
                    Ok(v) => v,
                    Err(reason) => {
                        written.skipped.push(Skipped {
                            path: entry.name.clone(),
                            reason,
                        });
                        continue;
                    }
                };
                options = options
                    .compression_method(zip::CompressionMethod::Deflated)
                    .compression_level(Some(i64::from(level.deflate())))
                    .large_file(len >= ZIP64_FROM);
                zip.start_file(entry.name.as_str(), options)
                    .map_err(|e| write_error(&entry.name, None, &e))?;
                let mut content = Content {
                    from: BufReader::with_capacity(READ_BUFFER, file),
                    left: len,
                    counter: &mut *counter,
                    stop: None,
                };
                if let Err(e) = io::copy(&mut content, &mut zip) {
                    return Err(write_error(&entry.name, content.stop, &e));
                }
                written.files += 1;
            }
        }
    }
    let out = zip
        .finish()
        .map_err(|e| write_error("the ZIP directory", None, &e))?;
    Ok((out, written))
}

fn write_tar<W: Write>(out: W, plan: &Plan, counter: &mut Counter<'_, '_>) -> Result<(W, Written)> {
    let mut builder = tar::Builder::new(out);
    let mut written = Written {
        files: 0,
        directories: 0,
        skipped: Vec::new(),
    };
    for entry in &plan.entries {
        counter.ctx.check_cancelled()?;
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(entry.mtime.and_then(|s| u64::try_from(s).ok()).unwrap_or(0));
        match entry.kind {
            Kind::Dir => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(entry.mode.map_or(0o755, |m| m & 0o7777));
                header.set_size(0);
                let name = format!("{}/", entry.name);
                builder
                    .append_data(&mut header, &name, io::empty())
                    .map_err(|e| write_error(&entry.name, None, &e))?;
                written.directories += 1;
            }
            Kind::File => {
                let (file, len) = match open_file(&entry.path) {
                    Ok(v) => v,
                    Err(reason) => {
                        written.skipped.push(Skipped {
                            path: entry.name.clone(),
                            reason,
                        });
                        continue;
                    }
                };
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(entry.mode.map_or(0o644, |m| m & 0o7777));
                header.set_size(len);
                let mut content = Content {
                    from: BufReader::with_capacity(READ_BUFFER, file),
                    left: len,
                    counter: &mut *counter,
                    stop: None,
                };
                if let Err(e) = builder.append_data(&mut header, &entry.name, &mut content) {
                    return Err(write_error(&entry.name, content.stop, &e));
                }
                written.files += 1;
            }
        }
    }
    let out = builder
        .into_inner()
        .map_err(|e| write_error("the tar end marker", None, &e))?;
    Ok((out, written))
}

/// A temporary name in the destination folder, hidden and recognisable.
fn temp_name(final_name: &str) -> Result<String> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|e| {
        MochiError::new(
            ErrorCode::IoError,
            format!("no random number for a temporary name: {e}"),
        )
    })?;
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(".{final_name}.{hex}.part"))
}

fn exists(path: &Path) -> bool {
    !matches!(fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// Write `inputs` (each with the archive path it becomes) as one archive at
/// `destination`, which must not exist. See the crate rules. The result is
/// either a complete archive under `destination` or nothing under that name.
pub fn export<T: SourceTree<Node = PathBuf>>(
    tree: &T,
    inputs: &[(ArchivePath, PathBuf)],
    destination: &Path,
    options: &Options,
    ctx: &JobContext<'_>,
) -> Result<Report> {
    if inputs.is_empty() {
        return Err(invalid("nothing to write: no files or folders were chosen"));
    }
    let name = destination
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            invalid(format!(
                "{}: the archive's file name must be valid Unicode",
                destination.display()
            ))
        })?
        .to_owned();
    let folder = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut seen = std::collections::BTreeSet::new();
    for (p, _) in inputs {
        if !seen.insert(p.clone()) {
            return Err(invalid(format!(
                "two inputs would both become {:?} in the archive; nothing was written",
                display(p)
            )));
        }
    }
    if exists(destination) {
        return Err(MochiError::new(
            ErrorCode::DestinationExists,
            format!("{name} already exists; nothing was written and it was not replaced"),
        ));
    }
    if !fs::metadata(folder).is_ok_and(|m| m.is_dir()) {
        return Err(invalid(format!(
            "the folder {} does not exist",
            folder.display()
        )));
    }
    // Reading the inputs comes first: a failed walk creates nothing.
    let plan = walk(tree, inputs, options.format, ctx)?;
    ctx.check_cancelled()?;

    let temp_file_name = temp_name(&name)?;
    let temp_path = folder.join(&temp_file_name);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|e| io_error(&format!("creating {}", temp_path.display()), &e))?;
    let mut temp = Temp {
        path: temp_path,
        armed: true,
    };

    let mut counter = Counter {
        ctx,
        done: 0,
        reported: 0,
        total: plan.total_bytes,
    };
    counter.flush();
    let out = BufWriter::with_capacity(READ_BUFFER, file);
    let level = options.level;
    let (file, written) = match options.format {
        Format::Zip => {
            let (out, w) = write_zip(out, &plan, level, &mut counter)?;
            (finish_buffer(out)?, w)
        }
        Format::Tar => {
            let (out, w) = write_tar(out, &plan, &mut counter)?;
            (finish_buffer(out)?, w)
        }
        Format::TarGz => {
            let enc = flate2::write::GzEncoder::new(out, flate2::Compression::new(level.deflate()));
            let (enc, w) = write_tar(enc, &plan, &mut counter)?;
            let out = enc
                .finish()
                .map_err(|e| io_error("finishing the gzip stream", &e))?;
            (finish_buffer(out)?, w)
        }
        Format::TarZst => {
            let enc = zstd::stream::write::Encoder::new(out, level.zstd())
                .map_err(|e| io_error("starting the zstd stream", &e))?;
            let (enc, w) = write_tar(enc, &plan, &mut counter)?;
            let out = enc
                .finish()
                .map_err(|e| io_error("finishing the zstd stream", &e))?;
            (finish_buffer(out)?, w)
        }
    };
    counter.flush();
    file.sync_all()
        .map_err(|e| io_error("flushing the archive to disk", &e))?;
    let output_bytes = file.metadata().map_or(0, |m| m.len());
    drop(file);

    // The last chance to stop: after this the archive appears.
    ctx.check_cancelled()?;
    let mut dir = OsDir::containing(destination)?;
    dir.publish_no_replace(&temp_file_name, &name)?;
    temp.armed = false;
    let durability_note = match dir.sync_directory() {
        Ok(DirectoryDurability::Confirmed) => None,
        Ok(DirectoryDurability::Unconfirmed(why)) => Some(why),
        Err(e) => Some(e.to_string()),
    };

    let mut skipped = plan.skipped;
    skipped.extend(written.skipped);
    Ok(Report {
        format: options.format,
        files: written.files,
        directories: written.directories,
        bytes: counter.done,
        output_bytes,
        skipped,
        durability_note,
    })
}

fn finish_buffer(out: BufWriter<File>) -> Result<File> {
    out.into_inner()
        .map_err(|e| io_error("writing the archive", e.error()))
}

#[cfg(test)]
mod tests;
