#!/usr/bin/env bash
# Mechanical checks for AGENTS.md rules that clippy cannot express.
# Runs on Linux and Windows (Git Bash), the supported platforms (plan O11).
# Exits non-zero on any violation.
set -u
cd "$(dirname "$0")/.."

fail=0
report() { echo "invariant violated: $1"; echo "$2" | sed 's/^/    /'; fail=1; }

# 1. All I/O in mochi-core goes through the Storage trait. std::fs is allowed in
#    exactly one file: the OS-backed implementation. (Comment lines are ignored.)
hits=$(grep -rnE --include='*.rs' '^[^/]*\bstd::fs\b' crates/mochi-core/src \
        | grep -v '^crates/mochi-core/src/storage/os.rs:' || true)
[ -n "$hits" ] && report "std::fs used in mochi-core outside storage/os.rs (AGENTS.md: Publication and safety)" "$hits"

# 2. Frame magic literals live only in mochi-format's registry module.
hits=$(grep -rniE --include='*.rs' '0x184D_?2A[0-9A-F]{2}|0xFD2F_?B528' crates apps 2>/dev/null \
        | grep -v '^crates/mochi-format/src/registry.rs:' || true)
[ -n "$hits" ] && report "frame magic literal outside mochi-format/src/registry.rs (AGENTS.md: Wire format)" "$hits"
#    Same for the 8-byte magics: the footer payload magic `MOCHI2\0\0` and the
#    tail-quarantine sidecar magic `MOCHITQ\0` (spec Annex B.2.2). Domain
#    separators such as "MOCHI2-FOOTER" are not magics and do not match.
hits=$(grep -rnE --include='*.rs' 'b"MOCHI(2|TQ)\\0' crates apps fuzz 2>/dev/null \
        | grep -v '^crates/mochi-format/src/registry.rs:' || true)
[ -n "$hits" ] && report "8-byte MOCHI magic literal outside mochi-format/src/registry.rs (AGENTS.md: Wire format)" "$hits"

# 3. unsafe is forbidden in the two format-critical library crates.
for f in crates/mochi-format/src/lib.rs crates/mochi-core/src/lib.rs; do
  grep -q 'forbid(unsafe_code)' "$f" || report "missing #![forbid(unsafe_code)]" "$f"
done

# 4. Dependency direction: mochi-format <- mochi-core <- {cli, desktop}. Nothing
#    below the app layer may know about Tauri or the UI.
hits=$(grep -inE 'tauri' crates/mochi-format/Cargo.toml crates/mochi-core/Cargo.toml crates/mochi-testkit/Cargo.toml || true)
[ -n "$hits" ] && report "Tauri referenced below the app layer" "$hits"
grep -qE '^mochi-core' crates/mochi-format/Cargo.toml \
  && report "mochi-format must not depend on mochi-core" "crates/mochi-format/Cargo.toml"

# 5. No secrets-in-Debug hazards or logging macros in library crates yet; keep
#    printing out of mochi-core and mochi-format (the CLI renders output).
hits=$(grep -rnE --include='*.rs' '\b(println|eprintln|print|eprint)!' crates/mochi-core/src crates/mochi-format/src \
        | grep -vE '^[^:]+:[0-9]+:\s*//' || true)
[ -n "$hits" ] && report "printing from a library crate (output belongs to the CLI/UI)" "$hits"

# 6. UnRAR stays in the isolated foreign-format crate (plan D8, O23). Its
#    vendored source is RARLAB freeware, not MIT/Apache as the crate metadata
#    says, and it parses hostile input in C++. No other crate may depend on it.
hits=$(grep -nE '^\s*unrar(_sys|-sys)?\s*=' $(find crates apps -name Cargo.toml 2>/dev/null) 2>/dev/null \
        | grep -v '^crates/mochi-foreign/' || true)
[ -n "$hits" ] && report "unrar dependency outside crates/mochi-foreign (AGENTS.md: Foreign archive formats)" "$hits"

# 7. Archive-supplied strings render as text only (spec §23.3 #9). React's
#    escape hatch is banned outright in the UI source.
if [ -d apps/mochi-desktop/ui ]; then
  hits=$(grep -rnE 'dangerouslySetInnerHTML|\.innerHTML\s*=' apps/mochi-desktop/ui \
          --include='*.ts' --include='*.tsx' --include='*.js' --include='*.jsx' \
          --exclude-dir=node_modules --exclude-dir=dist 2>/dev/null || true)
  [ -n "$hits" ] && report "HTML injection API in the UI (AGENTS.md: Tauri v2 rules)" "$hits"
fi

# 8. The catalog never touches the filesystem (catalog/mod.rs, plan O25):
#    SQLite runs in memory only, images move as bytes through Storage. Any
#    file-backed open or ATTACH would bypass fault injection and could leave
#    WAL/journal files beside an image.
hits=$(grep -rnE --include='*.rs' 'Connection::open\(|open_with_flags|ATTACH[[:space:]]|VACUUM[[:space:]]+INTO' crates/mochi-core/src \
        | grep -vE '^[^:]+:[0-9]+:\s*//' || true)
[ -n "$hits" ] && report "file-backed SQLite in mochi-core (catalog must stay in memory)" "$hits"

# 9. mochi-core's `test-controls` feature (fault injection, checkpoint
#    policies; review decision 14) is enabled by mochi-testkit only, and no
#    shipped crate may depend on mochi-testkit outside [dev-dependencies].
#    Cargo features are additive, so this is not an isolation boundary for a
#    workspace-wide build; it keeps release builds of shipped crates
#    (`cargo build -p mochi-cli`) free of the controls.
hits=$(grep -rnE 'test-controls' --include=Cargo.toml crates apps fuzz 2>/dev/null \
        | grep -vE '^crates/mochi-(core|testkit)/Cargo.toml:' || true)
[ -n "$hits" ] && report "test-controls enabled outside mochi-testkit (review decision 14)" "$hits"
for f in crates/mochi-cli/Cargo.toml crates/mochi-core/Cargo.toml crates/mochi-format/Cargo.toml; do
  deps=$(awk '/^\[/{sec=$0} sec=="[dependencies]" && /mochi-testkit/' "$f")
  [ -n "$deps" ] && report "shipped crate depends on mochi-testkit (review decision 14)" "$f: $deps"
done

if [ "$fail" -eq 0 ]; then echo "invariants: ok"; fi
exit "$fail"
