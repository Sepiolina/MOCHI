//! R6: the Redundancy profile's suite against the vectors in
//! `docs/ratification/R6-vectors/vectors.json`, which an independent Python
//! implementation (`gen_vectors.py`, bit-by-bit field arithmetic, no shared
//! code) generated from the R6 text. CI regenerates the file and fails if it
//! differs, so the two implementations agree on every value here.

use mochi_format::envelope::RecordIdentity;
use mochi_format::error::{ErrorClass, FormatError, ParityFault};
use mochi_format::parity::{self, Layout};
use mochi_format::Limits;
use serde_json::Value;

fn vectors() -> Value {
    let text = include_str!("../../../docs/ratification/R6-vectors/vectors.json");
    serde_json::from_str(text).expect("vectors.json parses")
}

fn hex(s: &Value) -> Vec<u8> {
    let s = s.as_str().expect("hex string");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

fn u(v: &Value) -> u64 {
    v.as_u64().expect("unsigned")
}

/// The generator's test bytes.
fn pattern(n: usize, seed: u64) -> Vec<u8> {
    (0..n as u64)
        .map(|i| ((i * 167 + seed * 13 + (i >> 8)) & 0xFF) as u8)
        .collect()
}

fn layout_of(v: &Value) -> Layout {
    Layout {
        range_offset: u(&v["range_offset"]),
        range_len: u(&v["range_len"]),
        percent: u(&v["percent"]) as u16,
        shard_size: u(&v["shard_size"]),
        groups: u(&v["groups"]),
        data_shards: u(&v["data_shards"]),
        parity_shards: u(&v["parity_shards"]),
    }
}

#[test]
fn field_and_matrix() {
    let v = vectors();
    assert_eq!(u(&v["suite"]), u64::from(parity::SUITE_CAUCHY_GF256));
    assert_eq!(
        u(&v["field_polynomial"]),
        u64::from(parity::FIELD_POLYNOMIAL)
    );
    for p in v["field"]["products"].as_array().unwrap() {
        let (a, b, c) = (u(&p[0]) as u8, u(&p[1]) as u8, u(&p[2]) as u8);
        assert_eq!(parity::gf_mul(a, b), c, "{a} * {b}");
    }
    for p in v["field"]["inverses"].as_array().unwrap() {
        assert_eq!(parity::gf_inv(u(&p[0]) as u8), Some(u(&p[1]) as u8));
    }
    let c = &v["coefficients"];
    for (p, row) in c["values"].as_array().unwrap().iter().enumerate() {
        for (j, x) in row.as_array().unwrap().iter().enumerate() {
            assert_eq!(
                parity::coefficient(p as u64, j as u64),
                Some(u(x) as u8),
                "C[{p}][{j}]"
            );
        }
    }
    assert_eq!(
        parity::coefficient(127, 127),
        Some(u(&c["corner_127_127"]) as u8)
    );
}

#[test]
fn layout_rule() {
    for l in vectors()["layouts"].as_array().unwrap() {
        let want = layout_of(l);
        let got = Layout::compute(want.range_offset, want.range_len, want.percent).unwrap();
        assert_eq!(got, want);
        assert_eq!(got.real_shards(), u(&l["real_shards"]));
    }
}

#[test]
fn codes_encode_and_rebuild() {
    for c in vectors()["codes"].as_array().unwrap() {
        let (k, m) = (u(&c["k"]) as usize, u(&c["m"]) as usize);
        let data: Vec<Vec<u8>> = c["data"].as_array().unwrap().iter().map(hex).collect();
        let want: Vec<Vec<u8>> = c["parity"].as_array().unwrap().iter().map(hex).collect();
        let n = data[0].len();
        let mut acc = vec![vec![0u8; n]; m];
        for (j, d) in data.iter().enumerate() {
            parity::accumulate(&mut acc, j as u64, d).unwrap();
        }
        assert_eq!(acc, want, "k={k} m={m}");
        // The first m data shards lost: rebuilt from the rest and parity.
        let mut shards: Vec<Option<Vec<u8>>> = data
            .iter()
            .chain(&want)
            .enumerate()
            .map(|(i, s)| (i >= m).then(|| s.clone()))
            .collect();
        parity::reconstruct(k, m, &mut shards, n).unwrap();
        for (i, d) in data.iter().enumerate() {
            assert_eq!(shards[i].as_ref(), Some(d));
        }
    }
}

#[test]
fn shard_hashes() {
    for s in vectors()["shard_hashes"].as_array().unwrap() {
        let size = u(&s["shard_size"]);
        let l = Layout {
            shard_size: size,
            ..Layout::compute(0, 1, 1).unwrap()
        };
        assert_eq!(
            parity::data_shard_hash(&hex(&s["stored"]), &l).to_vec(),
            hex(&s["hash"])
        );
    }
}

fn identity(c: &Value) -> RecordIdentity {
    let mut archive_id = [0u8; 32];
    archive_id.copy_from_slice(&hex(&c["archive_id"]));
    let mut transaction_id = [0u8; 16];
    transaction_id.copy_from_slice(&hex(&c["transaction_id"]));
    RecordIdentity {
        archive_id,
        commit_sequence: u(&c["seq"]),
        transaction_id,
    }
}

/// The frames the writer's procedure produces for a protected range.
fn frames_for(range: &[u8], layout: &Layout, id: RecordIdentity, features: &[u64]) -> Vec<Vec<u8>> {
    let s = layout.shard_size as usize;
    let mut out = Vec::new();
    for g in 0..layout.groups {
        let mut acc = vec![vec![0u8; s]; layout.parity_shards as usize];
        let mut hashes = Vec::new();
        for j in 0..layout.data_shards {
            let (off, n) = layout.shard_extent(layout.shard_index(g, j));
            let at = (off - layout.range_offset) as usize;
            let stored = &range[at..at + n as usize];
            hashes.push(parity::data_shard_hash(stored, layout));
            parity::accumulate(&mut acc, j, stored).unwrap();
        }
        for (p, shard) in acc.iter().enumerate() {
            let info = parity::ParityFrameInfo {
                layout: *layout,
                archive_id: id.archive_id,
                seq: id.commit_sequence,
                transaction_id: id.transaction_id,
                required_features: features.to_vec(),
            };
            out.push(
                parity::encode_frame(
                    &info,
                    &[1],
                    g,
                    p as u64,
                    &hashes,
                    shard,
                    &Limits::WRITER_DEFAULT,
                )
                .unwrap(),
            );
        }
    }
    out
}

#[test]
fn small_commit_frames_are_byte_exact() {
    let v = vectors();
    let c = &v["small_commit"];
    let range = hex(&c["range"]);
    let layout = layout_of(&c["layout"]);
    assert_eq!(
        Layout::compute(
            u(&c["range_offset"]),
            range.len() as u64,
            u(&c["percent"]) as u16
        )
        .unwrap(),
        layout
    );
    let want: Vec<Vec<u8>> = c["frames"].as_array().unwrap().iter().map(hex).collect();
    assert_eq!(frames_for(&range, &layout, identity(c), &[]), want);
    for f in &want {
        let d = parity::decode_frame(f, &[1], &Limits::default()).unwrap();
        assert_eq!(d.layout, layout);
        assert_eq!(d.envelope.identity, identity(c));
    }
}

#[test]
fn multi_shard_commit_frames_match_and_rebuild() {
    let v = vectors();
    let c = &v["multi_shard_commit"];
    let range = pattern(u(&c["range_len"]) as usize, u(&c["range_seed"]));
    let layout = layout_of(&c["layout"]);
    let features: Vec<u64> = c["required_features"]
        .as_array()
        .unwrap()
        .iter()
        .map(u)
        .collect();
    let frames = frames_for(&range, &layout, identity(c), &features);
    let want: Vec<Vec<u8>> = c["frame_blake3"]
        .as_array()
        .unwrap()
        .iter()
        .map(hex)
        .collect();
    assert_eq!(
        frames
            .iter()
            .map(|f| blake3::hash(f).as_bytes().to_vec())
            .collect::<Vec<_>>(),
        want
    );
    assert!(frames.iter().all(|f| f.len() as u64 == u(&c["frame_len"])));
    // Lose as many data shards of group 0 as it has parity: rebuilt exactly.
    let decoded: Vec<_> = frames
        .iter()
        .map(|f| parity::decode_frame(f, &[1], &Limits::default()).unwrap())
        .collect();
    let (k, m, s) = (
        layout.data_shards as usize,
        layout.parity_shards as usize,
        layout.shard_size as usize,
    );
    let mut shards: Vec<Option<Vec<u8>>> = (0..k as u64)
        .map(|j| {
            let (off, n) = layout.shard_extent(layout.shard_index(0, j));
            let mut b = range[off as usize..(off + n) as usize].to_vec();
            b.resize(s, 0);
            (j as usize >= m).then_some(b)
        })
        .chain(
            decoded
                .iter()
                .filter(|d| d.group == 0)
                .map(|d| Some(d.shard.to_vec())),
        )
        .collect();
    parity::reconstruct(k, m, &mut shards, s).unwrap();
    for j in 0..m as u64 {
        let (off, n) = layout.shard_extent(layout.shard_index(0, j));
        let got = &shards[j as usize].as_ref().unwrap()[..n as usize];
        assert_eq!(got, &range[off as usize..(off + n) as usize]);
        assert_eq!(
            Some(parity::data_shard_hash(got, &layout)),
            decoded[0].shard_hash(j)
        );
    }
}

#[test]
fn rejects_are_refused_for_their_reason() {
    for r in vectors()["rejects"].as_array().unwrap() {
        let name = r["name"].as_str().unwrap();
        let e = parity::decode_frame(&hex(&r["frame"]), &[1], &Limits::default()).unwrap_err();
        let ok = match r["fault"].as_str().unwrap() {
            "digest" => e == FormatError::Parity(ParityFault::DigestMismatch),
            "unsupported" => e.class() == ErrorClass::Unsupported,
            "layout" => e == FormatError::Parity(ParityFault::Layout),
            "envelope" => e.class() == ErrorClass::Envelope,
            other => panic!("unknown fault {other}"),
        };
        assert!(ok, "{name}: {e:?}");
    }
}
