//! R8 recipes, second half: corrupt, interrupted, copy, salvage, legacy.

use mochi_core::commit::Metadata;
use mochi_core::compact::{compact, CompactOptions, Keep};
use mochi_core::parity::scan;
use mochi_core::publish::{
    ArchiveWriter, CheckpointPolicy, ReadOptions, TailPolicy, TarTamper, Transaction,
};
use mochi_format::footer::{encode_footer_frame, FOOTER_FRAME_LEN};
use mochi_format::frame::encode_skippable_frame;
use mochi_format::parity::Layout;
use mochi_format::registry::FrameKind;
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::forge::{
    self, delta_record, empty_delta, put_forged, rule_base, txid, DictionaryDependency, Forge,
};
use mochi_testkit::replay::attrs;
use mochi_testkit::{Fault, SeqIds, SimDir, SimStorage};

use crate::recipes::*;

fn one(main: Vec<u8>) -> Built {
    Built {
        main,
        copies: Vec::new(),
    }
}

fn plain() -> ReadOptions {
    ReadOptions::default()
}

fn bytes_of(s: &SimStorage) -> Vec<u8> {
    s.contents()
}

/// The data shards of the head's parity group to damage, by layout.
fn layouts(bytes: &[u8], percent: u16) -> Vec<Layout> {
    let s = SimStorage::from_bytes(bytes.to_vec());
    let (sc, _) = scan(&s, &plain(), &Job::new().ctx()).unwrap();
    sc.commits
        .iter()
        .map(|c| Layout::compute(c.range_offset.unwrap(), c.range_len.unwrap(), percent).unwrap())
        .collect()
}

fn hit(b: &mut [u8], l: &Layout, data_indices: impl IntoIterator<Item = u64>) {
    for j in data_indices {
        let (off, n) = l.shard_extent(l.shard_index(0, j));
        assert!(n > 0);
        b[(off + n / 3) as usize] ^= 0x3C;
    }
}

