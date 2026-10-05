//! Metadata catalog (plan C3; spec §10).
//!
//! # Where the database lives (plan §9, O25)
//!
//! The working catalog is an **in-memory** SQLite database. Published images
//! are produced with `sqlite3_serialize` and opened with
//! `sqlite3_deserialize`, so an image is only ever a byte string that moves
//! through the [`crate::storage::Storage`] trait like every other object.
//! SQLite never opens a file (`temp_store = MEMORY`, no `ATTACH`, no file
//! URIs), which keeps fault injection complete (AGENTS.md) and means a
//! published image cannot depend on a WAL or journal (§10.5): there is no file
//! for one to sit beside. The header check on open additionally rejects any
//! image whose header says WAL.
//!
//! The working database may use transactions freely (§10.5); it is private
//! and rebuildable, and it is not the durability mechanism: publication is
//! (§12.2, C5). Trade-off: the whole catalog is held in memory. Very large
//! catalogs would need a SQLite VFS over `Storage` later; that is an
//! implementation change, not a format change.
//!
//! # Untrusted images
//!
//! [`Catalog::open_image`] treats the bytes as hostile (§22.1): header fields
//! are checked before SQLite parses anything; the connection runs with
//! SQLite's defensive mode on and `trusted_schema` off; the schema must equal
//! [`schema::DDL`] exactly (so no trigger, view, or index can be smuggled in);
//! then SQLite integrity and foreign-key checks, then every MOCHI check
//! (extents, commit chain, operation sequence, full namespace replay).

mod apply;
pub mod extent;
pub mod namespace;
pub mod path;
pub mod schema;

use std::collections::HashMap;

use rusqlite::config::DbConfig;
use rusqlite::{params, Connection, OptionalExtension, MAIN_DB};

use mochi_format::codec::{Encoding, Protection};
use mochi_format::digest::{ChunkContentHash, FileContentHash, StoredObjectHash};

use crate::error::{ErrorCode, MochiError, Result};
use crate::object::{Dependency, ObjectId, ObjectRecord};

pub use apply::SegmentApplier;

use extent::{validate_extents, Extent, ExtentSource};
use namespace::{EntryKind, FileVersionId, NamespaceOp, Snapshot};
use path::ArchivePath;

/// An immutable file version (§10.1 `file_versions`). Preserved attributes
/// arrive with decision D6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersion {
    pub id: FileVersionId,
    pub kind: EntryKind,
    pub logical_len: u64,
    /// Plain BLAKE3 of the logical stream (O20). `None` exactly for directories.
    pub content_hash: Option<FileContentHash>,
}

/// One commit's namespace transition. Commit IDs and the rest of the commit
/// record arrive in C5; here a commit is its place in the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub seq: u64,
    pub parent: Option<u64>,
    pub ops: Vec<NamespaceOp>,
}

/// A published, self-contained catalog image: bytes to be framed and stored.
#[derive(Clone, PartialEq, Eq)]
pub struct PublishedImage(Vec<u8>);

impl PublishedImage {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl std::fmt::Debug for PublishedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublishedImage({} bytes)", self.0.len())
    }
}

/// Limits for opening untrusted images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogLimits {
    /// Largest SQLite image accepted. Defaults to the B.2.3 image payload
    /// budget, *S* − 592 = 268,434,864 bytes, which replaced the unreachable
    /// 1 GiB placeholder. Inside an archive the envelope decoder enforces the
    /// same budget first (`mochi_format::envelope::image_payload_budget`);
    /// this check also covers images opened outside an envelope.
    pub max_image_len: u64,
}

impl Default for CatalogLimits {
    fn default() -> Self {
        CatalogLimits {
            max_image_len: mochi_format::limits::DEFAULT_IMAGE_PAYLOAD,
        }
    }
}

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::CatalogInvalid, msg)
}

fn sql(e: rusqlite::Error) -> MochiError {
    invalid(format!("catalog database: {e}"))
}

fn to_i64(v: u64, what: &str) -> Result<i64> {
    i64::try_from(v).map_err(|_| {
        MochiError::new(
            ErrorCode::InvalidArgument,
            format!("{what} {v} exceeds the catalog's signed 64-bit range"),
        )
    })
}

fn to_u64(v: i64, what: &str) -> Result<u64> {
    u64::try_from(v).map_err(|_| invalid(format!("negative {what} {v}")))
}

fn id32(v: Vec<u8>, what: &str) -> Result<[u8; 32]> {
    <[u8; 32]>::try_from(v).map_err(|_| invalid(format!("{what} is not 32 bytes")))
}

fn encoding_str(e: Encoding) -> &'static str {
    match e {
        Encoding::ZstdFrame => schema::ENCODING_ZSTD_FRAME,
    }
}

fn protection_str(p: Protection) -> &'static str {
    match p {
        Protection::None => schema::PROTECTION_NONE,
        Protection::Aead => schema::PROTECTION_AEAD,
    }
}

fn kind_str(k: EntryKind) -> &'static str {
    match k {
        EntryKind::File => "file",
        EntryKind::Directory => "directory",
    }
}

fn kind_from(s: &str) -> Result<EntryKind> {
    match s {
        "file" => Ok(EntryKind::File),
        "directory" => Ok(EntryKind::Directory),
        other => Err(invalid(format!("unknown entry kind {other:?}"))),
    }
}

/// Settings applied to every connection before it reads anything.
fn harden(conn: &Connection) -> Result<()> {
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .map_err(sql)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)
        .map_err(sql)?;
    conn.execute_batch(
        "PRAGMA temp_store = MEMORY;
         PRAGMA mmap_size = 0;
         PRAGMA cell_size_check = ON;
         PRAGMA foreign_keys = ON;",
    )
    .map_err(sql)
}

