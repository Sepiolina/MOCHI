//! 7z split-volume naming and the concatenated reader (plan D8 rule 8).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use mochi_foreign::volumes::*;

#[test]
fn names_are_classified_by_suffix() {
    assert_eq!(classify(Path::new("a.7z.001")), SplitName::First);
    assert_eq!(classify(Path::new("A.7Z.001")), SplitName::First);
    assert_eq!(classify(Path::new("a.7z.0001")), SplitName::First);
    assert_eq!(classify(Path::new("a.7z.002")), SplitName::Later(2));
    assert_eq!(classify(Path::new("a.7z")), SplitName::NotSplit);
    assert_eq!(classify(Path::new("a.zip.001")), SplitName::NotSplit);
    assert_eq!(classify(Path::new("a.7z.01")), SplitName::NotSplit);
}

#[test]
fn a_set_reads_as_one_stream_and_seeks_across_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for (i, chunk) in [&b"hello "[..], &b"wide "[..], &b"world"[..]]
        .iter()
        .enumerate()
    {
        let p = dir.path().join(format!("x.7z.{:03}", i + 1));
        std::fs::write(&p, chunk).unwrap();
        paths.push(p);
    }
    let found = find_volumes(&paths[0]);
    assert_eq!(found.len(), 3);
    let mut m = MultiFile::open(&found).unwrap();
    assert_eq!(m.len(), 16);
    let mut all = String::new();
    m.read_to_string(&mut all).unwrap();
    assert_eq!(all, "hello wide world");
    m.seek(SeekFrom::Start(4)).unwrap();
    let mut buf = [0u8; 8];
    let mut got = 0;
    while got < 8 {
        let n = m.read(&mut buf[got..]).unwrap();
        assert!(n > 0);
        got += n;
    }
    assert_eq!(&buf, b"o wide w");
    assert!(m.seek(SeekFrom::End(-100)).is_err());
}

#[test]
fn a_gap_ends_the_set() {
    let dir = tempfile::tempdir().unwrap();
    for n in [1, 2, 4] {
        std::fs::write(dir.path().join(format!("x.7z.{n:03}")), b"z").unwrap();
    }
    assert_eq!(find_volumes(&dir.path().join("x.7z.001")).len(), 2);
}
