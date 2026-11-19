//! Round trips: every format is written, then read back with the format's own
//! reader (the `zip`, `tar`, `flate2` and `zstd` crates, as a test oracle), for
//! nested folders, empty folders, empty files, long and non-ASCII names,
//! multi-chunk content, skipped kinds, cancellation, and collisions.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use mochi_core::job::{CancellationToken, NullProgress, ProgressEvent, ProgressSink};
use mochi_core::storage::os::OsSourceTree;

use super::*;

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

/// A deterministic pseudo-random block, so that deflate has work to do.
fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

const LONG: &str = "a-rather-long-directory-name-to-pass-the-one-hundred-byte-tar-name-limit-0123456789-abcdefghij";

/// `src/` holds a tree; `loose.txt` is a second input beside it.
struct Fixture {
    tmp: tempfile::TempDir,
    inputs: Vec<(ArchivePath, PathBuf)>,
    files: BTreeMap<String, Vec<u8>>,
    dirs: Vec<String>,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("src");
        let big = noise(700_000);
        let mut files = BTreeMap::new();
        let mut add = |rel: &str, bytes: &[u8]| {
            put(&root, rel, bytes);
            files.insert(format!("src/{rel}"), bytes.to_vec());
        };
        add("docs/a.txt", b"alpha");
        add("docs/deep/er/b.txt", b"bravo bravo bravo bravo");
        add("empty-file", b"");
        add("big.bin", &big);
        add("Grüße/名前 ☃.txt", "unicode".as_bytes());
        add(&format!("{LONG}/{LONG}.txt"), b"long");
        add("with space/new file.txt", b"spaces");
        fs::create_dir_all(root.join("empty dir/inner")).unwrap();
        put(tmp.path(), "loose.txt", b"loose");
        files.insert("loose.txt".into(), b"loose".to_vec());
        let tree = OsSourceTree;
        let inputs = vec![
            tree.input(&root).unwrap(),
            tree.input(tmp.path().join("loose.txt")).unwrap(),
        ];
        let dirs = [
            "src",
            "src/docs",
            "src/docs/deep",
            "src/docs/deep/er",
            "src/Grüße",
            "src/empty dir",
            "src/empty dir/inner",
            "src/with space",
        ]
        .iter()
        .map(ToString::to_string)
        .chain([format!("src/{LONG}")])
        .collect();
        Fixture {
            tmp,
            inputs,
            files,
            dirs,
        }
    }

    fn out(&self, name: &str) -> PathBuf {
        let dir = self.tmp.path().join("out");
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn run(&self, format: Format, level: Level, name: &str) -> Result<Report> {
        let cancel = CancellationToken::new();
        let ctx = JobContext {
            progress: &NullProgress,
            cancel: &cancel,
        };
        export(
            &OsSourceTree,
            &self.inputs,
            &self.out(name),
            &Options { format, level },
            &ctx,
        )
    }

    /// Everything in `out/` (to see that no temporary file is left).
    fn leftovers(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.tmp.path().join("out"))
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }
}

type Listing = (BTreeMap<String, Vec<u8>>, Vec<String>);

fn read_zip(path: &Path) -> Listing {
    let mut zip = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
    let (mut files, mut dirs) = (BTreeMap::new(), Vec::new());
    for i in 0..zip.len() {
        let mut e = zip.by_index(i).unwrap();
        let name = e.name().unwrap().into_owned();
        // Entries are relative and use `/`.
        assert!(
            !name.starts_with('/') && !name.contains('\\') && !name.contains(".."),
            "{name}"
        );
        if e.is_dir() {
            dirs.push(name.trim_end_matches('/').to_owned());
        } else {
            let mut data = Vec::new();
            e.read_to_end(&mut data).unwrap(); // also checks the CRC
            files.insert(name, data);
        }
    }
    (files, dirs)
}

