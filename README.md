# MOCHI

**A versioned, integrity-verified archive format and archiver.**

MOCHI is an archive format designed around version history, data integrity, recoverability, and reliable storage.

A single `.mochi` archive preserves previous versions of its contents, stores unchanged data only once, and verifies data before extraction. Every verification report explicitly distinguishes what was checked, what passed, what failed, and what could not be verified.

MOCHI is available as:

- A desktop application for Windows and Ubuntu.
- A command-line tool for scripting and automation.
- A Rust library for programmatic archive operations and integration.

[Download](#download) · [Features](#features) · [Quick start](#quick-start) · [Documentation](#documentation) · [Specification](docs/spec.md)

## Features

### Versioned archives

Every archive creation or modification produces a commit with its own snapshot. Previous snapshots remain accessible, even after files have been modified or deleted from the current version.

Unchanged content is stored only once, avoiding unnecessary duplication across snapshots.

### Data integrity

Every archive object is verified against its BLAKE3-256 hash before use. Extracted files are written only after their content hashes have been validated.

Verification reports distinguish individual integrity checks and never report an unperformed check as successful.

### Interrupted-write protection

New commits become visible only after they have been completely written to disk. An interrupted write does not replace the last successfully committed snapshot.

### Optional archive profiles

Additional capabilities can be enabled when creating an archive:

| Profile | Description |
|---|---|
| Encryption | Passphrase-based XChaCha20-Poly1305 encryption protects file names, sizes, and contents. |
| Recovery record | Reed–Solomon parity enables reconstruction of damaged archive data within the available recovery capacity. |
| TAR compatibility | Archive history can be read using standard `zstd` and `tar` tools. |

### Desktop experience

The MOCHI desktop application provides a graphical interface for archive management:

- Create, browse, modify, extract, verify, and repair MOCHI archives.
- Browse and restore historical snapshots.
- Open supported third-party archive formats.
- Create ZIP and TAR archives.
- Search file names across archives within a directory and retrieve matching files.
- Integrate with Windows Explorer and supported Ubuntu file managers.

### Archive compatibility

| Format | Read | Write |
|---|:---:|:---:|
| MOCHI (`.mochi`) | Yes | Yes |
| ZIP (`.zip`) | Yes | Yes |
| 7-Zip (`.7z`) | Yes | No |
| RAR (`.rar`) | Yes | No |
| TAR (`.tar`) | Yes | Yes |
| Gzip-compressed TAR (`.tar.gz`) | Yes | — |
| Zstandard-compressed TAR (`.tar.zst`) | Yes | — |

Third-party archive readers operate in an isolated helper process.

## Format specification

The MOCHI archive format is ratified at **version 1.0**, with a normative specification, schemas, conformance fixtures, and frozen ratification artifacts.

- [Format specification](docs/spec.md)
- [Ratification artifacts](docs/ratification/README.md)
- [Format schemas](docs/schemas/)

An independent Python verifier, implemented directly from the specification, is also included in the repository.

Archives created by pre-1.0 versions can be converted using `mochi migrate`.

## Download

Prebuilt installers are available on the **Releases** page.

| Platform | Architecture | Installer |
|---|---|---|
| Windows 10 22H2 / Windows 11 | x86-64 | `MOCHI_<version>_x64-setup.exe` |
| Ubuntu 22.04 / 24.04 | x86-64 | `mochi_<version>_amd64.deb` |

### Windows

Download and run the Windows installer. MOCHI installs on a per-user basis.

Installers are currently unsigned, so Windows SmartScreen may display an unknown-publisher warning. After confirming that the installer was obtained from the official project release, select *More info* followed by *Run anyway*.

### Ubuntu

Download the Debian package and install it using:

```bash
sudo apt install ./mochi_*.deb
```

### Updates and platform support

The built-in updater verifies update signatures before installation.

macOS is not currently supported.

## Quick start

The `mochi` command-line interface provides direct access to archive operations without requiring the desktop application.

**Create an archive**

```bash
mochi create photos.mochi ~/Pictures/2026
```

Creates a new archive without overwriting an existing file.

**Update an archive**

```bash
mochi append photos.mochi new.jpg --delete 2026/old.jpg
```

Creates a new snapshot while preserving previous versions.

**List archive contents**

```bash
mochi list photos.mochi
```

Displays the latest snapshot. Use `--snapshot N` to inspect a historical snapshot.

**Extract files**

```bash
mochi get photos.mochi -C restored/
```

Verifies file contents before writing them to the destination.

**Verify archive integrity**

```bash
mochi verify photos.mochi
```

Performs read-only integrity checks. Use `--json` for machine-readable results.

**Search across archives**

```bash
mochi find --name report.pdf --name '*.log' ~/Archives
```

Searches for matching file names across archives within a directory.

For the complete command reference, including options, exit codes, and output formats, see the [CLI documentation](docs/cli.md).

## Reliability and limitations

MOCHI is designed to preserve archive history, detect corruption, and recover damaged data when sufficient recovery information is available.

However, these capabilities have limits:

- Integrity verification detects corruption; it does not guarantee that damaged data can be recovered.
- Recovery records provide redundancy within an archive, not protection against the loss of the entire archive.
- Version history does not protect against disk failure, theft, or accidental deletion of the archive itself.
- Verification reports describe the checks actually performed, rather than making unconditional guarantees about archive health.

> **A `.mochi` archive is not a backup.**
>
> An archive stored on a single device remains a single point of failure. Always maintain independent copies of important or irreplaceable data.
>
> See [Why an archive is not a backup](docs/user/not-a-backup.md).

## Documentation

| Resource | Description |
|---|---|
| [User guide](docs/user/README.md) | Getting started, archive management, encryption, verification, recovery records, and TAR compatibility. |
| [CLI reference](docs/cli.md) | Commands, arguments, exit codes, and machine-readable output. |
| [Format specification](docs/spec.md) | Normative definition of the MOCHI archive format. |
| [Desktop application](apps/mochi-desktop/README.md) | Application architecture, IPC, security boundaries, and testing. |
| [Developer documentation](docs/dev/README.md) | Design decisions, benchmarks, packaging, release procedures, and platform-specific implementation details. |
| [Release notes](docs/release-notes/) | Changes, improvements, and fixes across releases. |

## Building from source

### Requirements

| Dependency | Version |
|---|---|
| Rust | 1.91, pinned by `rust-toolchain.toml` |
| Minimum supported Rust version | 1.89 |
| Node.js | 22 or newer, desktop application only |
| pnpm | Required for desktop development |
| WebKitGTK | 4.1 development packages, Linux desktop only |

### Build the CLI

```bash
cargo build --release -p mochi-cli
```

The compiled executable is available in `target/release/`.

### Run tests

```bash
cargo test --workspace
```

### Build the desktop application

The desktop application is built with Tauri 2, React, TypeScript, and Rust.

For complete setup instructions, platform dependencies, and development commands, see the [desktop application documentation](apps/mochi-desktop/README.md).

## Repository structure

MOCHI is organized as a modular Rust workspace, with separate components for the archive format, archive operations, platform integration, and application interfaces.

| Path | Responsibility |
|---|---|
| `crates/mochi-format` | Format primitives, record framing, envelopes, and digests, without I/O. |
| `crates/mochi-core` | Archive operations, catalogs, commits, verification, restoration, repair, jobs, and reporting. |
| `crates/mochi-cli` | Command-line interface. |
| `crates/mochi-export` | ZIP and TAR archive creation. |
| `crates/mochi-foreign` | Isolated readers for ZIP, 7z, RAR, and TAR. |
| `crates/mochi-sys` | Windows system-level integration. |
| `crates/mochi-shellext` | Windows Explorer context-menu integration. |
| `crates/mochi-testkit` | Fault-injecting storage, golden fixtures, and testing utilities. |
| `apps/mochi-desktop` | Cross-platform desktop application. |
| `interop/pyverify` | Independent, read-only Python verifier implemented from the specification. |
| `fixtures/` | Reference archives and conformance test vectors. |
| `fuzz/` | Fuzz-testing targets. |
| `packaging/` | Platform installers and bundled documentation. |
| `ci/` | Continuous integration scripts. |

## Contributing

Contributions, bug reports, and technical feedback are welcome.

Before submitting changes, please read the [contribution guidelines](CONTRIBUTING.md). The archive format defines invariants and correctness requirements that cannot be enforced through automated tests alone.

Changes affecting archive compatibility, integrity guarantees, or recovery behavior should be evaluated against the normative format specification.

## Security

Please report security vulnerabilities using the process described in the [security policy](SECURITY.md).

## License

MOCHI is licensed under the [MIT License](LICENSE).

Third-party components and their respective licenses are documented in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

The RAR reader uses UnRAR source code under its separate freeware license, exclusively for reading archives.
