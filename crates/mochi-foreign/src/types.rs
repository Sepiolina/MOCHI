//! Shared vocabulary of the helper protocol and the parent-side reports.
//!
//! Nothing here uses MOCHI's verification wording or health dimensions
//! (plan D8 rule 6): a foreign check is reported as the check that ran.

use std::fmt;

use zeroize::Zeroizing;

/// The status word for every MOCHI health dimension a foreign format cannot
/// have (spec §20.4).
pub const UNSUPPORTED: &str = "UNSUPPORTED";

/// A password. Bytes, because ZIP passwords are bytes. Never `Debug`-printed.
#[derive(Clone)]
pub struct Password(Zeroizing<Vec<u8>>);

impl Password {
    pub fn new(bytes: Vec<u8>) -> Self {
        Password(Zeroizing::new(bytes))
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

/// Formats this crate reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForeignFormat {
    Zip,
    Tar,
    TarGz,
    /// A tar stream inside one or more Zstandard frames (`.tar.zst`, `.tzst`).
    TarZst,
    SevenZ,
    /// RAR 4 and RAR 5 (one library reads both; the per-entry checks differ
    /// and are reported per entry).
    Rar,
}

impl ForeignFormat {
    pub fn code(self) -> u8 {
        match self {
            ForeignFormat::Zip => 1,
            ForeignFormat::Tar => 2,
            ForeignFormat::TarGz => 3,
            ForeignFormat::SevenZ => 4,
            ForeignFormat::Rar => 5,
            ForeignFormat::TarZst => 6,
        }
    }
    pub fn from_code(c: u8) -> Option<Self> {
        match c {
            1 => Some(ForeignFormat::Zip),
            2 => Some(ForeignFormat::Tar),
            3 => Some(ForeignFormat::TarGz),
            4 => Some(ForeignFormat::SevenZ),
            5 => Some(ForeignFormat::Rar),
            6 => Some(ForeignFormat::TarZst),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            ForeignFormat::Zip => "ZIP",
            ForeignFormat::Tar => "tar",
            ForeignFormat::TarGz => "tar.gz",
            ForeignFormat::SevenZ => "7z",
            ForeignFormat::Rar => "RAR",
            ForeignFormat::TarZst => "tar.zst",
        }
    }
}

/// Resource limits against decompression bombs (plan D8 rule 5). Every breach
/// is reported by entry; none of them panics or aborts the helper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Total decompressed bytes the operation may produce.
    pub max_total_output: u64,
    /// Decompressed bytes of any one entry.
    pub max_entry_size: u64,
    /// Entries the helper will list or test.
    pub max_entries: u64,
    /// Decompressed:compressed ratio. Applied per entry against the
    /// *declared* compressed size (ZIP; a claim) and to the whole operation
    /// against the *real* archive bytes consumed (ZIP file length, gzip bytes
    /// read).
    pub max_ratio: u64,
    /// The ratio is not evaluated until this many bytes were produced, so a
    /// tiny file of zeros is not a bomb.
    pub ratio_min_output: u64,
    /// Longest entry name carried; a longer one is a breach, never truncated.
    pub max_name_len: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_total_output: 8 << 30,
            max_entry_size: 4 << 30,
            max_entries: 1_000_000,
            max_ratio: 200,
            ratio_min_output: 1 << 20,
            max_name_len: 65_535,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreachKind {
    TotalOutput,
    EntrySize,
    EntryCount,
    Ratio,
    NameLength,
}

impl BreachKind {
    pub fn code(self) -> u8 {
        match self {
            BreachKind::TotalOutput => 1,
            BreachKind::EntrySize => 2,
            BreachKind::EntryCount => 3,
            BreachKind::Ratio => 4,
            BreachKind::NameLength => 5,
        }
    }
    pub fn from_code(c: u8) -> Option<Self> {
        match c {
            1 => Some(BreachKind::TotalOutput),
            2 => Some(BreachKind::EntrySize),
            3 => Some(BreachKind::EntryCount),
            4 => Some(BreachKind::Ratio),
            5 => Some(BreachKind::NameLength),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            BreachKind::TotalOutput => "LIMIT_TOTAL_OUTPUT",
            BreachKind::EntrySize => "LIMIT_ENTRY_SIZE",
            BreachKind::EntryCount => "LIMIT_ENTRY_COUNT",
            BreachKind::Ratio => "LIMIT_COMPRESSION_RATIO",
            BreachKind::NameLength => "LIMIT_NAME_LENGTH",
        }
    }
}

/// One limit breach. `entry` is the entry being processed (`None` when the
/// breach is about the archive as a whole, e.g. the entry count).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breach {
    pub kind: BreachKind,
    pub entry: Option<u64>,
    pub limit: u64,
    pub observed: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    HardLink,
    /// Devices, FIFOs, pax headers, anything else.
    Other,
}

