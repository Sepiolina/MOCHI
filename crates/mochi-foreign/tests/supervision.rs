//! The parent survives the helper: a panic, abort, memory-limit breach, hang,
//! flood, or protocol violation each becomes a typed, reported error, and the
//! next operation works (plan D8 rule 3). Faults come from the `test-faults`
//! hooks in the helper, selected by an archive path of the form `fault:<kind>`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
use std::time::{Duration, Instant};

use common::*;
use mochi_foreign::*;

fn fault(kind: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("fault:{kind}"))
}

fn assert_survived(c: &ForeignClient) {
    let l = c
        .list(&fixture("basic.tar"), None)
        .expect("the parent still works after a helper fault");
    assert_eq!(l.entries.len(), 3);
}

#[test]
fn a_helper_panic_is_reported_as_helper_panic() {
    let c = client();
    let e = c.list(&fault("panic"), None).unwrap_err();
    assert_eq!(e.code, ErrorCode::HelperPanic);
    assert!(e.message.contains("injected panic"));
    assert_survived(&c);
}

#[cfg(unix)]
#[test]
fn a_helper_abort_is_reported_as_a_crash_with_the_signal() {
    let c = client();
    let e = c.test(&fault("abort"), None).unwrap_err();
    assert_eq!(e.code, ErrorCode::HelperCrashed);
    assert!(e.message.contains("SIGABRT"), "{}", e.message);
    assert_survived(&c);
}

#[cfg(any(unix, windows))]
#[test]
fn a_memory_limit_breach_is_a_reported_crash_not_a_dead_parent() {
    // The helper tries to allocate 2 GiB under a 256 MiB address-space limit.
    let c = client().with_run_limits(RunLimits {
        memory_bytes: Some(256 << 20),
        ..RunLimits::default()
    });
    let e = c.list(&fault("alloc"), None).unwrap_err();
    assert_eq!(e.code, ErrorCode::HelperCrashed);
    assert!(e.message.contains("memory-limit"), "{}", e.message);
    assert_survived(&c);
}

#[test]
fn a_hung_helper_is_killed_at_the_wall_time_limit() {
    let c = client().with_run_limits(RunLimits {
        wall_time: Duration::from_millis(600),
        ..RunLimits::default()
    });
    let t0 = Instant::now();
    let e = c.list(&fault("hang"), None).unwrap_err();
    assert_eq!(e.code, ErrorCode::HelperTimeout);
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "took {:?}",
        t0.elapsed()
    );
    assert_survived(&client());
}

#[test]
fn a_flooding_helper_is_cut_off_at_the_output_limit() {
    let c = client().with_run_limits(RunLimits {
        max_metadata_bytes: 1 << 20,
        wall_time: Duration::from_secs(60),
        ..RunLimits::default()
    });
    let t0 = Instant::now();
    let e = c.list(&fault("flood"), None).unwrap_err();
    assert_eq!(e.code, ErrorCode::HelperOutputLimit, "{e}");
    assert!(
        t0.elapsed() < Duration::from_secs(30),
        "took {:?}",
        t0.elapsed()
    );
    assert_survived(&c);
}

#[test]
fn a_helper_that_stops_without_finishing_is_a_protocol_error() {
    let e = client().list(&fault("silent_exit"), None).unwrap_err();
    assert_eq!(e.code, ErrorCode::HelperProtocol);
}

#[test]
fn malformed_helper_output_is_a_protocol_error() {
    let c = client();
    assert_eq!(
        c.list(&fault("bad_kind"), None).unwrap_err().code,
        ErrorCode::HelperProtocol
    );
    assert_eq!(
        c.list(&fault("huge_frame"), None).unwrap_err().code,
        ErrorCode::HelperProtocol
    );
    assert_survived(&c);
}

#[test]
fn a_missing_helper_binary_is_a_spawn_error() {
    let e = ForeignClient::new("/nonexistent/mochi-foreign-helper")
        .list(&fixture("basic.tar"), None)
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::HelperSpawnFailed);
}