fn read_tar<R: Read>(reader: R) -> Listing {
    let mut tar = tar::Archive::new(reader);
    let (mut files, mut dirs) = (BTreeMap::new(), Vec::new());
    for e in tar.entries().unwrap() {
        let mut e = e.unwrap();
        let name = e.path().unwrap().to_string_lossy().into_owned();
        assert!(
            !name.starts_with('/') && !name.contains('\\') && !name.contains(".."),
            "{name}"
        );
        let header = e.header();
        // No owner is recorded.
        assert_eq!((header.uid().unwrap(), header.gid().unwrap()), (0, 0));
        assert!(header.username().unwrap().is_none_or(str::is_empty));
        if e.header().entry_type().is_dir() {
            dirs.push(name.trim_end_matches('/').to_owned());
        } else {
            assert!(e.header().entry_type().is_file(), "{name}");
            let mut data = Vec::new();
            e.read_to_end(&mut data).unwrap();
            files.insert(name, data);
        }
    }
    (files, dirs)
}

fn read_back(format: Format, path: &Path) -> Listing {
    match format {
        Format::Zip => read_zip(path),
        Format::Tar => read_tar(File::open(path).unwrap()),
        Format::TarGz => read_tar(flate2::read::GzDecoder::new(File::open(path).unwrap())),
        Format::TarZst => {
            read_tar(zstd::stream::read::Decoder::new(File::open(path).unwrap()).unwrap())
        }
    }
}

#[test]
fn every_format_round_trips_nested_empty_unicode_and_long_names() {
    for format in Format::ALL {
        let fx = Fixture::new();
        let name = format!("out{}", format.extension());
        let report = fx.run(format, Level::Balanced, &name).unwrap();
        let (files, mut dirs) = read_back(format, &fx.out(&name));
        assert_eq!(files, fx.files, "{format:?}");
        dirs.sort();
        let mut want = fx.dirs.clone();
        want.sort();
        assert_eq!(dirs, want, "{format:?}");
        assert_eq!(report.files, fx.files.len() as u64, "{format:?}");
        assert_eq!(report.directories, fx.dirs.len() as u64, "{format:?}");
        assert_eq!(
            report.bytes,
            fx.files.values().map(|v| v.len() as u64).sum::<u64>()
        );
        assert_eq!(
            report.output_bytes,
            fs::metadata(fx.out(&name)).unwrap().len()
        );
        assert!(
            report.skipped.is_empty(),
            "{format:?}: {:?}",
            report.skipped
        );
        // Only the archive is left in the folder: no temporary file.
        assert_eq!(fx.leftovers(), vec![name], "{format:?}");
    }
}

#[test]
fn every_level_produces_a_readable_archive() {
    for format in [Format::Zip, Format::TarGz, Format::TarZst] {
        for level in Level::ALL {
            let fx = Fixture::new();
            let name = format!("l{}", format.extension());
            fx.run(format, level, &name).unwrap();
            assert_eq!(
                read_back(format, &fx.out(&name)).0,
                fx.files,
                "{format:?} {level:?}"
            );
        }
    }
}

#[test]
fn levels_map_to_the_named_zstd_and_deflate_levels() {
    assert_eq!(Level::ALL.map(Level::zstd), [-4, -1, 1, 3, 19, 20, 22]);
    assert_eq!(Level::ALL.map(Level::deflate), [1, 1, 1, 6, 9, 9, 9]);
}

#[test]
fn deflate_formats_offer_only_levels_that_differ() {
    for format in [Format::Zip, Format::TarGz] {
        let deflate: Vec<u32> = format.levels().iter().map(|l| l.deflate()).collect();
        assert_eq!(deflate, [1, 6, 9], "{format:?}");
    }
    assert_eq!(Format::TarZst.levels(), Level::ALL);
    assert!(Format::Tar.levels().is_empty());
}

/// The window a Zstandard frame header declares (RFC 8878 §3.1.1.1): after
/// the 4-byte magic, the frame header descriptor, then the window descriptor.
fn declared_window(frame: &[u8]) -> u64 {
    let (descriptor, wd) = (frame[4], frame[5]);
    assert_eq!(
        descriptor & 0x20,
        0,
        "a streamed frame is not single-segment"
    );
    let base = 1u64 << (10 + u32::from(wd >> 3));
    base + base / 8 * u64::from(wd & 7)
}

