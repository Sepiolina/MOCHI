//! Exit-code behaviour of the command surface (spec §23.2). The built
//! commands' end-to-end behaviour is in `c14_commands.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mochi_cli::{exit, run};

fn go(args: &[&str]) -> (u8, String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut argv = vec!["mochi"];
    argv.extend_from_slice(args);
    let code = run(argv, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn post_1_0_commands_exit_4_not_3() {
    for cmd in [
        &["inventory", "reconcile", "--inventory", "inv.db"][..],
        &["split", "a.mochi"],
        &["join", "a.mochi.001"],
        &["mount", "a.mochi", "/mnt/x"],
    ] {
        let (code, _, err) = go(cmd);
        assert_eq!(code, exit::UNSUPPORTED, "{cmd:?}");
        assert!(err.contains("UNSUPPORTED_FEATURE"), "{cmd:?}: {err}");
    }
}

#[test]
fn in_scope_but_unbuilt_commands_are_an_operational_error_never_success() {
    let cmd = &["rekey", "a.mochi"];
    let (code, _, err) = go(cmd);
    assert_eq!(code, exit::ERROR, "{cmd:?}");
    assert!(err.contains("NOT_IMPLEMENTED"), "{cmd:?}: {err}");
}

/// A missing archive is an operational error with the JSON envelope, never
/// a report that could be read as success.
#[test]
fn verify_of_a_missing_archive_is_an_error_not_a_result() {
    let (code, out, _) = go(&[
        "verify",
        "no-such-archive.mochi",
        "--json",
        "--no-local-history",
    ]);
    assert_eq!(code, exit::ERROR);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["error"]["code"], "IO_ERROR");
}

#[test]
fn json_mode_puts_a_stable_error_envelope_on_stdout() {
    let (code, out, err) = go(&["--json", "mount", "a.mochi"]);
    assert_eq!(code, exit::UNSUPPORTED);
    assert!(err.is_empty());
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["error"]["code"], "UNSUPPORTED_FEATURE");
    assert!(v["error"]["message"].as_str().unwrap().contains("mount"));
}

#[test]
fn usage_errors_exit_3_because_2_means_degraded() {
    let (code, _, err) = go(&["no-such-command"]);
    assert_eq!(code, exit::ERROR);
    assert!(!err.is_empty());
    let (code, _, _) = go(&["--no-such-flag", "verify"]);
    assert_eq!(code, exit::ERROR);
}

#[test]
fn no_arguments_is_an_invocation_error() {
    let (code, out, err) = go(&[]);
    assert_eq!(code, exit::ERROR);
    assert!(out.is_empty());
    assert!(err.contains("Usage"));
}

#[test]
fn help_and_version_succeed_and_carry_the_draft_label() {
    let (code, out, _) = go(&["--help"]);
    assert_eq!(code, exit::OK);
    assert!(out.contains("not a backup tool"));

    let (code, out, _) = go(&["--version"]);
    assert_eq!(code, exit::OK);
    assert!(out.contains("experimental / draft-compatible"), "{out}");
    assert!(out.contains("not a backup"), "{out}");
}

#[test]
fn every_1_0_command_from_spec_23_2_is_present() {
    // Guards against silently dropping a command from the 1.0 scope. Every
    // one has help; the unbuilt ones still refuse with NOT_IMPLEMENTED.
    for name in [
        "create",
        "append",
        "get",
        "list",
        "snapshot",
        "search",
        "verify",
        "fsck",
        "health",
        "restore-test",
        "repair",
        "checkpoint",
        "compact",
        "gc",
        "rekey",
        "dump-index",
    ] {
        let (code, out, err) = go(&[name, "--help"]);
        assert_eq!(code, exit::OK, "{name} --help: {err}");
        assert!(out.contains("Usage"), "{name}: {out}");
    }
    let (code, _, err) = go(&["rekey"]);
    assert_eq!(code, exit::ERROR, "rekey should parse: {err}");
    assert!(err.contains("NOT_IMPLEMENTED"), "rekey: {err}");
}

