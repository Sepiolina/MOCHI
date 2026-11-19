//! The formats a new archive can have (plan O31) and the names that go with
//! them. Pure functions; nothing here touches the file system.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::ipc::{ArchiveFormat, CompressionPreset};

/// Every extension that names an archive this app writes or opens by name,
/// longest first so `.tar.gz` is stripped as one piece.
const KNOWN: [&str; 7] = [
    ".tar.zst", ".tar.gz", ".mochi", ".tzst", ".tgz", ".zip", ".tar",
];

impl ArchiveFormat {
    /// The file extension, with its dot.
    pub fn extension(self) -> &'static str {
        match self {
            ArchiveFormat::Mochi => ".mochi",
            ArchiveFormat::Zip => ".zip",
            ArchiveFormat::Tar => ".tar",
            ArchiveFormat::TarGz => ".tar.gz",
            ArchiveFormat::TarZst => ".tar.zst",
        }
    }

    /// The `mochi-export` format; `None` for `.mochi`, which `mochi-core` writes.
    pub fn export(self) -> Option<mochi_export::Format> {
        match self {
            ArchiveFormat::Mochi => None,
            ArchiveFormat::Zip => Some(mochi_export::Format::Zip),
            ArchiveFormat::Tar => Some(mochi_export::Format::Tar),
            ArchiveFormat::TarGz => Some(mochi_export::Format::TarGz),
            ArchiveFormat::TarZst => Some(mochi_export::Format::TarZst),
        }
    }

    /// `name` with this format's extension: an archive extension it already
    /// has is kept, another archive extension is replaced, and a name with
    /// none (or with a dot that is not an archive extension) gets it added.
    pub fn name_for(self, name: &str) -> String {
        let lower = name.to_ascii_lowercase();
        let ext = self.extension();
        if lower.ends_with(ext) && name.len() > ext.len() {
            return name.to_owned();
        }
        format!("{}{ext}", strip_archive_extension(name))
    }

    /// [`ArchiveFormat::name_for`] on a path's last component (also when it is
    /// not valid Unicode: the extension is appended to the raw name).
    pub fn path_for(self, path: &Path) -> PathBuf {
        let Some(name) = path.file_name() else {
            return path.to_path_buf();
        };
        match name.to_str() {
            Some(text) => path.with_file_name(self.name_for(text)),
            None => {
                let mut raw: OsString = name.to_owned();
                raw.push(self.extension());
                path.with_file_name(raw)
            }
        }
    }
}

/// `name` without a trailing archive extension (any of [`KNOWN`], any case).
pub fn strip_archive_extension(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    for ext in KNOWN {
        if lower.ends_with(ext) && name.len() > ext.len() {
            // ASCII extensions: the byte offset is a character boundary.
            return &name[..name.len() - ext.len()];
        }
    }
    name
}

impl ArchiveFormat {
    /// Whether the format offers `preset`: `.mochi` offers all seven, ZIP and
    /// tar.gz only Fast, Balanced and Maximum (Deflate has no more levels
    /// that differ), tar.zst all seven, and plain tar ignores the level.
    /// The window shows the same lists (`ui/src/write/format.ts`).
    pub fn offers(self, preset: CompressionPreset) -> bool {
        match self.export() {
            None => true,
            Some(f) => !f.has_level() || f.levels().contains(&preset.export_level()),
        }
    }
}

impl CompressionPreset {
    /// The `mochi-export` level.
    pub fn export_level(self) -> mochi_export::Level {
        match self {
            CompressionPreset::Turbo => mochi_export::Level::Turbo,
            CompressionPreset::VeryFast => mochi_export::Level::VeryFast,
            CompressionPreset::Fast => mochi_export::Level::Fast,
            CompressionPreset::Balanced => mochi_export::Level::Balanced,
            CompressionPreset::Maximum => mochi_export::Level::Maximum,
            CompressionPreset::Ultra => mochi_export::Level::Ultra,
            CompressionPreset::Extreme => mochi_export::Level::Extreme,
        }
    }
}

/// A name the user typed for a new archive: one component, no separators, no
/// control characters, not a dot name. The archive is made in a folder Rust
/// chose, so the webview never names a path.
pub fn check_archive_name(name: &str) -> Result<&str, &'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Err("type a name for the archive");
    }
    if name.len() > 200 {
        return Err("that name is too long");
    }
    if name == "." || name == ".." || name.ends_with('.') || name.ends_with(' ') {
        return Err("that name cannot be used for a file");
    }
    if name.chars().any(|c| {
        c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    }) {
        return Err("a file name cannot contain / \\ : * ? \" < > | or control characters");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_follows_the_format() {
        use ArchiveFormat::*;
        assert_eq!(Zip.name_for("Photos"), "Photos.zip");
        assert_eq!(Zip.name_for("Photos.mochi"), "Photos.zip");
        assert_eq!(Mochi.name_for("Photos.ZIP"), "Photos.mochi");
        assert_eq!(TarGz.name_for("Photos.tar.zst"), "Photos.tar.gz");
        assert_eq!(TarZst.name_for("Photos.tar"), "Photos.tar.zst");
        assert_eq!(Tar.name_for("Photos.tar.gz"), "Photos.tar");
        assert_eq!(Tar.name_for("Photos.tar"), "Photos.tar");
        assert_eq!(
            TarGz.name_for("a.tar.GZ"),
            "a.tar.GZ",
            "an own extension is kept as typed"
        );
        // A dot that is not an archive extension is part of the name.
        assert_eq!(Zip.name_for("report v2.1"), "report v2.1.zip");
        // A name that is only an extension is a name.
        assert_eq!(Zip.name_for(".zip"), ".zip.zip");
        assert_eq!(Zip.name_for("Grüße ☃"), "Grüße ☃.zip");
    }

    #[test]
    fn paths_are_renamed_in_their_folder_only() {
        let p = ArchiveFormat::Zip.path_for(Path::new("/a/b.c/x.mochi"));
        assert_eq!(p, PathBuf::from("/a/b.c/x.zip"));
    }

    #[test]
    fn export_formats_map_and_mochi_does_not() {
        assert!(ArchiveFormat::Mochi.export().is_none());
        for (f, e) in [
            (ArchiveFormat::Zip, mochi_export::Format::Zip),
            (ArchiveFormat::Tar, mochi_export::Format::Tar),
            (ArchiveFormat::TarGz, mochi_export::Format::TarGz),
            (ArchiveFormat::TarZst, mochi_export::Format::TarZst),
        ] {
            assert_eq!(f.export(), Some(e));
            assert_eq!(f.extension(), e.extension());
        }
    }

    #[test]
    fn typed_names_are_one_plain_component() {
        assert_eq!(check_archive_name("  Photos.zip "), Ok("Photos.zip"));
        for bad in [
            "", "  ", "a/b", "a\\b", "..", ".", "x.", "con:", "a*b", "a\nb", "a|b",
        ] {
            assert!(check_archive_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_archive_name(&"x".repeat(201)).is_err());
    }
}
