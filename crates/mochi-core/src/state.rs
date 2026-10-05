//! The authoritative state a checkpoint represents (spec Annex B.2 D10.3), in
//! a form that two representations can be compared in (D10.7, adoption) and
//! that recovery can be checked against (D10.8).
//!
//! * [`AuthoritativeState::from_snapshot`] reads a decoded snapshot manifest
//!   and never touches SQLite: D10.7 requires the snapshot half of adoption to
//!   be checked without it.
//! * [`AuthoritativeState::from_catalog`] projects a catalog the same way
//!   [`Manifest::snapshot_from_catalog`] does (what the namespace reaches),
//!   but through its own code, so comparing the two is not comparing a
//!   function with itself.
//!
//! Historical rows (versions or chunks no longer reachable at the commit) are
//! not part of the state: a snapshot does not carry them (D10.3).

use std::collections::BTreeMap;

use crate::catalog::extent::{Extent, ExtentSource};
use crate::catalog::namespace::FileVersionId;
use crate::catalog::path::ArchivePath;
use crate::catalog::{Catalog, FileVersion};
use crate::error::{ErrorCode, MochiError, Result};
use crate::manifest::{Attributes, Manifest, ManifestKind};
use crate::object::{ObjectId, ObjectRecord};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoritativeState {
    pub seq: u64,
    pub namespace: BTreeMap<ArchivePath, FileVersionId>,
    pub versions: BTreeMap<FileVersionId, (FileVersion, Vec<Extent>)>,
    /// Record and, in a monolithic archive, stored offset.
    pub chunks: BTreeMap<ObjectId, (ObjectRecord, Option<u64>)>,
    /// `None`: this representation carries no promised attributes (a catalog
    /// image until C6, checklist Q6). Comparing `Some` with `None` is a
    /// difference.
    pub attributes: Option<BTreeMap<FileVersionId, Attributes>>,
}

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::RecordInvalid, msg)
}

fn chunks_of(extents: &[Extent]) -> impl Iterator<Item = ObjectId> + '_ {
    extents.iter().filter_map(|e| match e.source {
        ExtentSource::Chunk { chunk, .. } => Some(chunk),
        ExtentSource::Hole => None,
    })
}

