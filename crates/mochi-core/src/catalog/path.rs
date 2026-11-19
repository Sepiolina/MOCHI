//! Authoritative archive paths (spec §10.4; plan §9, O24).
//!
//! An [`ArchivePath`] is a non-empty sequence of name components; each
//! component is raw bytes. The separator is structural: components are kept as
//! a list, and a component may never contain `/`, so the stored form (the
//! components joined by `/`) is reversible and a separator can never be
//! confused with a byte inside a name (§10.4).
//!
//! Where the bytes come from (O24, decided):
//!
//! * **POSIX** names are stored exactly as the filesystem returned them,
//!   including bytes that are not valid UTF-8.
//! * **Windows** names (UTF-16, possibly with unpaired surrogates) are stored
//!   as **WTF-8**: identical to UTF-8 for valid Unicode, and a reversible
//!   3-byte encoding for each unpaired surrogate.
//!
//! So a path added on Windows and the same path added on Ubuntu have one
//! identity whenever the name is valid Unicode, and each OS round-trips its own
//! names exactly. A name one OS cannot represent (a non-UTF-8 POSIX name on
//! Windows) is reported as unsupported at restore time (C6), never altered.
//!
//! Identity is the bytes. Display and search normalization (Unicode
//! normalization, case folding) are separate functions over the bytes and
//! never change identity (§10.4); none is defined here.

use std::fmt;

use crate::error::{ErrorCode, MochiError, Result};

/// The structural separator in the stored form. Never a name byte.
pub const SEPARATOR: u8 = b'/';

/// Draft limits (placeholders, like the §8.5 limits): bound work on hostile
/// catalogs. Linux's own limits are 255 bytes per name and 4096 per path.
pub const MAX_COMPONENT_LEN: usize = 1024;
pub const MAX_PATH_LEN: usize = 16 * 1024;
pub const MAX_DEPTH: usize = 1024;

/// Why a path or component was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathFault {
    Empty,
    EmptyComponent,
    /// `.` or `..`: never stored, so traversal cannot be encoded (§22.1).
    DotComponent,
    ContainsSeparator,
    ContainsNul,
    ComponentTooLong,
    PathTooLong,
    TooDeep,
    /// Stored bytes are not valid WTF-8 when a Windows name was required.
    NotWtf8,
}

fn fault(f: PathFault) -> MochiError {
    MochiError::new(
        ErrorCode::PathInvalid,
        format!("invalid archive path: {f:?}"),
    )
}

fn check_component(c: &[u8]) -> std::result::Result<(), PathFault> {
    if c.is_empty() {
        return Err(PathFault::EmptyComponent);
    }
    if c == b"." || c == b".." {
        return Err(PathFault::DotComponent);
    }
    if c.contains(&SEPARATOR) {
        return Err(PathFault::ContainsSeparator);
    }
    if c.contains(&0) {
        return Err(PathFault::ContainsNul);
    }
    if c.len() > MAX_COMPONENT_LEN {
        return Err(PathFault::ComponentTooLong);
    }
    Ok(())
}

/// A validated archive path. Ordering is bytewise on the stored form, which
/// sorts every directory immediately before its descendants.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArchivePath(Vec<u8>);

impl ArchivePath {
    /// Build from components, validating each.
    pub fn from_components<I, C>(components: I) -> Result<Self>
    where
        I: IntoIterator<Item = C>,
        C: AsRef<[u8]>,
    {
        let mut out = Vec::new();
        let mut depth = 0usize;
        for c in components {
            let c = c.as_ref();
            check_component(c).map_err(fault)?;
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(fault(PathFault::TooDeep));
            }
            if !out.is_empty() {
                out.push(SEPARATOR);
            }
            out.extend_from_slice(c);
            if out.len() > MAX_PATH_LEN {
                return Err(fault(PathFault::PathTooLong));
            }
        }
        if out.is_empty() {
            return Err(fault(PathFault::Empty));
        }
        Ok(ArchivePath(out))
    }

    /// Parse the stored form (components joined by `/`). Untrusted input from
    /// a catalog: every rule is re-checked.
    pub fn from_stored(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Err(fault(PathFault::Empty));
        }
        if bytes.len() > MAX_PATH_LEN {
            return Err(fault(PathFault::PathTooLong));
        }
        Self::from_components(bytes.split(|b| *b == SEPARATOR))
    }

    /// Build from Windows (UTF-16) components, encoding each as WTF-8.
    pub fn from_utf16_components<I, C>(components: I) -> Result<Self>
    where
        I: IntoIterator<Item = C>,
        C: AsRef<[u16]>,
    {
        let encoded: Vec<Vec<u8>> = components
            .into_iter()
            .map(|c| wtf8_from_utf16(c.as_ref()))
            .collect();
        Self::from_components(encoded)
    }

    pub fn as_stored(&self) -> &[u8] {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.0.split(|b| *b == SEPARATOR)
    }

    /// Components decoded for Windows, or `None` if any component is not
    /// WTF-8 (for example a POSIX name with arbitrary bytes): the caller must
    /// report it as an unsupported name, not substitute characters.
    pub fn to_utf16_components(&self) -> Option<Vec<Vec<u16>>> {
        self.components().map(utf16_from_wtf8).collect()
    }

    /// The parent directory, or `None` for a top-level entry.
    pub fn parent(&self) -> Option<ArchivePath> {
        let i = self.0.iter().rposition(|b| *b == SEPARATOR)?;
        Some(ArchivePath(self.0[..i].to_vec()))
    }

    /// True if `self` is a strict descendant of `dir`.
    pub fn is_descendant_of(&self, dir: &ArchivePath) -> bool {
        self.0.len() > dir.0.len() && self.0.starts_with(&dir.0) && self.0[dir.0.len()] == SEPARATOR
    }

    pub fn depth(&self) -> usize {
        self.components().count()
    }
}

