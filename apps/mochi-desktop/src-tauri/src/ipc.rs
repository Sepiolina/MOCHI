//! Types that cross the IPC boundary. Each derives [`TS`] and is exported to
//! `ui/src/ipc/generated/` (`cargo test` writes them; never edit those files
//! by hand). Counters are `u64` in Rust and `number` in TypeScript: they
//! count steps, objects, or bytes of one job and stay far below 2^53.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Identifies one job for the lifetime of the process.
pub type JobId = u32;

/// What the UI asks the job runner to do.
///
/// A request can carry a passphrase ([`Secret`]), so it is only ever
/// *deserialized*: it has no `Serialize` (nothing can write one out), its
/// `Debug` text redacts every secret, and it has no equality (D5).
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum JobRequest {
    /// A synthetic job with no archive I/O: `steps` steps of `step_ms`
    /// milliseconds each, reporting progress and honouring cancellation
    /// (plan D0 exit). Limits: 1 to 1,000 steps, at most 1,000 ms each.
    SelfTest { steps: u32, step_ms: u32 },
    /// T13: download, verify and install the update the user confirmed
    /// (`check_for_update` returned it as an offer). `version` must be the
    /// offered version; the webview names no URL. Download progress is
    /// reported in bytes under the phase `update_download`.
    InstallUpdate { version: String },
    /// D2: create a new archive at the destination the user chose
    /// (`choose_destination`) from the sources they staged (`stage_sources`,
    /// or dropped onto the window). The webview names no path.
    CreateArchive {
        preset: CompressionPreset,
        /// "Readable with standard zstd + tar" (O4): fixed at creation.
        tar_compatible: bool,
        /// D5: make the archive Encrypted (D20), wrapped under this
        /// passphrase. It crosses IPC here, once, and goes straight into a
        /// zeroizing type. Fixed at creation; conflicts with `tar_compatible`.
        #[serde(default)]
        #[ts(optional)]
        passphrase: Option<Secret>,
        /// D5: give the archive a recovery record of this many percent
        /// (D21; 1 to 100). Fixed at creation.
        #[serde(default)]
        #[ts(optional)]
        recovery_percent: Option<u16>,
        /// "Delete files after archiving" (spec Annex B.2.23 item 3): once
        /// the commit is published, move each source whose archived copy
        /// reads back and still matches it to the Recycle Bin or Trash.
        #[serde(default)]
        #[ts(optional)]
        delete_sources: Option<bool>,
    },
    /// Plan O31: write the staged sources as a plain ZIP, `.tar`, `.tar.gz`, or
    /// `.tar.zst` file at the destination the user chose (`mochi-export`; a
    /// temporary file, then a no-replace rename; an existing file is never
    /// replaced). `format` must not be `mochi`. `level` is ignored for `.tar`.
    /// The webview names no path.
    ExportArchive {
        format: ArchiveFormat,
        level: CompressionPreset,
    },
    /// D2: add the staged sources to an open archive as one new commit.
    AddToArchive {
        handle: ArchiveHandle,
        /// "Delete files after archiving" (spec Annex B.2.23 item 3), as for
        /// `CreateArchive`.
        #[serde(default)]
        #[ts(optional)]
        delete_sources: Option<bool>,
    },
    /// D2: remove entries from an open archive's current snapshot as one
    /// new commit. Their content stays in the archive's history.
    DeleteFromArchive {
        handle: ArchiveHandle,
        /// Entries of the head snapshot, as the listing returned them.
        keys: Vec<PathKey>,
    },
    /// Set or clear the archive comment of an open archive as one new commit
    /// (spec Annex B.2.19, D29): `comment` is the new text, `null` removes
    /// it. Earlier versions keep the comment they had. An Encrypted archive
    /// cannot have one.
    SetArchiveComment {
        handle: ArchiveHandle,
        comment: Option<String>,
    },
    /// D3: extract entries of one snapshot of an open archive into the folder
    /// the user chose (`plan_extract` shows the native folder dialog from
    /// Rust and remembers the choice). The webview names no path.
    ExtractArchive {
        handle: ArchiveHandle,
        /// The snapshot on screen.
        seq: u32,
        /// Entries (and what is under them) as the listing returned them;
        /// empty extracts the whole snapshot.
        keys: Vec<PathKey>,
        on_conflict: ConflictChoice,
    },
    /// D4 "Test archive": verify the chosen archive (`choose_check_subject`)
    /// read-only with `mochi_core::verify`, as `mochi verify` (or `mochi
    /// fsck` with `deep`) does. The webview names no path.
    TestArchive {
        subject: SubjectId,
        level: CheckLevel,
        /// Also open every commit at its own footer and recover every replay
        /// segment (`fsck`).
        deep: bool,
    },
    /// D4: plan a repair of the chosen archive (`mochi_core::repair`,
    /// read-only), with the copies chosen through `repair_setup` and, when
    /// `salvage`, the bounded salvage scan (step 7).
    PlanRepair {
        subject: SubjectId,
        salvage: bool,
        /// With `salvage`: the archive to keep when the scan finds several
        /// (64 hexadecimal digits). Typed by the user.
        archive_id: Option<String>,
    },
    /// D4: write the approved plan into the new file chosen through
    /// `repair_setup` (never the source, never an existing file), then
    /// re-verify it (`mochi_core::repair::apply`).
    ApplyRepair {
        subject: SubjectId,
        /// The plan the user reviewed and approved.
        plan: PlanId,
        accept_retention_loss: bool,
    },
    /// D5: search the open archive's names (`mochi_core::search`, spec
    /// §19.1); the coverage is part of the result (§19.3).
    SearchArchive {
        handle: ArchiveHandle,
        /// Bytes that must occur in a stored path; empty matches every path.
        query: String,
        /// Fold ASCII letters only (search normalization; identity is
        /// untouched).
        ignore_case: bool,
        scope: SearchScopeChoice,
        /// Only files or only folders; `null` for both.
        entry_kind: Option<EntryKindView>,
    },
    /// D5 rekey: add a passphrase to an open Encrypted archive (one commit,
    /// `mochi rekey --add-passphrase`). The new passphrase crosses once.
    AddPassphrase {
        handle: ArchiveHandle,
        passphrase: Secret,
    },
    /// D5 rekey: take one key envelope out of the head's set (one commit,
    /// `mochi rekey --remove-passphrase`). **Not revocation.**
    RemovePassphrase {
        handle: ArchiveHandle,
        /// 32 hexadecimal digits, from `list_envelopes`.
        envelope_id: String,
    },
    /// D5 rekey: write the open Encrypted archive again as a **new file**
    /// (the destination chosen with `choose_destination`) under a new data
    /// key and this passphrase (`mochi rekey --reencrypt`). The source is
    /// untouched.
    Reencrypt {
        handle: ArchiveHandle,
        passphrase: Secret,
    },
    /// D8: read every entry of an open foreign archive (ZIP, 7z, RAR, tar,
    /// tar.gz) through the format's own checks and report which checks ran.
    /// Read-only; it is not a MOCHI verification.
    TestForeign { handle: ArchiveHandle },
    /// D8: extract entries of an open foreign archive into the folder the
    /// user chose (`plan_extract_foreign` remembers the choice). Every byte is
    /// written by the C6 restore rules (`mochi_core::restore_foreign`).
    ExtractForeign {
        handle: ArchiveHandle,
        /// Entries (and what is under them) as the listing returned them;
        /// empty extracts everything.
        keys: Vec<PathKey>,
        on_conflict: ConflictChoice,
    },
    /// Scan snapshot `seq` with a virus scanner the user has (owner request
    /// 2026-11-14): extract it to a temporary folder, run `program` with
    /// `arguments` (`%f` is that folder), report, delete the folder
    /// (`scan`).
    ScanArchive {
        handle: ArchiveHandle,
        seq: u32,
        /// The scanner's full path, as the user chose or typed it.
        program: String,
        arguments: String,
    },
    /// MOCHI Find (spec Annex B.2.21): extract found entries of the window's
    /// latest scan `run`, from every archive they came from, into the folder
    /// the user chose (`find_choose_retrieve_folder`). Each item is checked
    /// against the names the scan read, and an archive that changed since is
    /// left alone. `on_conflict` is `add_new` or `replace`. The webview names
    /// no path.
    RetrieveFound {
        run: u32,
        /// The hits ticked. Empty when `all` is given.
        #[serde(default)]
        items: Vec<crate::retrieve::FoundItem>,
        /// Instead of `items`: every match of this search (or of its one
        /// name, `term`), found again in Rust, so that any number can be
        /// retrieved without listing them. Its page fields are ignored.
        #[serde(default)]
        #[ts(optional)]
        all: Option<crate::find::FindQuery>,
        layout: crate::retrieve::RetrieveLayout,
        on_conflict: ConflictChoice,
    },
}

