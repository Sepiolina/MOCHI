# Fuzz regression inputs

Inputs a fuzz target crashed on, kept byte for byte, one directory per target.
`crates/mochi-testkit/tests/c8_salvage.rs::salvage_fuzz_regression_inputs` runs
every file in `salvage_scan/` through `exercise_salvage_scan` on stable Rust.
CI's `salvage_scan` fuzz job also seeds from `salvage_scan/`; `foreign_sevenz/` is run by `crates/mochi-foreign/tests/sevenz_fixtures.rs` (the same bytes are in `fixtures/foreign/7z/`, which `fuzz smoke (foreign_sevenz)` seeds from). Files are named as libFuzzer wrote them (`crash-<sha1>`). Add a hand-built test
for the same case next to the one that runs the file.

| File | Found | Fixed by |
|---|---|---|
| `salvage_scan/crash-599502c9474a11dc9cd4110c9d26a8c296b1c513` | CI run 38452009022 (job 115485024551) | a salvaged chain holds only commits the scan confirmed (`c8_salvage.rs::salvage_19`; plan W12 "Fix (2026-10-13)") |
| `salvage_scan/crash-0e608cfbfda3fdf1358c3d1e2a0749b75a9700ce` | CI run 42501819636 (`fuzz smoke (salvage_scan)`) | not a salvage defect: the exerciser's oracle now allows a salvage-labelled snapshot from the trusted head's chain down to its break, which the ordinary walk reached (`c8_salvage.rs::salvage_21`) |
| `foreign_sevenz/timeout-9b2ba6ed372fbec405e789b92be1223e9c9fbf77` | CI run 38558591812 (`fuzz smoke (foreign_sevenz)`) | `sevenfmt::check_truncation` refuses a zeroed start header (audit F16); `sevenz_fixtures.rs::a_zeroed_start_header_is_refused_promptly_not_scanned_for_headers` |