/// The canonical `sqlite_schema` contents, for exact comparison.
fn schema_rows(conn: &Connection) -> Result<SchemaRows> {
    let mut stmt = conn
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name")
        .map_err(sql)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map_err(sql)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(sql)?;
    Ok(rows)
}

type SchemaRows = Vec<(String, String, String, Option<String>)>;

/// The schema a fresh working catalog has, computed once per process.
fn expected_schema() -> Result<&'static SchemaRows> {
    static EXPECTED: std::sync::OnceLock<std::result::Result<SchemaRows, MochiError>> =
        std::sync::OnceLock::new();
    EXPECTED
        .get_or_init(|| schema_rows(&Catalog::new_working()?.conn))
        .as_ref()
        .map_err(Clone::clone)
}

/// Header checks before SQLite parses the image (SQLite file format §1.3).
fn check_header(bytes: &[u8], limits: &CatalogLimits) -> Result<()> {
    let len = bytes.len() as u64;
    if len > limits.max_image_len {
        return Err(MochiError::new(
            ErrorCode::LimitExceeded,
            format!(
                "catalog image of {len} bytes exceeds limit {}",
                limits.max_image_len
            ),
        ));
    }
    let page = u64::from(schema::PAGE_SIZE);
    if len < page || !len.is_multiple_of(page) {
        return Err(invalid(
            "catalog image is not a whole number of 4096-byte pages",
        ));
    }
    let h = &bytes[..100];
    if &h[..16] != b"SQLite format 3\0" {
        return Err(invalid("not a SQLite image"));
    }
    if u16::from_be_bytes([h[16], h[17]]) as u32 != schema::PAGE_SIZE {
        return Err(invalid("catalog page size is not 4096 (spec §10.5)"));
    }
    // File format write/read versions: 1 = legacy (rollback journal), 2 = WAL.
    if h[18] != 1 || h[19] != 1 {
        return Err(invalid(
            "catalog image is in WAL mode; published images must be self-contained (spec §10.5)",
        ));
    }
    let user_version = i32::from_be_bytes([h[60], h[61], h[62], h[63]]);
    let application_id = i32::from_be_bytes([h[68], h[69], h[70], h[71]]);
    if application_id != schema::APPLICATION_ID {
        return Err(invalid("not a MOCHI catalog (application_id)"));
    }
    if user_version != schema::SCHEMA_VERSION {
        return Err(MochiError::new(
            ErrorCode::UnsupportedFeature,
            format!("catalog schema version {user_version} is not supported by this build"),
        ));
    }
    Ok(())
}

pub struct Catalog {
    conn: Connection,
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Catalog")
    }
}

