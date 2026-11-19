//! The wire protocol: round trips and strict rejection of malformed input.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mochi_foreign::protocol::*;
use mochi_foreign::*;

fn entry() -> EntryInfo {
    EntryInfo {
        index: 7,
        // Not UTF-8, with a NUL and a traversal: carried as bytes.
        name: vec![b'.', b'.', b'/', 0x82, 0xff, 0x00, b'x'],
        kind: EntryKind::Symlink,
        size: Some(u64::MAX),
        compressed_size: Some(0),
        encryption: Encryption::Aes(192),
        method: "aes+deflate".to_string(),
        link_target: Some(vec![0xfe, b'/']),
        unix_mode: Some(0o120777),
        mtime_unix: Some(-5),
        name_is_utf8_flag: Some(false),
        error: Some("boom".to_string()),
    }
}

fn all_messages() -> Vec<Message> {
    let mut v = vec![
        Message::Hello {
            version: PROTOCOL_VERSION,
            id: "h 1".into(),
        },
        Message::Format(ForeignFormat::TarGz),
        Message::Format(ForeignFormat::TarZst),
        Message::StreamCheck(StreamCheck::Matched(CheckSet::of(&[
            CheckKind::ZstdFrameChecksum,
        ]))),
        Message::Entry(entry()),
        Message::Entry(EntryInfo {
            encryption: Encryption::ZipCryptoWeak,
            size: None,
            compressed_size: None,
            link_target: None,
            unix_mode: None,
            mtime_unix: None,
            name_is_utf8_flag: Some(true),
            error: None,
            ..entry()
        }),
        Message::Data(vec![1, 2, 3]),
        Message::Data(vec![]),
        Message::StreamCheck(StreamCheck::NotApplicable),
        Message::StreamCheck(StreamCheck::Matched(CheckSet::of(&[
            CheckKind::GzipStreamCrc32,
        ]))),
        Message::StreamCheck(StreamCheck::Failed("x".into())),
        Message::StreamCheck(StreamCheck::NotReached),
        Message::Breach(Breach {
            kind: BreachKind::Ratio,
            entry: Some(3),
            limit: 200,
            observed: 1000,
        }),
        Message::Breach(Breach {
            kind: BreachKind::EntryCount,
            entry: None,
            limit: 1,
            observed: 2,
        }),
        Message::Done(Done {
            complete: false,
            entries: 3,
            bytes_out: 99,
            stop_reason: Some("why".into()),
        }),
        Message::Done(Done {
            complete: true,
            entries: 0,
            bytes_out: 0,
            stop_reason: None,
        }),
    ];
    for code in 1..=9u8 {
        v.push(Message::Error {
            code: ErrorCode::from_wire(code).unwrap(),
            message: "m".into(),
        });
    }
    for o in [
        EntryOutcome::Matched(CheckSet::of(&[CheckKind::Crc32, CheckKind::AesAuthCode])),
        EntryOutcome::Mismatch {
            failed: CheckSet::of(&[CheckKind::Crc32]),
            wrong_password_possible: true,
        },
        EntryOutcome::NoEntryCheck,
        EntryOutcome::NoData,
        EntryOutcome::PasswordRequired,
        EntryOutcome::WrongPassword,
        EntryOutcome::Unsupported("u".into()),
        EntryOutcome::Breach(BreachKind::NameLength),
        EntryOutcome::ReadError("r".into()),
        EntryOutcome::NotRun,
    ] {
        v.push(Message::Result {
            index: 1,
            outcome: o,
        });
    }
    v
}

#[test]
fn every_message_round_trips_through_frames() {
    for m in all_messages() {
        let mut wire = Vec::new();
        m.write_to(&mut wire).unwrap();
        let (k, body) = read_frame(&mut wire.as_slice()).unwrap().unwrap();
        assert_eq!(Message::decode(k, &body).unwrap(), m);
    }
}

