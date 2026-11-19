//! What the menu entries say and run. Pure: no Windows types, so the rules
//! are tested on every platform.
//!
//! Three entries exist. "Add to archive..." and Add to "<Name>.mochi" appear for
//! any selection. "Find in MOCHI archives" (`--find`) appears only when the
//! selection contains a folder (it searches the names inside every `.mochi` under
//! the folders, and is given only the folders), and on a folder's background
//! (right-click on the empty space of an open folder: that folder alone, with no
//! "Add" entries).
//!
//! The label of "Add to <Name>.mochi" must match the archive that
//! `mochi-desktop --add-now` makes (`quick::add_target` in the desktop app):
//! one item gives that item's file name without its extension (a folder keeps
//! its whole name); several items give the name of the folder they are in; a
//! name that cannot be found is `archive`. A name already taken becomes
//! `<name> (2).mochi` in the app; the label cannot know that and names the
//! first choice.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Longest name shown in the label, in characters; longer ones end in an ellipsis.
pub const MAX_LABEL_NAME: usize = 40;

/// English text of the panel entry.
pub const PANEL_LABEL: &str = "Add to archive...";

/// English text of the Find entry.
pub const FIND_LABEL: &str = "Find in MOCHI archives";

/// The language of the menu text. Chosen from the Windows display language
/// (`shell.rs`), the same three languages the app ships; anything else is English.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Th,
    ZhCn,
}

impl Lang {
    /// From a Windows LANGID (primary language in the low ten bits): Thai is
    /// 0x1E, Chinese 0x04 (every Chinese variant maps to the one Chinese
    /// translation, as the app does for `zh*`).
    pub fn from_langid(langid: u16) -> Self {
        match langid & 0x03ff {
            0x1e => Lang::Th,
            0x04 => Lang::ZhCn,
            _ => Lang::En,
        }
    }

    /// Text of the panel entry.
    pub fn panel_label(self) -> &'static str {
        match self {
            Lang::En => PANEL_LABEL,
            Lang::Th => "เพิ่มไปยังไฟล์เก็บถาวร...",
            Lang::ZhCn => "添加到压缩文件...",
        }
    }

    /// Text of the Find entry (searches the file names inside `.mochi` archives).
    pub fn find_label(self) -> &'static str {
        match self {
            Lang::En => FIND_LABEL,
            Lang::Th => "ค้นหาในไฟล์เก็บถาวร MOCHI",
            Lang::ZhCn => "在 MOCHI 压缩文件中查找",
        }
    }

    /// Text before and after the archive name of the second entry.
    fn add_now_frame(self) -> (&'static str, &'static str) {
        match self {
            Lang::En => ("Add to \"", ".mochi\""),
            Lang::Th => ("เพิ่มไปยัง \"", ".mochi\""),
            Lang::ZhCn => ("添加到 \"", ".mochi\""),
        }
    }
}

/// Verbs of the entry, as the app's command line spells them (`launch.rs`).
pub const VERB_PANEL: &str = "--add";
pub const VERB_NOW: &str = "--add-now";
pub const VERB_FIND: &str = "--find";

/// Command-string verbs for `IContextMenu::GetCommandString` (not shown to users).
pub const CANONICAL_PANEL: &str = "MochiAdd";
pub const CANONICAL_NOW: &str = "MochiAddNow";
pub const CANONICAL_FIND: &str = "MochiFind";

/// One selected item. `is_dir` is whether it is a folder (the shell extension
/// asks the file system for each item, up to a limit; beyond it, `false`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// Command offsets of the entries (added to the first id the shell grants).
pub const CMD_PANEL: u32 = 0;
pub const CMD_NOW: u32 = 1;
pub const CMD_FIND: u32 = 2;
/// Number of ids the handler asks the shell for (the most it can use).
pub const CMD_COUNT: u32 = 3;

/// One menu row: its command offset and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub offset: u32,
    pub label: String,
}

/// The rows in the order they are inserted: both at the top level of the
/// classic menu (never in a submenu), "Add to archive..." directly followed by
/// Add to "<Name>.mochi", so they sit next to each other. Both carry the MOCHI
/// icon when the handler can load it (`shell.rs`). No items, no rows.
pub fn menu_entries(items: &[Item], lang: Lang) -> Vec<Entry> {
    if items.is_empty() {
        return Vec::new();
    }
    vec![
        Entry {
            offset: CMD_PANEL,
            label: lang.panel_label().to_owned(),
        },
        Entry {
            offset: CMD_NOW,
            label: add_now_label(items, lang),
        },
    ]
}

/// The Find row, if the items hold a folder.
pub fn find_entry(items: &[Item], lang: Lang) -> Option<Entry> {
    items.iter().any(|i| i.is_dir).then(|| Entry {
        offset: CMD_FIND,
        label: lang.find_label().to_owned(),
    })
}