/// `zstd --ultra -22` asks for a 128 MiB window when the size is unknown, as it
/// is for a streamed `.tar.zst`. MOCHI's own reader (`mochi-foreign`, ruzstd)
/// caps the window at 128 MiB, so the highest level must not exceed it.
#[test]
fn the_highest_level_stays_within_the_readers_window_cap() {
    let fx = Fixture::new();
    fx.run(Format::TarZst, Level::Extreme, "x.tar.zst").unwrap();
    let bytes = fs::read(fx.out("x.tar.zst")).unwrap();
    let window = declared_window(&bytes);
    assert!(window <= 128 << 20, "window {window}");
    assert!(
        window > 8 << 20,
        "level 22 should use a large window, got {window}"
    );
}

#[test]
fn compressing_formats_are_smaller_than_plain_tar_for_repetitive_content() {
    let tmp = tempfile::tempdir().unwrap();
    put(tmp.path(), "src/zeros.bin", &vec![0u8; 2_000_000]);
    let inputs = vec![OsSourceTree.input(tmp.path().join("src")).unwrap()];
    let cancel = CancellationToken::new();
    let ctx = JobContext {
        progress: &NullProgress,
        cancel: &cancel,
    };
    let mut size = BTreeMap::new();
    for format in Format::ALL {
        let dest = tmp.path().join(format!("z{}", format.extension()));
        export(
            &OsSourceTree,
            &inputs,
            &dest,
            &Options {
                format,
                level: Level::Balanced,
            },
            &ctx,
        )
        .unwrap();
        size.insert(format.extension(), fs::metadata(dest).unwrap().len());
    }
    assert!(size[".tar"] > 2_000_000);
    for ext in [".zip", ".tar.gz", ".tar.zst"] {
        assert!(size[ext] < 100_000, "{ext}: {}", size[ext]);
    }
}

#[cfg(unix)]
#[test]
fn symlinks_and_sockets_are_skipped_and_listed_not_followed() {
    use std::os::unix::net::UnixListener;
    for format in Format::ALL {
        let fx = Fixture::new();
        let src = fx.tmp.path().join("src");
        std::os::unix::fs::symlink("/etc/passwd", src.join("link")).unwrap();
        let _socket = UnixListener::bind(src.join("sock")).unwrap();
        let name = format!("s{}", format.extension());
        let report = fx.run(format, Level::Fast, &name).unwrap();
        let skipped: BTreeMap<_, _> = report
            .skipped
            .iter()
            .map(|s| (s.path.as_str(), s.reason.as_str()))
            .collect();
        assert!(skipped["src/link"].contains("symbolic link"), "{skipped:?}");
        assert!(skipped["src/sock"].contains("regular files"), "{skipped:?}");
        assert_eq!(skipped.len(), 2);
        let (files, _) = read_back(format, &fx.out(&name));
        assert_eq!(
            files, fx.files,
            "{format:?}: the link target was not stored"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_name_that_is_not_unicode_is_skipped_and_listed() {
    use std::os::unix::ffi::OsStrExt;
    let fx = Fixture::new();
    let bad = std::ffi::OsStr::from_bytes(b"bad\xffname");
    fs::write(fx.tmp.path().join("src").join(bad), b"x").unwrap();
    let report = fx.run(Format::Zip, Level::Fast, "n.zip").unwrap();
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].reason.contains("Unicode"));
    assert_eq!(read_back(Format::Zip, &fx.out("n.zip")).0, fx.files);
}

#[cfg(unix)]
#[test]
fn a_file_that_cannot_be_read_is_skipped_and_listed() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    let secret = fx.tmp.path().join("src/docs/a.txt");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    // Running as root reads it anyway; the test is about the unreadable case.
    let readable = File::open(&secret).is_ok();
    let report = fx.run(Format::Tar, Level::Fast, "u.tar").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    if readable {
        assert!(report.skipped.is_empty());
    } else {
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].path, "src/docs/a.txt");
        let (files, _) = read_back(Format::Tar, &fx.out("u.tar"));
        assert!(!files.contains_key("src/docs/a.txt"));
        assert_eq!(files.len(), fx.files.len() - 1);
    }
}