impl AuthoritativeState {
    /// From a decoded snapshot manifest. Pure: no SQLite. A delta manifest is
    /// refused (`INVALID_ARGUMENT`). The decoder already guarantees that the
    /// manifest is closed (every namespace version and every extent chunk is
    /// listed); that is re-checked here and reported as `RECORD_INVALID`
    /// rather than assumed.
    pub fn from_snapshot(m: &Manifest) -> Result<Self> {
        if m.kind != ManifestKind::Snapshot {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "authoritative state comes from a snapshot manifest, not a delta",
            ));
        }
        let versions: BTreeMap<_, _> = m
            .file_versions
            .iter()
            .map(|v| (v.version.id, (v.version.clone(), v.extents.clone())))
            .collect();
        let chunks: BTreeMap<_, _> = m
            .chunks
            .iter()
            .map(|c| (c.record.id, (c.record.clone(), c.location)))
            .collect();
        let namespace: BTreeMap<_, _> = m.entries.iter().cloned().collect();
        if versions.len() != m.file_versions.len()
            || chunks.len() != m.chunks.len()
            || namespace.len() != m.entries.len()
        {
            return Err(invalid("snapshot manifest lists an entry twice"));
        }
        for (path, id) in &namespace {
            if !versions.contains_key(id) {
                return Err(invalid(format!(
                    "snapshot manifest: the entry at {path:?} names an unlisted version"
                )));
            }
        }
        for (id, (_, extents)) in &versions {
            if let Some(c) = chunks_of(extents).find(|c| !chunks.contains_key(c)) {
                return Err(invalid(format!(
                    "snapshot manifest: version {id:?} has an extent in the unlisted chunk {}",
                    c.to_hex()
                )));
            }
        }
        let attributes = m
            .file_versions
            .iter()
            .map(|v| (v.version.id, v.attributes))
            .collect();
        Ok(AuthoritativeState {
            seq: m.commit_seq,
            namespace,
            versions,
            chunks,
            attributes: Some(attributes),
        })
    }

    /// From a catalog at commit `seq`: the namespace replayed to `seq`, then
    /// each version the namespace reaches with its extents, then each chunk
    /// those extents reach. `attributes` is `None`.
    pub fn from_catalog(c: &Catalog, seq: u64) -> Result<Self> {
        let replayed = c.replay(Some(seq))?;
        let mut namespace = BTreeMap::new();
        let mut versions = BTreeMap::new();
        let mut chunks = BTreeMap::new();
        for (path, entry) in replayed.iter() {
            namespace.insert(path.clone(), entry.version);
            if versions.contains_key(&entry.version) {
                continue;
            }
            let (version, extents) = c.file_version(&entry.version)?.ok_or_else(|| {
                MochiError::new(
                    ErrorCode::CatalogInvalid,
                    "the namespace names a version the catalog does not hold",
                )
            })?;
            for id in chunks_of(&extents) {
                if chunks.contains_key(&id) {
                    continue;
                }
                let record = c.object(&id)?.ok_or_else(|| {
                    MochiError::new(
                        ErrorCode::CatalogInvalid,
                        "an extent names a chunk the catalog does not hold",
                    )
                })?;
                chunks.insert(id, (record, c.object_location(&id)?));
            }
            versions.insert(entry.version, (version, extents));
        }
        Ok(AuthoritativeState {
            seq,
            namespace,
            versions,
            chunks,
            attributes: None,
        })
    }

    pub fn with_attributes(mut self, a: BTreeMap<FileVersionId, Attributes>) -> Self {
        self.attributes = Some(a);
        self
    }

    pub fn without_attributes(mut self) -> Self {
        self.attributes = None;
        self
    }

    /// Up to `max` differences between `self` and `other`, in a fixed order
    /// (sequence, namespace, versions, chunks, attributes), each a short
    /// human-readable line. Empty exactly when the states are equal.
    pub fn differences(&self, other: &Self, max: usize) -> Vec<String> {
        let mut out = Vec::new();
        let push = |s: String, out: &mut Vec<String>| {
            if out.len() < max {
                out.push(s);
            }
        };
        if self.seq != other.seq {
            push(format!("sequence {} vs {}", self.seq, other.seq), &mut out);
        }
        diff_maps(
            "path",
            &self.namespace,
            &other.namespace,
            |p| format!("{p:?}"),
            |a, b| format!("version {a:?} vs {b:?}"),
            &mut out,
            max,
        );
        diff_maps(
            "version",
            &self.versions,
            &other.versions,
            |id| format!("{id:?}"),
            |a, b| {
                if a.0 != b.0 {
                    "record differs".to_string()
                } else {
                    format!("extents differ ({} vs {})", a.1.len(), b.1.len())
                }
            },
            &mut out,
            max,
        );
        diff_maps(
            "chunk",
            &self.chunks,
            &other.chunks,
            |id| id.to_hex(),
            |a, b| {
                if a.0 != b.0 {
                    "record differs".to_string()
                } else {
                    format!("location {:?} vs {:?}", a.1, b.1)
                }
            },
            &mut out,
            max,
        );
        match (&self.attributes, &other.attributes) {
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => {
                push("attributes: one side carries none".to_string(), &mut out)
            }
            (Some(a), Some(b)) => diff_maps(
                "attributes of version",
                a,
                b,
                |id| format!("{id:?}"),
                |_, _| "differ".to_string(),
                &mut out,
                max,
            ),
        }
        out
    }
}

/// Differences between two maps: keys on one side only, and values that
/// differ. `what` names the kind of key.
fn diff_maps<K: Ord, V: PartialEq>(
    what: &str,
    a: &BTreeMap<K, V>,
    b: &BTreeMap<K, V>,
    key: impl Fn(&K) -> String,
    value: impl Fn(&V, &V) -> String,
    out: &mut Vec<String>,
    max: usize,
) {
    for (k, va) in a {
        if out.len() >= max {
            return;
        }
        match b.get(k) {
            None => out.push(format!("{what} {}: only on the left", key(k))),
            Some(vb) if va != vb => out.push(format!("{what} {}: {}", key(k), value(va, vb))),
            Some(_) => {}
        }
    }
    for k in b.keys().filter(|k| !a.contains_key(k)) {
        if out.len() >= max {
            return;
        }
        out.push(format!("{what} {}: only on the right", key(k)));
    }
}

#[cfg(test)]
mod tests {
    use mochi_format::digest::file_content_hash;
    use mochi_format::repr::{DecodedBytes, DecodedSlice};

