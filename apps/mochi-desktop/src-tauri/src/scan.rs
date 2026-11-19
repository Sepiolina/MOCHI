//! Scan an archive for viruses with a scanner the user has (owner request
//! 2026-11-14, after WinRAR's "Scan archive for viruses").
//!
//! # How it works
//!
//! 1. The window shows the scanners found on this computer ([`detect`]):
//!    Microsoft Defender's `MpCmdRun.exe` on Windows, ClamAV's `clamscan` on
//!    Ubuntu. The user may instead name any program and its parameters; `%f`
//!    stands for the folder to scan. The choice is saved ([`SETTINGS_FILE`]).
//! 2. The scan job extracts the version on screen into a fresh, randomly
//!    named, owner-only folder under the app's cache directory
//!    (`<cache>/virus-scan/<random>/stage`) through the **same code the
//!    Extract job runs** (`extract::run`): every byte is written by the C6
//!    restore engine. There is no second extractor.
//! 3. It runs the scanner on that folder, without a shell, with standard
//!    input closed and its output captured (the last [`MAX_OUTPUT`] bytes),
//!    then deletes the folder. Cancelling stops the scanner.
//!
//! # What it does not do
//!
//! * MOCHI does not judge files. The report says what the scanner reported,
//!   read from its exit code for the two scanners it knows, and shows its
//!   output; for any other program it gives the exit code only. A scan that
//!   reports nothing is **not** a statement that the archive is harmless,
//!   and nothing here says so (spec §23.3 wording).
//! * No archive-supplied text reaches the command line: `%f` is the random
//!   folder's path, made here. The program is one the user chose.
//! * Entries that could not be extracted were not scanned; the report counts
//!   them, and a scan with any is labelled partial.
//! * A scanner may quarantine or delete files in the scan folder: that is the
//!   app's own copy, never the archive.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use mochi_core::job::{JobContext, ProgressEvent};
use mochi_core::{ErrorCode, MochiError, Result};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::browse::Archives;
use crate::dragout::{self, DragFolder};
use crate::extract::{self, Destination};
use crate::ipc::{ConflictChoice, JobRequest};
use crate::write::JobDone;

/// The saved choice, in the app's config folder.
pub const SETTINGS_FILE: &str = "virus-scan.json";

/// The folder under the app's cache directory that holds scan folders.
pub const FOLDER: &str = "virus-scan";

/// What a scan keeps of the scanner's output (the end of it, where scanners
/// print their summary).
pub const MAX_OUTPUT: usize = 64 * 1024;

/// Most declared bytes one scan may extract (8 GiB), as for drag-out.
pub const MAX_BYTES: u64 = dragout::MAX_BYTES;

/// Longest parameters line accepted.
pub const MAX_ARGUMENTS: usize = 2048;

/// The placeholder for the folder to scan.
pub const FOLDER_PLACEHOLDER: &str = "%f";

/// How often a running scanner is checked for an end or a cancel.
const POLL: Duration = Duration::from_millis(100);

/// One scanner: a name to show, the program, and its parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ScannerView {
    /// "Microsoft Defender", "ClamAV"; empty for one the user named.
    pub name: String,
    pub program: String,
    pub arguments: String,
}

/// What the scan window starts with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ScanSetup {
    /// Scanners found on this computer, in the order shown.
    pub detected: Vec<ScannerView>,
    /// The program and parameters used last, else the first one found, else
    /// empty.
    pub chosen: ScannerView,
}

/// What the scanner reported, read from its exit code. Only for scanners
/// whose exit codes are documented; never a verdict of MOCHI's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum ScanVerdict {
    /// The scanner reported no threats.
    NoneReported,
    /// The scanner reported one or more threats.
    ThreatsReported,
    /// The scanner reported that it could not finish.
    ScannerFailed,
    /// The program's exit codes are not known here: read its output.
    Unknown,
}

