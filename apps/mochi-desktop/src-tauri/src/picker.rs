//! Choosing an archive to open (plan D1). The native dialog runs in Rust, so
//! the webview never names a path and needs no dialog or filesystem
//! permission (least privilege, AGENTS.md "Tauri v2 rules"): the only file the
//! app reads is one the user picked in the operating system's own dialog.

use std::path::PathBuf;

use tauri::{AppHandle, Runtime};
use tauri_plugin_dialog::DialogExt;

use crate::ipc::{ArchiveFormat, DialogText, SourcePick};

/// Extensions the foreign reader opens (a name is a hint; the helper decides).
const FOREIGN_EXTENSIONS: [&str; 10] = [
    "zip", "7z", "rar", "tar", "tgz", "gz", "tzst", "zst", "001", "z01",
];

/// Show the open dialog for any archive MOCHI opens (MOCHI, ZIP, 7z, RAR, tar,
/// tar.gz, tar.zst) and wait for the user. `None` if they cancelled. The first
/// filter lists them all; the others narrow it to one format. Blocks: call it
/// off the async runtime.
pub fn pick_archive<R: Runtime>(app: &AppHandle<R>, text: &DialogText) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(path);
    }
    let mut all = vec!["mochi"];
    all.extend(FOREIGN_EXTENSIONS);
    app.dialog()
        .file()
        .set_title(text.title("Open an archive"))
        .add_filter(text.filter("Archives"), &all)
        .add_filter(text.mochi_archive(), &["mochi"])
        .add_filter("ZIP", &["zip"])
        .add_filter("7z", &["7z", "001"])
        .add_filter("RAR", &["rar"])
        .add_filter("tar, tar.gz, tar.zst", &["tar", "tgz", "gz", "tzst", "zst"])
        .add_filter(text.all_files(), &["*"])
        .blocking_pick_file()?
        .into_path()
        .ok()
}

/// Show the open dialog for a foreign archive (plan D8). `None` if the user
/// cancelled. Blocks: call it off the async runtime.
pub fn pick_foreign_archive<R: Runtime>(app: &AppHandle<R>, text: &DialogText) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(path);
    }
    app.dialog()
        .file()
        .set_title(text.title("Open a ZIP, 7z, RAR or tar archive (read-only)"))
        .add_filter("ZIP, 7z, RAR, tar", &FOREIGN_EXTENSIONS)
        .blocking_pick_file()?
        .into_path()
        .ok()
}

/// Show the dialog that chooses sources to add (plan D2). `None` if the user
/// cancelled. Blocks: call it off the async runtime.
pub fn pick_sources<R: Runtime>(
    app: &AppHandle<R>,
    what: SourcePick,
    text: &DialogText,
) -> Option<Vec<PathBuf>> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(vec![path]);
    }
    let dialog = app.dialog().file();
    match what {
        SourcePick::Files => Some(
            dialog
                .set_title(text.title("Choose files to add"))
                .blocking_pick_files()?
                .into_iter()
                .filter_map(|p| p.into_path().ok())
                .collect(),
        ),
        SourcePick::Folder => Some(vec![dialog
            .set_title(text.title("Choose a folder to add"))
            .blocking_pick_folder()?
            .into_path()
            .ok()?]),
    }
}

/// Show the save dialog for a new archive (plan D2, O31). `None` if the user
/// cancelled. The format's extension is applied to the chosen name (an archive
/// extension of another format is replaced, never doubled). The dialog starts
/// in `suggested`'s folder with its name filled in, when given. Blocks: call it
/// off the async runtime.
pub fn pick_destination<R: Runtime>(
    app: &AppHandle<R>,
    suggested: Option<&std::path::Path>,
    format: ArchiveFormat,
    text: &DialogText,
) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(format.path_for(&path));
    }
    let (label, filter): (&str, &[&str]) = match format {
        ArchiveFormat::Mochi => ("MOCHI archive", &["mochi"]),
        ArchiveFormat::Zip => ("ZIP archive", &["zip"]),
        ArchiveFormat::Tar => ("tar archive", &["tar"]),
        ArchiveFormat::TarGz => ("tar.gz archive", &["gz"]),
        ArchiveFormat::TarZst => ("tar.zst archive", &["zst"]),
    };
    let mut dialog = app
        .dialog()
        .file()
        .set_title(text.title("Save the new archive as"))
        .set_file_name(format!("archive{}", format.extension()))
        .add_filter(text.filter(label), filter);
    if let Some(suggested) = suggested {
        if let Some(dir) = suggested.parent() {
            dialog = dialog.set_directory(dir);
        }
        if let Some(name) = suggested.file_name().and_then(|n| n.to_str()) {
            dialog = dialog.set_file_name(name);
        }
    }
    let path = dialog.blocking_save_file()?.into_path().ok()?;
    Some(format.path_for(&path))
}

