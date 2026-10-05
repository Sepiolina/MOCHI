//! Working catalog DDL (spec §10.1, §10.5). **Draft**: the complete DDL is
//! ratification item R4, and this is the working version it will replace.
//!
//! Conventions:
//! * Every table is `STRICT`, so a column never silently holds another type.
//! * IDs and digests are 32-byte BLOBs, checked by length.
//! * SQLite integers are signed 64-bit. Lengths and offsets are `u64` in Rust
//!   and are converted with a checked `try_from`; a value above `i64::MAX` is
//!   refused at insert and rejected at read, never wrapped.
//! * Digest scopes live in separate, named columns (`stored_hash`,
//!   `content_hash`), never in a shared column, because the file-content hash
//!   is unseparated (plan O20).
//!
//! Tables from §10.1 **not** defined yet, because their columns depend on
//! decisions owned by later phases: `dictionaries` (O21, C8/C9),
//! `key_envelopes` (C11, R5), `parity_groups` (C12, R6), `retained_roots`
//! (C9), `search_documents` (C13). Preserved attributes on file versions wait
//! for decision D6 (plan O6). `commits` carries only ordering here; the commit
//! ID and record fields arrive with the commit record in C5.

/// SQLite `application_id` for MOCHI catalogs: ASCII `MOCH`. Draft (R4).
pub const APPLICATION_ID: i32 = 0x4D4F_4348;

/// SQLite `user_version`: catalog schema version. `0` = this draft; ratified
/// schemas start at 1 (the same convention as the draft record envelope).
pub const SCHEMA_VERSION: i32 = 0;

/// Page size (spec §10.5).
pub const PAGE_SIZE: u32 = 4096;

pub const ENCODING_ZSTD_FRAME: &str = "zstd-frame";
pub const PROTECTION_NONE: &str = "none";
pub const PROTECTION_AEAD: &str = "aead";

/// Everything that defines the schema. Opening an image compares its
/// `sqlite_schema` with the result of running exactly this, so an image with
/// any extra trigger, view, index, or table is rejected (spec §22.1: archives
/// are untrusted; a trigger would run on our own writes).
pub const DDL: &str = r#"
CREATE TABLE archive_meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
) STRICT;

CREATE TABLE commits (
    seq        INTEGER PRIMARY KEY CHECK (seq >= 0),
    parent_seq INTEGER REFERENCES commits(seq),
    CHECK (parent_seq IS NULL OR parent_seq < seq)
) STRICT;

CREATE TABLE objects (
    object_id   BLOB PRIMARY KEY CHECK (length(object_id) = 32),
    stored_len  INTEGER NOT NULL CHECK (stored_len > 0),
    stored_hash BLOB NOT NULL CHECK (length(stored_hash) = 32)
) STRICT, WITHOUT ROWID;

CREATE TABLE object_locations (
    object_id     BLOB PRIMARY KEY REFERENCES objects(object_id),
    stored_offset INTEGER NOT NULL CHECK (stored_offset >= 0)
) STRICT, WITHOUT ROWID;

CREATE TABLE chunks (
    object_id    BLOB PRIMARY KEY REFERENCES objects(object_id),
    encoding     TEXT NOT NULL CHECK (encoding IN ('zstd-frame')),
    protection   TEXT NOT NULL CHECK (protection IN ('none', 'aead')),
    decoded_len  INTEGER NOT NULL CHECK (decoded_len >= 0),
    content_hash BLOB NOT NULL CHECK (length(content_hash) = 32)
) STRICT, WITHOUT ROWID;

CREATE TABLE chunk_dependencies (
    object_id       BLOB NOT NULL REFERENCES chunks(object_id),
    dependency_kind TEXT NOT NULL CHECK (dependency_kind IN ('dictionary', 'key-envelope')),
    dependency_id   BLOB NOT NULL CHECK (length(dependency_id) = 32),
    PRIMARY KEY (object_id, dependency_kind, dependency_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE file_versions (
    file_version_id BLOB PRIMARY KEY CHECK (length(file_version_id) = 32),
    kind            TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
    logical_len     INTEGER NOT NULL CHECK (logical_len >= 0),
    content_hash    BLOB,
    CHECK ((kind = 'file' AND length(content_hash) = 32)
        OR (kind = 'directory' AND content_hash IS NULL AND logical_len = 0))
) STRICT, WITHOUT ROWID;

CREATE TABLE file_extents (
    file_version_id BLOB NOT NULL REFERENCES file_versions(file_version_id),
    ordinal         INTEGER NOT NULL CHECK (ordinal >= 0),
    logical_offset  INTEGER NOT NULL CHECK (logical_offset >= 0),
    length          INTEGER NOT NULL CHECK (length > 0),
    chunk_id        BLOB REFERENCES chunks(object_id),
    chunk_offset    INTEGER NOT NULL CHECK (chunk_offset >= 0),
    CHECK (chunk_id IS NOT NULL OR chunk_offset = 0),
    PRIMARY KEY (file_version_id, ordinal)
) STRICT, WITHOUT ROWID;

CREATE TABLE namespace_ops (
    commit_seq      INTEGER NOT NULL REFERENCES commits(seq),
    op_seq          INTEGER NOT NULL CHECK (op_seq >= 0),
    op              TEXT NOT NULL CHECK (op IN ('put', 'delete')),
    path            BLOB NOT NULL CHECK (length(path) > 0),
    file_version_id BLOB REFERENCES file_versions(file_version_id),
    CHECK ((op = 'put') = (file_version_id IS NOT NULL)),
    PRIMARY KEY (commit_seq, op_seq)
) STRICT, WITHOUT ROWID;
"#;