impl EntryKind {
    pub fn code(self) -> u8 {
        match self {
            EntryKind::File => 1,
            EntryKind::Directory => 2,
            EntryKind::Symlink => 3,
            EntryKind::HardLink => 4,
            EntryKind::Other => 5,
        }
    }
    pub fn from_code(c: u8) -> Option<Self> {
        match c {
            1 => Some(EntryKind::File),
            2 => Some(EntryKind::Directory),
            3 => Some(EntryKind::Symlink),
            4 => Some(EntryKind::HardLink),
            5 => Some(EntryKind::Other),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encryption {
    None,
    /// Traditional PKWARE encryption. Broken by known-plaintext attacks;
    /// always labelled weak (plan D8 rule 6).
    ZipCryptoWeak,
    /// WinZip AES with the key size in bits (128, 192, 256).
    Aes(u16),
}

impl Encryption {
    pub fn label(self) -> String {
        match self {
            Encryption::None => "not encrypted".to_string(),
            Encryption::ZipCryptoWeak => "weak encryption (ZipCrypto)".to_string(),
            Encryption::Aes(bits) => format!("AES-{bits}"),
        }
    }
}

/// Metadata of one entry. `name` and `link_target` are the exact bytes the
/// archive stored: never normalized, case-folded, or decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryInfo {
    pub index: u64,
    pub name: Vec<u8>,
    pub kind: EntryKind,
    /// Declared uncompressed size (a claim, not a measurement).
    pub size: Option<u64>,
    pub compressed_size: Option<u64>,
    pub encryption: Encryption,
    /// Container-specific method label (`deflate`, `stored`, `tar`, ...).
    pub method: String,
    pub link_target: Option<Vec<u8>>,
    pub unix_mode: Option<u32>,
    /// Seconds since the Unix epoch. tar only: ZIP stores local DOS time with
    /// no zone, which this slice does not guess at.
    pub mtime_unix: Option<i64>,
    /// ZIP only: the general-purpose bit 11 (name is UTF-8). `false` means the
    /// name is in an unspecified legacy code page; the bytes are still exact.
    pub name_is_utf8_flag: Option<bool>,
    /// Set when the entry's own header could not be read.
    pub error: Option<String>,
}

/// A check a format itself defines. Reported by name, never as "verified"
/// in MOCHI's sense.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckKind {
    Crc32,
    /// WinZip AES authentication code (HMAC-SHA1, 10 bytes).
    AesAuthCode,
    /// The gzip trailer: CRC-32 and length of the whole decompressed stream.
    GzipStreamCrc32,
    /// RAR 5's BLAKE2sp file hash (used in place of CRC32 by some writers).
    Blake2sp,
    /// The 32-bit content checksum of a Zstandard frame (low bits of XXH64 of
    /// the decompressed bytes), present only when the writer asked for it.
    ZstdFrameChecksum,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CheckSet(pub u8);

impl CheckSet {
    pub fn of(kinds: &[CheckKind]) -> Self {
        let mut s = CheckSet(0);
        for k in kinds {
            s.0 |= Self::bit(*k);
        }
        s
    }
    fn bit(k: CheckKind) -> u8 {
        match k {
            CheckKind::Crc32 => 1,
            CheckKind::AesAuthCode => 2,
            CheckKind::GzipStreamCrc32 => 4,
            CheckKind::Blake2sp => 8,
            CheckKind::ZstdFrameChecksum => 16,
        }
    }
    pub fn contains(self, k: CheckKind) -> bool {
        self.0 & Self::bit(k) != 0
    }
    pub fn describe(self) -> String {
        let mut parts = Vec::new();
        if self.contains(CheckKind::Crc32) {
            parts.push("CRC32");
        }
        if self.contains(CheckKind::AesAuthCode) {
            parts.push("AES authentication code");
        }
        if self.contains(CheckKind::GzipStreamCrc32) {
            parts.push("gzip stream CRC32");
        }
        if self.contains(CheckKind::Blake2sp) {
            parts.push("BLAKE2sp");
        }
        if self.contains(CheckKind::ZstdFrameChecksum) {
            parts.push("zstd frame checksum");
        }
        parts.join(" + ")
    }
}

/// What testing (or reading) one entry found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryOutcome {
    /// Every check the format defines for this entry ran and matched. Says
    /// nothing about anything the format does not check.
    Matched(CheckSet),
    /// A check ran and did not match. `wrong_password_possible` is set for
    /// encrypted entries: ZipCrypto's password test is 1 byte, so a wrong
    /// password can pass it and then fail here.
    Mismatch {
        failed: CheckSet,
        wrong_password_possible: bool,
    },
    /// The entry was read to its end without error but the format has no
    /// per-entry check (tar).
    NoEntryCheck,
    /// Directory, link, or other entry without a data stream.
    NoData,
    PasswordRequired,
    /// Fails closed: no bytes were delivered for the entry.
    WrongPassword,
    Unsupported(String),
    Breach(BreachKind),
    /// Corrupt or truncated data (decompressor or container error).
    ReadError(String),
    /// Not attempted because the operation stopped earlier.
    NotRun,
}

impl EntryOutcome {
    /// The wording a UI may show. It names the check; it never says "verified",
    /// "intact", or "healthy" (spec §23.3, plan D8 rule 6).
    pub fn describe(&self) -> String {
        match self {
            EntryOutcome::Matched(set) => format!("{} matched", set.describe()),
            EntryOutcome::Mismatch {
                failed,
                wrong_password_possible,
            } => {
                let mut s = format!("{} did NOT match", failed.describe());
                if *wrong_password_possible {
                    s.push_str(
                        " (damaged data, or a wrong password that passed the weak password test)",
                    );
                }
                s
            }
            EntryOutcome::NoEntryCheck => {
                "read without error; this format has no per-entry check".to_string()
            }
            EntryOutcome::NoData => "no data stream".to_string(),
            EntryOutcome::PasswordRequired => "password required".to_string(),
            EntryOutcome::WrongPassword => "wrong password; no data delivered".to_string(),
            EntryOutcome::Unsupported(why) => format!("unsupported: {why}"),
            EntryOutcome::Breach(k) => format!("stopped by limit {}", k.as_str()),
            EntryOutcome::ReadError(e) => format!("read error: {e}"),
            EntryOutcome::NotRun => "not run".to_string(),
        }
    }