/// Everything the runner says about one job, in order: `started`, any
/// number of `progress`, then exactly one `finished`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "event", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum JobEvent {
    Started {
        job_id: JobId,
    },
    Progress {
        job_id: JobId,
        /// A stable phase name chosen by the job.
        phase: String,
        #[ts(type = "number")]
        completed: u64,
        /// `null` while the total is not known.
        #[ts(type = "number | null")]
        total: Option<u64>,
    },
    Finished {
        job_id: JobId,
        outcome: JobOutcome,
    },
}

/// How a job ended. Only `completed` means the job did everything it was
/// asked; the UI gives no success styling to anything else (spec §23.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "status", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum JobOutcome {
    Completed {
        summary: String,
        /// Set by the jobs that publish a commit (D2).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        commit: Option<Box<CommitReport>>,
        /// Set by the extract job (D3).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        extract: Option<Box<ExtractReport>>,
        /// Set by the test and repair jobs (D4).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        check: Option<Box<CheckResult>>,
        /// Set by the search job (D5).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        search: Option<Box<SearchResultView>>,
        /// Set by the rekey jobs (D5).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        rekey: Option<Box<RekeyReport>>,
        /// Set by the foreign-archive test job (D8).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        foreign_test: Option<Box<ForeignTestReport>>,
        /// Set by the export job (plan O31): what was written, no more.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        export: Option<Box<ExportReportView>>,
        /// Set by the virus scan job.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        scan: Option<Box<crate::scan::ScanReportView>>,
        /// Set by MOCHI Find's retrieve job (B.2.21).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        retrieve: Option<Box<crate::retrieve::RetrieveReportView>>,
    },
    /// Stopped at a safe point because cancellation was requested.
    Cancelled,
    /// Ended with an error; `code` is a registered `mochi-core` error code.
    Failed { code: String, message: String },
    /// Ended without producing a result (an internal fault). Never a
    /// statement about any archive.
    Aborted { message: String },
}

/// An error returned by a command. `code` is a registered `mochi-core`
/// error code (`ErrorCode::as_str`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct IpcError {
    pub code: String,
    pub message: String,
}

impl From<mochi_core::MochiError> for IpcError {
    fn from(e: mochi_core::MochiError) -> Self {
        IpcError {
            code: e.code.as_str().to_owned(),
            message: e.message,
        }
    }
}

// ---- D1: browsing an archive ------------------------------------------------
//
// Archive-supplied strings (names, hold labels) cross as [`NameText`]: an
// escaped, single-line rendering (`mochi_core::text::escape`, the CLI's rule)
// that the UI shows as text only. Paths are identified by [`PathKey`], the
// stored bytes in hex, so a name that is not UTF-8 round-trips exactly.
// Sizes and other archive-derived 64-bit values are decimal strings: an
// archive is untrusted and may claim values above 2^53.

/// Identifies one opened archive for the lifetime of the process.
pub type ArchiveHandle = u32;

/// An archive path's stored bytes as lowercase hex.
pub type PathKey = String;

/// An archive-supplied string, rendered for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct NameText {
    /// One line, control and bidi characters and non-UTF-8 bytes escaped.
    pub text: String,
    /// `false` when `text` differs from the stored bytes (something was
    /// escaped); the UI then says so.
    pub verbatim: bool,
}

impl NameText {
    pub fn of(bytes: &[u8]) -> Self {
        NameText {
            text: mochi_core::text::escape(bytes),
            verbatim: mochi_core::text::is_verbatim(bytes),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum EntryKindView {
    File,
    Directory,
}

/// The profile fixed at the archive's creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ProfileView {
    pub tar_compatible: bool,
    pub encrypted: bool,
    /// Parity percentage of the recovery record, if the archive has one.
    pub redundancy_percent: Option<u16>,
}

/// One commit of the published history (the snapshot timeline).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct CommitView {
    /// Commit sequence numbers are verified to run 0, 1, 2, … along the
    /// parent chain, so they count commits and stay below 2^53.
    #[ts(type = "number")]
    pub seq: u64,
    pub commit_id: String,
    /// RFC 3339 UTC, or `null` when the commit recorded no time.
    pub time: Option<String>,
    pub checkpoint: bool,
    /// `null` when retention could not be determined.
    pub retained: Option<bool>,
    pub expired: Option<bool>,
    pub holds: Vec<NameText>,
}

/// An archive opened for browsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ArchiveSummary {
    pub handle: ArchiveHandle,
    /// The file's name on this computer (chosen by the user, not archive data).
    pub file_name: String,
    pub archive_id: String,
    pub profile: ProfileView,
    /// Oldest first; the last one is the head.
    pub commits: Vec<CommitView>,
    /// Why retention is unknown, when it is (`retained`/`expired` are then
    /// `null` on every commit).
    pub retention_error: Option<IpcError>,
}

/// Where an opened snapshot's catalog came from (`CatalogSource`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "source", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum CatalogSourceView {
    Image,
    /// The catalog image was damaged; the same commit's snapshot manifest
    /// stood in for it.
    SnapshotManifest {
        image_error: IpcError,
    },
}