/// Show the folder dialog for an extraction (plan D3). `None` if the user
/// cancelled. Blocks: call it off the async runtime.
pub fn pick_extract_folder<R: Runtime>(app: &AppHandle<R>, text: &DialogText) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(path);
    }
    app.dialog()
        .file()
        .set_title(text.title("Extract to…"))
        .blocking_pick_folder()?
        .into_path()
        .ok()
}

/// Show the folder dialog for MOCHI Find: one or more folders to search for
/// archives. `None` if the user cancelled. Blocks: call it off the async runtime.
pub fn pick_folders<R: Runtime>(app: &AppHandle<R>, text: &DialogText) -> Option<Vec<PathBuf>> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(vec![path]);
    }
    let picked: Vec<PathBuf> = app
        .dialog()
        .file()
        .set_title(text.title("Choose folders to search for MOCHI archives"))
        .blocking_pick_folders()?
        .into_iter()
        .filter_map(|p| p.into_path().ok())
        .collect();
    (!picked.is_empty()).then_some(picked)
}

/// [`pick_extract_folder`], starting in `start` when there is one (the
/// "Extract to..." window's Browse button).
pub fn pick_extract_folder_in<R: Runtime>(
    app: &AppHandle<R>,
    start: Option<&std::path::Path>,
    text: &DialogText,
) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(path);
    }
    let mut dialog = app.dialog().file().set_title(text.title("Extract to…"));
    if let Some(dir) = start {
        dialog = dialog.set_directory(dir);
    }
    dialog.blocking_pick_folder()?.into_path().ok()
}

/// Show the open dialog for the archive to test or repair (plan D4). Any
/// file can be chosen: a damaged archive may not end in `.mochi`. `None` if
/// the user cancelled. Blocks: call it off the async runtime.
pub fn pick_check_subject<R: Runtime>(app: &AppHandle<R>, text: &DialogText) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(path);
    }
    app.dialog()
        .file()
        .set_title(text.title("Choose an archive to test or repair"))
        .add_filter(text.mochi_archive(), &["mochi"])
        .add_filter(text.all_files(), &["*"])
        .blocking_pick_file()?
        .into_path()
        .ok()
}

/// Show the open dialog for an independent copy of the archive being
/// repaired (plan D4, `--copy`). `None` if the user cancelled. Blocks.
pub fn pick_copy<R: Runtime>(app: &AppHandle<R>, text: &DialogText) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(path);
    }
    app.dialog()
        .file()
        .set_title(text.title("Choose an independent copy of the same archive"))
        .add_filter(text.mochi_archive(), &["mochi"])
        .add_filter(text.all_files(), &["*"])
        .blocking_pick_file()?
        .into_path()
        .ok()
}

/// Show the save dialog for the new archive a repair writes (plan D4).
/// `None` if the user cancelled. Blocks.
pub fn pick_repair_output<R: Runtime>(app: &AppHandle<R>, text: &DialogText) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(with_extension(path));
    }
    let path = app
        .dialog()
        .file()
        .set_title(text.title("Save the new archive as"))
        .set_file_name("new-archive.mochi")
        .add_filter(text.mochi_archive(), &["mochi"])
        .blocking_save_file()?
        .into_path()
        .ok()?;
    Some(with_extension(path))
}

/// Show the save dialog for an exported report (plan D4). `None` if the user
/// cancelled. Blocks.
pub fn pick_report_destination<R: Runtime>(
    app: &AppHandle<R>,
    suggested: &str,
    text: &DialogText,
) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(path);
    }
    app.dialog()
        .file()
        .set_title(text.title("Export the report (JSON)"))
        .set_file_name(suggested)
        .add_filter(text.filter("JSON report"), &["json"])
        .blocking_save_file()?
        .into_path()
        .ok()
}