/// The scan job's report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ScanReportView {
    /// The scanner's name if known, else the program's file name.
    pub scanner: String,
    #[ts(type = "number")]
    pub seq: u64,
    /// Files and folders extracted for the scan.
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub folders: u64,
    /// Entries that could not be extracted, so were not scanned.
    #[ts(type = "number")]
    pub not_scanned: u64,
    /// `None` if the program was ended by a signal.
    pub exit_code: Option<i32>,
    pub verdict: ScanVerdict,
    /// The end of what the program printed (standard output, then standard
    /// error), as text.
    pub output: String,
    pub output_truncated: bool,
    /// The scan folder could not be deleted at once (a scanner may still hold
    /// a file); it is deleted at the next start of the app.
    pub cleanup_pending: bool,
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

static CONFIG: OnceLock<PathBuf> = OnceLock::new();
static CACHE: OnceLock<PathBuf> = OnceLock::new();
static SAVED: Mutex<Option<ScannerView>> = Mutex::new(None);

/// Remember where the choice is saved and where scan folders go, read the
/// saved choice, and delete scan folders an earlier run left (an hour old or
/// more), off the setup path.
pub fn configure(config: PathBuf, cache: PathBuf) {
    let saved = read(&config);
    *SAVED.lock().unwrap_or_else(|p| p.into_inner()) = saved;
    let _ = CONFIG.set(config);
    let base = cache.join(FOLDER);
    let _ = CACHE.set(cache);
    std::thread::spawn(move || {
        dragout::sweep(&base, dragout::MAX_AGE, SystemTime::now());
    });
}

/// Delete the scan folders an earlier scan could not (the app is exiting).
pub fn sweep_on_exit() {
    if let Some(cache) = CACHE.get() {
        dragout::sweep(&cache.join(FOLDER), Duration::ZERO, SystemTime::now());
    }
}

fn read(dir: &Path) -> Option<ScannerView> {
    std::fs::read(dir.join(SETTINGS_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<ScannerView>(&b).ok())
}

fn save(dir: &Path, chosen: &ScannerView) -> Result<()> {
    let io = |e: std::io::Error| {
        MochiError::new(
            ErrorCode::IoError,
            format!("saving the virus scanner choice: {e}"),
        )
    };
    std::fs::create_dir_all(dir).map_err(io)?;
    let json = serde_json::to_vec(chosen).map_err(|e| {
        MochiError::new(
            ErrorCode::IoError,
            format!("saving the virus scanner choice: {e}"),
        )
    })?;
    let tmp = dir.join(format!("{SETTINGS_FILE}.tmp"));
    std::fs::write(&tmp, json).map_err(io)?;
    std::fs::rename(&tmp, dir.join(SETTINGS_FILE)).map_err(io)
}

/// The scan window's starting point.
pub fn setup() -> ScanSetup {
    let detected = detect();
    let saved = SAVED.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let chosen = saved
        .or_else(|| detected.first().cloned())
        .unwrap_or(ScannerView {
            name: String::new(),
            program: String::new(),
            arguments: format!("\"{FOLDER_PLACEHOLDER}\""),
        });
    ScanSetup { detected, chosen }
}

// ---------------------------------------------------------------------------
// Known scanners
// ---------------------------------------------------------------------------

/// A scanner whose location and exit codes are documented.
struct Known {
    name: &'static str,
    /// The program's file name, lower case.
    file: &'static str,
    arguments: &'static str,
    /// Exit codes that mean "no threats" and "threats found".
    none: &'static [i32],
    threats: &'static [i32],
}

/// Microsoft Defender's command-line tool: 0 no threats, 2 threats found
/// (MpCmdRun documentation). ScanType 3 is a custom scan of the given folder.
const DEFENDER: Known = Known {
    name: "Microsoft Defender",
    file: "mpcmdrun.exe",
    arguments: "-Scan -ScanType 3 -File \"%f\" -DisableRemediation",
    none: &[0],
    threats: &[2],
};

/// ClamAV's scanner: 0 no virus found, 1 virus(es) found, 2 errors
/// (`man clamscan`).
const CLAMSCAN: Known = Known {
    name: "ClamAV",
    file: "clamscan",
    arguments: "--recursive --infected \"%f\"",
    none: &[0],
    threats: &[1],
};

fn known_for(program: &Path) -> Option<&'static Known> {
    let file = program.file_name()?.to_str()?.to_ascii_lowercase();
    [&DEFENDER, &CLAMSCAN].into_iter().find(|k| k.file == file)
}