/// One snapshot opened for browsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct SnapshotView {
    #[ts(type = "number")]
    pub seq: u64,
    pub commit_id: String,
    /// Entries in the snapshot, directories included.
    pub entries: u32,
    pub root_dirs: u32,
    pub root_files: u32,
    pub catalog_source: CatalogSourceView,
}

/// The archive Info window (owner request 2026-11-14): one snapshot's totals and
/// the writer's recorded settings. Sizes are bytes, decimal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ArchiveInfoView {
    #[ts(type = "number")]
    pub seq: u64,
    pub folders: String,
    pub files: String,
    /// Sum of the files' sizes.
    pub total_size: String,
    /// What the snapshot's distinct chunks occupy in the archive (each shared
    /// chunk once; metadata and other versions' data not counted).
    pub packed_size: String,
    /// Distinct chunks behind `packed_size`.
    pub chunks: String,
    /// The archive file's size on disk (every version and its metadata).
    pub file_size: Option<String>,
    /// The chunk size the archive's writer recorded: no chunk is larger, and
    /// each is compressed on its own, so it bounds the compression window.
    /// `null` when the archive recorded none.
    pub chunk_size: Option<String>,
    /// The Zstandard level the writer recorded, `null` when none was.
    pub zstd_level: Option<i32>,
    /// The archive comment at this version (spec Annex B.2.19, D29), exactly
    /// as written; `null` when it has none. Archive text: render as text.
    pub comment: Option<String>,
}

/// One row of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct EntryRow {
    pub key: PathKey,
    pub name: NameText,
    pub kind: EntryKindView,
    /// Logical size in bytes, decimal; "0" for a directory.
    pub size: String,
    pub child_dirs: u32,
    pub child_files: u32,
}

/// A slice of one directory's children (`mochi_core::browse` order:
/// directories first, then files, each in name byte order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ChildrenPage {
    pub total: u32,
    pub offset: u32,
    pub entries: Vec<EntryRow>,
}

/// POSIX attributes as recorded (spec D6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct PosixView {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// The promised attributes of an entry (spec §10.4.1, D6). `null` fields
/// were not recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "state", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum AttributesView {
    Recorded {
        posix: Option<PosixView>,
        /// Windows attribute bits MOCHI promises, by name.
        windows: Option<Vec<String>>,
        /// RFC 3339 UTC; `null` if not recorded or not representable.
        mtime: Option<String>,
    },
    /// The attributes could not be read (for example a damaged manifest);
    /// they are not guessed.
    Unavailable { error: IpcError },
}

/// Everything the details pane shows for one entry. Listing reads the
/// catalog only: the content hash is the recorded one, not a verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct EntryDetails {
    pub key: PathKey,
    pub path: NameText,
    pub kind: EntryKindView,
    pub size: String,
    /// Plain BLAKE3 of the content (O20), hex; `null` for a directory.
    pub content_hash: Option<String>,
    pub file_version_id: String,
    pub attributes: AttributesView,
}

// ---- D2: create and add -----------------------------------------------------
//
// The webview never names a path. The user picks sources in a native dialog
// shown from Rust, or drops them on the window (Rust receives the window's
// drop event); Rust keeps the paths and the webview sees only display names.

/// How hard the compressor works (Zstandard level; spec §7 leaves the level to
/// the writer, and readers cannot tell the difference). Not a format choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum CompressionPreset {
    /// Level -4 (`zstd --fast=4`): quickest, largest.
    Turbo,
    /// Level -1 (`zstd --fast=1`).
    VeryFast,
    /// Level 1.
    Fast,
    /// The writer's default level (3).
    Balanced,
    /// Level 19.
    Maximum,
    /// Level 20 (the first of zstd's `--ultra` levels).
    Ultra,
    /// Level 22 (zstd's highest): slowest, smallest, most memory.
    Extreme,
}

/// The words of a native dialog, in the language the window is showing
/// (`mochi-desktop/ui/src/i18n`). The native dialog is Rust's, so the webview
/// can only pass text; every field is optional and an absent, empty, too long
/// or control-character-bearing one falls back to the English default, so the
/// webview can never put anything but a short plain label on a dialog. The
/// extensions a filter matches are Rust's and are not passed.
#[derive(Debug, Clone, Default, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct DialogText {
    /// The dialog's title.
    #[serde(default)]
    #[ts(optional)]
    pub title: Option<String>,
    /// The label of the dialog's own file type (what the dialog is for).
    #[serde(default)]
    #[ts(optional)]
    pub filter: Option<String>,
    /// The label of the "MOCHI archive" file type, where the dialog lists others too.
    #[serde(default)]
    #[ts(optional)]
    pub mochi_archive: Option<String>,
    /// The label of the "All files" file type.
    #[serde(default)]
    #[ts(optional)]
    pub all_files: Option<String>,
}

impl DialogText {
    /// The longest label accepted, in characters. Longer is refused, not cut.
    pub const MAX_CHARS: usize = 120;

    /// `value` if it is a short plain string, else `fallback`.
    fn plain(value: &Option<String>, fallback: &str) -> String {
        match value.as_deref().map(str::trim) {
            Some(v)
                if !v.is_empty()
                    && v.chars().count() <= Self::MAX_CHARS
                    && !v.chars().any(char::is_control) =>
            {
                v.to_owned()
            }
            _ => fallback.to_owned(),
        }
    }

    pub fn title(&self, fallback: &str) -> String {
        Self::plain(&self.title, fallback)
    }

    pub fn filter(&self, fallback: &str) -> String {
        Self::plain(&self.filter, fallback)
    }

    pub fn mochi_archive(&self) -> String {
        Self::plain(&self.mochi_archive, "MOCHI archive")
    }

    pub fn all_files(&self) -> String {
        Self::plain(&self.all_files, "All files")
    }
}

/// Which native dialog `stage_sources` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum SourcePick {
    Files,
    Folder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum StagedKind {
    File,
    Folder,
    /// A link, device, or other entry that is not stored (it is reported
    /// when the commit is made).
    Other,
}

/// One staged source, by the name it would have in the archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct StagedItem {
    pub name: NameText,
    pub kind: StagedKind,
}

/// What is staged to add: the sources the next create or add would store,
/// and where a new archive would be written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct StagedView {
    pub items: Vec<StagedItem>,
    /// File name of the chosen destination for a new archive, if chosen.
    pub destination: Option<String>,
    /// Where a new archive would be written if the user does not choose: next
    /// to the first item, never over an existing file. `None` when nothing is
    /// staged, a destination was chosen, or no default can be made.
    pub default_destination: Option<String>,
    /// The format the next new archive is written in (plan O31); `mochi` until
    /// the user chooses another. Names above follow its extension.
    pub format: ArchiveFormat,
}

