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

fn escaped_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}')
        || ('\u{202A}'..='\u{202E}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}

/// One terminal-safe line for archive bytes.
pub fn text(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c == '\\' {
                out.push_str("\\\\");
            } else if escaped_char(c) {
                out.push_str(&format!("\\u{{{:x}}}", u32::from(c)));
            } else {
                out.push(c);
            }
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
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
    fn json_paths_keep_exact_bytes() {
        let m = path_fields(b"ok");
        assert_eq!(m["path"], "ok");
        assert!(!m.contains_key("path_hex"));
        let m = path_fields(b"bad\xff");
        assert_eq!(m["path_hex"], "626164ff");
    }
}
