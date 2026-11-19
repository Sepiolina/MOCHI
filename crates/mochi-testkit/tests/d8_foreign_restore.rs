//! D8 (plan rule 4): entries of a foreign archive are restored through the C6
//! engine's rules. Every §10.4 protection is exercised here with hostile
//! entry lists, on an in-memory destination that can behave like Windows or a
//! case-insensitive filesystem, and on a real directory.

use mochi_core::catalog::path::ArchivePath;
use mochi_core::job::JobContext;
use mochi_core::restore::{ExceptionKind, OnExisting, RestoreReport};
use mochi_core::restore_foreign::*;
use mochi_core::storage::{CaseBehavior, NameIssue, RestoreDir};
use mochi_core::ErrorCode;
use mochi_testkit::archive::Job;
use mochi_testkit::SimTree;

fn e(index: u64, name: &str, kind: ForeignKind) -> ForeignEntry {
    ForeignEntry {
        index,
        name: name.as_bytes().to_vec(),
        kind,
        size: Some(4),
        mtime_unix: Some(1_577_836_800),
    }
}

fn file(index: u64, name: &str) -> ForeignEntry {
    e(index, name, ForeignKind::File)
}

fn dir(index: u64, name: &str) -> ForeignEntry {
    e(index, name, ForeignKind::Directory)
}

/// What a file's bytes are in these tests: derived from its index.
fn body(index: u64) -> Vec<u8> {
    format!("data-{index}").into_bytes()
}

/// Streams every wanted file, in archive (index) order, as a helper would.
fn run_with<D: RestoreDir>(
    entries: &[ForeignEntry],
    selection: &[ArchivePath],
    root: D,
    fail: impl Fn(u64) -> Option<FileFailure>,
    options: ForeignRestoreOptions,
    ctx: &JobContext<'_>,
) -> mochi_core::Result<ForeignRestoreReport> {
    let mut r = ForeignRestorer::new(entries, selection, root, options, ctx)?;
    for index in r.wanted() {
        r.begin_file(index)?;
        let b = body(index);
        let (a, z) = b.split_at(b.len() / 2);
        r.write(a)?;
        r.write(z)?;
        r.end_file(match fail(index) {
            Some(f) => Err(f),
            None => Ok(()),
        })?;
    }
    r.finish("the source did not deliver it")
}

fn run<D: RestoreDir>(entries: &[ForeignEntry], root: D) -> ForeignRestoreReport {
    run_with(
        entries,
        &[],
        root,
        |_| None,
        ForeignRestoreOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap()
}

fn exception<'a>(r: &'a RestoreReport, p: &str) -> Option<&'a ExceptionKind> {
    r.exceptions
        .iter()
        .find(|x| x.path.as_stored() == p.as_bytes())
        .map(|x| &x.kind)
}

fn refusal<'a>(r: &'a ForeignRestoreReport, name: &str) -> Option<&'a RefusalReason> {
    r.refusals
        .iter()
        .find(|x| x.name == name.as_bytes())
        .map(|x| &x.reason)
}

#[test]
fn files_and_directories_are_created_with_implied_parents_and_times() {
    let tree = SimTree::new();
    let entries = [
        file(0, "top.txt"),
        file(1, "a/b/c.txt"), // no entry for a/ or a/b/
        dir(2, "explicit/"),
        file(3, "explicit/f.txt"),
        file(4, "./dot/x.txt"),
        dir(5, "./"), // the destination itself
    ];
    let r = run(&entries, tree.clone());
    assert!(r.report.complete(), "{:?}", r.report.exceptions);
    assert!(r.refusals.is_empty());
    assert_eq!(r.report.files, 4);
    assert_eq!(r.report.directories, 4); // a, a/b, explicit, dot
    assert_eq!(tree.file(b"a/b/c.txt").unwrap(), body(1));
    assert_eq!(tree.file(b"dot/x.txt").unwrap(), body(4));
    assert!(tree.is_dir(b"a/b") && tree.is_dir(b"explicit"));
    assert_eq!(
        tree.attributes(b"top.txt").unwrap().mtime.unwrap().secs,
        1_577_836_800
    );
    // Indices 0, 1, 3 and 4 are files; 2 is a directory.
    assert_eq!(
        r.report.bytes,
        [0, 1, 3, 4]
            .iter()
            .map(|i| body(*i).len() as u64)
            .sum::<u64>()
    );
    // No temporary file is left behind.
    assert!(tree.paths().iter().all(|p| !p.contains(".mochi-restore")));
}