/// Every row for a menu, in the order inserted: the two Add rows then Find for a
/// selection; Find alone on a folder's background (where "Add" would act on
/// nothing the user selected). `items` is the selection, or the one folder.
pub fn menu_rows(items: &[Item], lang: Lang, background: bool) -> Vec<Entry> {
    let mut rows = if background {
        Vec::new()
    } else {
        menu_entries(items, lang)
    };
    rows.extend(find_entry(items, lang));
    rows
}

/// The ids a set of rows uses: one more than the highest offset (what
/// `QueryContextMenu` returns), 0 for none.
pub fn ids_used(rows: &[Entry]) -> u32 {
    rows.iter().map(|r| r.offset + 1).max().unwrap_or(0)
}

/// What Find is given: the folders among `items`, in order. Files are not passed.
pub fn find_paths(items: &[Item]) -> Vec<PathBuf> {
    items
        .iter()
        .filter(|i| i.is_dir)
        .map(|i| i.path.clone())
        .collect()
}

/// The archive's name, without `.mochi`, exactly as the app chooses it.
pub fn archive_stem(items: &[Item]) -> String {
    let stem = match items {
        [] => None,
        [one] => {
            let name = if one.is_dir {
                one.path.file_name()
            } else {
                one.path.file_stem()
            };
            name.map(|n| n.to_string_lossy().into_owned())
        }
        [first, ..] => first
            .path
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned()),
    };
    stem.filter(|s| !s.is_empty())
        .unwrap_or_else(|| "archive".to_owned())
}

/// Menu text for "Add to <Name>.mochi". `&` is doubled because a menu reads a
/// single one as an accelerator marker.
pub fn add_now_label(items: &[Item], lang: Lang) -> String {
    let stem = archive_stem(items);
    let shown = if stem.chars().count() > MAX_LABEL_NAME {
        let head: String = stem.chars().take(MAX_LABEL_NAME - 1).collect();
        format!("{head}\u{2026}")
    } else {
        stem
    };
    let (before, after) = lang.add_now_frame();
    format!("{before}{}{after}", shown.replace('&', "&&"))
}