#[cfg(unix)]
#[test]
fn modes_and_times_are_recorded_and_owners_are_not() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    let script = fx.tmp.path().join("src/docs/a.txt");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o750)).unwrap();
    fx.run(Format::Tar, Level::Fast, "m.tar").unwrap();
    let mut tar = tar::Archive::new(File::open(fx.out("m.tar")).unwrap());
    let e = tar
        .entries()
        .unwrap()
        .map(|e| e.unwrap())
        .find(|e| e.path().unwrap().ends_with("docs/a.txt"))
        .unwrap();
    assert_eq!(e.header().mode().unwrap() & 0o7777, 0o750);
    assert!(e.header().mtime().unwrap() > 1_000_000_000);
    fx.run(Format::Zip, Level::Fast, "m.zip").unwrap();
    let mut zip = zip::ZipArchive::new(File::open(fx.out("m.zip")).unwrap()).unwrap();
    let z = zip.by_name("src/docs/a.txt").unwrap();
    assert_eq!(z.unix_mode().unwrap() & 0o7777, 0o750);
    assert!(z.last_modified().is_some_and(|t| t.year() >= 2024));
}

#[test]
fn an_existing_destination_is_refused_untouched() {
    for format in Format::ALL {
        let fx = Fixture::new();
        let name = format!("taken{}", format.extension());
        fs::write(fx.out(&name), b"mine").unwrap();
        let e = fx.run(format, Level::Fast, &name).unwrap_err();
        assert_eq!(e.code, ErrorCode::DestinationExists, "{format:?}");
        assert_eq!(fs::read(fx.out(&name)).unwrap(), b"mine");
        assert_eq!(fx.leftovers(), vec![name], "no temporary file is left");
    }
}