#[test]
fn traversal_absolute_and_ambiguous_names_are_refused_before_anything_is_written() {
    let tree = SimTree::new();
    let entries = [
        file(0, "../evil.txt"),
        file(1, "/abs/path.txt"),
        file(2, "a/../../b.txt"),
        file(3, "a/b/../c.txt"),
        file(4, "C:\\win.txt"),
        file(5, "C:win.txt"),
        file(6, "dir\\back.txt"),
        file(7, "a//b.txt"),
        file(8, "nul\0byte.txt"),
        file(9, "fine.txt"),
        file(10, ".."),
        file(11, ""),
    ];
    let r = run(&entries, tree.clone());
    assert_eq!(refusal(&r, "../evil.txt"), Some(&RefusalReason::Traversal));
    assert_eq!(refusal(&r, "/abs/path.txt"), Some(&RefusalReason::Absolute));
    assert_eq!(
        refusal(&r, "a/../../b.txt"),
        Some(&RefusalReason::Traversal)
    );
    assert_eq!(refusal(&r, "a/b/../c.txt"), Some(&RefusalReason::Traversal));
    assert_eq!(
        refusal(&r, "C:\\win.txt"),
        Some(&RefusalReason::DriveLetter)
    );
    assert_eq!(refusal(&r, "C:win.txt"), Some(&RefusalReason::DriveLetter));
    assert_eq!(
        refusal(&r, "dir\\back.txt"),
        Some(&RefusalReason::Backslash)
    );
    assert_eq!(
        refusal(&r, "a//b.txt"),
        Some(&RefusalReason::EmptyComponent)
    );
    assert_eq!(refusal(&r, "nul\0byte.txt"), Some(&RefusalReason::Nul));
    assert_eq!(refusal(&r, ".."), Some(&RefusalReason::Traversal));
    assert_eq!(refusal(&r, ""), Some(&RefusalReason::NoName));
    for x in &r.refusals {
        assert_eq!(x.reason.code(), ErrorCode::NameUnsupported);
        assert!(!x.reason.describe().is_empty());
    }
    // Only the harmless entry exists; nothing escaped, nothing was made for
    // the refused names (not even their implied parents).
    assert_eq!(tree.paths(), vec!["fine.txt".to_string()]);
    assert_eq!(r.report.files, 1);
    assert_eq!(r.refusals.len(), 11);
}

#[test]
fn links_devices_and_pipes_are_skipped_and_reported_never_created() {
    let tree = SimTree::new();
    let entries = [
        e(0, "link", ForeignKind::Symlink),
        e(1, "hard", ForeignKind::HardLink),
        e(2, "dev", ForeignKind::Other),
        // An entry written "through" the link is an ordinary directory entry,
        // because no link was made: it cannot land anywhere else.
        file(3, "link/escape.txt"),
    ];
    let r = run(&entries, tree.clone());
    for (n, kind) in [
        ("link", RefusalReason::Link(ForeignKind::Symlink)),
        ("hard", RefusalReason::Link(ForeignKind::HardLink)),
        ("dev", RefusalReason::Special),
    ] {
        assert_eq!(refusal(&r, n), Some(&kind), "{n}");
        assert_eq!(kind.code(), ErrorCode::UnsupportedFeature);
    }
    assert!(tree.is_dir(b"link"), "a real directory, not a link");
    assert_eq!(tree.file(b"link/escape.txt").unwrap(), body(3));
    assert!(!tree.paths().contains(&"hard".to_string()));
    assert!(!tree.paths().contains(&"dev".to_string()));
}

#[test]
fn windows_reserved_names_and_stream_syntax_are_rejected_by_the_destination() {
    let tree = SimTree::new().windows_rules();
    let entries = [
        file(0, "CON"),
        file(1, "nul.txt"),
        file(2, "file.txt:stream"),
        file(3, "trail."),
        file(4, "ok/fine.txt"),
        file(5, "bad/aux/inner.txt"),
    ];
    let r = run(&entries, tree.clone());
    for p in ["CON", "nul.txt", "file.txt:stream", "trail.", "bad/aux"] {
        assert!(
            matches!(
                exception(&r.report, p),
                Some(ExceptionKind::UnsupportedName(_))
            ),
            "{p}: {:?}",
            exception(&r.report, p)
        );
    }
    assert!(matches!(
        exception(&r.report, "bad/aux/inner.txt"),
        Some(ExceptionKind::ParentNotRestored { .. })
    ));
    assert_eq!(tree.file(b"ok/fine.txt").unwrap(), body(4));
    assert!(matches!(
        exception(&r.report, "file.txt:stream"),
        Some(ExceptionKind::UnsupportedName(NameIssue::IllegalCharacter(
            b':'
        )))
    ));
}