/// A folder the window asks Rust to show in the file manager. The window
/// never names a path: Rust resolves the folder from its own state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum ShowTarget {
    /// The folder the last extraction went to.
    ExtractDestination,
    /// The folder that holds an open archive.
    Archive { handle: ArchiveHandle },
    /// The folder the last ZIP / tar export was written into (plan O31).
    LastExport,
}

/// What reaches the webview about drag and drop (Rust handles the window's
/// event; the webview needs no permission to hear it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "event", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum DropEvent {
    /// Something is being dragged over (or has left) the window.
    Hover { active: bool },
    /// Items were dropped. `staged` is the whole staged set after adding
    /// them; `problems` explains each dropped item that was not staged.
    Dropped {
        staged: StagedView,
        problems: Vec<IpcError>,
        /// Staged by the shell's "Add to archive" (D6), not dropped on the
        /// window: the window asks before it adds, where a drop on an open
        /// archive adds at once.
        from_shell: bool,
    },
    /// One archive file was dropped: a `.mochi` file or a ZIP, 7z, RAR, tar,
    /// tar.gz or tar.zst file (`foreign`). It is not staged yet: the window
    /// asks Rust to open it (`open_dropped_archive`) or, to add the file to an
    /// archive as an ordinary source, to stage it (`stage_dropped_archive`).
    /// Only the file name is shown.
    Archive { name: String, foreign: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum CommitOperation {
    Create,
    Add,
    Delete,
    /// The archive comment was set or cleared (D29); no entry changed.
    Comment,
}

/// A source entry a commit left out, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct SkippedView {
    pub path: NameText,
    pub reason: String,
}

/// The commit a create, add, or delete job published. `status` is always
/// `LOCAL_COMMITTED` (spec §5.3: nothing in 1.0 reports more).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct CommitReport {
    pub operation: CommitOperation,
    pub status: String,
    #[ts(type = "number")]
    pub seq: u64,
    pub commit_id: String,
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub directories: u64,
    /// Content bytes read from the sources.
    #[ts(type = "number")]
    pub bytes: u64,
    /// Entries removed from the snapshot (a delete).
    #[ts(type = "number")]
    pub removed: u64,
    pub skipped: Vec<SkippedView>,
    /// "Delete files after archiving" (Annex B.2.23 item 3): sources moved
    /// to the Recycle Bin or Trash (files and emptied folders). 0 when it
    /// was not asked for.
    #[ts(type = "number")]
    pub sources_moved: u64,
    /// Sources kept although it was asked for, each with its reason (its
    /// archived copy did not read back, it changed, a folder is not empty, …).
    pub sources_kept: Vec<SkippedView>,
    /// Set when the new file's directory entry is not confirmed durable
    /// (the commit is reported degraded, never durable).
    pub durability_note: Option<String>,
    /// The archive as it now stands, reopened for browsing (same handle as
    /// before for an add or delete; a new one for a create). `null` if it
    /// could not be reopened; `reopen_error` says why.
    pub archive: Option<ArchiveSummary>,
    pub reopen_error: Option<IpcError>,
}

/// The format of a new archive the desktop app writes (plan O31, owner
/// decision 2026-10-20): `.mochi`, or one of four plain formats written by
/// `mochi-export`. RAR is never written; 7z is not written now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum ArchiveFormat {
    #[default]
    Mochi,
    Zip,
    Tar,
    TarGz,
    TarZst,
}

/// What an export wrote (plan O31). It says what ran and no more: no commit
/// status, no health dimension, no verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ExportReportView {
    pub format: ArchiveFormat,
    /// The new file's name.
    pub name: NameText,
    /// Its full path, as display text.
    pub location: NameText,
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub directories: u64,
    /// Content bytes read from the sources.
    #[ts(type = "number")]
    pub bytes: u64,
    /// The size of the new file.
    #[ts(type = "number")]
    pub output_bytes: u64,
    /// Entries left out (a link, a device, a name the format cannot hold, an
    /// unreadable file) and why.
    pub skipped: Vec<SkippedView>,
    /// The directory entry is not confirmed durable (degraded).
    pub durability_note: Option<String>,
}

/// The staged set after `stage_sources`, and any source that could not be
/// staged (the rest still were).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct StageOutcome {
    pub staged: StagedView,
    pub problems: Vec<IpcError>,
}

// ---- D3: extract ------------------------------------------------------------
//
// The folder is chosen in a native dialog shown from Rust and kept there; the
// webview sees its display text and what the destination holds, never names a
// path. Archive-supplied strings cross as [`NameText`], as in D1.

/// What an extract does about names that cannot be extracted into the folder
/// (an entry with that name is already there, the folder treats two names as
/// one, or the file system cannot hold the name).
///
/// The restore engine (plan C6) has exactly one behaviour for these: the entry
/// is skipped, nothing is overwritten, and no name is changed. Keeping both
/// under a new name and replacing need engine support that does not exist
/// (plan D3 status, "Gap").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum ConflictChoice {
    /// End the job before anything is written if any entry cannot be
    /// extracted (what the plan showed was none).
    Stop,
    /// Skip those entries (and what is under a skipped folder), extract the
    /// rest, and report each one. An existing folder is one of them.
    SkipConflicts,
    /// "Add only new files" (owner decision 2026-11-11): enter folders that
    /// already exist and add what they lack; an existing file keeps its name
    /// and its bytes and is skipped. Entries that cannot be extracted at all
    /// are skipped and reported, as with `SkipConflicts`.
    AddNew,
    /// "Replace existing files" (owner decision 2026-11-11): as `AddNew`, and
    /// an existing file is moved to the Recycle Bin / Trash, once the
    /// extracted file has been checked, and replaced by it. Never a folder,
    /// never a link.
    Replace,
}

impl ConflictChoice {
    /// What the restore engine does with a name that already exists.
    pub fn on_existing(self) -> mochi_core::restore::OnExisting {
        use mochi_core::restore::OnExisting;
        match self {
            Self::Stop | Self::SkipConflicts => OnExisting::Keep,
            Self::AddNew => OnExisting::Merge,
            Self::Replace => OnExisting::Replace,
        }
    }
}

/// One entry that cannot be extracted into the chosen folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ConflictView {
    pub path: NameText,
    /// `NAME_COLLISION` or `NAME_UNSUPPORTED`.
    pub code: String,
    /// Why, as one sentence.
    pub reason: String,
    /// Why, as one of a few kinds the window words in the user's language.
    pub why: ConflictWhy,
}

/// Why an entry cannot be extracted into the chosen folder, whatever is chosen
/// (owner decision 2026-11-11: the window says it in a few plain words).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum ConflictWhy {
    /// The folder's file system cannot hold the name.
    NameNotAllowed,
    /// Something of the other kind is there: a file where the archive has a
    /// folder or the reverse, a link, a device.
    OtherKind,
    /// A folder is there that MOCHI does not add to: a link, or (Linux) one
    /// that other users can change.
    FolderNotEntered,
    /// The archive holds another entry the folder would treat as the same name.
    SameNameInArchive,
    /// Anything else; `reason` says what.
    Other,
}

