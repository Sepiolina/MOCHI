# MOCHI

**MOCHI** is an archive format and an archiver: a `.mochi` file keeps every version
of what you put in it, checks every byte it reads, and says plainly what it could
and could not verify. It ships the way WinRAR does: a desktop application for
Windows and Ubuntu, a `mochi` command-line tool, and the Rust library both are built on.

> **A `.mochi` file is not a backup.** It is one file on one disk. Keep independent
> copies of anything you cannot recreate. See [why](docs/user/not-a-backup.md).

## What it does

- **Versions, not just files.** Each `create` or `append` is a commit. Every earlier
  snapshot stays readable; deleting a file removes it from the current snapshot, not
  from history. Unchanged content is stored once.
- **Verified reads.** Every object is checked against its BLAKE3-256 hash before it is
  used, and a file is only written out after its content hash matches. `verify`
  reports each health dimension separately and never calls a check that did not run
  a pass.
- **Safe publication.** A commit becomes visible only after it is completely on disk;
  an interrupted write never replaces the last good version.
- **Optional profiles, chosen at creation:**
  - *Encrypted* (XChaCha20-Poly1305, passphrase-only): names, sizes and content are sealed.
  - *Recovery record*: Reed–Solomon parity that `repair` can use to rebuild damaged parts.
  - *TAR-compatible*: standard `zstd` and `tar` can read the archive's history.
- **Desktop app.** Browse, create, add, extract, test and repair `.mochi` archives;
  open ZIP, 7z, RAR and `.tar`/`.tar.gz`/`.tar.zst` read-only; write ZIP and TAR
  archives; search file names across every archive under a folder and retrieve the
  matches. Explorer and file-manager integration on Windows and Ubuntu.

The format is **ratified at version 1.0** ([specification](docs/spec.md)).
Archives written by pre-1.0 builds are converted with `mochi migrate`.

## Download

Installers are on the **Releases** page:

| Platform | Package |
|---|---|
| Windows 10 22H2 / 11 (x86-64) | `MOCHI_<version>_x64-setup.exe` (per-user install) |
| Ubuntu 22.04 / 24.04 (x86-64) | `mochi_<version>_amd64.deb` (`sudo apt install ./mochi_*.deb`) |

The installers are not code-signed, so Windows SmartScreen shows "unknown publisher"
(choose *More info*, then *Run anyway*). The in-app updater checks its own signature on
every update before installing it. macOS is not supported.

## The command line

```bash
mochi create photos.mochi ~/Pictures/2026        # a new archive (never replaces a file)
mochi append photos.mochi new.jpg --delete 2026/old.jpg
mochi list photos.mochi                          # the latest snapshot; --snapshot N for history
mochi get photos.mochi -C restored/              # each file verified before it is written
mochi verify photos.mochi                        # read-only health report (--json for machines)
mochi find --name report.pdf --name '*.log' ~/Archives   # names across many archives
```

Every command, option and exit code: [`docs/cli.md`](docs/cli.md).

## Documentation

| | |
|---|---|
| [User guide](docs/user/README.md) | Getting started, encryption, what the checks prove, recovery records, TAR compatibility |
| [Command-line reference](docs/cli.md) | Commands, exit codes, JSON output |
| [Specification](docs/spec.md) | The `.mochi` format (normative), with the frozen [ratification artifacts](docs/ratification/README.md) and [schemas](docs/schemas/) |
| [Desktop app](apps/mochi-desktop/README.md) | Architecture, IPC, security rules, testing |
| [Developer documents](docs/dev/README.md) | Design decisions, packaging and releases, benchmarks, platform notes |
| [Release notes](docs/release-notes/) | What changed in each version |

## Building from source

Rust 1.91 (pinned in `rust-toolchain.toml`; the minimum supported version is 1.89).

```bash
cargo build --release -p mochi-cli          # the `mochi` binary
cargo test --workspace
```

The desktop app also needs Node 22 or newer with pnpm, and on Linux the WebKitGTK 4.1
development packages. See [`apps/mochi-desktop/README.md`](apps/mochi-desktop/README.md).

## Repository layout

| Path | What it is |
|---|---|
| `crates/mochi-format` | Framing, record envelopes, digests. Pure: no I/O |
| `crates/mochi-core` | Archive operations: catalog, commits, verification, restore, repair, jobs, reports |
| `crates/mochi-cli` | The `mochi` command-line tool |
| `crates/mochi-export` | Writes ZIP and TAR archives for the desktop app |
| `crates/mochi-foreign` | Reads ZIP, 7z, RAR and TAR in an isolated helper process |
| `crates/mochi-sys`, `crates/mochi-shellext` | Windows system calls and the Explorer context menu |
| `crates/mochi-testkit` | Fault-injecting storage, golden fixtures, test tools |
| `apps/mochi-desktop` | The desktop app (Tauri 2, React, TypeScript) |
| `interop/pyverify` | An independent read-only verifier in Python, written from the specification alone |
| `fixtures/`, `fuzz/` | Golden archives and vectors; fuzz targets |
| `packaging/`, `ci/` | Installers, the bundled user guide, CI scripts |

## Contributing

Contributions are welcome. Read [`CONTRIBUTING.md`](CONTRIBUTING.md) first: the format
has invariants that tests alone cannot enforce. To report a security problem, see
[`SECURITY.md`](SECURITY.md).

## License

[MIT](LICENSE). Third-party components and their licenses are listed in
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md); the RAR reader uses the UnRAR source,
which is freeware under its own license and is used for reading only.