#[test]
fn case_collisions_and_duplicates_keep_the_first_and_report_the_rest() {
    let tree = SimTree::new().case_insensitive();
    let entries = [
        file(0, "A.txt"),
        file(1, "a.txt"),
        file(2, "same.txt"),
        file(3, "same.txt"),
        dir(4, "D/"),
        file(5, "d/inner.txt"),
    ];
    let r = run(&entries, tree.clone());
    assert_eq!(r.report.case_behavior, CaseBehavior::Insensitive);
    assert_eq!(
        tree.file(b"A.txt").unwrap(),
        body(0),
        "the first keeps its bytes"
    );
    assert!(matches!(
        exception(&r.report, "a.txt"),
        Some(ExceptionKind::Collision)
    ));
    assert!(matches!(
        exception(&r.report, "same.txt"),
        Some(ExceptionKind::Collision)
    ));
    assert_eq!(tree.file(b"same.txt").unwrap(), body(2));
    // `d/inner.txt` implies a directory `d`, which folds onto the explicit `D`.
    assert!(tree.is_dir(b"D"));
    assert!(tree.file(b"D/inner.txt").is_some() || exception(&r.report, "d").is_some());
}

#[test]
fn nothing_that_exists_at_the_destination_is_overwritten_or_merged() {
    let tree = SimTree::new();
    tree.insert_file("keep.txt", b"original");
    tree.insert_file("old/dir/f.txt", b"original too");
    let entries = [
        file(0, "keep.txt"),
        file(1, "old/new.txt"), // `old` exists: never merged into
        file(2, "fresh.txt"),
    ];
    let r = run(&entries, tree.clone());
    assert_eq!(tree.file(b"keep.txt").unwrap(), b"original");
    assert!(matches!(
        exception(&r.report, "keep.txt"),
        Some(ExceptionKind::Collision)
    ));
    assert!(matches!(
        exception(&r.report, "old"),
        Some(ExceptionKind::Collision)
    ));
    assert!(matches!(
        exception(&r.report, "old/new.txt"),
        Some(ExceptionKind::ParentNotRestored { .. })
    ));
    assert!(tree.file(b"old/new.txt").is_none());
    assert_eq!(tree.file(b"old/dir/f.txt").unwrap(), b"original too");
    assert_eq!(tree.file(b"fresh.txt").unwrap(), body(2));
}

