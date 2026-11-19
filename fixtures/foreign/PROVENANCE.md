# Where the foreign-format fixtures come from

Fixtures are test inputs only. None is a MOCHI archive. Writers are independent
of the code under test.

| Fixture set | Writer | Version |
|---|---|---|
| `*.zip` (Info-ZIP) | Info-ZIP `zip` | 3.0 (July 5th 2008) |
| `basic.tar`, `*.tar`, `*.tar.gz` | GNU tar, gzip, Python `tarfile` | tar 1.35, gzip 1.12 |
| `7z/*.7z`, `7z/zip_aes256_7zip.zip` | 7-Zip | 23.01 (Debian `7zip 23.01+dfsg-11`) |
| `7z/traversal.7z`, `7z/long_name.7z` | py7zr (names 7-Zip will not store) | 1.1.4 |
| `rar/*.rar` (base files) | RARLAB `rar` (WinRAR's command-line sibling), as shipped in the `unrar` crate 0.5.8's test data; MIT OR Apache-2.0 | not recorded in the files; the RAR 4 header says format 2.9 (RAR 3.x-4.x), `solid.rar` and the others RAR 5; file times 2015-2019 |
| `rar/*` mutations | `gen_rar_mutations.py`: flipped or cut bytes of the base files | n/a |

**WinRAR is not available to this repository or its CI**, so no archive here was
written by WinRAR itself, and no RAR fixture was made by anything else. Nothing
in the repository writes RAR (plan D8 rule 1).

The pinned corpus (`CORPUS.sha256`) is compared, byte for byte, with an
independent extraction by 7-Zip, Info-ZIP `unzip` and GNU `tar` in CI
(`tests/corpus_interop.rs`; `MOCHI_REQUIRE_TOOLS=1` on Ubuntu makes a missing
tool a failure). On the Ubuntu runners 7-Zip is `p7zip-full`, an older build
than the one that wrote the fixtures; reading is what is compared.
