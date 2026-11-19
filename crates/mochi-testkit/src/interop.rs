//! The reference side of `mochi-interop-summary-v1` (plan W16, W17 "Summary
//! format"; R8). One JSON document per case, compared **exactly** with the
//! independent verifier's (R9). The rules that make it reproducible are in
//! `fixtures/golden/README.md` ("R8 summaries"):
//!
//! * the chain and the snapshots are read through the parity-healed view
//!   when the archive declares a recovery record, otherwise from the raw
//!   bytes;
//! * `snapshots` has one entry per commit of the verified chain that opens;
//! * `findings` holds the violation-class codes `verify` reports (plus the
//!   head's refusal), each with the stored-object hash of the damaged
//!   *data object* it concerns, or `null`.
//!
//! Test infrastructure: unwraps freely.

use std::collections::{BTreeMap, BTreeSet};

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::catalog::namespace::EntryKind;
use mochi_core::job::{CancellationToken, JobContext, NullProgress};
use mochi_core::object::{decode_verified, load_stored, verify_stored};
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, promised_attributes, ReadOptions,
};
use mochi_core::report::{Report, Severity};
use mochi_core::status::VerificationLevel;
use mochi_core::storage::ReadStorage;
use mochi_core::verify::{
    classify, parse_commit_id, verify, ErrorClass, FreshnessAnchor, VerifyOptions,
};
use mochi_core::ErrorCode;
use serde_json::{json, Value};

use crate::archive::keyed_read;

pub const FORMAT: &str = "mochi-interop-summary-v1";

/// Everything one summary is computed from.
pub struct Input<'a> {
    pub name: &'a str,
    pub storage: &'a dyn ReadStorage,
    /// The test passphrase of an Encrypted case; `None` reads keylessly.
    pub passphrase: Option<&'a str>,
    /// Lowercase hex of the head the verifier is told to expect (freshness).
    pub expected_head: Option<&'a str>,
}

