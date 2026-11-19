# Third-party notices

MOCHI is licensed under the MIT license (see `LICENSE`).
Some components it distributes are under other licenses. This file lists the
ones that impose conditions beyond the MIT license. A complete, generated list of
every dependency's license ships with each release (plan D7).

## UnRAR (RARLAB) — in the foreign-archive helper only

Used only by the isolated foreign-archive helper (`mochi-foreign-helper`,
plan phase D8) to **read** RAR archives. It is not part of `mochi-format`,
`mochi-core`, the `mochi` CLI, or the desktop application's own process, and
MOCHI never uses it to create RAR archives. The helper links it through the
`unrar_sys` crate (the FFI crate of the `unrar` project, MIT OR Apache-2.0)
version 0.5.8, which compiles RARLAB's UnRAR source (the copy vendored in that
crate, with its patches).

The Rust crates are labelled MIT/Apache-2.0, but the UnRAR C++ source they
vendor is **not**; it is under RARLAB's own freeware license. Its distribution
condition requires this paragraph to be reproduced:

> UnRAR source code may be used in any software to handle RAR archives without
> limitations free of charge, but cannot be used to develop RAR (WinRAR)
> compatible archiver and to re-create RAR compression algorithm, which is
> proprietary. Distribution of modified UnRAR source code in separate form or
> as a part of other software is permitted, provided that full text of this
> paragraph, starting from "UnRAR source code" words, is included in license,
> or in documentation if license is not available, and in source code comments
> of resulting package.

The full license text follows, verbatim from the vendored `license.txt`:

```text
 ******    *****   ******   UnRAR - free utility for RAR archives
 **   **  **   **  **   **  ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
 ******   *******  ******    License for use and distribution of
 **   **  **   **  **   **   ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
 **   **  **   **  **   **         FREE portable version
                                   ~~~~~~~~~~~~~~~~~~~~~

      The source code of UnRAR utility is freeware. This means:

   1. All copyrights to RAR and the utility UnRAR are exclusively
      owned by the author - Alexander Roshal.

   2. UnRAR source code may be used in any software to handle
      RAR archives without limitations free of charge, but cannot be
      used to develop RAR (WinRAR) compatible archiver and to
      re-create RAR compression algorithm, which is proprietary.
      Distribution of modified UnRAR source code in separate form
      or as a part of other software is permitted, provided that
      full text of this paragraph, starting from "UnRAR source code"
      words, is included in license, or in documentation if license
      is not available, and in source code comments of resulting package.

   3. The UnRAR utility may be freely distributed. It is allowed
      to distribute UnRAR inside of other software packages.

   4. THE RAR ARCHIVER AND THE UnRAR UTILITY ARE DISTRIBUTED "AS IS".
      NO WARRANTY OF ANY KIND IS EXPRESSED OR IMPLIED.  YOU USE AT 
      YOUR OWN RISK. THE AUTHOR WILL NOT BE LIABLE FOR DATA LOSS, 
      DAMAGES, LOSS OF PROFITS OR ANY OTHER KIND OF LOSS WHILE USING
      OR MISUSING THIS SOFTWARE.

   5. Installing and using the UnRAR utility signifies acceptance of
      these terms and conditions of the license.

   6. If you don't agree with terms of the license you must remove
      UnRAR files from your storage devices and cease to use the
      utility.

      Thank you for your interest in RAR and UnRAR.


                                            Alexander L. Roshal
```

It is freeware, not open source, and is generally treated as incompatible with
the GPL; this is why MOCHI's own license is permissive (plan §9, O23). The
license forbids using the UnRAR code to build a RAR-compatible archiver, so
MOCHI's RAR support is read-only by design, and `ci/check-invariants.sh`
checks that no extract-to-disk entry point of UnRAR is used. This file is not
legal advice.

## Other foreign-format libraries (all MIT or Apache-2.0 compatible)

The foreign-archive helper also links, for reading ZIP, 7z, tar, gzip and zstd:
`zip` (MIT), `sevenz-rust2` (Apache-2.0, decoders only: its compressor feature
is off), `lzma-rust2` (Apache-2.0), `bzip2` (MIT OR Apache-2.0) with
`libbz2-rs-sys` (bzip2-1.0.6, a BSD-style license), `ppmd-rust` (CC0-1.0 OR
MIT-0), `flate2`, `tar`, `ruzstd` (MIT, a pure-Rust Zstandard decoder, with
`twox-hash`, MIT), and their dependencies. The generated list that ships
with each release (plan D7) names every one.