#[test]
fn a_byte_sink_failure_stops_the_read_and_is_reported() {
    let e = client()
        .read_entry(&fixture("basic_infozip.zip"), 2, None, &mut |_| {
            Err(std::io::Error::other("disk full"))
        })
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::SinkFailed);
    assert_survived(&client());
}

#[cfg(unix)]
mod fake_helpers {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let p = dir.path().join("fake-helper.sh");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// Runs `op`, starting it again while the fresh script is "executable file
    /// busy". On Linux, another test thread that forks while `script` still
    /// holds its write descriptor carries that descriptor until it execs, and
    /// exec of the script fails with ETXTBSY meanwhile (rust-lang/rust#114554).
    /// The window is a few microseconds; nothing else is retried.
    fn unless_busy<T>(
        mut op: impl FnMut() -> Result<T, mochi_foreign::ForeignError>,
    ) -> Result<T, mochi_foreign::ForeignError> {
        for _ in 0..50 {
            match op() {
                Err(e) if e.code == ErrorCode::HelperSpawnFailed && e.message.contains("busy") => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                r => return r,
            }
        }
        op()
    }

    #[test]
    fn a_helper_speaking_another_protocol_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // Hello frame: len 7, kind 0x80, version 99, empty id.
        let h = script(
            &dir,
            r"printf '\007\000\000\000\200\143\000\000\000\000\000'; sleep 1",
        );
        let e =
            unless_busy(|| ForeignClient::new(&h).list(&fixture("basic.tar"), None)).unwrap_err();
        assert_eq!(e.code, ErrorCode::HelperProtocol);
        assert!(e.message.contains("protocol 99"), "{}", e.message);
    }

    #[test]
    fn the_password_is_never_a_command_line_argument() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("argv.txt");
        let h = script(&dir, &format!("echo \"$#:$*\" > {}\nexit 3", out.display()));
        let pw = Password::new(b"hunter2-very-secret".to_vec());
        let e = unless_busy(|| ForeignClient::new(&h).list(&fixture("zipcrypto.zip"), Some(&pw)))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::HelperCrashed);
        let argv = std::fs::read_to_string(out).unwrap();
        assert_eq!(
            argv.trim(),
            "0:",
            "the helper is started with no arguments at all"
        );
        assert!(!e.message.contains("hunter2"));
    }
}

#[test]
fn password_debug_and_errors_never_show_the_secret() {
    let pw = Password::new(b"hunter2-very-secret".to_vec());
    assert!(!format!("{pw:?}").contains("hunter2"));
    let e = client()
        .test(&fixture("zipcrypto.zip"), Some(&pw))
        .map(|t| format!("{t:?}"))
        .unwrap_or_else(|e| e.to_string());
    assert!(!e.contains("hunter2"));
}

#[test]
fn cancelling_stops_the_helper_and_reports_cancelled() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let flag = Arc::new(AtomicBool::new(false));
    let c = client()
        .with_cancel(Arc::clone(&flag))
        .with_run_limits(RunLimits {
            wall_time: Duration::from_secs(120),
            ..RunLimits::default()
        });
    let f2 = Arc::clone(&flag);
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        f2.store(true, Ordering::SeqCst);
    });
    let t0 = Instant::now();
    let e = c.list(&fault("hang"), None).unwrap_err();
    t.join().unwrap();
    assert_eq!(e.code, ErrorCode::Cancelled);
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "took {:?}",
        t0.elapsed()
    );
    assert_survived(&client());
}

#[test]
fn progress_counts_finished_entries() {
    let c = client();
    let mut last = 0;
    let t = c
        .test_with_progress(&fixture("basic_infozip.zip"), None, &mut |n| last = n)
        .unwrap();
    assert_eq!(last, t.entries.len() as u64);
}