pub fn build(cx: &mut Ctx, name: &str) -> Built {
    let enc = name.starts_with("enc");
    let read = read_for(enc);
    match name {
        "red-valid-50pct" => one(write(red(50), 36, &history_big()).contents()),

        // ---- corrupt: Core ----
        "core-corrupt-chunk-head" | "tar-corrupt-chunk-head" | "enc-corrupt-chunk-head" => {
            let base = cx.base(&name.replace("corrupt-chunk-head", "valid-history"));
            let mut b = base.clone();
            let (at, n) = chunk_range(&base, &read, 6, "h");
            flip(&mut b, at + n / 2);
            one(b)
        }
        "core-corrupt-chunk-older" => {
            let base = cx.base("core-valid-history");
            let mut b = base.clone();
            let (at, n) = chunk_range(&base, &read, 0, "d/a");
            flip(&mut b, at + n / 2);
            one(b)
        }
        "core-corrupt-all-images" => {
            let base = cx.base("core-valid-history");
            let mut b = base.clone();
            for e in history_of(&base, &plain()) {
                if let Metadata::Checkpoint { image, .. } = e.commit.metadata {
                    damage_ref(&mut b, image);
                }
            }
            one(b)
        }
        "core-corrupt-delta-manifest" => {
            let base = cx.base("core-valid-history");
            let mut b = base.clone();
            damage_ref(&mut b, history_of(&base, &plain())[3].commit.delta_manifest);
            one(b)
        }
        "core-corrupt-latest-footer" => {
            let mut b = cx.base("core-valid-history");
            let n = b.len();
            b[n - 1] ^= 1;
            one(b)
        }
        "core-corrupt-descriptor" => {
            let base = cx.base("core-valid-history");
            let mut b = base.clone();
            damage_ref(&mut b, history_of(&base, &plain())[0].commit.descriptor);
            one(b)
        }
        "core-corrupt-middle-commit" => {
            let base = cx.base("core-valid-history");
            let mut b = base.clone();
            flip(&mut b, history_of(&base, &plain())[3].commit_offset + 16);
            one(b)
        }
        "core-corrupt-oversized-frame" => {
            let base = cx.base("core-valid-history");
            let mut b = base.clone();
            let m = history_of(&base, &plain())[4].commit.delta_manifest;
            let at = m.offset as usize + 4;
            b[at..at + 4].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
            one(b)
        }
        "core-corrupt-noncanonical-commit" => {
            let base = cx.base("core-valid-history");
            let mut f = Forge::new(base);
            let head = f.history().pop().unwrap();
            let r = f.append_manifest(&empty_delta(&head, txid(0x41)));
            let rec = delta_record(&head, rule_base(&head), r, txid(0x41));
            let (payload, _) = rec.encode().unwrap();
            // Key 0's header in a longer-than-shortest form, as the c5 vector does.
            let mut p = vec![payload[0], 0x18];
            p.extend_from_slice(&payload[1..]);
            let frame = encode_skippable_frame(FrameKind::CommitRecord, &p).unwrap();
            let at = f.bytes.len() as u64;
            f.bytes.extend_from_slice(&frame);
            f.bytes
                .extend_from_slice(&encode_footer_frame(at, rec.seq, &frame));
            one(f.bytes)
        }
        "core-corrupt-dictionary-dependency" => {
            let base = cx.base("core-valid-history");
            one(forge::put_with_dependency(base, "dict.bin", DictionaryDependency::Present).0)
        }
        "core-corrupt-missing-dictionary" => {
            let base = cx.base("core-valid-history");
            one(forge::put_with_dependency(base, "dict.bin", DictionaryDependency::Missing).0)
        }
        "core-corrupt-content-hash" => {
            let base = cx.base("core-valid-history");
            one(put_forged(
                base,
                "bad.bin",
                DictionaryDependency::None,
                forge::Fault::ContentHash,
            )
            .0)
        }
        "core-corrupt-traversal-path" => {
            let base = cx.base("core-valid-history");
            one(put_forged(
                base,
                "x/y",
                DictionaryDependency::None,
                forge::Fault::RawPath(b"docs/../x"),
            )
            .0)
        }
        "core-corrupt-snapshot-bound-elsewhere" => {
            let mut f = Forge::new(cx.base("core-valid-history"));
            let head = f.history().pop().unwrap();
            let mut rec = head.commit.clone();
            rec.seq = head.commit.seq + 1;
            rec.transaction_id = txid(0x33);
            rec.parent = Some(forge::link(&head));
            f.append_commit(&rec);
            one(f.bytes)
        }
        "core-corrupt-orphan-put" => one(put_forged(
            cx.base("core-valid-history"),
            "nodir/x",
            DictionaryDependency::None,
            forge::Fault::None,
        )
        .0),
        "core-corrupt-short-extents" => one(put_forged(
            cx.base("core-valid-history"),
            "short.bin",
            DictionaryDependency::None,
            forge::Fault::ExtentShort,
        )
        .0),
        "core-corrupt-undefined-version" => one(put_forged(
            cx.base("core-valid-history"),
            "undefined.bin",
            DictionaryDependency::None,
            forge::Fault::UndefinedVersion,
        )
        .0),
        "core-corrupt-reused-chunk-id" => {
            let base = cx.base("core-valid-history");
            let head =
                mochi_core::publish::open_head(&SimStorage::from_bytes(base.clone()), &plain())
                    .unwrap();
            let id = *head.catalog.object_ids().unwrap()[0].as_bytes();
            one(put_forged(
                base,
                "conflict.bin",
                DictionaryDependency::None,
                forge::Fault::ReuseChunkId(id),
            )
            .0)
        }
        "core-corrupt-image-pages" => one(crate::recipes3::damaged_image_checkpoint(
            cx.base("core-valid-history"),
        )),
        "core-corrupt-oversized-tail-frame" => {
            let mut b = cx.base("core-valid-history");
            b.extend_from_slice(&mochi_format::registry::SEARCH_INDEX.to_le_bytes());
            b.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
            one(b)
        }
        "core-corrupt-manifest-not-a-frame" => one(forge::delta_with_raw_manifest(
            cx.base("core-valid-history"),
            vec![0xAB; 48],
        )),
        "core-corrupt-manifest-cut-short" => {
            let mut frame =
                encode_skippable_frame(FrameKind::RecoveryManifest, &[1u8; 40]).unwrap();
            frame.truncate(frame.len() - 10);
            one(forge::delta_with_raw_manifest(
                cx.base("core-valid-history"),
                frame,
            ))
        }
        "core-corrupt-older-archive" => {
            let base = cx.base("core-valid-history");
            let h = history_of(&base, &plain());
            let cut = (h[4].footer_offset + FOOTER_FRAME_LEN) as usize;
            one(base[..cut].to_vec())
        }
        "core-corrupt-truncated-mid-history" => {
            let base = cx.base("core-valid-history");
            let h = history_of(&base, &plain());
            one(base[..(h[4].commit_offset + 10) as usize].to_vec())
        }

        // ---- corrupt: TAR ----
        "tar-corrupt-stream" => {
            let plan = Plan {
                prof: TAR,
                chunk: None,
                policy: CheckpointPolicy::Every(3),
                seed: 41,
            };
            let s = write_with(&plan, &history(), |i, w| {
                w.set_tar_tamper((i == 4).then_some(TarTamper::ModeOff));
            });
            one(bytes_of(&s))
        }

        // ---- corrupt: Encrypted ----
        "enc-corrupt-wrong-passphrase" => one(cx.base("enc-valid-history")),
        "enc-corrupt-envelopes-unordered" => one(crate::recipes3::forged_enc_commit(
            cx.base("enc-valid-rekeyed"),
            crate::recipes3::Enc11::Unordered,
        )),
        "enc-corrupt-envelope-duplicate" => one(crate::recipes3::forged_enc_commit(
            cx.base("enc-valid-rekeyed"),
            crate::recipes3::Enc11::Duplicate,
        )),
        "enc-corrupt-data-region-kind" => one(crate::recipes3::forged_enc_commit(
            cx.base("enc-valid-rekeyed"),
            crate::recipes3::Enc11::RegionNotData,
        )),
        "enc-corrupt-aead-tag" => one(crate::recipes3::aead_tag(cx.base("enc-valid-history"))),

        // ---- corrupt: Redundancy ----
        "red-corrupt-within-capacity" | "tar-red-corrupt-within-capacity" => {
            let base = cx.base(if name.starts_with("tar") {
                "tar-red-valid-history"
            } else {
                "red-valid-50pct"
            });
            let ls = layouts(&base, 50);
            let (head, first) = (ls[0], ls[ls.len() - 1]);
            let mut b = base.clone();
            hit(&mut b, &first, 0..first.parity_shards);
            hit(&mut b, &head, 0..head.parity_shards);
            one(b)
        }
        "red-corrupt-beyond-capacity" | "enc-red-corrupt-beyond-capacity" => {
            let base = cx.base(if name.starts_with("enc") {
                "enc-red-valid-history"
            } else {
                "red-valid-50pct"
            });
            // The largest commit (the first): enough shards to overrun its parity.
            let ls = layouts(&base, 50);
            let head = *ls.last().unwrap();
            let mut b = base.clone();
            let over = head.parity_shards + 1;
            assert!(
                over <= head.real_shards(),
                "the commit has enough shards to overrun"
            );
            hit(&mut b, &head, 0..over);
            one(b)
        }
        "red-corrupt-parity-frame" => {
            let base = cx.base("red-valid-50pct");
            let head = layouts(&base, 50)[0];
            let mut b = base.clone();
            let commit_end = head.range_offset + head.range_len;
            flip(&mut b, commit_end + head.frame_len(0) / 2);
            one(b)
        }
        "red-corrupt-forged-parity" => {
            use mochi_format::digest::parity_frame_digest;
            use mochi_format::repr::StoredObjectBytes;
            let base = cx.base("red-valid-50pct");
            let l = layouts(&base, 50)[0];
            let mut b = base.clone();
            let commit_end = (l.range_offset + l.range_len) as usize;
            let flen = l.frame_len(0) as usize;
            for j in 0..l.frame_count() as usize {
                let f = &mut b[commit_end + j * flen..commit_end + (j + 1) * flen];
                f[8 + 56] ^= 1; // the envelope's commit sequence
                let at = flen - 32;
                let d = parity_frame_digest(StoredObjectBytes::new(&f[..at]));
                f[at..].copy_from_slice(d.as_bytes());
            }
            one(b)
        }

        // ---- interrupted ----
        "core-interrupted-after-data" => {
            interrupted(cx, "core-valid-history", false, Stage::AfterData)
        }
        "core-interrupted-after-commit-frame" => {
            interrupted(cx, "core-valid-history", false, Stage::AfterCommitFrame)
        }
        "core-interrupted-torn-footer" => {
            interrupted(cx, "core-valid-history", false, Stage::TornFooter)
        }
        "core-interrupted-lost-write" => {
            // The commit and its footer are complete, but the last sync never
            // returned and a sector under a referenced object reads as zeros.
            let base = cx.base("core-valid-history");
            let full = append_one(&base, false, 990, next_tx());
            let mut b = full.contents();
            let h = history_of(&b, &plain());
            let (at, n) = chunk_range(&b, &plain(), h.len() as u64 - 1, "next.bin");
            b[(at + n / 2) as usize..(at + n) as usize].fill(0);
            one(b)
        }
        "tar-interrupted-after-commit-frame" => {
            interrupted(cx, "tar-valid-history", false, Stage::AfterCommitFrame)
        }
        "enc-interrupted-after-data" => {
            interrupted(cx, "enc-valid-history", true, Stage::AfterData)
        }
        "red-interrupted-after-parity" => {
            interrupted(cx, "red-valid-50pct", false, Stage::AfterParity)
        }
        "tar-red-interrupted-after-parity" => {
            interrupted(cx, "tar-red-valid-history", false, Stage::AfterParity)
        }
        "enc-red-interrupted-after-commit-frame" => {
            interrupted(cx, "enc-red-valid-history", true, Stage::AfterCommitFrame)
        }
        "core-interrupted-compaction" => {
            let base = cx.base("core-valid-history");
            let src = SimStorage::from_bytes(base);
            let lock = || {
                ArchiveWriter::open_append(
                    src.clone(),
                    Box::new(SeqIds::new(61)),
                    test_options(),
                    TailPolicy::Refuse,
                )
                .unwrap()
                .0
            };
            // Find how many mutations a complete compaction makes, then stop
            // three short of it.
            let mut total = 0;
            for k in 0.. {
                let w = lock();
                let d = SimDir::new();
                d.add_next_file_fault(Fault::HaltBeforeMutation { mutation_index: k });
                let r = compact(
                    &w,
                    &mut { d },
                    "out.mochi",
                    Box::new(SeqIds::new(62)),
                    &Keep::Every,
                    &CompactOptions::default(),
                    &Job::new().ctx(),
                );
                if r.is_ok() {
                    total = k;
                    break;
                }
            }
            let w = lock();
            let mut d = SimDir::new();
            d.add_next_file_fault(Fault::HaltBeforeMutation {
                mutation_index: total - 3,
            });
            compact(
                &w,
                &mut d,
                "out.mochi",
                Box::new(SeqIds::new(62)),
                &Keep::Every,
                &CompactOptions::default(),
                &Job::new().ctx(),
            )
            .unwrap_err();
            let img = d.crash_image(mochi_testkit::CrashMode::KeepAll);
            let names = img.names();
            assert_eq!(
                names.len(),
                1,
                "the partial output under its temporary name: {names:?}"
            );
            one(img.file(&names[0]).unwrap().contents())
        }

        // ---- copy ----
        "copy-damaged-chunk" => {
            let good = cx.base("core-valid-history");
            let mut bad = good.clone();
            let (at, n) = chunk_range(&good, &plain(), 0, "b");
            flip(&mut bad, at + n / 2);
            Built {
                main: bad,
                copies: vec![good],
            }
        }
        "copy-head-destroyed" => {
            let good = cx.base("core-valid-history");
            let head = history_of(&good, &plain()).pop().unwrap();
            let mut bad = good.clone();
            flip(&mut bad, head.commit_offset + 20);
            flip(&mut bad, head.footer_offset + 30);
            Built {
                main: bad,
                copies: vec![good],
            }
        }
        "copy-diverged" => {
            let good = cx.base("core-valid-history");
            let h = history_of(&good, &plain());
            let three = good[..(h[2].footer_offset + FOOTER_FRAME_LEN) as usize].to_vec();
            let mut a = Transaction::new();
            a.put_file(path("only-in-source"), vec![1; 90], attrs(0o644, 70));
            let mut c = Transaction::new();
            c.put_file(path("only-in-copy"), vec![2; 90], attrs(0o644, 71));
            let mut source = append_one(&three, false, 71, a).contents();
            let copy = append_one(&three, false, 72, c).contents();
            let (at, n) = chunk_range(&source, &plain(), 1, "f");
            flip(&mut source, at + n / 2);
            Built {
                main: source,
                copies: vec![copy],
            }
        }

        // ---- salvage ----
        "salvage-destroyed-footers" => {
            let base = cx.base("core-valid-history");
            let mut b = base.clone();
            for e in history_of(&base, &plain()) {
                flip(&mut b, e.footer_offset + 30);
            }
            one(b)
        }
        "salvage-destroyed-descriptor" => {
            let base = cx.base("core-valid-history");
            let len = history_of(&base, &plain())[0].commit.descriptor.stored_len as usize;
            let mut b = base;
            b[..len].fill(0);
            one(b)
        }
        "salvage-archive-inside" => {
            let pristine = cx.base("core-valid-history");
            let h = history_of(&pristine, &plain());
            let copy_len = (h[3].footer_offset + FOOTER_FRAME_LEN - 20) as usize;
            let copy = pristine[..copy_len].to_vec();
            let mut b = pristine.clone();
            flip(&mut b, h[6].footer_offset + 30);
            b.extend_from_slice(&[0u8; 8]);
            b.extend_from_slice(&copy);
            one(b)
        }

        // ---- legacy ----
        "legacy-draft-1" => one(std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/golden/b2/reject-archive-draft-1.mochi"),
        )
        .unwrap()),
        other => panic!("no recipe for {other}"),
    }
}

/// The commit the interrupted and lost-write cases append.
fn next_tx() -> Transaction {
    let mut t = Transaction::new();
    t.put_file(
        path("next.bin"),
        mochi_testkit::deterministic_bytes(91, 400),
        attrs(0o644, 80),
    );
    t
}

fn interrupted(cx: &mut Ctx, base: &str, enc: bool, stage: Stage) -> Built {
    let b = cx.base(base);
    one(interrupt(&b, enc, &next_tx(), stage))
}

#[allow(dead_code)]
fn _keep(_: CheckpointPolicy) {}
