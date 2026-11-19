//! Rendering archive-supplied strings (spec §23.3 #9: archive strings are
//! untrusted data).
//!
//! In text output an entry name could carry terminal control sequences
//! (`ESC [ … m`, OSC 8 hyperlinks, a carriage return that overwrites a line)
//! or bidirectional overrides that make one name look like another. Text
//! output therefore escapes every control character, every bidi control,
//! the backslash, and every byte that is not UTF-8, so what is printed is
//! exactly one line and reads back to the stored bytes. JSON output carries
//! the name as a JSON string (JSON escapes control characters itself) and,
//! when the bytes are not UTF-8, adds the exact bytes in hex.

use serde_json::{json, Value};

/// One terminal-safe line for archive bytes. The rule lives in
/// [`mochi_core::text::escape`], shared with the desktop app.
pub fn text(bytes: &[u8]) -> String {
    mochi_core::text::escape(bytes)
}

/// A path for JSON: `{"path": …}`, plus `"path_hex"` with the exact bytes
/// when they are not UTF-8 (then `path` is a lossy rendering).
pub fn path_fields(bytes: &[u8]) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    match std::str::from_utf8(bytes) {
        Ok(s) => {
            m.insert("path".into(), json!(s));
        }
        Err(_) => {
            m.insert("path".into(), json!(String::from_utf8_lossy(bytes)));
            m.insert("path_hex".into(), json!(hex(bytes)));
        }
    }
    m
}

/// One terminal-safe line for a path on this computer. A file name found on
/// disk is not archive-supplied, but it is not the user's typing either (an
/// archive may have been downloaded under any name), so it is escaped by the
/// same rule. Unix: the exact bytes; Windows: the name as text, unpaired
/// surrogates replaced.
pub fn os_path_text(path: &std::path::Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        text(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        text(path.to_string_lossy().as_bytes())
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse an archive path given on the command line: `/`-separated
/// components, taken as the argument's bytes (WTF-8 on Windows, O24).
pub fn archive_path_arg(s: &str) -> mochi_core::Result<mochi_core::catalog::path::ArchivePath> {
    let trimmed = s.trim_end_matches('/');
    mochi_core::catalog::path::ArchivePath::from_stored(trimmed.as_bytes()).map_err(|e| {
        mochi_core::MochiError::new(
            mochi_core::ErrorCode::InvalidArgument,
            format!("{s:?} is not an archive path: {}", e.message),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_names_print_as_one_inert_line() {
        assert_eq!(text(b"plain.txt"), "plain.txt");
        assert_eq!(text("caf\u{e9}".as_bytes()), "caf\u{e9}");
        assert_eq!(text(b"\x1b[31mred"), "\\u{1b}[31mred");
        assert_eq!(text(b"a\rb\nc"), "a\\u{d}b\\u{a}c");
        assert_eq!(text("x\u{202E}txt.exe".as_bytes()), "x\\u{202e}txt.exe");
        assert_eq!(text(b"\xff\xfe"), "\\xff\\xfe");
        assert_eq!(text(b"back\\slash"), "back\\\\slash");
        assert_eq!(
            text(b"<img src=x onerror=alert(1)>"),
            "<img src=x onerror=alert(1)>"
        );
    }

    #[test]
    fn local_paths_are_escaped_too() {
        let p = std::path::Path::new("dir/evil\u{1b}[2J.mochi");
        assert_eq!(os_path_text(p), "dir/evil\\u{1b}[2J.mochi");
    }

    #[test]
    fn json_paths_keep_exact_bytes() {
        let m = path_fields(b"ok");
        assert_eq!(m["path"], "ok");
        assert!(!m.contains_key("path_hex"));
        let m = path_fields(b"bad\xff");
        assert_eq!(m["path_hex"], "626164ff");
    }
}