/// What extracting would meet in the chosen folder, found before anything is
/// written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ExtractPlan {
    /// The folder the user chose (their own path, not archive data).
    pub destination: String,
    /// Entries the extraction handles: the selection, what is under it, and
    /// the folders above it.
    pub entries: u32,
    /// Files that already exist in the folder (owner decision 2026-11-11):
    /// "Add only new files" keeps them, "Replace existing files" moves them to
    /// the Recycle Bin / Trash. The first ones in path order (at most 200),
    /// as archive paths.
    pub existing: Vec<NameText>,
    pub existing_total: u32,
    /// Folders that already exist and that the extraction adds to.
    pub merged_folders: u32,
    /// Entries that cannot be extracted here whatever is chosen (a name this
    /// folder cannot hold, a file where the archive has a folder or the
    /// reverse, a link): the first ones, in path order (at most 200).
    pub conflicts: Vec<ConflictView>,
    pub conflicts_total: u32,
    /// Whether the folder treats names that differ only in case as one.
    pub case_insensitive: bool,
}

/// One entry an extraction did not restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct NotExtractedView {
    pub path: NameText,
    /// A registered error code (`NAME_COLLISION`, `NAME_UNSUPPORTED`,
    /// `CONTENT_INTEGRITY_FAILED`, …).
    pub code: String,
    pub reason: NameText,
}

/// An attribute that was not applied to extracted entries, summarised per
/// kind (plan O6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct AttributeNote {
    /// What was not applied, in words ("owner and group", …).
    pub attribute: String,
    #[ts(type = "number")]
    pub count: u64,
    pub first_path: NameText,
    pub reason: NameText,
}

/// What an extract job did. It is not a verification: it speaks only of the
/// entries it extracted (spec §23.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ExtractReport {
    pub destination: String,
    pub seq: u32,
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub directories: u64,
    /// Content bytes written to extracted files.
    #[ts(type = "number")]
    pub bytes: u64,
    /// The first entries not extracted (at most 1,000).
    pub not_extracted: Vec<NotExtractedView>,
    #[ts(type = "number")]
    pub not_extracted_total: u64,
    pub attribute_notes: Vec<AttributeNote>,
    /// Why no attribute was applied, when the promised attributes could not
    /// be reconstructed.
    pub attributes_unavailable: Option<NameText>,
    /// Set when the folder's directory entries are not confirmed durable.
    pub durability_note: Option<String>,
    /// Fast extract was on: files were not flushed one by one, so a power
    /// failure soon after could leave some incomplete.
    pub files_not_flushed: bool,
    pub case_insensitive: bool,
    /// The folder was opened in the file manager when the job ended ("Open the folder when
    /// done", owner request 2026-11-10).
    #[serde(default)]
    pub opened_folder: bool,
    /// Existing files that were moved to the Recycle Bin / Trash and replaced
    /// ("Replace existing files", owner decision 2026-11-11); counted in `files`.
    #[serde(default)]
    #[ts(type = "number")]
    pub replaced: u64,
    /// Folders that already existed and were added to.
    #[serde(default)]
    #[ts(type = "number")]
    pub merged_folders: u64,
}

// ---- D4: test, health, and repair -------------------------------------------
//
// The archive to test or repair, the copies, and the new file are chosen in
// native dialogs shown from Rust and kept there; the webview refers to them by
// ID. Reports are converted from `mochi_core::report::Report` (schema v1) for
// display, and the exact JSON core produced travels with them (`json`) and is
// what "Export report JSON" writes. Archive-supplied text inside findings and
// plans crosses as [`NameText`].

/// Identifies the archive chosen for testing, health, or repair.
pub type SubjectId = u32;
/// Identifies a report kept for export.
pub type ReportId = u32;
/// Identifies a repair plan kept for approval.
pub type PlanId = u32;

/// The verification levels `mochi verify --level` offers that this build
/// runs (spec §20.1). Inventory, search, and disaster recovery are
/// `UNSUPPORTED` in 1.0 and not offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum CheckLevel {
    Structural,
    Referential,
    StoredIntegrity,
    ContentIntegrity,
    Restoration,
}

/// Where the archive to test or repair comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "from", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum SubjectSource {
    /// The native open dialog, shown from Rust (any file: an archive too
    /// damaged, or an Encrypted one, cannot be opened in the browser).
    Dialog,
    /// The file of an archive open in the browser.
    OpenArchive { handle: ArchiveHandle },
    /// The new archive the last repair wrote.
    RepairOutput,
}

/// The archive chosen for testing, health, or repair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct SubjectView {
    pub id: SubjectId,
    /// The file's name on this computer (the user's, not archive data).
    pub file_name: String,
    /// The head commit says the archive is Encrypted (D20), so a full check
    /// or a repair needs its passphrase (D5). Plaintext commit records say so
    /// without a key.
    pub encrypted: bool,
    /// Rust holds a key that opened this archive: a check runs keyed and a
    /// repair is possible. Never `true` for a Core archive.
    pub unlocked: bool,
}

/// A spec §20.4 status value, spelled as in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum StatusView {
    Pass,
    Fail,
    Degraded,
    Unknown,
    Overdue,
    Unsupported,
}

/// One health dimension of a report (spec §20.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct DimensionView {
    /// As in the report (`integrity`, `key_availability`, …).
    pub dimension: String,
    pub status: StatusView,
    /// The report's policy requires it.
    pub required: bool,
}

/// One finding of a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct FindingView {
    /// A registered error code.
    pub code: String,
    /// `info`, `warning`, or `error`.
    pub severity: String,
    pub message: Option<NameText>,
    pub expected: Option<NameText>,
    pub observed: Option<NameText>,
    /// The commits affected, as an inclusive range.
    pub affected: Option<SeqRangeView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct SeqRangeView {
    #[ts(type = "number")]
    pub first: u64,
    #[ts(type = "number")]
    pub last: u64,
}

/// Something a report did not check, or a note, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ReportNoteView {
    pub item: NameText,
    pub reason: NameText,
}

/// Objects and bytes checked versus expected, as decimal strings; `null`
/// means not measured, which is not zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct CoverageView {
    pub expected_objects: Option<String>,
    pub checked_objects: Option<String>,
    pub expected_bytes: Option<String>,
    pub checked_bytes: Option<String>,
    pub evidence_age_seconds: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum ReportKind {
    /// "Test archive" (`verify`, or `fsck` when deep).
    Test,
    /// The health summary (`mochi_core::health`).
    Health,
}