impl Catalog {
    /// A new, empty private working catalog.
    pub fn new_working() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(sql)?;
        harden(&conn)?;
        conn.pragma_update(None, "page_size", schema::PAGE_SIZE)
            .map_err(sql)?;
        conn.pragma_update(None, "application_id", schema::APPLICATION_ID)
            .map_err(sql)?;
        conn.pragma_update(None, "user_version", schema::SCHEMA_VERSION)
            .map_err(sql)?;
        conn.execute_batch(&format!("BEGIN;{}COMMIT;", schema::DDL))
            .map_err(sql)?;
        Ok(Catalog { conn })
    }

    /// Open a published image read-only, after full verification.
    ///
    /// **Precondition: the caller has already verified the image's bytes**
    /// against the metadata-object hash recorded for it (plan C5). The checks
    /// here catch structural, relational, and MOCHI-rule damage, but not a
    /// flipped bit inside a stored value, which SQLite cannot detect (see the
    /// test `sqlite_checks_alone_cannot_detect_value_corruption`).
    pub fn open_image(bytes: &[u8], limits: &CatalogLimits) -> Result<Self> {
        Self::open_image_inner(bytes, limits, false)
    }

    /// As [`Catalog::open_image`], with the same precondition and the same
    /// checks, but the result accepts writes: the writer (C5) extends the
    /// head commit's catalog. The image bytes are never modified; the
    /// connection holds a private copy.
    pub fn open_image_writable(bytes: &[u8], limits: &CatalogLimits) -> Result<Self> {
        Self::open_image_inner(bytes, limits, true)
    }

    fn open_image_inner(bytes: &[u8], limits: &CatalogLimits, writable: bool) -> Result<Self> {
        check_header(bytes, limits)?;
        let mut conn = Connection::open_in_memory().map_err(sql)?;
        harden(&conn)?;
        conn.deserialize_read_exact(MAIN_DB, bytes, bytes.len(), !writable)
            .map_err(sql)?;
        conn.execute_batch("PRAGMA query_only = ON;").map_err(sql)?;
        let catalog = Catalog { conn };
        if schema_rows(&catalog.conn)? != *expected_schema()? {
            return Err(invalid(
                "catalog schema differs from the expected DDL (extra or altered objects)",
            ));
        }
        catalog.verify()?;
        if writable {
            catalog
                .conn
                .execute_batch("PRAGMA query_only = OFF;")
                .map_err(sql)?;
        }
        Ok(catalog)
    }

    /// Refuse further writes on this connection, as [`Catalog::open_image`]
    /// does. For a catalog a reader built by replaying deltas onto a
    /// verified checkpoint (Annex B.2 D10.4).
    pub(crate) fn make_query_only(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA query_only = ON;")
            .map_err(sql)
    }

    /// A private, writable copy of this catalog, without re-verifying it.
    /// Crate-internal on purpose: it is only for catalogs this process built
    /// or already verified (the writer's head), never for archive bytes.
    pub(crate) fn duplicate(&self) -> Result<Self> {
        let data = self.conn.serialize(MAIN_DB).map_err(sql)?;
        let mut conn = Connection::open_in_memory().map_err(sql)?;
        harden(&conn)?;
        let bytes: &[u8] = &data;
        conn.deserialize_read_exact(MAIN_DB, bytes, bytes.len(), false)
            .map_err(sql)?;
        Ok(Catalog { conn })
    }

    /// The latest commit sequence in this catalog: the commit state it
    /// materializes (spec §10.6). `None` for an empty catalog.
    pub fn head_commit(&self) -> Result<Option<u64>> {
        self.head_seq()
    }

    /// Read an `archive_meta` value (spec §10.1).
    pub fn meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.conn
            .query_row(
                "SELECT value FROM archive_meta WHERE key = ?1",
                params![key],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(sql)
    }

    /// Set an `archive_meta` value once. Re-setting the same value is a no-op;
    /// a different value is refused: archive identity and recorded
    /// parameters do not change after they are published.
    pub fn set_meta(&mut self, key: &str, value: &[u8]) -> Result<()> {
        match self.meta(key)? {
            Some(existing) if existing == value => Ok(()),
            Some(_) => Err(MochiError::new(
                ErrorCode::IdentityConflict,
                format!("archive_meta {key:?} is already set to a different value"),
            )),
            None => self
                .conn
                .execute(
                    "INSERT INTO archive_meta (key, value) VALUES (?1, ?2)",
                    params![key, value],
                )
                .map(|_| ())
                .map_err(sql),
        }
    }

    /// Verify, compact, and serialize a self-contained image (§10.5).
    pub fn publish(&self) -> Result<PublishedImage> {
        self.verify()?;
        self.conn.execute_batch("VACUUM;").map_err(sql)?;
        let data = self.conn.serialize(MAIN_DB).map_err(sql)?;
        let bytes = data.to_vec();
        check_header(
            &bytes,
            &CatalogLimits {
                max_image_len: u64::MAX,
            },
        )?;
        Ok(PublishedImage(bytes))
    }

    // ---- objects -----------------------------------------------------------------

    /// Insert a data object's record. Re-inserting an identical record is a
    /// no-op; the same ID with different content is corruption (§10.2).
    pub fn insert_object(
        &mut self,
        record: &ObjectRecord,
        stored_offset: Option<u64>,
    ) -> Result<()> {
        if let Some(existing) = self.object(&record.id)? {
            return if existing == *record {
                Ok(())
            } else {
                Err(MochiError::new(
                    ErrorCode::IdentityConflict,
                    format!(
                        "object {} already exists with different content",
                        record.id.to_hex()
                    ),
                ))
            };
        }
        let tx = self.conn.transaction().map_err(sql)?;
        insert_object_rows(&tx, record, stored_offset, &mut no_fault)?;
        tx.commit().map_err(sql)
    }

    /// Read a data object's record back.
    pub fn object(&self, id: &ObjectId) -> Result<Option<ObjectRecord>> {
        let row = self
            .conn
            .query_row(
                "SELECT o.stored_len, o.stored_hash, c.encoding, c.protection, c.decoded_len, c.content_hash
                 FROM objects o JOIN chunks c USING (object_id) WHERE o.object_id = ?1",
                params![&id.as_bytes()[..]],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, Vec<u8>>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(sql)?;
        let Some((stored_len, stored_hash, encoding, protection, decoded_len, content_hash)) = row
        else {
            return Ok(None);
        };
        let encoding = match encoding.as_str() {
            schema::ENCODING_ZSTD_FRAME => Encoding::ZstdFrame,
            other => return Err(invalid(format!("unknown encoding {other:?}"))),
        };
        let protection = match protection.as_str() {
            schema::PROTECTION_NONE => Protection::None,
            schema::PROTECTION_AEAD => Protection::Aead,
            other => return Err(invalid(format!("unknown protection {other:?}"))),
        };
        let mut stmt = self
            .conn
            .prepare(
                "SELECT dependency_kind, dependency_id FROM chunk_dependencies
                 WHERE object_id = ?1 ORDER BY dependency_kind, dependency_id",
            )
            .map_err(sql)?;
        let mut dependencies = Vec::new();
        for row in stmt
            .query_map(params![&id.as_bytes()[..]], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(sql)?
        {
            let (kind, dep) = row.map_err(sql)?;
            let dep = ObjectId::from_bytes(id32(dep, "dependency id")?);
            dependencies.push(match kind.as_str() {
                "dictionary" => Dependency::Dictionary(dep),
                "key-envelope" => Dependency::KeyEnvelope(dep),
                other => return Err(invalid(format!("unknown dependency kind {other:?}"))),
            });
        }
        Ok(Some(ObjectRecord {
            id: *id,
            encoding,
            protection,
            stored_len: to_u64(stored_len, "stored length")?,
            stored_hash: StoredObjectHash::from_bytes(id32(stored_hash, "stored hash")?),
            decoded_len: to_u64(decoded_len, "decoded length")?,
            content_hash: ChunkContentHash::from_bytes(id32(content_hash, "content hash")?),
            dependencies,
        }))
    }

    /// Where an object is stored in a monolithic archive, if recorded.
    pub fn object_location(&self, id: &ObjectId) -> Result<Option<u64>> {
        self.conn
            .query_row(
                "SELECT stored_offset FROM object_locations WHERE object_id = ?1",
                params![&id.as_bytes()[..]],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(sql)?
            .map(|v| to_u64(v, "stored offset"))
            .transpose()
    }

    fn chunk_decoded_len(&self, id: &ObjectId) -> Result<Option<u64>> {
        self.conn
            .query_row(
                "SELECT decoded_len FROM chunks WHERE object_id = ?1",
                params![&id.as_bytes()[..]],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(sql)?
            .map(|v| to_u64(v, "decoded length"))
            .transpose()
    }

    // ---- file versions -------------------------------------------------------------

    fn check_file_version(&self, v: &FileVersion, extents: &[Extent]) -> Result<()> {
        match v.kind {
            EntryKind::Directory => {
                if v.logical_len != 0 || v.content_hash.is_some() || !extents.is_empty() {
                    return Err(MochiError::new(
                        ErrorCode::InvalidArgument,
                        "a directory has no length, content hash, or extents",
                    ));
                }
            }
            EntryKind::File => {
                if v.content_hash.is_none() {
                    return Err(MochiError::new(
                        ErrorCode::InvalidArgument,
                        "a file version needs its content hash",
                    ));
                }
            }
        }
        // Chunk lengths are looked up once, so the validator stays pure.
        let mut lens: HashMap<ObjectId, Option<u64>> = HashMap::new();
        for e in extents {
            if let ExtentSource::Chunk { chunk, .. } = e.source {
                if let std::collections::hash_map::Entry::Vacant(slot) = lens.entry(chunk) {
                    slot.insert(self.chunk_decoded_len(&chunk)?);
                }
            }
        }
        validate_extents(v.logical_len, extents, |c| lens.get(c).copied().flatten())
            .map_err(MochiError::from)
    }

    /// Insert a file version and its extents, validated (§10.3). Identical
    /// re-insertion is a no-op; same ID, different content is corruption.
    pub fn insert_file_version(&mut self, v: &FileVersion, extents: &[Extent]) -> Result<()> {
        self.check_file_version(v, extents)?;
        if let Some((existing, existing_extents)) = self.file_version(&v.id)? {
            return if existing == *v && existing_extents == extents {
                Ok(())
            } else {
                Err(MochiError::new(
                    ErrorCode::IdentityConflict,
                    "file version already exists with different content",
                ))
            };
        }
        let tx = self.conn.transaction().map_err(sql)?;
        insert_file_version_rows(&tx, v, extents, &mut no_fault)?;
        tx.commit().map_err(sql)
    }

    /// A file version and its extents in ordinal order.
    pub fn file_version(&self, id: &FileVersionId) -> Result<Option<(FileVersion, Vec<Extent>)>> {
        let row = self
            .conn
            .query_row(
                "SELECT kind, logical_len, content_hash FROM file_versions WHERE file_version_id = ?1",
                params![&id.as_bytes()[..]],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<Vec<u8>>>(2)?)),
            )
            .optional()
            .map_err(sql)?;
        let Some((kind, len, hash)) = row else {
            return Ok(None);
        };
        let version = FileVersion {
            id: *id,
            kind: kind_from(&kind)?,
            logical_len: to_u64(len, "logical length")?,
            content_hash: hash
                .map(|h| id32(h, "content hash").map(FileContentHash::from_bytes))
                .transpose()?,
        };
        let mut stmt = self
            .conn
            .prepare(
                "SELECT ordinal, logical_offset, length, chunk_id, chunk_offset FROM file_extents
                 WHERE file_version_id = ?1 ORDER BY ordinal",
            )
            .map_err(sql)?;
        let mut extents = Vec::new();
        for row in stmt
            .query_map(params![&id.as_bytes()[..]], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<Vec<u8>>>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(sql)?
        {
            let (ordinal, off, len, chunk, chunk_off) = row.map_err(sql)?;
            extents.push(Extent {
                ordinal: u32::try_from(ordinal)
                    .map_err(|_| invalid("extent ordinal out of range"))?,
                logical_offset: to_u64(off, "logical offset")?,
                length: to_u64(len, "extent length")?,
                source: match chunk {
                    Some(c) => ExtentSource::Chunk {
                        chunk: ObjectId::from_bytes(id32(c, "chunk id")?),
                        chunk_offset: to_u64(chunk_off, "chunk offset")?,
                    },
                    None => ExtentSource::Hole,
                },
            });
        }
        Ok(Some((version, extents)))
    }

    fn kinds_for<'a>(
        &self,
        ids: impl Iterator<Item = &'a FileVersionId>,
    ) -> Result<HashMap<FileVersionId, EntryKind>> {
        let mut out = HashMap::new();
        for id in ids {
            if out.contains_key(id) {
                continue;
            }
            let kind: Option<String> = self
                .conn
                .query_row(
                    "SELECT kind FROM file_versions WHERE file_version_id = ?1",
                    params![&id.as_bytes()[..]],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            if let Some(k) = kind {
                out.insert(*id, kind_from(&k)?);
            }
        }
        Ok(out)
    }

    // ---- commits and replay ---------------------------------------------------------

    fn head_seq(&self) -> Result<Option<u64>> {
        self.conn
            .query_row("SELECT max(seq) FROM commits", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .map_err(sql)?
            .map(|v| to_u64(v, "commit sequence"))
            .transpose()
    }

    /// Append one commit after validating it against the current head
    /// snapshot. Cost is a full replay per commit for now; C6 caches it.
    pub fn append_commit(&mut self, commit: &Commit) -> Result<Snapshot> {
        let head = self.head_seq()?;
        let chain_ok = match head {
            None => commit.parent.is_none(),
            Some(h) => commit.parent == Some(h) && commit.seq > h,
        };
        if !chain_ok {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "commit {} (parent {:?}) does not extend head {head:?}",
                    commit.seq, commit.parent
                ),
            ));
        }
        let mut snapshot = self.replay(None)?;
        let kinds = self.kinds_for(commit.ops.iter().filter_map(|op| match op {
            NamespaceOp::Put { version, .. } => Some(version),
            NamespaceOp::Delete { .. } => None,
        }))?;
        snapshot
            .apply_commit(&commit.ops, |v| kinds.get(v).copied())
            .map_err(MochiError::from)?;

        let tx = self.conn.transaction().map_err(sql)?;
        insert_commit_rows(&tx, commit, &mut no_fault)?;
        tx.commit().map_err(sql)?;
        Ok(snapshot)
    }

    /// Replay the namespace from the first commit through `upto` (inclusive;
    /// `None` = head). Deterministic: order comes only from the commit chain
    /// and operation sequence, never from row storage order.
    pub fn replay(&self, upto: Option<u64>) -> Result<Snapshot> {
        #[cfg(test)]
        REPLAYS.with(|n| n.set(n.get() + 1));
        let mut stmt = self
            .conn
            .prepare("SELECT seq, parent_seq FROM commits ORDER BY seq")
            .map_err(sql)?;
        let commits: Vec<(i64, Option<i64>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sql)?
            .collect::<std::result::Result<_, _>>()
            .map_err(sql)?;
        let kinds = self.all_kinds()?;
        let mut snapshot = Snapshot::new();
        let mut prev: Option<i64> = None;
        for (seq, parent) in commits {
            if parent != prev {
                return Err(invalid(format!(
                    "commit {seq} has parent {parent:?}, expected {prev:?}: the chain is not linear"
                )));
            }
            if let Some(limit) = upto {
                if to_u64(seq, "commit sequence")? > limit {
                    break;
                }
            }
            let ops = self.ops_of(seq)?;
            snapshot
                .apply_commit(&ops, |v| kinds.get(v).copied())
                .map_err(|f| {
                    let e = MochiError::from(f);
                    MochiError::new(e.code, format!("replaying commit {seq}: {}", e.message))
                })?;
            prev = Some(seq);
        }
        if let Some(limit) = upto {
            if prev.map(|p| to_u64(p, "commit sequence")).transpose()? != Some(limit) {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    format!("commit {limit} is not in the catalog"),
                ));
            }
        }
        Ok(snapshot)
    }

    fn all_kinds(&self) -> Result<HashMap<FileVersionId, EntryKind>> {
        let mut stmt = self
            .conn
            .prepare("SELECT file_version_id, kind FROM file_versions")
            .map_err(sql)?;
        let mut out = HashMap::new();
        for row in stmt
            .query_map([], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(sql)?
        {
            let (id, kind) = row.map_err(sql)?;
            out.insert(
                FileVersionId::from_bytes(id32(id, "file version id")?),
                kind_from(&kind)?,
            );
        }
        Ok(out)
    }

    fn ops_of(&self, seq: i64) -> Result<Vec<NamespaceOp>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT op_seq, op, path, file_version_id FROM namespace_ops
                 WHERE commit_seq = ?1 ORDER BY op_seq",
            )
            .map_err(sql)?;
        let mut ops = Vec::new();
        for (expected, row) in stmt
            .query_map(params![seq], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Option<Vec<u8>>>(3)?,
                ))
            })
            .map_err(sql)?
            .enumerate()
        {
            let (op_seq, op, path, version) = row.map_err(sql)?;
            if op_seq != expected as i64 {
                // A missing operation silently changes the snapshot; refuse.
                return Err(invalid(format!(
                    "commit {seq}: operation sequence jumps from {} to {op_seq}",
                    expected as i64 - 1
                )));
            }
            let path = ArchivePath::from_stored(&path)?;
            ops.push(match (op.as_str(), version) {
                ("put", Some(v)) => NamespaceOp::Put {
                    path,
                    version: FileVersionId::from_bytes(id32(v, "file version id")?),
                },
                ("delete", None) => NamespaceOp::Delete { path },
                _ => {
                    return Err(invalid(format!(
                        "commit {seq}: malformed operation {op_seq}"
                    )))
                }
            });
        }
        Ok(ops)
    }

    // ---- verification -----------------------------------------------------------------

    /// SQLite structural integrity, foreign keys, and every MOCHI rule
    /// (§10.5): extents of every file version, the commit chain, operation
    /// sequences, and a full namespace replay re-validated in full.
    pub fn verify(&self) -> Result<()> {
        let mut stmt = self.conn.prepare("PRAGMA integrity_check").map_err(sql)?;
        let results: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .map_err(sql)?
            .collect::<std::result::Result<_, _>>()
            .map_err(sql)?;
        if results != ["ok"] {
            return Err(invalid(format!(
                "SQLite integrity check failed ({} problems)",
                results.len()
            )));
        }
        let mut stmt = self.conn.prepare("PRAGMA foreign_key_check").map_err(sql)?;
        if stmt.query([]).map_err(sql)?.next().map_err(sql)?.is_some() {
            return Err(invalid("foreign-key check failed"));
        }
        let mut stmt = self
            .conn
            .prepare("SELECT file_version_id FROM file_versions")
            .map_err(sql)?;
        let ids: Vec<Vec<u8>> = stmt
            .query_map([], |r| r.get(0))
            .map_err(sql)?
            .collect::<std::result::Result<_, _>>()
            .map_err(sql)?;
        for id in ids {
            let id = FileVersionId::from_bytes(id32(id, "file version id")?);
            let (v, extents) = self
                .file_version(&id)?
                .ok_or_else(|| invalid("file version vanished during verification"))?;
            self.check_file_version(&v, &extents).map_err(|e| {
                MochiError::new(e.code, format!("file version {id:?}: {}", e.message))
            })?;
        }
        let snapshot = self.replay(None)?;
        snapshot.validate_all().map_err(MochiError::from)
    }
}

