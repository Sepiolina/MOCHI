//! Rendering archive-supplied strings for people (spec §23.3 #9: archive
//! strings are untrusted data). One rule shared by the CLI and the desktop
//! app, so that a name reads the same in both.
//!
//! A name could carry terminal control sequences (`ESC [ … m`, OSC 8
//! hyperlinks, a carriage return that overwrites a line) or bidirectional
//! overrides that make one name look like another. [`escape`] therefore
//! escapes every control character, every bidi control, the backslash, and
//! every byte that is not UTF-8, so the result is exactly one line and reads
//! back to the stored bytes. It is not markup escaping: a UI renders the
//! result as text (never as HTML), and markup characters stay as they are.

fn escaped_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}')
        || ('\u{202A}'..='\u{202E}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}

/// One inert line for archive bytes: `\\` for a backslash, `\u{…}` for a
/// control or bidi character, `\xNN` for a byte that is not UTF-8.
pub fn escape(bytes: &[u8]) -> String {
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

/// `true` if [`escape`] returns the bytes unchanged (they are UTF-8 and need
/// no escape), so a UI can say when what it shows is not the stored name.
pub fn is_verbatim(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|s| !s.chars().any(|c| c == '\\' || escaped_char(c)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_names_become_one_inert_line() {
        assert_eq!(escape(b"plain.txt"), "plain.txt");
        assert_eq!(
            escape(b"<img src=x onerror=alert(1)>"),
            "<img src=x onerror=alert(1)>"
        );
        assert_eq!(escape(b"a\x1b[31mred"), "a\\u{1b}[31mred");
        assert_eq!(escape(b"line\r\nnext"), "line\\u{d}\\u{a}next");
        assert_eq!(
            escape("evil\u{202E}txt.exe".as_bytes()),
            "evil\\u{202e}txt.exe"
        );
        assert_eq!(escape(b"bad\xff\xfe"), "bad\\xff\\xfe");
        assert_eq!(escape(b"back\\slash"), "back\\\\slash");
        assert!(is_verbatim("caf\u{e9}".as_bytes()));
        for hostile in [
            b"a\x1bb".as_slice(),
            b"bad\xff",
            b"back\\slash",
            "x\u{2066}y".as_bytes(),
        ] {
            assert!(!is_verbatim(hostile), "{hostile:?}");
            assert!(!escape(hostile).contains('\n'));
        }
    }
}