/// The scanners found on this computer.
pub fn detect() -> Vec<ScannerView> {
    let mut found = Vec::new();
    let mut add = |k: &Known, path: PathBuf| {
        if path.is_file() {
            found.push(ScannerView {
                name: k.name.to_owned(),
                program: path.to_string_lossy().into_owned(),
                arguments: k.arguments.to_owned(),
            });
        }
    };
    if cfg!(windows) {
        if let Some(pf) = std::env::var_os("ProgramFiles") {
            add(
                &DEFENDER,
                PathBuf::from(pf)
                    .join("Windows Defender")
                    .join("MpCmdRun.exe"),
            );
        }
    } else {
        for dir in ["/usr/bin", "/usr/local/bin"] {
            let p = Path::new(dir).join(CLAMSCAN.file);
            if p.is_file() {
                add(&CLAMSCAN, p);
                break;
            }
        }
    }
    found
}

/// What the exit code means for this program, if it is a known scanner.
pub fn verdict(program: &Path, exit_code: Option<i32>) -> ScanVerdict {
    let (Some(k), Some(code)) = (known_for(program), exit_code) else {
        return ScanVerdict::Unknown;
    };
    if k.none.contains(&code) {
        ScanVerdict::NoneReported
    } else if k.threats.contains(&code) {
        ScanVerdict::ThreatsReported
    } else {
        ScanVerdict::ScannerFailed
    }
}

// ---------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------

fn invalid(what: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, what)
}

/// The program the user named: an absolute path to an existing file, and on
/// Windows an `.exe` or `.com` (a batch file would run through `cmd.exe`,
/// which reads its arguments differently).
pub fn check_program(text: &str) -> Result<PathBuf> {
    let text = text.trim();
    if text.is_empty() {
        return Err(invalid("choose the virus scanner's program"));
    }
    if text.chars().any(char::is_control) {
        return Err(invalid("the program's name has a control character"));
    }
    let path = PathBuf::from(text);
    if !path.is_absolute() {
        return Err(invalid(
            "give the program's full path, or choose it with Browse",
        ));
    }
    if cfg!(windows) {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if !matches!(ext.as_deref(), Some("exe" | "com")) {
            return Err(invalid("the virus scanner must be a program (.exe)"));
        }
    }
    match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => Ok(path),
        Ok(_) => Err(invalid("that is a folder, not a program")),
        Err(_) => Err(invalid("no program was found there")),
    }
}

/// Split a parameters line into arguments: whitespace separates them, and
/// double quotes group a part with spaces (the quotes themselves are
/// removed). There are no escapes. `%f` is replaced by `folder` wherever it
/// occurs; it must occur at least once.
pub fn arguments(text: &str, folder: &Path) -> Result<Vec<String>> {
    if text.len() > MAX_ARGUMENTS {
        return Err(invalid("the parameters are too long"));
    }
    if text.chars().any(|c| c.is_control() && c != '\t') {
        return Err(invalid("the parameters have a control character"));
    }
    if !text.contains(FOLDER_PLACEHOLDER) {
        return Err(invalid(
            "put %f in the parameters where the folder to scan goes",
        ));
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return Err(invalid("a quote in the parameters is not closed"));
    }
    if started {
        out.push(current);
    }
    let folder = folder.to_string_lossy();
    Ok(out
        .into_iter()
        .map(|a| a.replace(FOLDER_PLACEHOLDER, &folder))
        .collect())
}