#[cfg(test)]
thread_local! {
    static REPLAYS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Full namespace replays run on this thread (unit tests only): lets the
/// applier's tests show it never rebuilds the namespace per manifest.
#[cfg(test)]
pub(crate) fn replay_count() -> u64 {
    REPLAYS.with(std::cell::Cell::get)
}

#[cfg(any(test, feature = "test-controls"))]
impl Catalog {
    /// Every row of every table, rendered and sorted: a logical view for
    /// comparing catalog states (rollback tests), independent of page layout.
    pub fn logical_dump(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let tables: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(sql)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql)?;
            rows
        };
        for t in tables {
            let mut stmt = self
                .conn
                .prepare(&format!("SELECT * FROM \"{t}\""))
                .map_err(sql)?;
            let cols = stmt.column_count();
            let mut rows = stmt.query([]).map_err(sql)?;
            while let Some(r) = rows.next().map_err(sql)? {
                let mut line = format!("{t}:");
                for i in 0..cols {
                    let v: rusqlite::types::Value = r.get(i).map_err(sql)?;
                    line.push_str(&format!(" {v:?}"));
                }
                out.push(line);
            }
        }
        out.sort();
        Ok(out)
    }

    /// A writable copy of this catalog with the same logical state (see
    /// `duplicate`, which stays crate-internal for production). For tests
    /// that need writable controls matching a reader's catalog (checklist
    /// Q25); unlike `publish`, it does not write to the source connection.
    pub fn writable_copy_for_tests(&self) -> Result<Self> {
        self.duplicate()
    }

    /// Every object ID in the catalog, sorted. For tests that check each
    /// recorded object independently (the T12 oracle's physical-location
    /// validation, review amendment 4).
    pub fn object_ids(&self) -> Result<Vec<ObjectId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT object_id FROM objects ORDER BY object_id")
            .map_err(sql)?;
        let ids = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .map_err(sql)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sql)?;
        ids.into_iter()
            .map(|b| {
                let a: [u8; 32] = b
                    .try_into()
                    .map_err(|_| invalid("object ID is not 32 bytes"))?;
                Ok(ObjectId::from_bytes(a))
            })
            .collect()
    }
}

