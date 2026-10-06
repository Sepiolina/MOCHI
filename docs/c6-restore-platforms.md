# C6 restore destinations, per platform

Status 2026-10-06. Decided by the owner's delegation of the same date: "make the best call that finishes the project without opening more blockers; record concessions so they can be refined later; keep it modular enough to be worked on separately."

## Where the code lives

All of these implement `mochi_core::storage::RestoreDir`. The restore engine (`mochi_core::restore`) and its collision preflight are the same on every platform.

| Target | Module | Status |
|---|---|---|
| Linux | `crates/mochi-core/src/storage/os/restore_linux.rs` | Race-resistant: descriptor-relative (`openat`, `O_NOFOLLOW`, `rustix`) |
| Windows | `crates/mochi-core/src/storage/os/restore_windows.rs` | Race-*limited*: pinned directory handles, `std` only |
| Anything else | `crates/mochi-core/src/storage/os/restore_unsupported.rs` | Refused, `UNSUPPORTED_FEATURE`, before anything is written (not a 1.0 platform, plan O11) |

`storage/os.rs` selects one by `cfg` and re-exports it as `storage::os::OsRestoreDir`. To replace an implementation, change only its file. The contract is the trait, and the tests listed below must keep passing. `ci/check-invariants.sh` rule 1 allows `std::fs` in `storage/os.rs` and `storage/os/*.rs` only.

## Windows: what is guaranteed

The constraint is no `unsafe` in `mochi-core` (AGENTS.md; T21's decision), and `std` has no handle-relative file creation on Windows. The implementation therefore uses the strongest mechanism `std` exposes:

1. **Pinning.** The root and every directory the restoration creates are opened with read access and a share mode **without `FILE_SHARE_DELETE`**, and held until the restoration ends. Windows refuses any other open that requests `DELETE`, and renaming or deleting a directory needs one. So no directory in the restored tree can be moved away and replaced by a junction or link while names are resolved through it.
   - Read access is required. An open with attribute access only takes no part in share-mode checks and would pin nothing.
2. **No reparse point is followed.** Every open uses `FILE_FLAG_OPEN_REPARSE_POINT`. A directory that turns out to be a reparse point once pinned fails that entry. The check is made on the held handle, so it cannot change afterwards.
3. **Nothing is overwritten.** Files are created `CREATE_NEW`, so a planted link at the name is a collision. Publication is hard link then unlink, which never replaces (D13's mechanism).
4. **Attributes are applied through the entry's own handle**, opened with `FILE_FLAG_OPEN_REPARSE_POINT` and checked not to be a reparse point.

Plus everything the engine gives on every platform: case-behaviour detection on the root, collision and unsupported-name preflight before writing, verified files only, and no renaming of names.

Evidence: `crates/mochi-testkit/tests/c6_restore_windows.rs`, plus the cross-platform `c6_os_restore_and_restore_again` and `c6_os_attributes`, all on `windows-latest` CI:
- held directories (and the root) cannot be renamed or deleted, and can be once released;
- NTFS is detected as case-insensitive;
- planted symbolic links are collisions and never followed;
- a root that is a link is refused.

## Windows: concessions (accepted for 1.0, recorded for refinement)

| # | Concession | Why | Exposure | Remedy, when wanted |
|---|---|---|---|---|
| W1 | The root's **ancestors** are not pinned; operations pass paths through them | Pinning every ancestor would fail whenever another process holds one with delete access, and `std` cannot open relative to a handle | Someone able to rename a directory *above* the chosen destination could redirect later writes | Handle-relative creation (`NtCreateFile` with `RootDirectory`) in a separate crate with audited `unsafe`, or a vetted dependency; or pin ancestors best-effort |
| W2 | The destination's **ACL is not checked**. Linux refuses a root other users can modify; Windows does not | No ACL API in `std` | A user with write access to the destination could swap a temporary file before it is published. Directories cannot be swapped (pinned). | Read the DACL (`GetSecurityInfo`) in the same separate crate and refuse as Linux does; meanwhile, restore into folders only you can write (the default for your profile) |
| W3 | Temporary files are not pinned | The engine publishes while it still holds the file, and unlinking the temporary name needs `DELETE` | As W2 | `SetFileInformationByHandle(FileLinkInformation)` or rename-by-handle in the separate crate |
| W4 | Hidden and system attributes are reported, not set | No `std` API (`SetFileAttributesW`) | Cosmetic; reported as `ATTRIBUTE_NOT_RESTORED` | Same crate, or `OpenOptionsExt::attributes` at creation if the engine passes attributes to `create_file` |
| W5 | Directory durability is `Unconfirmed` | Plan O12, until gate G6 | Reported as degraded, never as durable | G6 (T33, Windows hardware) |
| W6 | The case-behaviour probe checks for the upper-case name, then creates the lower-case one; another process could create the upper-case name in between | No file-identity API in stable `std` | Only the *report* of case behaviour could be wrong; exclusive creation still refuses every collision | File identity (`GetFileInformationByHandle`) in the same crate |

None of these allows an overwrite, a write through a link, or a write outside the tree, given that only the user (and administrators) can modify the destination and its ancestors. That is the stated requirement for Windows restores. User-facing text (desktop D3) should say: "Restore into a folder only you can modify."

## Linux: concessions

- **L1.** Insensitive detection relies on a probe file in the root, created and removed. CI job `casefold` proves it on ext4 casefold.
- **L2.** Unicode-normalization-only collisions are not foreseen by the preflight. Exclusive creation still catches them.
- **L3.** A root other users can modify is refused, not worked around.

## Suggested separate work item

`mochi-winfs` (name tentative): a small crate with audited `unsafe` (`windows-sys`, MIT/Apache-2.0) offering handle-relative create, open, rename and attribute calls, file identity, and a DACL check. `restore_windows.rs` would switch to it and close W1–W4 and W6. Like `mochi-foreign`, it is isolated from `mochi-core`'s `forbid(unsafe_code)`. Only `restore_windows.rs` and this document change.