/// Lets ordered maps keyed by paths be range-queried by stored bytes.
/// Consistent with `Ord`/`Eq`/`Hash`, which are all over the stored bytes.
impl std::borrow::Borrow<[u8]> for ArchivePath {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for ArchivePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Escaped, never raw: archive names are untrusted (§23.3 #9).
        write!(f, "ArchivePath(\"{}\")", self.0.escape_ascii())
    }
}

// ---- WTF-8 -------------------------------------------------------------------
//
// WTF-8 ("wobbly transformation format"): UTF-8 generalized to encode every
// code point 0..=0x10FFFF including surrogates, with the one restriction that a
// high surrogate followed by a low surrogate is encoded as the supplementary
// code point they form, never as two 3-byte sequences. That restriction is
// what makes the encoding a bijection with sequences of UTF-16 code units.

/// Encode UTF-16 code units (possibly ill-formed) as WTF-8.
pub fn wtf8_from_utf16(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len() * 3);
    let mut i = 0;
    while i < units.len() {
        let u = u32::from(units[i]);
        let cp = if (0xD800..0xDC00).contains(&u) {
            match units.get(i + 1).map(|n| u32::from(*n)) {
                Some(l) if (0xDC00..0xE000).contains(&l) => {
                    i += 1;
                    0x10000 + ((u - 0xD800) << 10) + (l - 0xDC00)
                }
                _ => u, // unpaired high surrogate
            }
        } else {
            u // BMP scalar or unpaired low surrogate
        };
        push_code_point(&mut out, cp);
        i += 1;
    }
    out
}

fn push_code_point(out: &mut Vec<u8>, cp: u32) {
    // Truncating casts are intended: each is masked to its byte first.
    match cp {
        0..=0x7F => out.push(cp as u8),
        0x80..=0x7FF => {
            out.push(0xC0 | (cp >> 6) as u8);
            out.push(0x80 | (cp & 0x3F) as u8);
        }
        0x800..=0xFFFF => {
            out.push(0xE0 | (cp >> 12) as u8);
            out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
            out.push(0x80 | (cp & 0x3F) as u8);
        }
        _ => {
            out.push(0xF0 | (cp >> 18) as u8);
            out.push(0x80 | ((cp >> 12) & 0x3F) as u8);
            out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
            out.push(0x80 | (cp & 0x3F) as u8);
        }
    }
}

/// Decode WTF-8 to UTF-16 code units. `None` if `bytes` is not well-formed
/// WTF-8: overlong forms, out-of-range values, truncated sequences, and an
/// encoded surrogate *pair* (which WTF-8 forbids) are all rejected.
pub fn utf16_from_wtf8(bytes: &[u8]) -> Option<Vec<u16>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut prev_high = false;
    while i < bytes.len() {
        let b0 = u32::from(bytes[i]);
        let (cp, len) = if b0 < 0x80 {
            (b0, 1)
        } else if (0xC2..0xE0).contains(&b0) {
            (((b0 & 0x1F) << 6) | cont(bytes, i + 1)?, 2)
        } else if (0xE0..0xF0).contains(&b0) {
            let cp = ((b0 & 0x0F) << 12) | (cont(bytes, i + 1)? << 6) | cont(bytes, i + 2)?;
            if cp < 0x800 {
                return None; // overlong
            }
            (cp, 3)
        } else if (0xF0..0xF5).contains(&b0) {
            let cp = ((b0 & 0x07) << 18)
                | (cont(bytes, i + 1)? << 12)
                | (cont(bytes, i + 2)? << 6)
                | cont(bytes, i + 3)?;
            if !(0x10000..=0x10FFFF).contains(&cp) {
                return None;
            }
            (cp, 4)
        } else {
            return None;
        };
        let is_high = (0xD800..0xDC00).contains(&cp);
        let is_low = (0xDC00..0xE000).contains(&cp);
        if is_low && prev_high {
            return None; // a pair must be encoded as one supplementary code point
        }
        prev_high = is_high;
        if cp >= 0x10000 {
            let v = cp - 0x10000;
            out.push(0xD800 | (v >> 10) as u16);
            out.push(0xDC00 | (v & 0x3FF) as u16);
        } else {
            out.push(cp as u16);
        }
        i += len;
    }
    Some(out)
}