// ---- row writers ------------------------------------------------------------------
//
// Each runs inside a transaction its caller owns and commits, so the same
// statements serve the idempotent public inserts (one transaction each) and
// the strict segment applier (one transaction per manifest, T12). `hook` is
// called before every row mutation; production passes `no_fault`, and the
// applier uses it for its fault-injection points.

pub(crate) fn no_fault() -> Result<()> {
    Ok(())
}

pub(crate) fn insert_object_rows(
    conn: &Connection,
    record: &ObjectRecord,
    stored_offset: Option<u64>,
    hook: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    let id = &record.id.as_bytes()[..];
    hook()?;
    conn.execute(
        "INSERT INTO objects (object_id, stored_len, stored_hash) VALUES (?1, ?2, ?3)",
        params![
            id,
            to_i64(record.stored_len, "stored length")?,
            &record.stored_hash.as_bytes()[..]
        ],
    )
    .map_err(sql)?;
    hook()?;
    conn.execute(
        "INSERT INTO chunks (object_id, encoding, protection, decoded_len, content_hash)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id,
            encoding_str(record.encoding),
            protection_str(record.protection),
            to_i64(record.decoded_len, "decoded length")?,
            &record.content_hash.as_bytes()[..]
        ],
    )
    .map_err(sql)?;
    for dep in &record.dependencies {
        let (kind, dep_id) = match dep {
            Dependency::Dictionary(d) => ("dictionary", d),
            Dependency::KeyEnvelope(d) => ("key-envelope", d),
        };
        hook()?;
        conn.execute(
            "INSERT INTO chunk_dependencies (object_id, dependency_kind, dependency_id)
             VALUES (?1, ?2, ?3)",
            params![id, kind, &dep_id.as_bytes()[..]],
        )
        .map_err(sql)?;
    }
    if let Some(off) = stored_offset {
        hook()?;
        conn.execute(
            "INSERT INTO object_locations (object_id, stored_offset) VALUES (?1, ?2)",
            params![id, to_i64(off, "stored offset")?],
        )
        .map_err(sql)?;
    }
    Ok(())
}