/// A report schema v1 (spec §20.5) for display, and its exact JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ReportView {
    /// Pass to `export_report`.
    pub report_id: ReportId,
    pub kind: ReportKind,
    /// The file that was checked (the user's name for it).
    pub file_name: String,
    /// Also opened every commit and recovered every segment (`fsck`).
    pub deep: bool,
    /// The archive is Encrypted and was checked without its key (stored
    /// integrity only; names, content, and recoverability need the
    /// passphrase, which the app cannot take until D5).
    pub keyless: bool,
    pub schema_version: u32,
    pub archive_id: Option<String>,
    pub checked_commit: Option<String>,
    pub expected_head: Option<String>,
    /// `none`, `user`, or `local-history`.
    pub freshness_anchor: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    /// As in the report (`restoration`, `stored_integrity`, …).
    pub level: String,
    pub scope: Option<NameText>,
    /// In the report's order.
    pub dimensions: Vec<DimensionView>,
    pub overall_status: StatusView,
    pub policy_result: StatusView,
    pub exit_code: u8,
    pub operational_error: bool,
    pub coverage: CoverageView,
    pub findings: Vec<FindingView>,
    pub skipped: Vec<ReportNoteView>,
    /// The report exactly as `mochi verify --json` prints it (one line and a
    /// newline). "Export report JSON" writes these bytes unchanged.
    pub json: String,
}

/// The health summary (spec §20.3, §21; `mochi_core::health::assess`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct HealthView {
    pub report: ReportView,
    /// Evidence records of this archive that this app recorded since it
    /// started (nothing is saved; plan D4 "Gap").
    pub records: u32,
}

/// What the copies, the new file, and the plan of a repair are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct RepairSetupView {
    /// File names of the independent copies, in the order given (step 5).
    pub copies: Vec<String>,
    /// File name of the new archive the repair would write, if chosen.
    pub output: Option<String>,
}

/// Changes to a repair's setup. Files are chosen in native dialogs shown from
/// Rust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum RepairSetupAction {
    /// Show the open dialog and add the chosen file as a copy (`--copy`).
    AddCopy,
    RemoveCopy {
        index: u32,
    },
    ClearCopies,
    /// Show the save dialog for the new archive.
    ChooseOutput,
    /// Only report the setup.
    Show,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum RepairOutcomeView {
    /// Every snapshot and entry the archive's history holds, with retention.
    Complete,
    /// Something is left out, or the bounded salvage scan was used.
    Partial,
    /// Nothing can be written.
    NothingRecoverable,
}

/// One rung of the §22 ladder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct LadderStepView {
    pub step: u8,
    pub name: String,
    /// `used`, `not_needed`, `not_available`, `not_attempted`, or
    /// `incomplete`.
    pub status: String,
    pub detail: NameText,
}

/// A snapshot the plan writes, one commit of the new archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct PlannedSnapshotView {
    #[ts(type = "number")]
    pub seq: u64,
    /// `null` before a chain break: the commit record is not trusted.
    pub commit_id: Option<String>,
    pub entries: String,
    pub omitted: String,
    /// `image` or `snapshot_manifest`.
    pub catalog: String,
    /// `chain`, `copy_confirmed`, or `salvage`.
    pub trust: String,
}

/// A snapshot that is not written, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct LostSnapshotView {
    #[ts(type = "number")]
    pub seq: u64,
    pub commit_id: Option<String>,
    pub code: String,
    pub reason: NameText,
}

/// An entry left out of every written snapshot that held it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct OmittedView {
    pub path: NameText,
    /// `file` or `directory`.
    pub kind: String,
    /// The snapshots that held it.
    #[ts(type = "Array<number>")]
    pub snapshots: Vec<u64>,
    pub code: String,
    pub reason: NameText,
}

/// What a repair does with the source's retention state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "state", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum RetentionView {
    Carried {
        holds: u32,
        /// Holds on snapshots that are lost: they are lost too.
        holds_lost: u32,
        expired: u32,
    },
    /// Not rebuildable: applying needs the user to accept writing the new
    /// archive with no holds and nothing expired.
    Unresolved { code: String, reason: NameText },
}

/// An independent copy (step 5) and what it did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct CopyView {
    pub file_name: String,
    /// `used`, `not_needed`, `not_available`, or `unreadable`.
    pub status: String,
    pub frames_donated: String,
    pub detail: NameText,
}

/// The parity scan (step 6) of an archive with a recovery record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ParityView {
    pub declared_percent: Option<u16>,
    pub shards_damaged: String,
    pub shards_rebuilt: String,
    pub groups_beyond_capacity: String,
}

/// The bounded salvage scan (step 7), when it ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct SalvageView {
    pub confirmed_commits: String,
    /// Never recovered: nothing confirms their publication.
    pub unconfirmed_commits: String,
    pub orphan_data_objects: String,
    pub limit_reached: bool,
}

/// A repair plan (`mochi_core::repair::RepairPlan`) for review. Nothing was
/// written to make it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct RepairPlanView {
    /// Pass to `ApplyRepair` to approve exactly this plan.
    pub plan_id: PlanId,
    pub file_name: String,
    pub outcome: RepairOutcomeView,
    /// As `mochi repair plan` exits: 0 complete, 2 partial, 1 nothing.
    pub exit_code: u8,
    pub archive_id: Option<String>,
    /// How the head was found: `eof`, `scan`, or `null` when none was.
    pub head_found_by: Option<String>,
    #[ts(type = "number | null")]
    pub head_seq: Option<u64>,
    /// `clean`, `uncommitted`, or `unresolved`, and how many bytes.
    pub tail_state: Option<String>,
    pub tail_len: Option<String>,
    /// The first trusted commit after a break in the chain, and why.
    #[ts(type = "number | null")]
    pub chain_break_at: Option<u64>,
    pub chain_break_reason: Option<NameText>,
    pub ladder: Vec<LadderStepView>,
    pub snapshots: Vec<PlannedSnapshotView>,
    pub lost: Vec<LostSnapshotView>,
    /// The first entries left out (at most 1,000); the total is always given.
    pub omitted: Vec<OmittedView>,
    pub omitted_total: String,
    pub manifest_failures: u32,
    /// `null` when nothing is recoverable.
    pub retention: Option<RetentionView>,
    pub parity: Option<ParityView>,
    pub copies: Vec<CopyView>,
    pub donations: String,
    pub salvage: Option<SalvageView>,
    /// A snapshot the plan writes rests on the salvage scan.
    pub salvage_used: bool,
    pub damage_found: bool,
}

/// One commit a repair wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct RepairedCommitView {
    #[ts(type = "number")]
    pub source_seq: u64,
    #[ts(type = "number")]
    pub new_seq: u64,
    pub entries: String,
    pub omitted: String,
    pub trust: String,
}

/// The independent verification of the new archive before it was published
/// (spec §22.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ReverificationView {
    pub level: String,
    pub overall_status: StatusView,
    pub policy_result: StatusView,
    pub exit_code: u8,
    pub findings: String,
}