pub fn read_options(passphrase: Option<&str>) -> ReadOptions {
    match passphrase {
        Some(p) => keyed_read(&[p]),
        None => ReadOptions::default(),
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn run_ctx<R>(f: impl FnOnce(&JobContext<'_>) -> R) -> R {
    let (progress, cancel) = (NullProgress, CancellationToken::default());
    f(&JobContext {
        progress: &progress,
        cancel: &cancel,
    })
}

/// `verify` at `restoration`, deep (what `fsck` asks for), as the reference
/// for each case's `verify_exit`.
pub fn verify_case(i: &Input<'_>) -> Report {
    let anchor = match i.expected_head {
        Some(h) => FreshnessAnchor::User {
            commit_id: parse_commit_id(h).expect("expected head is a commit ID"),
        },
        None => FreshnessAnchor::None,
    };
    run_ctx(|ctx| {
        verify(
            i.storage,
            &VerifyOptions {
                level: VerificationLevel::Restoration,
                read: read_options(i.passphrase),
                anchor,
                require_freshness: false,
                deep: true,
            },
            ctx,
        )
        .report
    })
}

/// The violation-class codes a report carries, in registry order.
fn violation_codes(r: &Report) -> BTreeSet<&'static str> {
    r.findings
        .iter()
        .filter(|f| classify(f.code) == ErrorClass::Violation)
        .filter(|f| f.severity != Severity::Info)
        .map(|f| f.code.as_str())
        .collect()
}

pub fn summarize(i: &Input<'_>) -> Value {
    let ro = read_options(i.passphrase);
    let report = verify_case(i);

    // The parity scan (Redundancy profile): numbers, and the healed view.
    let scan = run_ctx(|ctx| mochi_core::parity::scan(i.storage, &ro, ctx)).ok();
    let declared = scan.as_ref().and_then(|(s, _)| s.declared_percent);
    let (parity, view): (Value, &dyn ReadStorage) = match (&scan, declared) {
        (Some((s, v)), Some(_)) => (
            json!({
                "commits": s.commits.len(),
                "damaged_shards": s.shards_damaged(),
                "rebuilt": s.shards_rebuilt(),
                "beyond_capacity": s.beyond_capacity().count(),
            }),
            v as &dyn ReadStorage,
        ),
        _ => (Value::Null, i.storage),
    };

    let mut out = json!({
        "format": FORMAT,
        "case": i.name,
        "head": {"status": "opened", "code": null},
        "archive_id": null,
        "profile": {"encrypted": false, "tar_compatible": false, "recovery_percent": null},
        "commits": [],
        "snapshots": [],
        "findings": [],
        "parity": parity,
    });

    let mut codes: BTreeSet<&'static str> = violation_codes(&report);
    let mut object_findings: BTreeSet<(&'static str, String)> = BTreeSet::new();

    let history = commit_history(view, &ro);
    let head = open_head(view, &ro);
    match (&history, &head) {
        (Ok(history), Ok(h)) => {
            let d = h.descriptor.profile();
            out["archive_id"] = json!(h.commit.archive_id.to_hex());
            out["profile"] = json!({
                "encrypted": d.encrypted,
                "tar_compatible": d.tar_compatible,
                "recovery_percent": d.redundancy,
            });
            let mut commits = Vec::new();
            let mut snapshots = Vec::new();
            for e in history {
                commits.push(json!({
                    "seq": e.commit.seq,
                    "commit_id": e.commit_id.to_hex(),
                    "footer_offset": e.footer_offset,
                    "checkpoint": e.commit.metadata.is_checkpoint(),
                }));
                let Ok(o) = open_at_footer(view, e.footer_offset, &ro) else {
                    continue;
                };
                let attrs = promised_attributes(view, &o, &ro).ok();
                let ns = o.catalog.replay(None).expect("replay");
                let mut entries = Vec::new();
                for (p, entry) in ns.iter() {
                    let (v, _) = o
                        .catalog
                        .file_version(&entry.version)
                        .expect("version")
                        .expect("version exists");
                    let a = attrs.as_ref().and_then(|m| m.get(&entry.version));
                    let posix = a.and_then(|a| a.posix);
                    entries.push(json!({
                        "path": hex(p.as_stored()),
                        "kind": match entry.kind { EntryKind::File => "file", EntryKind::Directory => "dir" },
                        "size": v.logical_len,
                        "content_hash": v.content_hash.map(|h| h.to_hex()),
                        "target": null,
                        "attributes": {
                            "posix_mode": posix.map(|p| p.mode),
                            "uid": posix.map(|p| p.uid),
                            "gid": posix.map(|p| p.gid),
                            "mtime_ns": a.and_then(|a| a.mtime).map(|m| {
                                m.secs.checked_mul(1_000_000_000)
                                    .and_then(|s| s.checked_add(i64::from(m.nanos)))
                                    .expect("mtime fits")
                            }),
                            "windows": a.and_then(|a| a.windows),
                        },
                    }));
                }
                snapshots.push(json!({"seq": e.commit.seq, "entries": entries}));
            }
            out["commits"] = Value::Array(commits);
            out["snapshots"] = Value::Array(snapshots);

            // Damaged data objects of the head's catalog, by stored hash.
            let seal = h.seal_context();
            for id in h.catalog.object_ids().expect("ids") {
                let rec = h.catalog.object(&id).unwrap().unwrap();
                let Some(at) = h.catalog.object_location(&id).unwrap() else {
                    continue;
                };
                let hash = rec.stored_hash.to_hex();
                match load_stored(view, at, &rec, &ro.limits) {
                    Err(_) => {
                        object_findings.insert((ErrorCode::StoredIntegrityFailed.as_str(), hash));
                    }
                    Ok(stored) => {
                        if verify_stored(&rec, &stored).is_err() {
                            object_findings
                                .insert((ErrorCode::StoredIntegrityFailed.as_str(), hash));
                        } else if let Err(e) =
                            decode_verified(&rec, &stored, &ro.limits, seal.as_ref())
                        {
                            if e.code == ErrorCode::ContentIntegrityFailed {
                                object_findings.insert((e.code.as_str(), hash));
                            }
                        }
                    }
                }
            }
            let _ = ExtentSource::Hole;
        }
        (Ok(history), Err(e)) if e.code == ErrorCode::KeyUnavailable && i.passphrase.is_none() => {
            // Keyless: commit records are plaintext; names and content are not.
            out["head"] = json!({"status": "keyless", "code": null});
            out["snapshots"] = Value::Null;
            out["commits"] = Value::Array(
                history
                    .iter()
                    .map(|e| {
                        json!({
                            "seq": e.commit.seq,
                            "commit_id": e.commit_id.to_hex(),
                            "footer_offset": e.footer_offset,
                            "checkpoint": e.commit.metadata.is_checkpoint(),
                        })
                    })
                    .collect(),
            );
            if let Some(h) = history.last() {
                out["archive_id"] = json!(h.commit.archive_id.to_hex());
            }
            // The descriptor is plaintext: the profile is known without the key.
            if let Some(d) = mochi_core::parity::read_descriptor_raw(view, &ro) {
                let d = d.profile();
                out["profile"] = json!({
                    "encrypted": d.encrypted,
                    "tar_compatible": d.tar_compatible,
                    "recovery_percent": d.redundancy,
                });
            }
        }
        (_, Err(e)) | (Err(e), _) => {
            out["head"] = json!({"status": "refused", "code": e.code.as_str()});
            codes.insert(e.code.as_str());
        }
    }

    // One finding per (code, object); a code with no object-specific
    // finding appears once with a null object.
    let mut rows: BTreeMap<(String, Option<String>), ()> = BTreeMap::new();
    let with_object: BTreeSet<&str> = object_findings.iter().map(|(c, _)| *c).collect();
    for (c, h) in &object_findings {
        rows.insert(((*c).to_owned(), Some(h.clone())), ());
    }
    for c in &codes {
        if !with_object.contains(c) {
            rows.insert(((*c).to_owned(), None), ());
        }
    }
    out["findings"] = Value::Array(
        rows.keys()
            .map(|(c, o)| json!({"code": c, "object": o}))
            .collect(),
    );
    out
}