/// Rows only: the caller has run [`Catalog::check_file_version`] (§10.3).
pub(crate) fn insert_file_version_rows(
    conn: &Connection,
    v: &FileVersion,
    extents: &[Extent],
    hook: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    let id = &v.id.as_bytes()[..];
    hook()?;
    conn.execute(
        "INSERT INTO file_versions (file_version_id, kind, logical_len, content_hash)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            id,
            kind_str(v.kind),
            to_i64(v.logical_len, "logical length")?,
            v.content_hash.as_ref().map(|h| h.as_bytes().to_vec())
        ],
    )
    .map_err(sql)?;
    for e in extents {
        let (chunk, chunk_offset) = match &e.source {
            ExtentSource::Chunk {
                chunk,
                chunk_offset,
            } => (Some(chunk.as_bytes().to_vec()), *chunk_offset),
            ExtentSource::Hole => (None, 0),
        };
        hook()?;
        conn.execute(
            "INSERT INTO file_extents
             (file_version_id, ordinal, logical_offset, length, chunk_id, chunk_offset)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                i64::from(e.ordinal),
                to_i64(e.logical_offset, "logical offset")?,
                to_i64(e.length, "extent length")?,
                chunk,
                to_i64(chunk_offset, "chunk offset")?
            ],
        )
        .map_err(sql)?;
    }
    Ok(())
}