    use super::*;
    use crate::catalog::namespace::{EntryKind, NamespaceOp};
    use crate::catalog::path::ArchivePath;
    use crate::catalog::Commit;
    use crate::manifest::{Mtime, PosixAttributes};
    use crate::object::{build_object, IdSource};
    use mochi_format::codec::{EncodeParams, Protection};
    use mochi_format::Limits;

    struct Ids(u8);
    impl IdSource for Ids {
        fn next_id(&mut self) -> Result<[u8; 32]> {
            self.0 += 1;
            Ok([self.0; 32])
        }
    }

    fn p(s: &str) -> ArchivePath {
        ArchivePath::from_stored(s.as_bytes()).unwrap()
    }

    fn attrs(mode: u32) -> Attributes {
        Attributes {
            posix: Some(PosixAttributes {
                mode,
                uid: 1,
                gid: 2,
            }),
            windows: None,
            mtime: Some(Mtime { secs: 5, nanos: 6 }),
        }
    }

    /// Two commits: 0 puts `a` (one chunk) and a directory `d`; 1 replaces
    /// `a` and deletes `d`, so version/chunk of the first `a` are history.
    struct Built {
        cat: Catalog,
        attrs: BTreeMap<FileVersionId, Attributes>,
    }

    fn build() -> Built {
        let mut ids = Ids(0);
        let mut cat = Catalog::new_working().unwrap();
        let mut all_attrs = BTreeMap::new();
        let file = |cat: &mut Catalog,
                    ids: &mut Ids,
                    all: &mut BTreeMap<FileVersionId, Attributes>,
                    bytes: &[u8],
                    mode: u32,
                    at: u64| {
            let obj = build_object(
                &DecodedBytes::new(bytes.to_vec()),
                &EncodeParams::default(),
                Protection::None,
                ids,
                &Limits::default(),
            )
            .unwrap();
            cat.insert_object(&obj.record, Some(at)).unwrap();
            let v = FileVersion {
                id: FileVersionId::from_bytes(ids.next_id().unwrap()),
                kind: EntryKind::File,
                logical_len: bytes.len() as u64,
                content_hash: Some(file_content_hash(DecodedSlice::from_logical(bytes))),
            };
            let ext = Extent {
                ordinal: 0,
                logical_offset: 0,
                length: bytes.len() as u64,
                source: ExtentSource::Chunk {
                    chunk: obj.record.id,
                    chunk_offset: 0,
                },
            };
            cat.insert_file_version(&v, &[ext]).unwrap();
            all.insert(v.id, attrs(mode));
            v.id
        };
        let a1 = file(&mut cat, &mut ids, &mut all_attrs, b"first", 0o600, 100);
        let d = FileVersion {
            id: FileVersionId::from_bytes(ids.next_id().unwrap()),
            kind: EntryKind::Directory,
            logical_len: 0,
            content_hash: None,
        };
        cat.insert_file_version(&d, &[]).unwrap();
        all_attrs.insert(d.id, attrs(0o755));
        cat.append_commit(&Commit {
            seq: 0,
            parent: None,
            ops: vec![
                NamespaceOp::Put {
                    path: p("a"),
                    version: a1,
                },
                NamespaceOp::Put {
                    path: p("d"),
                    version: d.id,
                },
            ],
        })
        .unwrap();
        let a2 = file(&mut cat, &mut ids, &mut all_attrs, b"second", 0o640, 200);
        cat.append_commit(&Commit {
            seq: 1,
            parent: Some(0),
            ops: vec![
                NamespaceOp::Put {
                    path: p("a"),
                    version: a2,
                },
                NamespaceOp::Delete { path: p("d") },
            ],
        })
        .unwrap();
        Built {
            cat,
            attrs: all_attrs,
        }
    }

    fn snapshot(b: &Built, seq: u64) -> Manifest {
        Manifest::snapshot_from_catalog(
            &b.cat,
            crate::object::ArchiveId::from_bytes([9; 32]),
            seq,
            [3; 16],
            &b.attrs,
        )
        .unwrap()
    }

    fn state_of_snapshot(b: &Built, seq: u64) -> AuthoritativeState {
        AuthoritativeState::from_snapshot(&snapshot(b, seq)).unwrap()
    }

    #[test]
    fn snapshot_and_catalog_projections_agree() {
        let b = build();
        for seq in 0..=1 {
            let from_snap = state_of_snapshot(&b, seq);
            let attrs = from_snap.attributes.clone().unwrap();
            let from_cat = AuthoritativeState::from_catalog(&b.cat, seq)
                .unwrap()
                .with_attributes(attrs);
            assert_eq!(from_snap, from_cat, "commit {seq}");
            assert!(from_snap.differences(&from_cat, 8).is_empty());
        }
    }

