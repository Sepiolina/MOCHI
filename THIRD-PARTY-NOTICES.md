# Third-party notices

MOCHI is licensed under MIT OR Apache-2.0 (see `LICENSE-MIT`, `LICENSE-APACHE`).
Some components it distributes are under other licenses. This file lists the
ones that impose conditions beyond MIT/Apache-2.0. A complete, generated list of
every dependency's license ships with each release (plan D7).

## UnRAR (RARLAB) — desktop app only, not yet included

Used only by the isolated foreign-archive helper (`mochi-foreign`, plan phase
D8) to **read** RAR archives. It is not part of `mochi-format`, `mochi-core`, or
the `mochi` CLI, and MOCHI never uses it to create RAR archives.

The Rust `unrar` / `unrar_sys` crates are labelled MIT/Apache-2.0, but the
UnRAR C++ source they vendor is **not**; it is under RARLAB's own freeware
license. Its distribution condition requires this paragraph to be reproduced:

> UnRAR source code may be used in any software to handle RAR archives without
> limitations free of charge, but cannot be used to develop RAR (WinRAR)
> compatible archiver and to re-create RAR compression algorithm, which is
> proprietary. Distribution of modified UnRAR source code in separate form or
> as a part of other software is permitted, provided that full text of this
> paragraph, starting from "UnRAR source code" words, is included in license,
> or in documentation if license is not available, and in source code comments
> of resulting package.

The full UnRAR license text will be added here, verbatim from the vendored
`license.txt`, when `mochi-foreign` lands. It is freeware, not open source, and
is generally treated as incompatible with the GPL; this is why MOCHI's own
license is permissive (plan §9, O23). This file is not legal advice.