#[test]
fn a_name_that_appears_during_the_write_is_not_replaced() {
    // The destination is created by someone else after the walk has started.
    struct Intruder(PathBuf, AtomicUsize);
    impl ProgressSink for Intruder {
        fn report(&self, event: &ProgressEvent) {
            if event.phase == WRITE_PHASE && self.1.fetch_add(1, Ordering::SeqCst) == 0 {
                fs::write(&self.0, b"theirs").unwrap();
            }
        }
    }
    let fx = Fixture::new();
    let dest = fx.out("race.zip");
    let sink = Intruder(dest.clone(), AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let ctx = JobContext {
        progress: &sink,
        cancel: &cancel,
    };
    let e = export(
        &OsSourceTree,
        &fx.inputs,
        &dest,
        &Options {
            format: Format::Zip,
            level: Level::Fast,
        },
        &ctx,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::DestinationExists);
    assert_eq!(fs::read(&dest).unwrap(), b"theirs");
    assert_eq!(fx.leftovers(), vec!["race.zip".to_owned()]);
}

/// Cancels the job when a progress event of `phase` arrives for the `nth` time.
struct CancelAt {
    phase: &'static str,
    nth: usize,
    seen: AtomicUsize,
    token: CancellationToken,
}

impl ProgressSink for CancelAt {
    fn report(&self, event: &ProgressEvent) {
        if event.phase == self.phase && self.seen.fetch_add(1, Ordering::SeqCst) + 1 == self.nth {
            self.token.cancel();
        }
    }
}

#[test]
fn cancelling_while_scanning_or_writing_leaves_no_file_at_all() {
    for format in Format::ALL {
        for (phase, nth) in [(SCAN_PHASE, 3), (WRITE_PHASE, 2), (WRITE_PHASE, 3)] {
            let fx = Fixture::new();
            let token = CancellationToken::new();
            let sink = CancelAt {
                phase,
                nth,
                seen: AtomicUsize::new(0),
                token: token.clone(),
            };
            let ctx = JobContext {
                progress: &sink,
                cancel: &token,
            };
            let name = format!("c{}", format.extension());
            let e = export(
                &OsSourceTree,
                &fx.inputs,
                &fx.out(&name),
                &Options {
                    format,
                    level: Level::Fast,
                },
                &ctx,
            )
            .unwrap_err();
            assert_eq!(e.code, ErrorCode::Cancelled, "{format:?} {phase} {nth}");
            assert!(
                fx.leftovers().is_empty(),
                "{format:?} {phase} {nth}: {:?}",
                fx.leftovers()
            );
        }
    }
}

#[test]
fn cancelling_before_the_start_writes_nothing() {
    let fx = Fixture::new();
    let token = CancellationToken::new();
    token.cancel();
    let ctx = JobContext {
        progress: &NullProgress,
        cancel: &token,
    };
    let e = export(
        &OsSourceTree,
        &fx.inputs,
        &fx.out("never.zip"),
        &Options {
            format: Format::Zip,
            level: Level::Fast,
        },
        &ctx,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
    assert!(fx.leftovers().is_empty());
}

#[test]
fn progress_is_in_content_bytes_with_a_known_total() {
    struct Log(std::sync::Mutex<Vec<(String, u64, Option<u64>)>>);
    impl ProgressSink for Log {
        fn report(&self, e: &ProgressEvent) {
            self.0
                .lock()
                .unwrap()
                .push((e.phase.to_owned(), e.completed, e.total));
        }
    }
    let fx = Fixture::new();
    let log = Log(Default::default());
    let cancel = CancellationToken::new();
    let ctx = JobContext {
        progress: &log,
        cancel: &cancel,
    };
    let report = export(
        &OsSourceTree,
        &fx.inputs,
        &fx.out("p.tar"),
        &Options {
            format: Format::Tar,
            level: Level::Fast,
        },
        &ctx,
    )
    .unwrap();
    let events = log.0.lock().unwrap();
    let writes: Vec<_> = events.iter().filter(|e| e.0 == WRITE_PHASE).collect();
    assert!(writes.len() >= 2);
    assert!(writes.windows(2).all(|w| w[0].1 <= w[1].1), "monotonic");
    let last = writes.last().unwrap();
    assert_eq!((last.1, last.2), (report.bytes, Some(report.bytes)));
    assert!(events.iter().any(|e| e.0 == SCAN_PHASE));
}

#[test]
fn bad_requests_are_refused_before_anything_is_written() {
    let fx = Fixture::new();
    let cancel = CancellationToken::new();
    let ctx = JobContext {
        progress: &NullProgress,
        cancel: &cancel,
    };
    let opts = Options {
        format: Format::Zip,
        level: Level::Fast,
    };
    // Nothing chosen.
    let e = export(&OsSourceTree, &[], &fx.out("a.zip"), &opts, &ctx).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    // Two inputs with one name.
    let twice = vec![fx.inputs[0].clone(), fx.inputs[0].clone()];
    let e = export(&OsSourceTree, &twice, &fx.out("a.zip"), &opts, &ctx).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    // A folder that does not exist.
    let missing = fx.tmp.path().join("no-such-folder/a.zip");
    let e = export(&OsSourceTree, &fx.inputs, &missing, &opts, &ctx).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(fx.leftovers().is_empty());
}

#[test]
fn an_input_that_vanished_fails_the_job_and_leaves_nothing() {
    let fx = Fixture::new();
    fs::remove_dir_all(fx.tmp.path().join("src")).unwrap();
    let e = fx
        .run(Format::TarZst, Level::Fast, "gone.tar.zst")
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::IoError);
    assert!(fx.leftovers().is_empty());
}

#[test]
fn zip_times_are_calendar_correct_and_clamped() {
    // 2024-02-29 12:34:56 UTC is 1709210096.
    let t = zip_time(1_709_210_096).unwrap();
    assert_eq!(
        (
            t.year(),
            t.month(),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        ),
        (2024, 2, 29, 12, 34, 56)
    );
    assert!(zip_time(0).is_none(), "before 1980 has no ZIP time");
    assert!(zip_time(-5).is_none());
    assert!(zip_time(5_000_000_000).is_none(), "after 2107");
    assert_eq!(civil_from_days(0), (1970, 1, 1));
}

#[test]
fn extensions_are_the_formats_own() {
    let all: Vec<_> = Format::ALL.iter().map(|f| f.extension()).collect();
    assert_eq!(all, [".zip", ".tar", ".tar.gz", ".tar.zst"]);
    assert!(Format::Zip.has_level() && Format::TarZst.has_level() && !Format::Tar.has_level());
}