#[test]
fn integrity_failures_exit_1_everything_else_is_classified() {
    use mochi_cli::exit_code_for;
    use mochi_core::ErrorCode;
    for code in ErrorCode::ALL {
        let want = match code {
            ErrorCode::StoredIntegrityFailed | ErrorCode::ContentIntegrityFailed => exit::FAILED,
            ErrorCode::UnsupportedFeature => exit::UNSUPPORTED,
            // Spec Annex B.2.2 "New error codes" table.
            ErrorCode::DescriptorInvalid | ErrorCode::RetentionUnresolved => exit::FAILED,
            ErrorCode::ProfileChangeUnsupported => exit::UNSUPPORTED,
            // C7: verification evidence that the archive is wrong.
            ErrorCode::ReferenceInvalid | ErrorCode::FreshnessFailed => exit::FAILED,
            // CHECKPOINT_MISMATCH: 3 from a writer; a verify finding exits 1
            // through the report's FAIL dimension, not through this mapping.
            _ => exit::ERROR,
        };
        assert_eq!(exit_code_for(*code), want, "{code}");
        // No error code may ever map to success.
        assert_ne!(exit_code_for(*code), exit::OK, "{code}");
    }
}

/// Spec Annex B.2.2: each new code with its stated exit, spelled out so a
/// change to either the code name or its exit shows up here.
#[test]
fn b2_codes_have_their_table_exit_codes() {
    use mochi_cli::exit_code_for;
    use mochi_core::ErrorCode;
    let table = [
        ("DESTINATION_EXISTS", exit::ERROR),
        ("CAPACITY_EXCEEDED", exit::ERROR),
        ("CHECKPOINT_MISMATCH", exit::ERROR),
        ("DESCRIPTOR_INVALID", exit::FAILED),
        ("RETENTION_UNRESOLVED", exit::FAILED),
        ("QUARANTINE_FAILED", exit::ERROR),
        ("DURABILITY_UNCONFIRMED", exit::ERROR),
        ("PROFILE_CHANGE_UNSUPPORTED", exit::UNSUPPORTED),
    ];
    for (name, want) in table {
        let code = ErrorCode::ALL
            .iter()
            .find(|c| c.as_str() == name)
            .unwrap_or_else(|| panic!("{name} is not registered"));
        assert_eq!(exit_code_for(*code), want, "{name}");
    }
}

/// D16 / Q10 (decided 2026-10-08): `create --exceed-default-limits` is not in 1.0.
/// It is refused by name wherever it appears, never ignored, as `UNSUPPORTED_FEATURE`.
#[test]
fn exceed_default_limits_is_refused_by_name() {
    for cmd in [
        &["create", "--exceed-default-limits", "a.mochi", "dir"][..],
        &["create", "a.mochi", "dir", "--exceed-default-limits"],
        &["create", "a.mochi", "--exceed-default-limits=yes"],
    ] {
        let (code, _, err) = go(cmd);
        assert_eq!(code, exit::UNSUPPORTED, "{cmd:?}");
        assert!(err.contains("UNSUPPORTED_FEATURE"), "{cmd:?}: {err}");
        assert!(err.contains("--exceed-default-limits"), "{cmd:?}: {err}");
        assert!(err.contains("1.x"), "{cmd:?}: {err}");
    }
    let (code, out, _) = go(&["create", "a.mochi", "--exceed-default-limits", "--json"]);
    assert_eq!(code, exit::UNSUPPORTED);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["error"]["code"], "UNSUPPORTED_FEATURE");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("--exceed-default-limits"));
    // Only `create` takes it; elsewhere it is no more meaningful than before.
    let (_, _, err) = go(&["append", "a.mochi", "--exceed-default-limits"]);
    assert!(!err.contains("1.x feature"), "{err}");
}