/// The arguments after the program name: `VERB -- PATH...`. `--` ends the
/// options, so a file named like an option stays a path. Never a shell string.
pub fn launch_args(verb: &str, paths: &[PathBuf]) -> Vec<OsString> {
    let mut args = Vec::with_capacity(paths.len() + 2);
    args.push(OsString::from(verb));
    args.push(OsString::from("--"));
    args.extend(paths.iter().map(|p| p.clone().into_os_string()));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(p: &str) -> Item {
        Item {
            path: PathBuf::from(p),
            is_dir: false,
        }
    }
    fn dir(p: &str) -> Item {
        Item {
            path: PathBuf::from(p),
            is_dir: true,
        }
    }

    #[test]
    fn two_top_level_entries_adjacent_and_in_order() {
        let rows = menu_entries(&[file("/x/notes.txt")], Lang::En);
        assert_eq!(rows.len(), 2, "Find is separate: no folder is selected");
        assert_eq!(rows[0].offset, CMD_PANEL);
        assert_eq!(rows[0].label, "Add to archive...");
        assert_eq!(rows[1].offset, CMD_NOW);
        assert_eq!(rows[1].label, "Add to \"notes.mochi\"");
        assert!(menu_entries(&[], Lang::En).is_empty());
    }

    #[test]
    fn the_windows_display_language_picks_the_labels() {
        assert_eq!(Lang::from_langid(0x0409), Lang::En); // en-US
        assert_eq!(Lang::from_langid(0x041e), Lang::Th); // th-TH
        assert_eq!(Lang::from_langid(0x0804), Lang::ZhCn); // zh-CN
        assert_eq!(Lang::from_langid(0x0404), Lang::ZhCn); // zh-TW
        assert_eq!(Lang::from_langid(0x0407), Lang::En); // de-DE
        let items = [file("/x/notes.txt")];
        for lang in [Lang::En, Lang::Th, Lang::ZhCn] {
            let rows = menu_entries(&items, lang);
            assert_eq!(rows[0].label, lang.panel_label());
            assert!(rows[1].label.contains("notes.mochi"), "{}", rows[1].label);
        }
        assert_ne!(Lang::Th.panel_label(), PANEL_LABEL);
        assert_ne!(Lang::ZhCn.panel_label(), PANEL_LABEL);
    }

    #[test]
    fn one_file_drops_its_extension() {
        assert_eq!(
            archive_stem(&[file("/home/a/report.final.txt")]),
            "report.final"
        );
        assert_eq!(
            add_now_label(&[file("/x/notes.txt")], Lang::En),
            "Add to \"notes.mochi\""
        );
    }

    #[test]
    fn one_folder_keeps_its_whole_name() {
        assert_eq!(archive_stem(&[dir("/x/photos.2026")]), "photos.2026");
    }

    #[test]
    fn several_items_use_the_parent_folder() {
        let items = [
            file("/x/trip/a.jpg"),
            dir("/x/trip/b"),
            file("/x/trip/c.txt"),
        ];
        assert_eq!(archive_stem(&items), "trip");
    }

    #[test]
    fn nameless_cases_fall_back_to_archive() {
        assert_eq!(archive_stem(&[]), "archive");
        assert_eq!(archive_stem(&[file("/a.txt"), file("/b.txt")]), "archive");
        assert_eq!(archive_stem(&[dir("/")]), "archive");
    }

    #[test]
    fn ampersand_is_doubled_and_long_names_are_cut() {
        assert_eq!(
            add_now_label(&[file("/x/Tom & Jerry.avi")], Lang::En),
            "Add to \"Tom && Jerry.mochi\""
        );
        let long = "n".repeat(100);
        let label = add_now_label(&[dir(&format!("/x/{long}"))], Lang::En);
        assert_eq!(
            label.chars().count(),
            "Add to \"\".mochi".len() + MAX_LABEL_NAME
        );
        assert!(label.contains('\u{2026}'));
    }

    #[test]
    fn find_appears_only_when_the_selection_holds_a_folder() {
        let rows = |items: &[Item]| menu_rows(items, Lang::En, false);
        let labels =
            |items: &[Item]| -> Vec<String> { rows(items).into_iter().map(|r| r.label).collect() };
        // Files only: the two Add rows, no Find.
        assert_eq!(
            labels(&[file("/x/a.txt"), file("/x/b.txt")]),
            ["Add to archive...", "Add to \"x.mochi\""]
        );
        // One folder: both Add rows and Find, Find last, at offset 2.
        let one = rows(&[dir("/x/photos")]);
        assert_eq!(
            one.iter().map(|r| r.offset).collect::<Vec<_>>(),
            [CMD_PANEL, CMD_NOW, CMD_FIND]
        );
        assert_eq!(one[2].label, "Find in MOCHI archives");
        // Several folders, or folders among files, show Find once.
        assert_eq!(rows(&[dir("/x/a"), dir("/x/b")]).len(), 3);
        assert_eq!(rows(&[file("/x/a.txt"), dir("/x/b")]).len(), 3);
        // No items, no rows.
        assert!(rows(&[]).is_empty());
        assert!(find_entry(&[file("/x/a.txt")], Lang::En).is_none());
    }

    #[test]
    fn on_a_folders_background_only_find_is_offered() {
        let rows = menu_rows(&[dir("/x/open-folder")], Lang::En, true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].offset, CMD_FIND);
        assert_eq!(rows[0].label, "Find in MOCHI archives");
        // A background with no folder resolved shows nothing.
        assert!(menu_rows(&[], Lang::En, true).is_empty());
        assert!(menu_rows(&[file("/x/a.txt")], Lang::En, true).is_empty());
    }

    #[test]
    fn ids_used_covers_the_highest_row() {
        assert_eq!(ids_used(&[]), 0);
        assert_eq!(ids_used(&menu_rows(&[file("/x/a")], Lang::En, false)), 2);
        assert_eq!(ids_used(&menu_rows(&[dir("/x/a")], Lang::En, false)), 3);
        assert_eq!(ids_used(&menu_rows(&[dir("/x/a")], Lang::En, true)), 3);
        assert!(ids_used(&menu_rows(&[dir("/x/a")], Lang::En, false)) <= CMD_COUNT);
    }

    #[test]
    fn find_is_given_the_folders_only() {
        let items = [
            file("/x/a.txt"),
            dir("/x/d1"),
            file("/x/b.txt"),
            dir("/x/d2"),
        ];
        assert_eq!(
            find_paths(&items),
            [PathBuf::from("/x/d1"), PathBuf::from("/x/d2")]
        );
        let args = launch_args(VERB_FIND, &find_paths(&items));
        assert_eq!(
            args,
            vec![
                OsString::from("--find"),
                OsString::from("--"),
                OsString::from("/x/d1"),
                OsString::from("/x/d2")
            ]
        );
        assert!(find_paths(&[file("/x/a.txt")]).is_empty());
    }

    #[test]
    fn the_find_label_is_localized_and_keeps_the_product_name() {
        assert_eq!(Lang::En.find_label(), "Find in MOCHI archives");
        for lang in [Lang::Th, Lang::ZhCn] {
            let label = lang.find_label();
            assert_ne!(label, FIND_LABEL);
            assert!(label.contains("MOCHI"), "{label}");
            assert!(!label.contains('&'), "no accelerator marker");
        }
        let rows = menu_rows(&[dir("/x/d")], Lang::Th, false);
        assert_eq!(rows[2].label, Lang::Th.find_label());
        let rows = menu_rows(&[dir("/x/d")], Lang::ZhCn, false);
        assert_eq!(rows[2].label, Lang::ZhCn.find_label());
    }

    #[test]
    fn arguments_are_verb_separator_then_each_path() {
        let paths = [PathBuf::from("-rf"), PathBuf::from("/x/a b.txt")];
        let args = launch_args(VERB_NOW, &paths);
        assert_eq!(
            args,
            vec![
                OsString::from("--add-now"),
                OsString::from("--"),
                OsString::from("-rf"),
                OsString::from("/x/a b.txt")
            ]
        );
    }
}