/// Show the save dialog for an exported crash log (plan D7). `None` if the
/// user cancelled. Blocks.
pub fn pick_crash_log_destination<R: Runtime>(
    app: &AppHandle<R>,
    text: &DialogText,
) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(path);
    }
    app.dialog()
        .file()
        .set_title(text.title("Export the crash log (text)"))
        .set_file_name("mochi-crash-log.txt")
        .add_filter(text.filter("Text"), &["txt"])
        .blocking_save_file()?
        .into_path()
        .ok()
}

/// Show the open dialog for a virus scanner's program (the scan window's
/// Browse). On Windows only programs are offered. `None` if the user
/// cancelled. Blocks: call it off the async runtime.
pub fn pick_scanner_program<R: Runtime>(app: &AppHandle<R>, text: &DialogText) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Some(path) = crate::automation::next_pick() {
        return Some(path);
    }
    let dialog = app
        .dialog()
        .file()
        .set_title(text.title("Choose the virus scanner's program"));
    let dialog = if cfg!(windows) {
        dialog.add_filter(text.filter("Programs"), &["exe", "com"])
    } else {
        dialog
    };
    dialog.blocking_pick_file()?.into_path().ok()
}

/// `path` with `.mochi` appended when it has no extension.
pub fn with_extension(mut path: PathBuf) -> PathBuf {
    if path.extension().is_none() {
        path.set_extension("mochi");
    }
    path
}

#[cfg(test)]
mod tests {
    use super::with_extension;
    use std::path::PathBuf;

    #[test]
    fn a_destination_without_an_extension_gets_mochi() {
        assert_eq!(
            with_extension(PathBuf::from("/x/backup")),
            PathBuf::from("/x/backup.mochi")
        );
        assert_eq!(
            with_extension(PathBuf::from("/x/a.zip")),
            PathBuf::from("/x/a.zip"),
            "an extension the user typed is theirs"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod dialog_text_tests {
    use crate::ipc::DialogText;

    fn text(json: &str) -> DialogText {
        serde_json::from_str(json).expect("a DialogText")
    }

    #[test]
    fn a_localized_title_and_filter_labels_are_used() {
        let t = text(
            r#"{"title":"เปิดไฟล์บีบอัด","filter":"ไฟล์บีบอัด","mochi_archive":"ไฟล์บีบอัด MOCHI","all_files":"ทุกไฟล์"}"#,
        );
        assert_eq!(t.title("Open an archive"), "เปิดไฟล์บีบอัด");
        assert_eq!(t.filter("Archives"), "ไฟล์บีบอัด");
        assert_eq!(t.mochi_archive(), "ไฟล์บีบอัด MOCHI");
        assert_eq!(t.all_files(), "ทุกไฟล์");
    }

    #[test]
    fn absent_empty_long_or_control_text_falls_back_to_english() {
        let none = DialogText::default();
        assert_eq!(none.title("Open an archive"), "Open an archive");
        assert_eq!(none.mochi_archive(), "MOCHI archive");
        assert_eq!(none.all_files(), "All files");
        assert_eq!(text(r#"{"title":"   "}"#).title("T"), "T");
        assert_eq!(text(r#"{"title":"a\nb"}"#).title("T"), "T");
        assert_eq!(text(r#"{"title":"a\u0000b"}"#).title("T"), "T");
        let long = "x".repeat(DialogText::MAX_CHARS + 1);
        assert_eq!(text(&format!(r#"{{"title":"{long}"}}"#)).title("T"), "T");
        let exact = "ก".repeat(DialogText::MAX_CHARS);
        assert_eq!(text(&format!(r#"{{"title":"{exact}"}}"#)).title("T"), exact);
        assert_eq!(text(r#"{"title":"  trimmed  "}"#).title("T"), "trimmed");
    }

    #[test]
    fn an_unknown_field_is_refused() {
        assert!(serde_json::from_str::<DialogText>(r#"{"path":"/etc"}"#).is_err());
    }
}