/// Refuse a scan request that cannot work, before the job starts.
pub fn validate(archives: &Archives, request: &JobRequest) -> Result<()> {
    let JobRequest::ScanArchive {
        handle,
        seq,
        program,
        arguments: args,
    } = request
    else {
        return Err(invalid("not a scan request"));
    };
    check_program(program)?;
    arguments(args, Path::new("folder"))?;
    let info = archives.info(*handle, u64::from(*seq))?;
    let total: u64 = info.total_size.parse().unwrap_or(u64::MAX);
    if total > MAX_BYTES {
        return Err(MochiError::new(
            ErrorCode::LimitExceeded,
            "this version is larger than 8 GiB; extract it and scan the folder with your \
             scanner instead",
        ));
    }
    if CACHE.get().is_none() {
        return Err(MochiError::new(
            ErrorCode::IoError,
            "the app's cache folder is not known",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------

/// The scan job: extract, scan, delete, report.
pub fn run(archives: &Archives, request: &JobRequest, ctx: &JobContext<'_>) -> Result<JobDone> {
    let cache = CACHE.get().ok_or_else(|| {
        MochiError::new(ErrorCode::IoError, "the app's cache folder is not known")
    })?;
    run_in(archives, cache, request, ctx)
}

fn run_in(
    archives: &Archives,
    cache: &Path,
    request: &JobRequest,
    ctx: &JobContext<'_>,
) -> Result<JobDone> {
    let JobRequest::ScanArchive {
        handle,
        seq,
        program,
        arguments: args,
    } = request
    else {
        return Err(invalid("not a scan request"));
    };
    let program_path = check_program(program)?;
    // The choice is remembered once it is valid, as a scanner window does.
    let chosen = ScannerView {
        name: known_for(&program_path)
            .map(|k| k.name.to_owned())
            .unwrap_or_default(),
        program: program.trim().to_owned(),
        arguments: args.clone(),
    };
    if let Some(dir) = CONFIG.get() {
        // Not saving the choice does not stop the scan.
        let _ = save(dir, &chosen);
    }
    *SAVED.lock().unwrap_or_else(|p| p.into_inner()) = Some(chosen);

    let base = dragout::ensure_named_base(cache, FOLDER)?;
    let mut folder = DragFolder::create(&base)?;
    let stage = folder.stage();
    let argv = arguments(args, &stage)?;

    let destination = Destination::default();
    destination.set(stage.clone());
    let extract_request = JobRequest::ExtractArchive {
        handle: *handle,
        seq: *seq,
        keys: Vec::new(),
        // A name the file system cannot hold is skipped and counted, not a
        // reason to scan nothing.
        on_conflict: ConflictChoice::SkipConflicts,
    };
    let done = extract::run(archives, &destination, &extract_request, ctx)?;
    let (files, folders, not_scanned) = done
        .extract
        .as_deref()
        .map(|r| (r.files, r.directories, r.not_extracted_total))
        .unwrap_or_default();
    ctx.cancel.check()?;

    ctx.progress.report(&ProgressEvent {
        phase: "scan",
        completed: 0,
        total: None,
    });
    let outcome = run_scanner(&program_path, &argv, ctx)?;

    let cleanup_pending = dragout::remove_tree(folder.root()).is_err();
    // Deleted above, or left to the sweep.
    folder.keep();
    let scanner = known_for(&program_path).map_or_else(
        || {
            program_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        },
        |k| k.name.to_owned(),
    );
    let report = ScanReportView {
        scanner,
        seq: u64::from(*seq),
        files,
        folders,
        not_scanned,
        exit_code: outcome.code,
        verdict: verdict(&program_path, outcome.code),
        output: outcome.output,
        output_truncated: outcome.truncated,
        cleanup_pending,
    };
    Ok(JobDone {
        summary: format!(
            "scanned {files} files with {}; exit code {}",
            report.scanner,
            report
                .exit_code
                .map_or_else(|| "none".to_owned(), |c| c.to_string())
        ),
        commit: None,
        extract: None,
        foreign_test: None,
        check: None,
        search: None,
        rekey: None,
        export: None,
        scan: Some(Box::new(report)),
        retrieve: None,
    })
}

struct Outcome {
    code: Option<i32>,
    output: String,
    truncated: bool,
}

/// The last `MAX_OUTPUT` bytes of a stream, read on its own thread.
fn tail_of(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut kept: Vec<u8> = Vec::new();
        let mut cut = false;
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    kept.extend_from_slice(&buf[..n]);
                    if kept.len() > MAX_OUTPUT {
                        let drop = kept.len() - MAX_OUTPUT;
                        kept.drain(..drop);
                        cut = true;
                    }
                }
            }
        }
        (kept, cut)
    })
}

