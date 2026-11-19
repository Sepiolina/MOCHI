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

/// `rekey` is built (C11): asked to do nothing, it says what to choose and
/// exits 3, never success.
#[test]
fn rekey_with_nothing_chosen_is_an_operational_error_never_success() {
    let cmd = &["rekey", "a.mochi"];
    let (code, _, err) = go(cmd);
    assert_eq!(code, exit::ERROR, "{cmd:?}");
    assert!(err.contains("INVALID_ARGUMENT"), "{cmd:?}: {err}");
    assert!(err.contains("--list"), "{cmd:?}: {err}");
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
fn help_and_version_succeed_and_carry_the_format_status() {
    let (code, out, _) = go(&["--help"]);
    assert_eq!(code, exit::OK);
    assert!(out.contains("not a backup tool"));

    let (code, out, _) = go(&["--version"]);
    assert_eq!(code, exit::OK);
    assert!(out.contains("1.0 (ratified format)"), "{out}");
    assert!(!out.contains("draft-compatible"), "{out}");
    assert!(out.contains("not a backup"), "{out}");
}

#[test]
fn every_1_0_command_from_spec_23_2_is_present() {
    // Guards against silently dropping a command from the 1.0 scope. Every
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
    // The archive is required; a bare `rekey` is a usage error (exit 3).
    let (code, _, err) = go(&["rekey"]);
    assert_eq!(code, exit::ERROR, "{err}");
}

/// R7 decision (2026-10-11): an error exits 1 exactly when verification would
/// count the same code as a violation, so automation reading only the exit
/// code learns "the archive is wrong" from `ls` or `get` as from `verify`.
/// `CHECKPOINT_MISMATCH` is the one exception: as an error it only comes from
/// a writer (B.2.2: "3 (writer)"); `verify` reports it through a `FAIL`
/// dimension instead.
#[test]
fn exit_code_for_agrees_with_classify() {
    use mochi_cli::exit_code_for;
    use mochi_core::verify::{classify, ErrorClass};
    use mochi_core::ErrorCode;
    for code in ErrorCode::ALL {
        let want = match (code, classify(*code)) {
            (ErrorCode::CheckpointMismatch, _) => exit::ERROR,
            (_, ErrorClass::Violation) => exit::FAILED,
            (_, ErrorClass::Unsupported) => exit::UNSUPPORTED,
            (_, ErrorClass::Operational) => exit::ERROR,
        };
        assert_eq!(exit_code_for(*code), want, "{code}");
        // No error code may ever map to success.
        assert_ne!(exit_code_for(*code), exit::OK, "{code}");
    }
    // The cases the rule is about, spelled out.
    for code in [
        ErrorCode::MalformedFrame,
        ErrorCode::CatalogInvalid,
        ErrorCode::RecordInvalid,
        ErrorCode::NoValidHead,
        ErrorCode::TailUnresolved,
        ErrorCode::StoredIntegrityFailed,
    ] {
        assert_eq!(exit_code_for(code), exit::FAILED, "{code}");
    }
    for code in [
        ErrorCode::KeyUnavailable,
        ErrorCode::UncommittedTail,
        ErrorCode::InvalidArgument,
        ErrorCode::LimitExceeded,
        ErrorCode::ArchiveIdAmbiguous,
    ] {
        assert_eq!(exit_code_for(code), exit::ERROR, "{code}");
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

/// R7 decision (2026-10-11): `NOT_IMPLEMENTED` stays registered (retiring a
/// code is a breaking change), but a 1.0 build returns it from no command.
/// Every command spec §23.2 puts in the 1.0 scope is read from the spec and
/// run with minimal arguments against a missing archive, so that it gets past
/// argument parsing (its `--json` error is proof) to the scope check: it must
/// fail on the missing file, never as not implemented.
#[test]
fn no_one_dot_zero_command_is_not_implemented() {
    let spec = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/spec.md"),
    )
    .unwrap();
    let line = spec
        .lines()
        .find(|l| l.contains("*1.0 command scope.* In 1.0:"))
        .expect("spec §23.2 lists the 1.0 commands");
    let list = line
        .split("In 1.0:")
        .nth(1)
        .unwrap()
        .split(". Post-1.0")
        .next()
        .unwrap();
    let commands: Vec<String> = list
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect();
    assert_eq!(commands.len(), 21, "{commands:?}");
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.mochi").display().to_string();
    let out_dir = dir.path().join("out").display().to_string();
    let plan = dir.path().join("plan.json").display().to_string();
    let m = missing.as_str();
    for cmd in &commands {
        let args: Vec<&str> = match cmd.as_str() {
            "create" => vec!["create", m, m],
            "append" => vec!["append", m, "--delete", "x"],
            "get" => vec!["get", m, "-C", &out_dir],
            "list" => vec!["list", m],
            "snapshot" => vec!["snapshot", "list", m],
            "search" => vec!["search", m, "--path", "x"],
            // A missing folder: find's scope is folders, not an archive.
            "find" => vec!["find", "x", m],
            "comment" => vec!["comment", m],
            "verify" => vec!["verify", m],
            "fsck" => vec!["fsck", m],
            "health" => vec!["health", m],
            "restore-test" => vec!["restore-test", m, "-C", &out_dir],
            "repair plan" => vec!["repair", "plan", m],
            "repair apply" => vec!["repair", "apply", m, "--plan", &plan, "--output", &out_dir],
            "checkpoint" => vec!["checkpoint", m],
            "compact" => vec!["compact", m, "--output", &out_dir],
            "migrate" => vec!["migrate", m, "--output", &out_dir],
            "gc plan" => vec!["gc", "plan", m],
            "gc apply" => vec!["gc", "apply", m, "--plan", &plan, "--output", &out_dir],
            "rekey" => vec!["rekey", m, "--list"],
            "dump-index" => vec!["dump-index", m],
            other => panic!("spec §23.2 lists {other:?}; add its minimal arguments here"),
        };
        let mut argv = args.clone();
        argv.extend(["--json", "--no-local-history"]);
        let (code, out, err) = go(&argv);
        let v: serde_json::Value = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("{cmd}: arguments not accepted ({e}): {out} {err}"));
        let error = v["error"]["code"].as_str().unwrap_or_default();
        assert_ne!(error, "NOT_IMPLEMENTED", "{cmd}: {v}");
        assert_ne!(
            code,
            exit::OK,
            "{cmd}: a missing archive cannot succeed: {v}"
        );
    }
}

/// K13: a draft-1 archive (written before the format freeze) is a refusal,
/// not damage: `UNSUPPORTED_FEATURE`, exit 4, and the message names draft 1.
/// The fixture is the pre-freeze `c5/valid-archive-3-commits.mochi` kept
/// byte for byte.
#[test]
fn a_draft_1_archive_is_refused_exit_4_naming_the_draft() {
    let path = format!(
        "{}/../../fixtures/golden/b2/reject-archive-draft-1.mochi",
        env!("CARGO_MANIFEST_DIR")
    );
    for cmd in [
        &["verify", path.as_str(), "--no-local-history"][..],
        &["list", path.as_str()],
    ] {
        let (code, out, err) = go(cmd);
        assert_eq!(code, exit::UNSUPPORTED, "{cmd:?}: {out} {err}");
        let all = format!("{out}{err}");
        assert!(all.contains("draft 1"), "{cmd:?}: {all}");
        assert!(all.contains("UNSUPPORTED_FEATURE"), "{cmd:?}: {all}");
    }
}