    /// True when the outcome is a clean read: a matching check, a format with
    /// no check, or an entry without data.
    pub fn is_clean(&self) -> bool {
        matches!(
            self,
            EntryOutcome::Matched(_) | EntryOutcome::NoEntryCheck | EntryOutcome::NoData
        )
    }
}

/// The whole-container check, where the format has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamCheck {
    /// ZIP and plain tar: no whole-stream check exists.
    NotApplicable,
    /// The stream ended cleanly and these checks matched.
    Matched(CheckSet),
    /// The stream failed. For `.tar.gz` this means no entry read from it is
    /// vouched for, whatever its own outcome says.
    Failed(String),
    /// The stream was not read to its end (limit, early stop), so its trailer
    /// was never checked.
    NotReached,
}

impl StreamCheck {
    pub fn describe(&self) -> String {
        match self {
            StreamCheck::NotApplicable => "no whole-stream check in this format".to_string(),
            StreamCheck::Matched(s) => format!("{} matched", s.describe()),
            StreamCheck::Failed(e) => format!("stream check failed: {e}"),
            StreamCheck::NotReached => "stream end not reached; trailer not checked".to_string(),
        }
    }
}

/// MOCHI's health dimensions that a foreign format cannot have. Fixed at
/// `UNSUPPORTED` (plan D8 rule 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForeignDimensions {
    pub recoverability: &'static str,
    pub freshness: &'static str,
    pub retention: &'static str,
}

pub const FOREIGN_DIMENSIONS: ForeignDimensions = ForeignDimensions {
    recoverability: UNSUPPORTED,
    freshness: UNSUPPORTED,
    retention: UNSUPPORTED,
};