fn spawn(program: &Path, argv: &[String]) -> Result<Child> {
    let mut cmd = Command::new(program);
    cmd.args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window over the app.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn().map_err(|e| {
        MochiError::new(
            ErrorCode::IoError,
            format!("the virus scanner could not be started: {e}"),
        )
    })
}

fn run_scanner(program: &Path, argv: &[String], ctx: &JobContext<'_>) -> Result<Outcome> {
    let mut child = spawn(program, argv)?;
    let out = child.stdout.take().map(tail_of);
    let err = child.stderr.take().map(tail_of);
    let status = loop {
        if ctx.cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(MochiError::new(
                ErrorCode::Cancelled,
                "the scan was cancelled",
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => {
                let _ = child.kill();
                return Err(MochiError::new(
                    ErrorCode::IoError,
                    format!("the virus scanner could not be watched: {e}"),
                ));
            }
        }
    };
    let join = |h: Option<std::thread::JoinHandle<(Vec<u8>, bool)>>| {
        h.and_then(|h| h.join().ok()).unwrap_or_default()
    };
    let (mut bytes, mut truncated) = join(out);
    let (err_bytes, err_cut) = join(err);
    if !err_bytes.is_empty() {
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(&err_bytes);
    }
    truncated |= err_cut;
    if bytes.len() > MAX_OUTPUT {
        let drop = bytes.len() - MAX_OUTPUT;
        bytes.drain(..drop);
        truncated = true;
    }
    Ok(Outcome {
        code: status.code(),
        output: String::from_utf8_lossy(&bytes).replace("\r\n", "\n"),
        truncated,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parameters_split_on_spaces_and_quotes_and_name_the_folder() {
        let folder = Path::new("/tmp/scan me/stage");
        assert_eq!(
            arguments("-Scan -ScanType 3 -File \"%f\"", folder).unwrap(),
            ["-Scan", "-ScanType", "3", "-File", "/tmp/scan me/stage"]
        );
        assert_eq!(
            arguments("--path=%f  -v", folder).unwrap(),
            ["--path=/tmp/scan me/stage", "-v"]
        );
        assert_eq!(
            arguments("\"\" %f", folder).unwrap(),
            ["", "/tmp/scan me/stage"]
        );
    }

    #[test]
    fn parameters_without_the_folder_or_with_an_open_quote_are_refused() {
        let folder = Path::new("/x");
        for bad in ["-r", "\"%f", "%f\u{7}", &"%f ".repeat(1000)] {
            let e = arguments(bad, folder).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{bad:?}");
        }
    }

    #[test]
    fn only_documented_exit_codes_become_a_verdict() {
        let defender = Path::new("C:/Program Files/Windows Defender/MpCmdRun.exe");
        let clam = Path::new("/usr/bin/clamscan");
        let other = Path::new("/opt/scanner/scan");
        assert_eq!(verdict(defender, Some(0)), ScanVerdict::NoneReported);
        assert_eq!(verdict(defender, Some(2)), ScanVerdict::ThreatsReported);
        assert_eq!(verdict(defender, Some(1)), ScanVerdict::ScannerFailed);
        assert_eq!(verdict(clam, Some(0)), ScanVerdict::NoneReported);
        assert_eq!(verdict(clam, Some(1)), ScanVerdict::ThreatsReported);
        assert_eq!(verdict(clam, Some(2)), ScanVerdict::ScannerFailed);
        assert_eq!(verdict(clam, None), ScanVerdict::Unknown);
        assert_eq!(verdict(other, Some(0)), ScanVerdict::Unknown);
    }

    #[test]
    fn a_program_must_be_an_absolute_path_to_a_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            check_program("").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            check_program("clamscan").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let folder = dir.path().to_string_lossy().into_owned();
        assert_eq!(
            check_program(&folder).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let missing = dir.path().join("nope.exe").to_string_lossy().into_owned();
        assert_eq!(
            check_program(&missing).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let file = dir.path().join("scan.exe");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(check_program(&file.to_string_lossy()).unwrap(), file);
    }

    #[test]
    fn the_choice_is_read_back_and_a_damaged_file_means_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), None);
        let v = ScannerView {
            name: String::new(),
            program: "/usr/bin/clamscan".into(),
            arguments: "-r \"%f\"".into(),
        };
        save(dir.path(), &v).unwrap();
        assert_eq!(read(dir.path()), Some(v));
        std::fs::write(dir.path().join(SETTINGS_FILE), b"{oops").unwrap();
        assert_eq!(read(dir.path()), None);
    }

    /// A fake scanner (a shell script) sees the extracted files in `%f`,
    /// prints, and exits 3; the report keeps its output and exit code, and
    /// the folder is gone afterwards.
    #[cfg(unix)]
    #[test]
    fn a_scan_extracts_runs_the_program_on_the_folder_and_cleans_up() {
        use std::os::unix::fs::PermissionsExt;

        use mochi_core::job::{CancellationToken, NullProgress};

        use mochi_core::publish::ReadOptions;
        use mochi_core::storage::os::OsStorage;
        use mochi_testkit::archive::{build, scripted_history};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scripted.mochi");
        build(
            OsStorage::create_new(&path).unwrap(),
            7,
            &scripted_history(),
        )
        .unwrap();
        let archives = Archives::default();
        let summary = archives.open(&path, ReadOptions::default()).unwrap();
        let head_seq = summary.commits.last().unwrap().seq;
        let tools = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let script = tools.path().join("fake-scan");
        std::fs::write(
            &script,
            "#!/bin/sh\nfind \"$2\" -type f | wc -l | tr -d ' '\necho warn >&2\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let request = JobRequest::ScanArchive {
            handle: summary.handle,
            seq: u32::try_from(head_seq).unwrap(),
            program: script.to_string_lossy().into_owned(),
            arguments: "--dir \"%f\"".into(),
        };
        let token = CancellationToken::new();
        let ctx = JobContext {
            progress: &NullProgress,
            cancel: &token,
        };
        let done = run_in(&archives, cache.path(), &request, &ctx).unwrap();
        let r = done.scan.unwrap();
        assert_eq!(r.exit_code, Some(3));
        assert_eq!(r.verdict, ScanVerdict::Unknown);
        assert_eq!(r.scanner, "fake-scan");
        assert!(r.files > 0);
        assert_eq!(r.not_scanned, 0);
        assert_eq!(r.output, format!("{}\nwarn\n", r.files));
        assert!(!r.cleanup_pending);
        let left: Vec<_> = std::fs::read_dir(cache.path().join(FOLDER))
            .unwrap()
            .collect();
        assert!(left.is_empty(), "the scan folder is deleted");
    }

    /// Cancelling stops a scanner that would run for a minute.
    #[cfg(unix)]
    #[test]
    fn cancelling_stops_the_scanner() {
        use mochi_core::job::{CancellationToken, NullProgress};

        let token = CancellationToken::new();
        let ctx = JobContext {
            progress: &NullProgress,
            cancel: &token,
        };
        let stopper = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stopper.cancel();
        });
        let started = std::time::Instant::now();
        let e = run_scanner(Path::new("/bin/sleep"), &["60".into()], &ctx)
            .map(|_| ())
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