/// Rows only: the caller has validated the chain and the namespace.
pub(crate) fn insert_commit_rows(
    conn: &Connection,
    commit: &Commit,
    hook: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    hook()?;
    conn.execute(
        "INSERT INTO commits (seq, parent_seq) VALUES (?1, ?2)",
        params![
            to_i64(commit.seq, "commit sequence")?,
            commit
                .parent
                .map(|p| to_i64(p, "parent sequence"))
                .transpose()?
        ],
    )
    .map_err(sql)?;
    for (op_seq, op) in commit.ops.iter().enumerate() {
        let (name, version) = match op {
            NamespaceOp::Put { version, .. } => ("put", Some(version.as_bytes().to_vec())),
            NamespaceOp::Delete { .. } => ("delete", None),
        };
        hook()?;
        conn.execute(
            "INSERT INTO namespace_ops (commit_seq, op_seq, op, path, file_version_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                to_i64(commit.seq, "commit sequence")?,
                to_i64(op_seq as u64, "operation sequence")?,
                name,
                op.path().as_stored(),
                version
            ],
        )
        .map_err(sql)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Hostile-image tests: each plants one defect with raw SQL (which only
    //! this module can do), publishes the bytes, and requires `open_image` to
    //! refuse them with the right code.

    use super::*;
    use mochi_format::digest::chunk_content_hash;
    use mochi_format::repr::DecodedBytes;

    const F1: FileVersionId = FileVersionId::from_bytes([1; 32]);
    const D1: FileVersionId = FileVersionId::from_bytes([2; 32]);
    const C1: ObjectId = ObjectId::from_bytes([3; 32]);

    fn p(s: &str) -> ArchivePath {
        ArchivePath::from_stored(s.as_bytes()).unwrap()
    }

    fn record() -> ObjectRecord {
        ObjectRecord {
            id: C1,
            encoding: Encoding::ZstdFrame,
            protection: Protection::None,
            stored_len: 40,
            stored_hash: StoredObjectHash::from_bytes([4; 32]),
            decoded_len: 100,
            content_hash: chunk_content_hash(&DecodedBytes::new(vec![0; 100])),
            dependencies: vec![],
        }
    }

    /// dir `d`, file `d/f` backed by 60 bytes of chunk C1, in two commits.
    fn sample() -> Catalog {
        let mut c = Catalog::new_working().unwrap();
        c.insert_object(&record(), Some(0)).unwrap();
        c.insert_file_version(
            &FileVersion {
                id: D1,
                kind: EntryKind::Directory,
                logical_len: 0,
                content_hash: None,
            },
            &[],
        )
        .unwrap();
        c.insert_file_version(
            &FileVersion {
                id: F1,
                kind: EntryKind::File,
                logical_len: 60,
                content_hash: Some(FileContentHash::from_bytes([5; 32])),
            },
            &[Extent {
                ordinal: 0,
                logical_offset: 0,
                length: 60,
                source: ExtentSource::Chunk {
                    chunk: C1,
                    chunk_offset: 0,
                },
            }],
        )
        .unwrap();
        c.append_commit(&Commit {
            seq: 0,
            parent: None,
            ops: vec![NamespaceOp::Put {
                path: p("d"),
                version: D1,
            }],
        })
        .unwrap();
        c.append_commit(&Commit {
            seq: 1,
            parent: Some(0),
            ops: vec![NamespaceOp::Put {
                path: p("d/f"),
                version: F1,
            }],
        })
        .unwrap();
        c
    }

    /// Serialize without the verifying `publish`, to produce hostile images.
    fn raw_image(c: &Catalog) -> Vec<u8> {
        c.conn.serialize(MAIN_DB).unwrap().to_vec()
    }

    fn tampered(sql_text: &str) -> Vec<u8> {
        let c = sample();
        c.conn.execute_batch(sql_text).unwrap();
        raw_image(&c)
    }

    fn refused(bytes: &[u8]) -> ErrorCode {
        Catalog::open_image(bytes, &CatalogLimits::default())
            .unwrap_err()
            .code
    }

    #[test]
    fn published_image_round_trips() {
        let c = sample();
        let img = c.publish().unwrap();
        let opened = Catalog::open_image(img.as_bytes(), &CatalogLimits::default()).unwrap();
        assert_eq!(opened.replay(None).unwrap(), c.replay(None).unwrap());
        assert_eq!(opened.object(&C1).unwrap(), Some(record()));
        assert_eq!(
            opened.file_version(&F1).unwrap(),
            c.file_version(&F1).unwrap()
        );
        // Earlier snapshot by commit (§10.6 snapshot selection).
        assert_eq!(opened.replay(Some(0)).unwrap().len(), 1);
        assert!(opened.replay(Some(7)).is_err());
    }

    #[test]
    fn opened_images_are_read_only() {
        let img = sample().publish().unwrap();
        let mut opened = Catalog::open_image(img.as_bytes(), &CatalogLimits::default()).unwrap();
        let mut other = record();
        other.id = ObjectId::from_bytes([8; 32]);
        assert!(opened.insert_object(&other, None).is_err());
    }

    #[test]
    fn extra_schema_objects_are_refused() {
        // A trigger would run on our own later writes; a view or index is not
        // ours either. All are schema differences.
        for extra in [
            "CREATE TRIGGER t AFTER INSERT ON objects BEGIN DELETE FROM commits; END;",
            "CREATE VIEW v AS SELECT * FROM objects;",
            "CREATE INDEX i ON file_extents(chunk_id);",
            "CREATE TABLE smuggled (x INTEGER) STRICT;",
        ] {
            assert_eq!(
                refused(&tampered(extra)),
                ErrorCode::CatalogInvalid,
                "{extra}"
            );
        }
    }

    #[test]
    fn altered_table_definition_is_refused() {
        // Same table name, CHECK constraint removed: rows that violate the real
        // schema could then be present.
        let bytes = tampered(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE x AS SELECT * FROM archive_meta;
             DROP TABLE archive_meta;
             CREATE TABLE archive_meta (key TEXT PRIMARY KEY, value BLOB) STRICT;
             DROP TABLE x;",
        );
        assert_eq!(refused(&bytes), ErrorCode::CatalogInvalid);
    }

    #[test]
    fn header_level_rejections() {
        let good = sample().publish().unwrap().into_vec();

        let mut wal = good.clone();
        wal[18] = 2;
        wal[19] = 2;
        assert_eq!(refused(&wal), ErrorCode::CatalogInvalid);

        let mut app = good.clone();
        app[68..72].copy_from_slice(&0x1234_5678i32.to_be_bytes());
        assert_eq!(refused(&app), ErrorCode::CatalogInvalid);

        let mut ver = good.clone();
        ver[60..64].copy_from_slice(&7i32.to_be_bytes());
        assert_eq!(refused(&ver), ErrorCode::UnsupportedFeature);

        assert_eq!(refused(&good[..good.len() - 1]), ErrorCode::CatalogInvalid);
        assert_eq!(refused(&[]), ErrorCode::CatalogInvalid);
        assert_eq!(
            Catalog::open_image(
                &good,
                &CatalogLimits {
                    max_image_len: 4096
                }
            )
            .unwrap_err()
            .code,
            ErrorCode::LimitExceeded
        );
    }

    #[test]
    fn relationship_and_mochi_rule_violations_are_refused() {
        let cases = [
            // Dangling foreign key (enforcement off while planting it).
            "PRAGMA foreign_keys = OFF;
             DELETE FROM chunks;",
            // Extent gap: file claims 70 bytes, extents cover 60.
            "UPDATE file_versions SET logical_len = 70 WHERE kind = 'file';",
            // Out-of-range read past the chunk's decoded length.
            "UPDATE file_extents SET chunk_offset = 50;",
            // A lost operation: sequence starts at 1.
            "UPDATE namespace_ops SET op_seq = 1 WHERE commit_seq = 1;",
            // A broken chain: second commit re-parented to nothing.
            "UPDATE commits SET parent_seq = NULL WHERE seq = 1;",
            // An orphan: the directory's creation removed, its child remains.
            "DELETE FROM namespace_ops WHERE commit_seq = 0;",
            // A path that encodes traversal.
            "UPDATE namespace_ops SET path = CAST('d/../f' AS BLOB) WHERE commit_seq = 1;",
        ];
        for sql_text in cases {
            let code = refused(&tampered(sql_text));
            assert!(
                matches!(
                    code,
                    ErrorCode::CatalogInvalid
                        | ErrorCode::ExtentInvalid
                        | ErrorCode::NamespaceInvalid
                        | ErrorCode::PathInvalid
                ),
                "{sql_text}: got {code}"
            );
        }
    }

    /// Every single-byte flip either is refused or opens to a catalog that
    /// satisfies every structural, relational, and MOCHI rule. Never a panic,
    /// never a half-open catalog.
    #[test]
    fn flipped_bytes_are_refused_or_fully_consistent() {
        let good = sample().publish().unwrap().into_vec();
        let mut refused_count = 0;
        for i in (0..good.len()).step_by(7) {
            let mut b = good.clone();
            b[i] ^= 0x10;
            match Catalog::open_image(&b, &CatalogLimits::default()) {
                Err(_) => refused_count += 1,
                Ok(opened) => opened.verify().unwrap(),
            }
        }
        assert!(refused_count > 0);
    }

    /// **Limitation, pinned on purpose.** SQLite pages carry no content
    /// checksums, so a flipped bit inside a stored value yields a different,
    /// fully valid catalog that no catalog-level check can distinguish from
    /// the real one. Bit-level integrity of an image is therefore the
    /// metadata-object hash's job (the stored-object hash of the framed image,
    /// O20), verified *before* `open_image` (plan C5). If this test ever
    /// fails because every flip is caught, that is good news; until then,
    /// nothing may rely on SQLite to detect corruption of values.
    #[test]
    fn sqlite_checks_alone_cannot_detect_value_corruption() {
        let c = sample();
        let good = c.publish().unwrap().into_vec();
        let want = c.object(&C1).unwrap();
        // Aim at bytes holding 40, the record's stored length as a 1-byte
        // SQLite integer, so the demonstration is fast and deterministic.
        let silently_different = (0..good.len()).filter(|i| good[*i] == 40).any(|i| {
            let mut b = good.clone();
            b[i] ^= 0x10;
            Catalog::open_image(&b, &CatalogLimits::default())
                .map(|o| o.object(&C1).unwrap() != want)
                .unwrap_or(false)
        });
        assert!(silently_different);
    }

    #[test]
    fn identity_conflicts_and_idempotence() {
        let mut c = sample();
        c.insert_object(&record(), Some(0)).unwrap(); // identical: no-op
        let mut changed = record();
        changed.decoded_len = 99;
        assert_eq!(
            c.insert_object(&changed, None).unwrap_err().code,
            ErrorCode::IdentityConflict
        );

        let dir = FileVersion {
            id: D1,
            kind: EntryKind::Directory,
            logical_len: 0,
            content_hash: None,
        };
        c.insert_file_version(&dir, &[]).unwrap();
        let as_file = FileVersion {
            id: D1,
            kind: EntryKind::File,
            logical_len: 0,
            content_hash: Some(FileContentHash::from_bytes([0; 32])),
        };
        assert_eq!(
            c.insert_file_version(&as_file, &[]).unwrap_err().code,
            ErrorCode::IdentityConflict
        );
    }

    #[test]
    fn commits_must_extend_the_head() {
        let mut c = sample();
        for (seq, parent) in [(2, Some(0)), (1, Some(1)), (5, None)] {
            let e = c
                .append_commit(&Commit {
                    seq,
                    parent,
                    ops: vec![],
                })
                .unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{seq} {parent:?}");
        }
        c.append_commit(&Commit {
            seq: 9,
            parent: Some(1),
            ops: vec![],
        })
        .unwrap();
    }

    #[test]
    fn invalid_commit_writes_nothing() {
        let mut c = sample();
        let before = raw_image(&c);
        let e = c
            .append_commit(&Commit {
                seq: 2,
                parent: Some(1),
                ops: vec![NamespaceOp::Delete { path: p("d") }],
            })
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::NamespaceInvalid);
        assert_eq!(raw_image(&c), before);
    }

    #[test]
    fn no_file_io_settings_hold() {
        let c = Catalog::new_working().unwrap();
        let temp: i64 = c
            .conn
            .query_row("PRAGMA temp_store", [], |r| r.get(0))
            .unwrap();
        assert_eq!(temp, 2, "temp_store must be MEMORY");
        let fk: i64 = c
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        // No code path opens a file or attaches one; that is enforced by
        // ci/check-invariants.sh rule 8, not by SQLite settings.
    }
}