/// Stable error codes. The first group comes from the helper's parsing, the
/// second from the parent's supervision of the helper process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    // Reported by the helper.
    UnsupportedSplitZip,
    UnsupportedSfx,
    UnsupportedFormat,
    NotAnArchive,
    MalformedArchive,
    EntryNotFound,
    BadRequest,
    OpenFailed,
    HelperPanic,
    /// The archive's names (or everything in it) are encrypted and no
    /// password was given. Asked for before listing (plan D8 rule 7).
    PasswordRequired,
    /// The password was given and did not open the archive's headers.
    WrongPassword,
    /// A multi-volume archive is missing a volume (the name is in the text).
    MissingVolume,
    // Reported by the parent.
    HelperSpawnFailed,
    HelperCrashed,
    HelperTimeout,
    HelperOutputLimit,
    HelperProtocol,
    /// The caller's byte sink failed while receiving a streamed entry.
    SinkFailed,
    /// The caller cancelled; the helper was stopped. Parent-side only.
    Cancelled,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::UnsupportedSplitZip => "UNSUPPORTED_SPLIT_ZIP",
            ErrorCode::UnsupportedSfx => "UNSUPPORTED_SFX",
            ErrorCode::UnsupportedFormat => "UNSUPPORTED_FORMAT",
            ErrorCode::NotAnArchive => "NOT_AN_ARCHIVE",
            ErrorCode::MalformedArchive => "MALFORMED_ARCHIVE",
            ErrorCode::EntryNotFound => "ENTRY_NOT_FOUND",
            ErrorCode::BadRequest => "BAD_REQUEST",
            ErrorCode::OpenFailed => "OPEN_FAILED",
            ErrorCode::HelperPanic => "HELPER_PANIC",
            ErrorCode::PasswordRequired => "PASSWORD_REQUIRED",
            ErrorCode::WrongPassword => "WRONG_PASSWORD",
            ErrorCode::MissingVolume => "MISSING_VOLUME",
            ErrorCode::HelperSpawnFailed => "HELPER_SPAWN_FAILED",
            ErrorCode::HelperCrashed => "HELPER_CRASHED",
            ErrorCode::HelperTimeout => "HELPER_TIMEOUT",
            ErrorCode::HelperOutputLimit => "HELPER_OUTPUT_LIMIT",
            ErrorCode::HelperProtocol => "HELPER_PROTOCOL",
            ErrorCode::SinkFailed => "SINK_FAILED",
            ErrorCode::Cancelled => "CANCELLED",
        }
    }

    /// True for the `UNSUPPORTED_*` family (CLI exit code 4 territory).
    pub fn is_unsupported(self) -> bool {
        matches!(
            self,
            ErrorCode::UnsupportedSplitZip
                | ErrorCode::UnsupportedSfx
                | ErrorCode::UnsupportedFormat
        )
    }

    pub fn code(self) -> u8 {
        match self {
            ErrorCode::UnsupportedSplitZip => 1,
            ErrorCode::UnsupportedSfx => 2,
            ErrorCode::UnsupportedFormat => 3,
            ErrorCode::NotAnArchive => 4,
            ErrorCode::MalformedArchive => 5,
            ErrorCode::EntryNotFound => 6,
            ErrorCode::BadRequest => 7,
            ErrorCode::OpenFailed => 8,
            ErrorCode::HelperPanic => 9,
            ErrorCode::HelperSpawnFailed => 10,
            ErrorCode::HelperCrashed => 11,
            ErrorCode::HelperTimeout => 12,
            ErrorCode::HelperOutputLimit => 13,
            ErrorCode::HelperProtocol => 14,
            ErrorCode::SinkFailed => 15,
            ErrorCode::Cancelled => 19,
            ErrorCode::PasswordRequired => 16,
            ErrorCode::WrongPassword => 17,
            ErrorCode::MissingVolume => 18,
        }
    }

    /// Only the helper-originated codes are accepted off the wire.
    pub fn from_wire(c: u8) -> Option<Self> {
        match c {
            1 => Some(ErrorCode::UnsupportedSplitZip),
            2 => Some(ErrorCode::UnsupportedSfx),
            3 => Some(ErrorCode::UnsupportedFormat),
            4 => Some(ErrorCode::NotAnArchive),
            5 => Some(ErrorCode::MalformedArchive),
            6 => Some(ErrorCode::EntryNotFound),
            7 => Some(ErrorCode::BadRequest),
            8 => Some(ErrorCode::OpenFailed),
            9 => Some(ErrorCode::HelperPanic),
            16 => Some(ErrorCode::PasswordRequired),
            17 => Some(ErrorCode::WrongPassword),
            18 => Some(ErrorCode::MissingVolume),
            _ => None,
        }
    }
}

/// A typed, reported failure. Messages never contain passwords.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignError {
    pub code: ErrorCode,
    pub message: String,
}

impl ForeignError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ForeignError {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for ForeignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ForeignError {}

/// Helper operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Entry metadata only. For `.tar.gz` this still decompresses the stream
    /// (tar has no index), under the same limits.
    List,
    /// Stream every entry through its decompressor and run the format's check;
    /// the bytes are discarded.
    Test,
    /// Stream one entry's bytes out (for the C6 restore engine).
    Read,
    /// Stream the entries named in the request, in the order the archive
    /// stores them, in one pass (a solid archive is decoded once, not once
    /// per entry). Each is announced (`ENTRY`), streamed (`DATA`), and closed
    /// (`RESULT`).
    ReadMany,
}

impl Op {
    /// `Read` and `ReadMany` deliver entry bytes.
    pub fn is_read(self) -> bool {
        matches!(self, Op::Read | Op::ReadMany)
    }
    pub fn code(self) -> u8 {
        match self {
            Op::List => 1,
            Op::Test => 2,
            Op::Read => 3,
            Op::ReadMany => 4,
        }
    }
    pub fn from_code(c: u8) -> Option<Self> {
        match c {
            1 => Some(Op::List),
            2 => Some(Op::Test),
            3 => Some(Op::Read),
            4 => Some(Op::ReadMany),
            _ => None,
        }
    }
}