#[test]
fn truncating_any_message_body_is_rejected_and_never_panics() {
    for m in all_messages() {
        let (k, body) = m.encode();
        if k == kind::DATA {
            continue; // raw bytes: any length is valid
        }
        for cut in 0..body.len() {
            assert!(
                Message::decode(k, &body[..cut]).is_err(),
                "{m:?} cut at {cut}"
            );
        }
    }
}

#[test]
fn trailing_bytes_are_rejected() {
    for m in all_messages() {
        let (k, mut body) = m.encode();
        if k == kind::DATA {
            continue;
        }
        body.push(0);
        assert!(Message::decode(k, &body).is_err(), "{m:?}");
    }
}

#[test]
fn unknown_kinds_and_codes_are_rejected() {
    assert!(Message::decode(0x55, &[]).is_err());
    assert!(Message::decode(kind::FORMAT, &[9]).is_err());
    assert!(Message::decode(kind::FORMAT, &[7]).is_err());
    assert!(Message::decode(kind::FORMAT, &[0]).is_err());
    assert!(Message::decode(
        kind::BREACH,
        &[9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    )
    .is_err());
    // Client-side error codes never arrive from the helper.
    let mut e = Enc::new();
    e.u8(ErrorCode::HelperTimeout.code()).text("x");
    assert!(Message::decode(kind::ERROR, &e.finish()).is_err());
    // A check set must name something.
    assert!(Message::decode(kind::STREAM_CHECK, &[1, 0]).is_err());
    assert!(Message::decode(kind::STREAM_CHECK, &[1, 32]).is_err());
    assert!(Message::decode(kind::STREAM_CHECK, &[1, 16]).is_ok());
}

#[test]
fn frame_lengths_are_validated_before_allocating() {
    // Zero, one over the maximum (with the whole body present, so only the
    // length check can refuse it), and the largest u32.
    for len in [0u32, MAX_FRAME_LEN as u32 + 1, u32::MAX] {
        let mut wire = len.to_le_bytes().to_vec();
        wire.push(kind::DATA);
        if len as usize == MAX_FRAME_LEN + 1 {
            wire.extend(std::iter::repeat_n(7u8, MAX_FRAME_LEN));
        } else {
            wire.extend_from_slice(&[1, 2]);
        }
        assert!(
            matches!(
                read_frame(&mut wire.as_slice()),
                Err(FrameError::Protocol(_))
            ),
            "length {len}"
        );
    }
    // Cut mid-length and mid-body.
    assert!(matches!(
        read_frame(&mut [1u8, 0].as_slice()),
        Err(FrameError::Protocol(_))
    ));
    assert!(matches!(
        read_frame(&mut [5u8, 0, 0, 0, 0x84, 1].as_slice()),
        Err(FrameError::Protocol(_))
    ));
    // Clean end of stream at a boundary is not an error.
    assert!(read_frame(&mut [].as_slice()).unwrap().is_none());
    // The writer refuses what the reader would.
    assert!(write_frame(&mut Vec::new(), kind::DATA, &vec![0u8; MAX_FRAME_LEN]).is_err());
}

#[test]
fn request_round_trips_and_rejects_other_versions() {
    let r = Request {
        op: Op::Read,
        read_index: 9,
        read_indices: vec![],
        limits: Limits {
            max_total_output: 1,
            max_entry_size: 2,
            max_entries: 3,
            max_ratio: 4,
            ratio_min_output: 5,
            max_name_len: 6,
        },
        memory_limit: Some(7),
        cpu_seconds: None,
        archive_path: vec![0xff, b'/', 0],
        password: Some(Password::new(vec![0, 1, 2, 0xff])),
    };
    let back = Request::decode(&r.encode()).unwrap();
    assert_eq!(back.op, Op::Read);
    assert_eq!(back.read_index, 9);
    assert_eq!(back.limits, r.limits);
    assert_eq!(back.memory_limit, Some(7));
    assert_eq!(back.cpu_seconds, None);
    assert_eq!(back.archive_path, r.archive_path);
    assert_eq!(back.password.as_ref().unwrap().as_bytes(), &[0, 1, 2, 0xff]);

    let mut wrong = r.encode();
    wrong[0] = 3; // a protocol version this build does not speak
    assert!(Request::decode(&wrong).is_err());
    let mut op = r.encode();
    op[2] = 0; // unknown operation
    assert!(Request::decode(&op).is_err());
    assert!(Request::decode(&r.encode()[..20]).is_err());
}

#[test]
fn a_request_never_prints_its_password() {
    let r = Request {
        op: Op::List,
        read_index: 0,
        read_indices: vec![],
        limits: Limits::default(),
        memory_limit: None,
        cpu_seconds: None,
        archive_path: vec![],
        password: Some(Password::new(b"hunter2-very-secret".to_vec())),
    };
    assert!(!format!("{r:?}").contains("hunter2"));
}

#[test]
fn error_text_is_truncated_on_a_character_boundary() {
    let long = "é".repeat(MAX_MESSAGE_TEXT); // 2 bytes each
    let m = Message::Error {
        code: ErrorCode::BadRequest,
        message: long,
    };
    let (k, body) = m.encode();
    match Message::decode(k, &body).unwrap() {
        Message::Error { message, .. } => {
            assert!(message.len() <= MAX_MESSAGE_TEXT);
            assert!(message.chars().all(|c| c == 'é'));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn read_many_request_round_trips_sorted_and_unique() {
    let r = Request {
        op: Op::ReadMany,
        read_index: 0,
        read_indices: vec![9, 2, 2, 5],
        limits: Limits::default(),
        memory_limit: None,
        cpu_seconds: None,
        archive_path: vec![b'a'],
        password: None,
    };
    let back = Request::decode(&r.encode()).unwrap();
    assert_eq!(back.op, Op::ReadMany);
    assert_eq!(
        back.read_indices,
        vec![2, 5, 9],
        "sorted and deduplicated on decode"
    );
    assert!(back.wants(5) && back.wants(9) && !back.wants(3));
    assert_eq!(back.wanted_count(), Some(3));
    assert_eq!(
        Request {
            op: Op::List,
            ..back.clone()
        }
        .wanted_count(),
        None
    );
    // A count that promises more indices than the body holds is refused
    // before anything is allocated.
    let mut enc = r.encode();
    let at = 2 + 1 + 8; // version, op, read_index
    enc[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Request::decode(&enc).is_err());
    let mut enc = r.encode();
    enc[at..at + 4].copy_from_slice(&1_000_001u32.to_le_bytes());
    assert!(Request::decode(&enc).is_err());
}

#[test]
fn seven_z_and_rar_formats_and_their_error_codes_cross_the_wire() {
    for f in [ForeignFormat::SevenZ, ForeignFormat::Rar] {
        let (k, body) = Message::Format(f).encode();
        assert_eq!(Message::decode(k, &body).unwrap(), Message::Format(f));
    }
    for code in [
        ErrorCode::PasswordRequired,
        ErrorCode::WrongPassword,
        ErrorCode::MissingVolume,
    ] {
        let m = Message::Error {
            code,
            message: "x".into(),
        };
        let (k, body) = m.encode();
        assert_eq!(Message::decode(k, &body).unwrap(), m);
    }
    // A parent-side code is never accepted from the helper.
    let (k, mut body) = Message::Error {
        code: ErrorCode::BadRequest,
        message: String::new(),
    }
    .encode();
    body[0] = ErrorCode::HelperCrashed.code();
    assert!(Message::decode(k, &body).is_err());
    // BLAKE2sp is a check kind the decoder accepts.
    let m = Message::Result {
        index: 0,
        outcome: EntryOutcome::Matched(CheckSet::of(&[CheckKind::Blake2sp])),
    };
    let (k, body) = m.encode();
    assert_eq!(Message::decode(k, &body).unwrap(), m);
}
