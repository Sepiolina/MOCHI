# Contributing to MOCHI

Thank you for helping. MOCHI is an archive format: a mistake can make someone's data
unreadable years later, so changes are reviewed against rules that tests alone cannot
enforce. This page is the short version; [`AGENTS.md`](AGENTS.md) has every rule and is
what review checks against (it is written for human contributors and coding agents alike).

## Sources of truth

1. [`docs/spec.md`](docs/spec.md): the specification. Normative text wins.
2. [`docs/ratification/`](docs/ratification/README.md): frozen artifacts (frame
   registry, catalog DDL, serialization, vectors). **Never edit them**: a change to any
   byte or rule there is a new format identifier with a spec Annex B entry.
3. [`docs/dev/decisions.md`](docs/dev/decisions.md): product decisions (O1–O32) and
   the Definition of Done.

If the spec does not answer a question, do not invent an answer: open an issue that
states the gap and proposes options.

## Setting up

- Rust 1.91 (installed automatically from `rust-toolchain.toml`).
- For the desktop app: Node 22 or newer and pnpm; on Ubuntu,
  `sudo apt-get install libwebkit2gtk-4.1-dev libxdo-dev libssl-dev librsvg2-dev libayatana-appindicator3-dev`.
- Optional: nightly Rust and `cargo install cargo-fuzz` for fuzzing; Python 3.10+ for
  the interop verifier and the packaging tests.

## Checks to run before a pull request

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
ci/check-invariants.sh
```

Desktop app (from `apps/mochi-desktop/ui`):

```bash
pnpm install --frozen-lockfile && pnpm lint && pnpm typecheck && pnpm test
```

`cargo test -p mochi-desktop` regenerates the TypeScript IPC types in
`apps/mochi-desktop/ui/src/ipc/generated`; commit them (CI fails when they are stale).
More targets (golden sets, schema checks, fuzzing, benchmarks, TAR interoperability)
are listed under "Commands" in [`AGENTS.md`](AGENTS.md).

## Rules that matter most

- **Wire format.** Byte layouts, magic numbers and field semantics follow the spec and
  the ratification artifacts exactly. Magic numbers live in one module of
  `mochi-format`. Golden fixtures are updated deliberately, never by blanket
  regeneration, and the pull request explains the diff.
- **Integrity.** Every object is hash-verified before it is parsed; the digest scopes
  stay distinct; a partially written commit never becomes the head.
- **Untrusted input.** Archives are untrusted: bounds- and overflow-check before every
  seek or allocation. Parser changes come with a fuzz target extension and a vector.
- **No panics on archive data** in library crates; `unsafe` only where `AGENTS.md`
  allows it, with a `// SAFETY:` comment.
- **All I/O in `mochi-core` goes through the `Storage` trait**, so it can be
  fault-injected.
- **Reporting.** Only the spec's status values; a check that did not run is never
  `PASS`. Never describe an archive as "backed up", "safe" or "preserved".
- **Desktop app.** No format logic in the UI or in Tauri commands; least-privilege
  capabilities; archive-supplied names are rendered as text only; the webview never
  names a path.
- **Dependencies** must be MIT- or Apache-2.0-compatible.

## Tests that come with a change

Feature code comes with its Definition of Done test, not just a happy path.
Corruption, recovery or encryption code comes with a fault-injection or corruption
test; desktop behaviour with a UI or end-to-end test. See
[`docs/dev/decisions.md`](docs/dev/decisions.md) and spec §24.2.

## Pull requests

- Cite the spec sections affected.
- No wire-format change without a ratification artifact or Annex B entry.
- Required tests included and passing on Windows and Ubuntu (CI runs both).
- Any new Tauri permission is justified in the pull request.
- User-facing wording checked against spec §23.3.
- Open questions recorded (spec Annex B or `docs/dev/decisions.md`), not silently resolved.

By contributing you agree that your contribution is licensed under the
[MIT license](LICENSE).