/// What a repair wrote (`mochi_core::repair::RepairReport`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct RepairResultView {
    pub outcome: RepairOutcomeView,
    pub exit_code: u8,
    /// The new file (the user's name for it).
    pub file_name: String,
    pub new_archive_id: String,
    pub new_len: String,
    pub commits: Vec<RepairedCommitView>,
    pub lost: Vec<LostSnapshotView>,
    pub omitted: Vec<OmittedView>,
    pub omitted_total: String,
    pub profile_constraints_lost: Vec<String>,
    pub retention_loss_accepted: bool,
    pub reverification: ReverificationView,
    pub durability_note: Option<String>,
    pub source_kept: bool,
    pub salvage_used: bool,
}

/// What a D4 job produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum CheckResult {
    Test { report: ReportView },
    Plan { plan: RepairPlanView },
    Repair { result: RepairResultView },
}

// ---- D5: passphrases, rekey, search -----------------------------------------
//
// A passphrase crosses IPC **once per use**, inside the command or job request
// that needs it, and goes straight into a zeroizing type. It is never
// serialized (no type here that can hold one implements `Serialize`), never
// `Debug`-printed (the text is redacted), and never part of an error. What
// Rust keeps afterwards is a `mochi_core::keys::KeySession` (zeroizing, in
// Rust memory only) attached to the open archive, so the webview never has to
// hold the passphrase again.

/// A passphrase as it arrives over IPC.
#[derive(Clone, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/", type = "string")]
pub struct Secret(zeroize::Zeroizing<String>);

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(|s| Secret(zeroize::Zeroizing::new(s)))
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Secret {
    /// Wrap text (tests; the IPC path is `Deserialize`).
    pub fn new(text: &str) -> Self {
        Secret(zeroize::Zeroizing::new(text.to_owned()))
    }

    /// The passphrase as `mochi-core` takes it: NFC-normalised, 1 to 4,096
    /// bytes (D20 item 2). The error never contains the text.
    pub fn passphrase(&self) -> mochi_core::Result<mochi_format::secret::Passphrase> {
        mochi_format::secret::Passphrase::new(&self.0).map_err(mochi_core::MochiError::from)
    }
}

/// Identifies an archive that is waiting for its passphrase.
pub type PendingId = u32;

/// What `open_archive` and `unlock_archive` produce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "status", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum OpenOutcome {
    Opened {
        archive: ArchiveSummary,
    },
    /// The archive is Encrypted (D20): names, sizes, and content stay sealed
    /// until a passphrase opens it. Rust remembers the file; the webview
    /// answers with `unlock_archive`.
    NeedsPassphrase {
        pending: PendingId,
        /// The file's name on this computer.
        file_name: String,
    },
    /// The file was a ZIP, 7z, RAR, tar, tar.gz or tar.zst archive and is
    /// open read-only (D8). Its handle belongs to the foreign-archive
    /// commands (`foreign_children`, `close_foreign_archive`, ...).
    Foreign {
        archive: ForeignArchiveSummary,
    },
    /// The foreign archive needs a password (or the one given was wrong).
    /// Rust remembers the file: the window answers with
    /// `open_foreign_archive(retry = true, password)`.
    ForeignPassword {
        /// `true` when a password was given and did not open it.
        wrong: bool,
        /// The file's name on this computer.
        file_name: String,
    },
    /// The archive was opened in another window (this one already shows an
    /// archive, or a job is running in it), or that window already had it
    /// open and was brought forward. There is nothing to show here.
    Elsewhere,
}

/// One key envelope valid at the head (`mochi rekey --list`; needs no key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct EnvelopeView {
    /// 32 hexadecimal digits: what `RemovePassphrase` takes.
    pub envelope_id: String,
    /// The commit that wrote the envelope.
    #[ts(type = "number")]
    pub written_by_commit: u64,
    /// "argon2id m=64 MiB t=3 p=4".
    pub kdf: String,
    /// The passphrase that opened the archive in this session unlocked the
    /// data key through this envelope.
    pub opened_with: bool,
}

/// The head's key envelopes of an open Encrypted archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct EnvelopeList {
    pub envelopes: Vec<EnvelopeView>,
}

/// What a rekey job did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum RekeyAction {
    Added,
    Removed,
    Reencrypted,
}

/// The result of a rekey job. A rewrap is one commit (`LOCAL_COMMITTED`); a
/// re-encryption is a new file. Neither is revocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct RekeyReport {
    pub action: RekeyAction,
    /// The commit of a rewrap.
    #[ts(type = "number | null")]
    pub seq: Option<u64>,
    pub status: Option<String>,
    /// The archive that was changed, or the new file.
    pub file_name: String,
    /// Snapshots written to the new file (a re-encryption).
    #[ts(type = "number")]
    pub snapshots: u64,
    pub new_archive_id: Option<String>,
    pub durability_note: Option<String>,
    /// The archive as it now stands (a rewrap), `null` if it could not be
    /// reopened with the key held; `reopen_error` says why (for example
    /// after the passphrase in use was removed).
    pub archive: Option<ArchiveSummary>,
    pub reopen_error: Option<IpcError>,
}

/// Which snapshots a search covers (`SnapshotScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "scope", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum SearchScopeChoice {
    Head,
    Retained,
    All,
    Commit { seq: u32 },
}

/// A run of consecutive commit sequence numbers, inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct SeqRange {
    #[ts(type = "number")]
    pub from: u64,
    #[ts(type = "number")]
    pub to: u64,
}

/// One entry of one snapshot that matched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct HitView {
    #[ts(type = "number")]
    pub seq: u64,
    /// The stored path, escaped.
    pub path: NameText,
    /// The same key `list_children` returns for this entry. It is valid for
    /// selecting and extracting in the hit's own snapshot (`seq`), which may
    /// not be the one the window is showing.
    pub key: PathKey,
    /// Key of the containing folder; `null` for an entry at the root.
    pub parent: Option<PathKey>,
    pub kind: EntryKindView,
    /// Decimal string.
    pub size: String,
}

/// A requested snapshot that could not be searched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct UnavailableView {
    #[ts(type = "number")]
    pub seq: u64,
    pub code: String,
    pub message: NameText,
}

/// What a search covered and what it did not (spec §19.3, §23.3 #8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct SearchCoverageView {
    /// `searched == requested` and nothing pending, failed, unsupported, or
    /// excluded: the only state in which "no matches" means none.
    pub complete: bool,
    pub requested: Vec<SeqRange>,
    pub searched: Vec<SeqRange>,
    #[ts(type = "number")]
    pub requested_count: u64,
    #[ts(type = "number")]
    pub searched_count: u64,
    pub unavailable: Vec<UnavailableView>,
    /// Commits whose own catalog was opened because the head's catalog was
    /// rebuilt from a snapshot manifest.
    pub opened_separately: Vec<SeqRange>,
    #[ts(type = "number")]
    pub entries_examined: u64,
    /// The commit the catalog was opened at.
    #[ts(type = "number")]
    pub indexed_seq: u64,
    /// The catalog image was damaged; a snapshot manifest stood in.
    pub catalog_from_manifest: bool,
    /// Always `true` in this build: file content was not searched (§19.2).
    pub content_not_searched: bool,
}

