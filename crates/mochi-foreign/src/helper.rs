//! The helper process body: speaks the protocol on stdin/stdout, parses the
//! archive named in the request, and never writes a file.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::engine::{self, EngineError, Origin, Out};
use crate::protocol::{
    kind, path_from_bytes, read_frame, FrameError, Message, Request, PROTOCOL_VERSION,
};
use crate::types::*;
use crate::volumes::{self, SplitName};

/// Exit codes the parent can see (it reports them in `HELPER_CRASHED`).
pub const EXIT_OK: i32 = 0;
pub const EXIT_IO: i32 = 1;
pub const EXIT_BAD_REQUEST: i32 = 2;
pub const EXIT_PANIC: i32 = 70;

pub fn helper_id() -> String {
    format!("mochi-foreign-helper {}", env!("CARGO_PKG_VERSION"))
}

/// Applies the limits the request asks for to this process (Unix). Failure to
/// apply a requested limit is not silent: the helper refuses to run.
fn apply_process_limits(req: &Request) -> Result<(), String> {
    #[cfg(unix)]
    {
        use rustix::process::{setrlimit, Resource, Rlimit};
        let set = |res: Resource, v: u64| {
            setrlimit(
                res,
                Rlimit {
                    current: Some(v),
                    maximum: Some(v),
                },
            )
        };
        // No core dumps: a dump of this process would hold the password.
        set(Resource::Core, 0).map_err(|e| format!("cannot disable core dumps: {e}"))?;
        if let Some(m) = req.memory_limit {
            set(Resource::As, m).map_err(|e| format!("cannot apply the memory limit: {e}"))?;
        }
        if let Some(s) = req.cpu_seconds {
            set(Resource::Cpu, s).map_err(|e| format!("cannot apply the CPU limit: {e}"))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = req;
    }
    Ok(())
}

// Test-only fault hooks; the workspace's no-panic lint is for paths reachable
// from archive bytes, and this one is reachable only with the feature on.
#[cfg(feature = "test-faults")]
#[allow(clippy::panic)]
fn inject_fault(path: &[u8], out: &mut Out) -> Option<Result<(), EngineError>> {
    use std::time::Duration;
    // The path is platform bytes (UTF-16LE on Windows), so decode it first.
    let decoded = path_from_bytes(path);
    let name = decoded.to_str()?.strip_prefix("fault:")?.as_bytes();
    Some(match name {
        b"panic" => panic!("injected panic"),
        b"abort" => std::process::abort(),
        b"alloc" => {
            let v = vec![1u8; 2 << 30];
            std::hint::black_box(&v);
            Ok(())
        }
        b"hang" => loop {
            std::thread::sleep(Duration::from_secs(1));
        },
        b"flood" => {
            if let Err(e) = out(Message::Format(ForeignFormat::Zip)) {
                return Some(Err(EngineError::Sink(e)));
            }
            let mut index = 0u64;
            loop {
                let info = EntryInfo {
                    index,
                    name: vec![b'a'; 60_000],
                    kind: EntryKind::File,
                    size: Some(0),
                    compressed_size: None,
                    encryption: Encryption::None,
                    method: String::new(),
                    link_target: None,
                    unix_mode: None,
                    mtime_unix: None,
                    name_is_utf8_flag: None,
                    error: None,
                };
                index += 1;
                if let Err(e) = out(Message::Entry(info)) {
                    return Some(Err(EngineError::Sink(e)));
                }
            }
        }
        b"silent_exit" => Ok(()),
        b"bad_kind" => {
            let _ = crate::protocol::write_frame(&mut std::io::stdout(), 0x7e, b"x");
            Ok(())
        }
        b"huge_frame" => {
            let _ = std::io::stdout().write_all(&[0xff, 0xff, 0xff, 0x7f, 0x84]);
            let _ = std::io::stdout().flush();
            Ok(())
        }
        _ => return None,
    })
}

fn process(req: &Request, out: &mut Out) -> Result<(), EngineError> {
    #[cfg(feature = "test-faults")]
    if let Some(r) = inject_fault(&req.archive_path, out) {
        return r;
    }
    let path = path_from_bytes(&req.archive_path);
    let open_failed = |e: std::io::Error| {
        EngineError::Archive(ForeignError::new(
            ErrorCode::OpenFailed,
            format!("cannot open the archive: {}", e.kind()),
        ))
    };
    match volumes::classify(&path) {
        SplitName::Later(n) => Err(EngineError::Archive(ForeignError::new(
            ErrorCode::NotAnArchive,
            format!("this is volume {n} of a split 7z set; open the first volume (.7z.001)"),
        ))),
        SplitName::First => {
            // Plain byte splits of one archive: read as one stream.
            let found = volumes::find_volumes(&path);
            let set = volumes::MultiFile::open(&found).map_err(open_failed)?;
            let origin = Origin {
                path: Some(&path),
                split: SplitName::First,
                volumes: set.volume_count(),
            };
            engine::run(set, &origin, req, out)
        }
        SplitName::NotSplit => {
            let file = File::open(&path).map_err(open_failed)?;
            let origin = Origin {
                path: Some(&path),
                split: SplitName::NotSplit,
                volumes: 1,
            };
            engine::run(BufReader::new(file), &origin, req, out)
        }
    }
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    let s = p
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    crate::engine::truncate_text(s)
}

/// Runs one request/response exchange. Returns the process exit code.
pub fn serve<R: Read, W: Write>(mut input: R, output: W) -> i32 {
    let mut out = BufWriter::with_capacity(64 * 1024, output);
    let hello = Message::Hello {
        version: PROTOCOL_VERSION,
        id: helper_id(),
    };
    if hello.write_to(&mut out).and_then(|()| out.flush()).is_err() {
        return EXIT_IO;
    }
    let send_error = |out: &mut BufWriter<W>, code: ErrorCode, message: String| {
        let _ = Message::Error { code, message }.write_to(out);
        let _ = out.flush();
    };
    let req = match read_frame(&mut input) {
        Ok(Some((kind::REQUEST, body))) => match Request::decode(&body) {
            Ok(r) => r,
            Err(e) => {
                send_error(&mut out, ErrorCode::BadRequest, e.to_string());
                return EXIT_BAD_REQUEST;
            }
        },
        Ok(_) => {
            send_error(
                &mut out,
                ErrorCode::BadRequest,
                "expected a request frame".to_string(),
            );
            return EXIT_BAD_REQUEST;
        }
        Err(FrameError::Protocol(e)) => {
            send_error(&mut out, ErrorCode::BadRequest, e.to_string());
            return EXIT_BAD_REQUEST;
        }
        Err(FrameError::Io(_)) => return EXIT_IO,
    };
    if let Err(e) = apply_process_limits(&req) {
        send_error(&mut out, ErrorCode::BadRequest, e);
        return EXIT_BAD_REQUEST;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut sink = |m: Message| m.write_to(&mut out);
        process(&req, &mut sink)
    }));
    match result {
        Ok(Ok(())) => {
            if out.flush().is_err() {
                return EXIT_IO;
            }
            EXIT_OK
        }
        Ok(Err(EngineError::Archive(e))) => {
            send_error(&mut out, e.code, e.message);
            EXIT_OK
        }
        Ok(Err(EngineError::Sink(_))) => EXIT_IO,
        Err(p) => {
            send_error(&mut out, ErrorCode::HelperPanic, panic_text(p.as_ref()));
            EXIT_PANIC
        }
    }
}