    #[test]
    fn history_rows_are_not_state() {
        let b = build();
        // At commit 1 the first `a` version, its chunk, and the directory
        // are history: not reachable, so not in the state.
        let s = AuthoritativeState::from_catalog(&b.cat, 1).unwrap();
        assert_eq!(s.namespace.len(), 1);
        assert_eq!(s.versions.len(), 1);
        assert_eq!(s.chunks.len(), 1);
        let at0 = AuthoritativeState::from_catalog(&b.cat, 0).unwrap();
        assert_eq!(at0.namespace.len(), 2);
        assert_eq!(at0.versions.len(), 2);
        assert_eq!(at0.chunks.len(), 1);
    }

    #[test]
    fn differences_name_each_kind() {
        let b = build();
        let base = state_of_snapshot(&b, 0);
        assert!(base.differences(&base.clone(), 8).is_empty());

        let find = |s: &AuthoritativeState, what: &str| -> Vec<String> {
            base.differences(s, 8)
                .into_iter()
                .filter(|l| l.contains(what))
                .collect()
        };

        // a changed attribute
        let mut s = base.clone();
        let id = *s.attributes.as_ref().unwrap().keys().next().unwrap();
        s.attributes.as_mut().unwrap().insert(id, attrs(0o777));
        assert_eq!(find(&s, "attributes of version").len(), 1);

        // a missing path
        let mut s = base.clone();
        s.namespace.remove(&p("d"));
        assert!(find(&s, "only on the left")
            .iter()
            .any(|l| l.starts_with("path")));

        // an extra path
        let mut s = base.clone();
        s.namespace
            .insert(p("zz"), *base.namespace.values().next().unwrap());
        assert!(find(&s, "only on the right")
            .iter()
            .any(|l| l.starts_with("path")));

        // a changed extent
        let mut s = base.clone();
        let (_, ext) = s
            .versions
            .values_mut()
            .find(|(_, e)| !e.is_empty())
            .unwrap();
        ext[0].length += 1;
        assert!(find(&s, "extents differ").len() == 1);

        // a changed location
        let mut s = base.clone();
        s.chunks.values_mut().next().unwrap().1 = Some(9999);
        assert!(find(&s, "location").len() == 1);

        // attributes present vs absent
        let s = base.clone().without_attributes();
        assert_eq!(find(&s, "carries none").len(), 1);
        assert_ne!(base, s);

        // sequence
        let mut s = base.clone();
        s.seq += 1;
        assert!(find(&s, "sequence").len() == 1);
    }

    #[test]
    fn differences_stop_at_the_limit() {
        let b = build();
        let base = state_of_snapshot(&b, 0);
        let empty = AuthoritativeState {
            seq: 0,
            namespace: BTreeMap::new(),
            versions: BTreeMap::new(),
            chunks: BTreeMap::new(),
            attributes: None,
        };
        assert_eq!(base.differences(&empty, 2).len(), 2);
        assert!(base.differences(&empty, 100).len() > 2);
    }

    #[test]
    fn from_snapshot_refuses_a_delta() {
        let mut m = snapshot(&build(), 0);
        m.kind = ManifestKind::Delta;
        let e = AuthoritativeState::from_snapshot(&m).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn from_snapshot_refuses_an_unclosed_snapshot() {
        let b = build();
        let mut m = snapshot(&b, 0);
        let gone = m.file_versions.remove(0).version.id;
        assert!(m.entries.iter().any(|(_, v)| *v == gone));
        let e = AuthoritativeState::from_snapshot(&m).unwrap_err();
        assert_eq!(e.code, ErrorCode::RecordInvalid);

        let mut m = snapshot(&b, 0);
        m.chunks.clear();
        let e = AuthoritativeState::from_snapshot(&m).unwrap_err();
        assert_eq!(e.code, ErrorCode::RecordInvalid);
    }

    #[test]
    fn from_snapshot_does_not_use_sqlite() {
        let b = build();
        let m = snapshot(&b, 1);
        let before = crate::catalog::replay_count();
        let s = AuthoritativeState::from_snapshot(&m).unwrap();
        assert_eq!(crate::catalog::replay_count(), before);
        assert_eq!(s.seq, 1);
    }
}