/// A search's hits (at most [`crate::search::MAX_HITS_SHOWN`] of them) and
/// its coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct SearchResultView {
    pub scope: SearchScopeChoice,
    pub hits: Vec<HitView>,
    /// All hits found, shown or not.
    #[ts(type = "number")]
    pub total_hits: u64,
    pub coverage: SearchCoverageView,
}

// ---- D8: foreign archives ----------------------------------------------------
//
// ZIP, 7z, RAR, tar, tar.gz and tar.zst open read-only. Nothing here uses MOCHI's
// verification wording or health dimensions (plan D8 rule 6): a foreign
// "test" reports the checks that ran, recoverability/freshness/retention are
// fixed at UNSUPPORTED. Names cross as `NameText`, as in D1.

/// The MOCHI health dimensions a foreign format cannot have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ForeignDimensionsView {
    pub recoverability: String,
    pub freshness: String,
    pub retention: String,
}

/// A foreign archive opened for browsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ForeignArchiveSummary {
    pub handle: ArchiveHandle,
    /// The file's name on this computer (chosen by the user).
    pub file_name: String,
    /// `ZIP`, `7z`, `RAR`, `tar`, `tar.gz`, or `tar.zst`.
    pub format: String,
    #[ts(type = "number")]
    pub entries: u64,
    /// `false` when the listing stopped early; `stop_reason` says why. A
    /// partial listing is labelled partial, never presented as the whole.
    pub complete: bool,
    pub stop_reason: Option<NameText>,
    /// Limit breaches met while listing, one sentence each.
    pub notes: Vec<String>,
    /// What this format checks per entry, in words ("CRC32 per entry").
    pub checks: String,
    /// The password the user gave opened the archive (names encrypted).
    pub opened_with_password: bool,
    /// Some entries use ZipCrypto, which is weak.
    pub weak_encryption: bool,
    pub dimensions: ForeignDimensionsView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum ForeignKindView {
    File,
    Directory,
    /// A symbolic or hard link: listed, never extracted.
    Link,
    /// A device, pipe, or other special entry: listed, never extracted.
    Special,
}

/// One row of a foreign archive's listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ForeignRow {
    /// Selects or descends; `null` for an entry whose name was refused (it is
    /// shown, but cannot be selected).
    pub key: Option<PathKey>,
    pub name: NameText,
    pub kind: ForeignKindView,
    /// Declared size in bytes, decimal (a claim, not a measurement).
    pub size: Option<String>,
    pub child_dirs: u32,
    pub child_files: u32,
    /// `AES-256`, `weak encryption (ZipCrypto)`, ...
    pub encryption: Option<String>,
    /// Why this entry will not be extracted, when it will not be.
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ForeignChildrenPage {
    pub total: u32,
    pub offset: u32,
    pub entries: Vec<ForeignRow>,
}

/// One entry of a foreign archive whose full path matched a search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ForeignHit {
    /// Exactly the row `foreign_children` returns for this entry, so its key
    /// selects, extracts, and drags like any listed row.
    pub row: ForeignRow,
    /// The full path inside the archive, escaped for display.
    pub path: NameText,
    /// Key of the containing folder (what `foreign_children` takes as `dir`);
    /// `null` for an entry at the root, and for one whose name was refused.
    pub parent: Option<PathKey>,
}

/// What `foreign_search` found: names only, never file contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ForeignSearchResult {
    /// At most [`crate::foreign::MAX_SEARCH_HITS`] hits, folders and files
    /// alike, in path order.
    pub hits: Vec<ForeignHit>,
    /// All matches, shown or not.
    pub total_hits: u32,
}

/// What `plan_extract_foreign` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ForeignExtractPlan {
    pub plan: ExtractPlan,
    /// Links and special entries in the selection: always skipped.
    pub skipped_kinds: u32,
}

/// How many entries ended in one way, by the words the format's check used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct CheckCount {
    /// "CRC32 matched", "read without error; this format has no per-entry
    /// check", "password required", ...
    pub what: String,
    #[ts(type = "number")]
    pub count: u64,
}

/// An entry whose read did not end cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ForeignProblem {
    pub path: NameText,
    pub what: NameText,
}

/// What a foreign test job found. It names the checks that ran; it is not a
/// verification and says nothing about recoverability, freshness, or
/// retention (spec §23.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct ForeignTestReport {
    pub format: String,
    #[ts(type = "number")]
    pub entries: u64,
    pub checks: Vec<CheckCount>,
    /// The first entries that did not end cleanly (at most 1,000).
    pub problems: Vec<ForeignProblem>,
    #[ts(type = "number")]
    pub problems_total: u64,
    /// The whole-stream check, where the format has one.
    pub stream_check: String,
    pub complete: bool,
    pub stop_reason: Option<NameText>,
    #[ts(type = "number")]
    pub bytes_read: u64,
    pub dimensions: ForeignDimensionsView,
}

// ---- D7: crash-report policy ------------------------------------------------
//
// Opt-in and local: nothing is sent anywhere (`crashlog` module docs).

/// How many processor cores a write may use to compress (release 0.9.11),
/// and whether extraction skips the per-file flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct SpeedSettingsView {
    /// Compress with every core (at most 8). On by default.
    pub all_cores: bool,
    /// Fast extract: publish each verified file without flushing it first.
    /// Off by default.
    pub fast_extract: bool,
    /// The cores "every core" means on this computer.
    pub cores: u32,
    /// Windows: a folder flush on a local NTFS drive counts as saved when an
    /// archive is created (spec Annex B.2.20). Off by default (policy B).
    pub trust_ntfs_flush: bool,
    /// Remember archives on this computer: each one's last-seen head (for the
    /// freshness check) and the results of tests and commits, in the state
    /// directory the command-line tool shares (spec Annex B.2.22). On by
    /// default.
    pub local_history: bool,
    /// Adding to an archive another MOCHI program is writing to waits up to
    /// two minutes for it instead of refusing at once (spec Annex B.2.23).
    /// Off by default.
    pub wait_for_writer: bool,
    /// Whether that setting applies here (Windows only); the UI shows it then.
    pub windows: bool,
}

/// The crash-log switches and what the log holds now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct CrashSettingsView {
    /// Write a local record when the app panics. Off by default.
    pub enabled: bool,
    /// Also record the panic message, which can quote a file name. Off by default.
    pub include_details: bool,
    /// Records in the log on disk.
    pub entries: u32,
    /// The log's size on disk, in bytes.
    pub bytes: u32,
}

/// The newest part of the crash log, as text to show (never as markup).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct CrashLogView {
    pub text: String,
    /// Older text was left out of `text`.
    pub truncated: bool,
    pub entries: u32,
}