#[test]
fn a_failed_check_or_unreadable_entry_is_never_published() {
    let tree = SimTree::new();
    let entries = [file(0, "good.txt"), file(1, "bad.txt"), file(2, "gone.txt")];
    let r = run_with(
        &entries,
        &[],
        tree.clone(),
        |i| match i {
            1 => Some(FileFailure::CheckFailed("CRC32 did NOT match".into())),
            2 => Some(FileFailure::Unreadable("password required".into())),
            _ => None,
        },
        ForeignRestoreOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    assert_eq!(
        tree.paths(),
        vec!["good.txt".to_string()],
        "no partial file, no temp"
    );
    match exception(&r.report, "bad.txt") {
        Some(ExceptionKind::Integrity { code, message }) => {
            assert_eq!(*code, ErrorCode::ContentIntegrityFailed);
            assert!(message.contains("CRC32"));
        }
        o => panic!("{o:?}"),
    }
    assert!(matches!(
        exception(&r.report, "gone.txt"),
        Some(ExceptionKind::Failed {
            code: ErrorCode::IoError,
            ..
        })
    ));
    assert_eq!(r.report.files, 1);
}

#[test]
fn files_the_source_never_delivered_are_reported_and_an_open_file_is_discarded() {
    let tree = SimTree::new();
    let entries = [file(0, "a.txt"), file(1, "b.txt"), file(2, "c.txt")];
    let job = Job::new();
    let ctx = job.ctx();
    let mut r = ForeignRestorer::new(
        &entries,
        &[],
        tree.clone(),
        ForeignRestoreOptions::default(),
        &ctx,
    )
    .unwrap();
    assert_eq!(r.wanted(), vec![0, 1, 2]);
    r.begin_file(0).unwrap();
    r.write(b"abc").unwrap();
    r.end_file(Ok(())).unwrap();
    r.begin_file(1).unwrap();
    r.write(b"partial").unwrap();
    // The source dies here: file 1 is open, file 2 never began.
    let report = r.finish("the helper stopped").unwrap();
    assert_eq!(tree.paths(), vec!["a.txt".to_string()]);
    assert!(exception(&report.report, "b.txt").is_some());
    match exception(&report.report, "c.txt") {
        Some(ExceptionKind::Failed { message, .. }) => {
            assert!(message.contains("the helper stopped"))
        }
        o => panic!("{o:?}"),
    }
}

#[test]
fn the_source_cannot_name_files_that_were_not_asked_for() {
    let job = Job::new();
    let ctx = job.ctx();
    let entries = [file(0, "a.txt"), file(1, "../x")];
    let mut r = ForeignRestorer::new(
        &entries,
        &[],
        SimTree::new(),
        ForeignRestoreOptions::default(),
        &ctx,
    )
    .unwrap();
    assert_eq!(r.wanted(), vec![0], "a refused name is not streamed");
    assert!(r.begin_file(1).is_err());
    assert!(r.begin_file(7).is_err());
    r.begin_file(0).unwrap();
    assert!(r.begin_file(0).is_err(), "one file at a time");
    r.end_file(Ok(())).unwrap();
    assert!(r.begin_file(0).is_err(), "delivered once");
}

#[test]
fn a_selection_restores_the_subtree_and_its_parents_only() {
    let tree = SimTree::new();
    let entries = [
        file(0, "x/1.txt"),
        file(1, "x/y/2.txt"),
        file(2, "z/3.txt"),
        file(3, "../bad"),
    ];
    let sel = [ArchivePath::from_stored(b"x/y").unwrap()];
    let r = run_with(
        &entries,
        &sel,
        tree.clone(),
        |_| None,
        ForeignRestoreOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    let mut paths = tree.paths();
    paths.sort();
    assert_eq!(paths, vec!["x", "x/y", "x/y/2.txt"]);
    assert!(
        r.refusals.is_empty(),
        "a refusal outside the selection is not reported"
    );
}

#[test]
fn refuse_on_exceptions_ends_the_job_before_anything_is_written() {
    let tree = SimTree::new();
    tree.insert_file("exists.txt", b"x");
    let entries = [file(0, "ok.txt"), file(1, "exists.txt")];
    let err = run_with(
        &entries,
        &[],
        tree.clone(),
        |_| None,
        ForeignRestoreOptions {
            refuse_on_exceptions: true,
            ..ForeignRestoreOptions::default()
        },
        &Job::new().ctx(),
    )
    .err()
    .unwrap();
    assert_eq!(err.code, ErrorCode::NameCollision);
    assert_eq!(
        tree.paths(),
        vec!["exists.txt".to_string()],
        "nothing written"
    );
    // A refused name counts too.
    let err = run_with(
        &[file(0, "../x")],
        &[],
        SimTree::new(),
        |_| None,
        ForeignRestoreOptions {
            refuse_on_exceptions: true,
            ..ForeignRestoreOptions::default()
        },
        &Job::new().ctx(),
    )
    .err()
    .unwrap();
    assert_eq!(err.code, ErrorCode::NameUnsupported);
}

#[test]
fn cancellation_stops_the_job() {
    let job = Job::new();
    job.cancel.cancel();
    let ctx = job.ctx();
    let err = ForeignRestorer::new(
        &[file(0, "a")],
        &[],
        SimTree::new(),
        ForeignRestoreOptions::default(),
        &ctx,
    )
    .err()
    .unwrap();
    assert_eq!(err.code, ErrorCode::Cancelled);
}

#[test]
fn the_plan_shows_conflicts_and_refusals_without_writing() {
    let tree = SimTree::new().case_insensitive();
    tree.insert_file("Taken.txt", b"x");
    let entries = [
        file(0, "taken.txt"),
        file(1, "free.txt"),
        file(2, "../no"),
        e(3, "l", ForeignKind::Symlink),
    ];
    let mut root = tree.clone();
    let plan = plan_foreign(&entries, &[], &mut root, OnExisting::Keep).unwrap();
    assert_eq!(plan.items, 2);
    assert_eq!(plan.refusals.len(), 2);
    assert_eq!(plan.preflight.case_behavior, CaseBehavior::Insensitive);
    assert_eq!(plan.preflight.exceptions.len(), 1);
    assert_eq!(
        tree.paths(),
        vec!["Taken.txt".to_string()],
        "planning wrote nothing"
    );
}

#[test]
fn parse_name_accepts_ordinary_names_and_normalises_only_dot_components() {
    let p = |s: &str| parse_name(s.as_bytes());
    assert_eq!(p("a/b").unwrap().unwrap().as_stored(), b"a/b");
    assert_eq!(p("a/b/").unwrap().unwrap().as_stored(), b"a/b");
    assert_eq!(p("./a/./b").unwrap().unwrap().as_stored(), b"a/b");
    assert_eq!(p("./").unwrap(), None);
    assert_eq!(p(".").unwrap(), None);
    // Bytes that are not UTF-8 survive, unaltered.
    let raw = parse_name(&[b'a', 0xff, b'/', 0xfe]).unwrap().unwrap();
    assert_eq!(raw.as_stored(), &[b'a', 0xff, b'/', 0xfe]);
    // Case and Unicode are never folded.
    assert_eq!(
        p("Straße/ǅ").unwrap().unwrap().as_stored(),
        "Straße/ǅ".as_bytes()
    );
    assert!(matches!(
        p(&"a/".repeat(5000)),
        Err(RefusalReason::Unrepresentable(_))
    ));
}

#[test]
fn the_listing_tree_orders_implies_and_labels() {
    let entries = [
        file(0, "z.txt"),
        file(1, "d/b.txt"),
        file(2, "d/a.txt"),
        dir(3, "e/"),
        e(4, "d/link", ForeignKind::Symlink),
        file(5, "../evil"),
        file(6, "d/b.txt"), // a duplicate: the first stays
    ];
    let t = ForeignTree::build(&entries);
    assert_eq!(t.entries, 7);
    let root = t.children(None);
    let names: Vec<String> = root
        .iter()
        .map(|r| String::from_utf8_lossy(&r.name).into_owned())
        .collect();
    // Directories first (d is implied), then files, then refused names.
    assert_eq!(names, vec!["d", "e", "z.txt", "../evil"]);
    assert_eq!(root[0].index, None, "d has no entry of its own");
    assert_eq!(root[0].child_files, 3);
    assert_eq!(root[3].refused, Some(RefusalReason::Traversal));
    assert!(root[3].path.is_none());
    let d = ArchivePath::from_stored(b"d").unwrap();
    let kids = t.children(Some(&d));
    let kn: Vec<&[u8]> = kids.iter().map(|r| r.name.as_slice()).collect();
    assert_eq!(kn, vec![&b"a.txt"[..], b"b.txt", b"link"]);
    assert_eq!(kids[1].index, Some(1));
    assert_eq!(
        kids[2].refused,
        Some(RefusalReason::Link(ForeignKind::Symlink))
    );
}

#[cfg(unix)]
#[test]
fn on_a_real_directory_a_planted_symlink_is_never_followed() {
    use mochi_core::storage::os::OsRestoreDir;
    use std::os::unix::fs::symlink;
    let outside = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    // The destination already holds a link to somewhere else.
    symlink(outside.path(), dest.path().join("trap")).unwrap();
    std::fs::write(dest.path().join("keep.txt"), b"original").unwrap();
    let entries = [
        file(0, "trap/pwned.txt"),
        file(1, "keep.txt"),
        file(2, "sub/ok.txt"),
        e(3, "sub/lnk", ForeignKind::Symlink),
    ];
    let root = OsRestoreDir::open(dest.path()).unwrap();
    let r = run(&entries, root);
    assert!(
        std::fs::read_dir(outside.path()).unwrap().next().is_none(),
        "nothing escaped"
    );
    assert_eq!(
        std::fs::read(dest.path().join("keep.txt")).unwrap(),
        b"original"
    );
    assert_eq!(
        std::fs::read(dest.path().join("sub/ok.txt")).unwrap(),
        body(2)
    );
    assert!(std::fs::symlink_metadata(dest.path().join("sub/lnk")).is_err());
    assert!(matches!(
        exception(&r.report, "trap"),
        Some(ExceptionKind::Collision)
    ));
    assert!(matches!(
        exception(&r.report, "keep.txt"),
        Some(ExceptionKind::Collision)
    ));
    // The time was applied to the extracted file.
    let m = std::fs::metadata(dest.path().join("sub/ok.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        m.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        1_577_836_800
    );
}

/// Fast extract: the same files, published without a per-file flush, and
/// the report says so; a file whose own check failed is still not published.
#[test]
fn fast_extract_skips_only_the_file_flush() {
    let entries = [file(0, "a.txt"), file(1, "b.txt"), file(2, "bad.txt")];
    let go = |skip_file_sync: bool| {
        let tree = SimTree::new();
        let r = run_with(
            &entries,
            &[],
            tree.clone(),
            |i| (i == 2).then(|| FileFailure::CheckFailed("CRC32 did not match".into())),
            ForeignRestoreOptions {
                skip_file_sync,
                ..ForeignRestoreOptions::default()
            },
            &Job::new().ctx(),
        )
        .unwrap();
        (tree, r)
    };

    let (tree, r) = go(false);
    assert!(r.report.file_data_synced);
    assert_eq!(tree.file_synced_len(b"a.txt"), Some(body(0).len()));

    let (tree, r) = go(true);
    assert!(!r.report.file_data_synced);
    assert_eq!(r.report.files, 2);
    assert_eq!(tree.file(b"a.txt").unwrap(), body(0));
    assert_eq!(tree.file(b"b.txt").unwrap(), body(1));
    assert_eq!(tree.file_synced_len(b"a.txt"), Some(0));
    assert!(tree.file(b"bad.txt").is_none());
    assert!(matches!(
        exception(&r.report, "bad.txt"),
        Some(ExceptionKind::Integrity { .. })
    ));
    assert!(tree.paths().iter().all(|p| !p.contains(".mochi-restore")));
}

/// Owner decision 2026-11-11: a ZIP or 7z extracted into a folder that
/// already has some of its names, with "Add only new files" (Merge) and
/// "Replace existing files" (Replace), as for a MOCHI archive. Replace moves
/// the existing file to the Bin only once the new one has passed its checks.
#[test]
fn merge_and_replace_into_an_existing_folder() {
    let entries = [
        file(0, "docs/a.txt"),
        file(1, "docs/new.txt"),
        file(2, "docs/bad.txt"),
    ];
    let crowded = || {
        let tree = SimTree::new();
        tree.insert_file("docs/a.txt", b"mine a");
        tree.insert_file("docs/bad.txt", b"mine bad");
        tree
    };
    let options = |on_existing| ForeignRestoreOptions {
        on_existing,
        ..ForeignRestoreOptions::default()
    };
    let bad = |i| (i == 2).then(|| FileFailure::CheckFailed("CRC32 did not match".into()));

    let tree = crowded();
    let mut root = tree.clone();
    let plan = plan_foreign(&entries, &[], &mut root, OnExisting::Replace).unwrap();
    assert_eq!(plan.preflight.merged.len(), 1);
    assert_eq!(plan.preflight.replaced.len(), 2);

    let r = run_with(
        &entries,
        &[],
        tree.clone(),
        bad,
        options(OnExisting::Merge),
        &Job::new().ctx(),
    )
    .unwrap();
    assert_eq!(tree.file(b"docs/new.txt").unwrap(), body(1));
    assert_eq!(tree.file(b"docs/a.txt").unwrap(), b"mine a");
    assert_eq!(
        exception(&r.report, "docs/a.txt"),
        Some(&ExceptionKind::Collision)
    );
    assert_eq!((r.report.merged_directories, r.report.replaced), (1, 0));
    assert!(tree.set_aside_files().is_empty());

    let tree = crowded();
    let r = run_with(
        &entries,
        &[],
        tree.clone(),
        bad,
        options(OnExisting::Replace),
        &Job::new().ctx(),
    )
    .unwrap();
    assert_eq!(tree.file(b"docs/a.txt").unwrap(), body(0));
    assert_eq!(
        tree.file(b"docs/bad.txt").unwrap(),
        b"mine bad",
        "a failed check costs nothing"
    );
    assert_eq!(
        tree.set_aside_files(),
        [("docs/a.txt".to_owned(), b"mine a".to_vec())]
    );
    assert_eq!(r.report.replaced, 1);
    assert!(tree.paths().iter().all(|p| !p.contains(".mochi-restore")));
}