fn cont(bytes: &[u8], i: usize) -> Option<u32> {
    let b = *bytes.get(i)?;
    (b & 0xC0 == 0x80).then_some(u32::from(b & 0x3F))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ArchivePath {
        ArchivePath::from_stored(s.as_bytes()).unwrap()
    }

    #[test]
    fn rejects_every_structural_hazard() {
        let bad: &[(&[u8], PathFault)] = &[
            (b"", PathFault::Empty),
            (b"a//b", PathFault::EmptyComponent),
            (b"/abs", PathFault::EmptyComponent),
            (b"trailing/", PathFault::EmptyComponent),
            (b"a/../b", PathFault::DotComponent),
            (b"./a", PathFault::DotComponent),
            (b"a\0b", PathFault::ContainsNul),
        ];
        for (bytes, _why) in bad {
            let e = ArchivePath::from_stored(bytes).unwrap_err();
            assert_eq!(e.code, ErrorCode::PathInvalid, "{:?}", bytes.escape_ascii());
        }
        let e = ArchivePath::from_components([b"a/b".as_slice()]).unwrap_err();
        assert_eq!(e.code, ErrorCode::PathInvalid);
        let long = vec![b'x'; MAX_COMPONENT_LEN + 1];
        assert!(ArchivePath::from_components([long]).is_err());
        let deep = vec![b"d".as_slice(); MAX_DEPTH + 1];
        assert!(ArchivePath::from_components(deep).is_err());
    }

    #[test]
    fn posix_bytes_are_preserved_exactly() {
        // Not UTF-8, contains a backslash: both legal POSIX name bytes.
        let raw: &[u8] = &[0xFF, b'\\', 0x80, b'x'];
        let path = ArchivePath::from_components([b"dir".as_slice(), raw]).unwrap();
        let back: Vec<&[u8]> = path.components().collect();
        assert_eq!(back, vec![b"dir".as_slice(), raw]);
        // And it is not representable on Windows: reported, never altered.
        assert!(path.to_utf16_components().is_none());
    }

    #[test]
    fn parent_and_descendant() {
        assert_eq!(p("a/b/c").parent(), Some(p("a/b")));
        assert_eq!(p("a").parent(), None);
        assert!(p("a/b").is_descendant_of(&p("a")));
        assert!(!p("ab").is_descendant_of(&p("a")), "prefix is not ancestry");
        assert!(!p("a").is_descendant_of(&p("a")));
    }

    #[test]
    fn order_puts_directories_before_descendants() {
        let mut v = [p("a/b"), p("a.txt"), p("a"), p("a/b/c")];
        v.sort();
        assert_eq!(v[0], p("a"));
        assert!(v.iter().position(|x| *x == p("a/b")) < v.iter().position(|x| *x == p("a/b/c")));
    }

    #[test]
    fn wtf8_equals_utf8_for_valid_unicode() {
        for s in ["plain", "naïve", "日本語", "emoji 😀 pair", ""] {
            let units: Vec<u16> = s.encode_utf16().collect();
            assert_eq!(wtf8_from_utf16(&units), s.as_bytes(), "{s}");
            assert_eq!(utf16_from_wtf8(s.as_bytes()), Some(units));
        }
    }

    #[test]
    fn unpaired_surrogates_round_trip() {
        let cases: &[&[u16]] = &[
            &[0xD800],                 // lone high
            &[0xDC00],                 // lone low
            &[b'a' as u16, 0xDBFF],    // high at end
            &[0xDC00, 0xD800],         // low then high: two unpaired
            &[0xD800, b'x' as u16],    // high then non-low
            &[0xD83D, 0xDE00, 0xD800], // valid pair then lone high
        ];
        for units in cases {
            let w = wtf8_from_utf16(units);
            assert!(
                std::str::from_utf8(&w).is_err()
                    || !units.iter().any(|u| (0xD800..0xE000).contains(u))
            );
            assert_eq!(utf16_from_wtf8(&w).as_deref(), Some(*units), "{units:x?}");
        }
    }

    #[test]
    fn exhaustive_single_unit_round_trip() {
        for u in 0..=u16::MAX {
            let w = wtf8_from_utf16(&[u]);
            assert_eq!(utf16_from_wtf8(&w), Some(vec![u]), "{u:#x}");
        }
    }

    #[test]
    fn malformed_wtf8_is_rejected() {
        let bad: &[&[u8]] = &[
            &[0xC0, 0x80],             // overlong NUL
            &[0xE0, 0x80, 0x80],       // overlong
            &[0xED, 0xA0],             // truncated
            &[0xF4, 0x90, 0x80, 0x80], // above U+10FFFF
            &[0x80],                   // stray continuation
            // Surrogate pair encoded as two 3-byte sequences: forbidden.
            &[0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80],
        ];
        for b in bad {
            assert_eq!(utf16_from_wtf8(b), None, "{b:x?}");
        }
    }

    #[test]
    fn windows_components_become_identity_equal_to_utf8() {
        let win = ArchivePath::from_utf16_components([
            "Docs".encode_utf16().collect::<Vec<_>>(),
            "résumé.txt".encode_utf16().collect::<Vec<_>>(),
        ])
        .unwrap();
        assert_eq!(win, p("Docs/résumé.txt"));
    }
}
