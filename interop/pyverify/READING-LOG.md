# Reading log: where the specification had to be interpreted

This is evidence for W18. Every entry is a place where the text (spec, R1–R7, the schemas, W17) did
not fix one reading, or where I had to guess, with the reading I chose and the section that best
supports it. Nothing here was settled by looking at any `*.summary.json` (none exists yet), at
the Rust code, or at `gen_vectors.py`. Where a *fixture or vector* (used as test data, which the
rules allow) taught me something the text does not say, the entry says so under **Source**.

The log was written as the implementation went on, not before it: most ambiguities only showed
up when a rule had to become code. It was completed after the last test run.

Notation: **Chosen** is what the verifier does. **Risk** says what is likely to differ from the
reference, so W18 can look there first.

---

## A. Framing, head discovery, tail

**A1. Where a footer may be accepted.** §8.4 says a clean EOF permits reading the final 64 bytes,
but §8.5 says magic matches are candidates only. **Chosen:** a footer is a head candidate only at a
frame boundary of the structural forward walk (§8.6); if the walk itself broke earlier, a footer
beyond the break is also a candidate (otherwise damage in the middle would hide a valid head).
The latest candidate that passes R1 §3 validation *and* names a commit-record frame is the head.
`c1/reject-false-magic-inside-payload` is why. **Risk:** an archive whose mid-file damage breaks
the walk: the reference may use a backward scan, I use the forward walk (§22 step 2 mentions both).

**A2. Footer validation order and codes.** R1 §3 gives the order. §8.4 also requires the footer to
follow its commit frame (amendment). Sequence in the footer ≠ the commit's key 2 → `FOOTER_INVALID`
(R1 §3 says the caller checks it, without a code). **Risk:** low.

**A3. Tail eligibility (D14).** The text lists four conditions. Readings: (1) "walks as complete
frames … optionally ending in one frame cut short by EOF": a cut frame must begin with a recognised
magic and have ≥ 8 header bytes; fewer bytes, or an unrecognised magic, is "unrecognised bytes"
and makes the tail ineligible. (2) "neither footer marker occurs where a complete footer could
fit": I scan *every byte offset p* of the tail with p + 72 ≤ tail length for the footer skippable
magic at p or the payload magic `MOCHI2\0\0` at p + 8, including inside data payloads
(conservative; "a screen, not proof"). (3) any complete frame of kind footer or descriptor
makes it ineligible. **Reporting:** eligible → `UNCOMMITTED_TAIL` (operational, not a finding in
the summary); not eligible → finding `TAIL_UNRESOLVED` (violation class). **Risk:** medium; the
exact marker window is my reading.

**A4. A damaged non-head footer.** D10.6 says an unusable parent traversal offset is not
recoverable and the open fails (`FOOTER_INVALID`), so the head is *refused*, not "the previous
commit". A damaged *head* footer is different (the next-to-last footer is then the head and the
rest is a tail). **Risk:** the reference may treat these two differently from me only in the
case of damage at the boundary between them.

**A5. Duplicate footers.** A second footer that names the same commit is not examined; it
would be accepted as a head. I did not find text that forbids it. **Risk:** low.

**A6. What "open head" refuses on, versus what is only a finding.** The text splits this
nowhere; the c5 `vectors.txt` verdicts (`open_head`) are the guide. **Chosen:** the head is
**refused** (summary `head.status = refused`, one code) when any of these fails:
no valid footer; a footer/commit/parent-link/ID/base-rule check of any commit in the chain
(D10.6); the descriptor (hash, decoding, archive ID, Encrypted agreement, one descriptor per
segment); the head's own delta manifest and every delta manifest between the head's base and the
head (hash → `STORED_INTEGRITY_FAILED`, decoding, identity → `ENVELOPE_INVALID`, parent link,
replay → `RECORD_INVALID`/`NAMESPACE_INVALID`/`EXTENT_INVALID`). Everything else (other
commits' manifests, images, chunk objects, parity, TAR framing, key envelopes, data regions)
is a **finding** and the head stays `opened`. The listed c5 verdicts pass with this split.
**Risk:** high. Examples that may differ: a damaged *image* of the head's base (I report a
finding if the snapshot manifest and the replay give the state); a `LIMIT_EXCEEDED` in a commit
record (operational class, but I refuse with it because the summary has only `refused`/`opened`).

**A7. What a refused summary contains.** `head` and nothing else: `archive_id` null, default
`profile`, empty `commits`, `snapshots` and `findings`, `parity` null. The format says
`commits` is "the verified chain"; a refused head verified nothing. **Risk:** high (the
reference might still name the archive ID or the profile).

**A8. Descriptor missing/damaged (D12).** D12 says head discovery and stored-integrity checks are
permitted without a descriptor, "reported FAIL", interpretation refused. The summary has no
place for "head found but not interpreted", so a damaged descriptor is `refused` /
`DESCRIPTOR_INVALID`, as the c5 verdicts require (`reject-archive-descriptor-hash`).

**A9. Walk limits.** Skippable payload > 256 MiB is `LIMIT_EXCEEDED` before the cut-short check
(the c1 oversized vector is an 8-byte file). Frame limit 269,484,032 and commit frame limit 8 + 64 KiB
from B.2.3. Block size above `min(window, 128 KiB)` → `MALFORMED_FRAME` (R1 §2: "a Block_Size above
the frame's block maximum"; I derived the maximum from RFC 8878: the window size, or the content
size for a single-segment frame).

**A10. CBOR corner cases the table (R3 §1.1) does not list.** Additional information 31 on major
types 0, 1, 6 → `ReservedAdditionalInfo`; major 7 with additional information 24 → `SimpleValue`;
a map key of major 7 is `Float`/`SimpleValue` rather than `NonIntegerKey`. Item/depth counting as
R3 says (a key counts as an item; the root is depth 0). All R3 table rows are tests.

---

## B. Records

**B1. Check order inside a record.** R2 §6 and R3 §7.3 say precedence among simultaneous faults is
not wire format. **Chosen:** key 0 first (unknown/legacy → `UNSUPPORTED_FEATURE`), then the closed
schema (keys and types), then features (increasing → `ENVELOPE_INVALID`, then unknown →
`UNSUPPORTED_FEATURE`), then structure rules, then the commit ID. All c4/c5/b2 single-defect vectors
agree.

**B2. Codes for value-level violations** follow R3 §7.3: `RECORD_INVALID` (`DESCRIPTOR_INVALID`
in the descriptor). Extent violations inside a manifest I report as `EXTENT_INVALID`
(R4 §3.1 lists the rule; R3 §7.1 names no code for the manifest side). A chunk that an extent
names but the state lacks: `EXTENT_INVALID` (R4 lists "the chunk exists" among the extent rules),
not `REFERENCE_INVALID`.

**B3. Retention rules decoded standalone.** A manifest alone cannot judge "label not active" or
"already expired" against replayed state, so decoding checks only what is local (shapes, ranges,
duplicates inside the one manifest); the state-dependent half runs in replay. Provenance
"each entry before the source head": I read "source head" as the last source sequence of key 1
(the head is always kept, D18).

**B4. Provenance reason.** `? 3: 1` only; an explicit 0 is refused (R3 §6), reason is 0 when absent.

**B5. Commit schema 2 key 11 limit.** More than 16 envelopes → `LIMIT_EXCEEDED` (R3 §7.3);
zero → `RECORD_INVALID`. Key 12 `null` when the commit adds no data.

**B6. Descriptor draft check.** Descriptor key 3 must be 2; 1, 3, `null`, anything else →
`UNSUPPORTED_FEATURE`; a non-integer → `DESCRIPTOR_INVALID` (matches `descriptor-vectors.txt`). Order of
the wire-generation and draft refusals relative to unknown features: after the closed schema.

**B7. Binary envelope.** R2 §2 order implemented exactly. A reader's required-feature limit is
64 (equal to the wire maximum), so `LIMIT_EXCEEDED` for features is unreachable in practice.
SQLite `user_version` is read big-endian signed at bytes 60–63.

---

## C. Replay and state

**C1. What a snapshot manifest holds.** D10 item 2 and the v1 schema say the snapshot manifest is
"the complete authoritative state", with arrays 5/6 "every chunk referenced" / "every file version
referenced". **Source (fixture):** in `c5/valid-archive-3-commits.mochi` the snapshot manifest of
commit 1 holds 9 chunks and 5 file versions, while replaying every delta from commit 0 gives 13
and 6: superseded versions are absent. **Chosen:** a snapshot manifest is *self-contained for the
live namespace*: it must hold the namespace, the versions the namespace names and the chunks their
extents read, equal to the replayed records, and may carry more only if identical to the replay;
retention and key state must match exactly. That is my D10.7 "adoption" comparison; a difference is
`CHECKPOINT_MISMATCH`. **Risk:** high; if the reference means something else by "equals".

**C2. Which state do I call "the snapshot at c".** The *full* replay (delta 0 … c, with checkpoints
cross-checked, not reset) because the catalog image of the fixtures equals it exactly. When a
delta in the chain is damaged, a later checkpoint's snapshot manifest is the fallback (D10.9),
and its state is then only the live closure (later deltas can still replay on it). **Risk:** medium.

**C3. Retained snapshots in the summary.** D18: roots at head h are h, every commit not expired,
every held commit. **Chosen:** `snapshots` lists commits for which a state is available and that
are roots by that rule, using the head's retention state.

**C4. Baseline recovery.** B.2.8 "reference scope": a delta that reads something not in the
segment's snapshot manifest opens normally but fails baseline recovery → `REFERENCE_INVALID`
(violation). I replay each segment from its snapshot manifest alone and report that code.

**C5. Namespace rules** (R4 §3.1): judged after the whole commit; every entry's parent must be a
directory entry present at the end of the commit; top level has an implicit root; a delete of an
absent path is `NAMESPACE_INVALID`; a put of an unknown version is `NAMESPACE_INVALID`.
"Introduction" (D10.4): an introduced chunk/version ID that already exists is `RECORD_INVALID`.

**C6. Key state (Encrypted).** Manifest key 13 operations replay validly (no duplicate add, no
remove of a stranger, never empty); each commit's key-11 set must equal the replayed set
(B.2.10 item 10), checked in the keyed run; the envelope IDs of key 11 must be distinct and increasing
(commit-record-v2.cddl). All envelopes of the archive must carry one key ID (`RECORD_INVALID`).

---

## D. Objects and integrity

**D1. Hash scope and `object` in findings.** A stored-integrity finding carries the *recorded*
(expected) stored-object hash of the object, since the actual hash is not a property of the
archive. For a chunk: its stored-object hash from the manifest. For a data region: the region
hash in commit key 12. Findings other than integrity ones have `object: null`. **Risk:** medium.

**D2. Violation class.** "Violation-class findings" is read as R7 §6's *Verification class*
column. So `PARITY_DAMAGED` (class violation, although "reported as a warning finding") **is**
in the summary, `UNCOMMITTED_TAIL`, `KDF_COST_BELOW_DEFAULT`, `LIMIT_NOT_DEFAULT` and
`UNSUPPORTED_FEATURE` findings are not. **Risk:** high for `PARITY_DAMAGED`.

**D3. Chunk location.** `stored_offset` null means "rediscover by scan + stored hash"
(manifest schema comment): I hash every frame of a forward walk and look the chunk up by hash.

**D4. Chunk content checks.** Exactly one Zstandard data frame, `Frame_Content_Size` and the
checksum present (O21 via R1 §2/R4): else `MALFORMED_FRAME`. Decoded length ≠ record or frame size,
or content hash mismatch, or a decoder error: `CONTENT_INTEGRITY_FAILED`. A frame naming a
dictionary the catalog does not declare is `CONTENT_INTEGRITY_FAILED` (c2 `reject-object-dictionary-id`
lists `ContentIntegrity`, which I map to that code); a declared dependency is `UNSUPPORTED_FEATURE`.
**Source:** the Rust-style class names in `c2/vectors.txt` (`Malformed`, `ContentIntegrity`) are mapped to
the codes above; R7 does not define that mapping.

**D5. Files.** Every version's content is reassembled from its extents and compared with the recorded
length and plain BLAKE3; a mismatch with all chunks individually fine is `CONTENT_INTEGRITY_FAILED`
(object null). Files that touch a failing chunk are skipped (no double report).

**D6. Catalog image.** R4 §3 steps 1, 3, 4. Hardening pragmas (step 2) are the reference reader's;
I open the temporary file read-only (`mode=ro&immutable=1`). The image's state (chunks, versions,
extents, objects, locations, namespace replay) must equal the replay state (exactly, or as a
superset when the state came from a snapshot manifest alone) else `CHECKPOINT_MISMATCH`; its
`archive_id` and head sequence must equal the commit's (`RECORD_INVALID`).

**D7. Freshness.** `--expected-head HEX` is a commit ID; D8 says the user anchor gives PASS/FAIL and
`FRESHNESS_FAILED` means "the verified history does not contain the anchor". **Chosen:** present
anywhere in the verified chain → no finding; absent → `FRESHNESS_FAILED`.

---

## E. Encrypted profile

**E1. The passphrase of the c11 archive is not stated in any file I may read.** R5 §12 says "its
golden passphrase". I tried 12 plausible strings against the archive, in this order, and the twelfth,
`golden passphrase`, opened it. The tests use it. This is a guess confirmed by success, nothing more.

**E2. DEK acceptance.** R5 §3 says the DEK is trusted only after a first sealed object opens under
it. An AEAD-authenticated unwrap already proves the passphrase; I accept the first envelope
that opens, in the commit's listed order (B.2.10 item 3), and every later sealed object that fails
to open is `CONTENT_INTEGRITY_FAILED`. A wrong passphrase raises `KEY_UNAVAILABLE` and the CLI
exits 3 without a summary (§B.2.10 item 9: never a verification result).

**E3. Keyless runs.** Only structure and stored bytes: footers, commits, descriptor, key envelope
frames, each commit's data region hash and the hashes of the manifests/images the commit lists.
`snapshots` is `null` (W17: "content-level fields are null"); retention is sealed, so which
snapshots are retained cannot be known. `findings` hold only stored-integrity and structural codes.

**E4. Sealed manifests.** Plaintext of kinds 2/3 is the manifest CBOR exactly (no frame header);
kind 1 is the binary envelope + SQLite image; kind 0 is the whole Zstandard frame. The associated
data is built as R5 §4.

**E5. Argon2.** Reader limits are checked before any derivation. Lower bounds
(`m ≥ 8p`, `t ≥ 1`, `p ≥ 1`) are checked when a passphrase is applied (R3 §7.3), so a keyless run does
not refuse such an envelope. The Argon2 RFC 9106 §5.3 vector uses a secret and associated data that
MOCHI does not; neither argon2-cffi's low-level API nor libsodium takes them, so that vector is not
reproduced (the generator's own cross-checks are, through the NFC and writer-default vectors).

---

## F. Redundancy profile

**F1. When parity is read.** The descriptor says so (constraint 1). If the descriptor cannot be
decoded, parity is attempted anyway when a parity-kind frame sits right after the head's commit
frame (R6: the scan needs no descriptor). The scan runs on a copy in memory (ladder step 6); the
archive file is never written.

**F2. Which footer starts the walk.** R6 §6 step 1: the footer at EOF if it parses, else the latest
valid footer from the forward scan; parsed, not validated. After rebuilding, ordinary validation applies.

**F3. Counters of `parity`.** The summary says `{commits, damaged_shards, rebuilt, beyond_capacity}`
without units. **Chosen:** `commits` = commits whose parity region was processed; `damaged_shards` =
damaged data shards (hash mismatch against the table) plus damaged/missing parity frames;
`rebuilt` = data shards rebuilt (and hash-checked); `beyond_capacity` = *groups* beyond capacity.
Shard counts of a commit whose frames fail the cross-checks are not added. **Risk:** high.

**F4. Findings from parity.** `PARITY_DAMAGED` for a damaged/missing frame or a group with no intact
frame; `PARITY_CAPACITY_EXCEEDED` per group over capacity; `PROFILE_VIOLATION` for frames that
verify but disagree (identity, features, range, frame count, slots, hash tables), and for a commit
with no parity frames at all. Rebuilt bytes are used for everything after, so a healed shard
produces **no** `STORED_INTEGRITY_FAILED`; damage beyond capacity stays visible in the object
checks. **Risk:** high (the reference may report the healed damage as well).

**F5. Region partition.** The region between a commit frame and its footer must hold exactly
G·m frames of one length. If some frame decodes but the length does not partition the region →
`PROFILE_VIOLATION`; if none decodes → `PARITY_DAMAGED`.

**F6. R6 vectors.** `multi_shard_commit` stores only BLAKE3 hashes of frames generated from a
`range_seed` I cannot reproduce without the generator; I check its layout and frame length, and
byte-exactness on `small_commit` (frames rebuilt by my encoder equal the vector).

---

## G. TAR-compatible profile

**G1. Which frames form a commit's stream.** The data frames between the parent's footer and the
commit frame, in physical order, decoded and concatenated (B.2.9 item 5). A commit with no puts
must have none. Members must equal the commit's puts in operation order: path bytes, type, size,
mode (low 12 bits), uid, gid, mtime; defaults 0644/0755, uid/gid 0, mtime 0 when not recorded.
A recorded mtime of exactly `(0, 0)` is indistinguishable from none (item 4). Re-emitted content must
hash to the file version's content hash. **Not checked:** that fresh puts' content chunks are
the version's own extent chunks in order (item 8), because that needs the physical placement
rule that I cannot read from the text alone.

**G2. Strictness.** Header checksum, `ustar\0` + `00`, empty user/group/prefix, zero device fields, typeflag
`0`/`5`/`x` only, zero padding, exactly two end blocks and nothing after, known pax keys only, pax
records self-consistent. The stdlib `tarfile` must read the same member sizes and types.
**Source (fixture):** `c10/reject-unknown-pax-key.tar` contains `12 foo=bar\n`, whose length field is
off by one; my parser refuses it for the length before it reaches the unknown keyword. Same code.

---

## H. Summary mapping (W17)

- `path`: hex of the stored path bytes. `kind`: `file` / `dir` (symlinks are reserved). `size`: logical
  length. `target`: always null. `content_hash`: null for directories.
- `attributes`: `posix_mode` is the permission value recorded (≤ 0o7777), `uid`, `gid` as recorded;
  `mtime_ns` = seconds · 10⁹ + nanoseconds; `windows` is the recorded attribute integer; every
  attribute that was not recorded is `null`.
- `commits[].footer_offset`: offset of the first byte of the 72-byte footer frame.
- Document formatting: sorted keys, two-space indent, trailing newline. `case` defaults to the file
  name without extension (`--case` overrides).
- `profile.recovery_percent`: descriptor constraint 1's percentage.
- Findings sorted by `(code, object)`.

## I. Observations about the artifacts themselves (not the format)

1. `R5-vectors` `commit_layout` descriptor carries draft identifier **1**; R1 §1 freezes **2**. The vector
   predates the freeze and a draft-2 reader refuses it (tested as such).
2. R7 §6 calls `PARITY_DAMAGED` a *violation* while spec B.2.11 item 12 calls it a warning with integrity unaffected.
3. R3 §1.1's table is the only place the item-limit accounting is fixed (key + value count separately).
4. R1 §7 cold-read finding 9 (codes for a record naming a wrong-kind frame) is open in R1; I use `ENVELOPE_INVALID` for
   the binary envelope (R2 §2) and `MALFORMED_FRAME` for a CBOR record in a frame of the wrong kind (the c4/c5 vectors).

## J. Not implemented (read-only verifier, scope of W17)

Salvage (ladder step 7), copies (step 5), repair, quarantine, `LIMIT_NOT_DEFAULT`/`--limit`, the KDF-cost
information finding (it would not appear in the summary anyway), search, anything Windows-specific,
declared-limit (key 6) enforcement (decoded, never acted on, D16), and reader limits beyond the
frame, CBOR depth/item and KDF limits.

---

## W18. Changes after the comparison with R8 (2026-10-13)

The sections above are the log as the cold session left it, and they stay as written. W18 ran the
verifier over `fixtures/golden/r8/` for the first time: 53 of 76 cases differed. Each difference is
in `docs/ratification/R9-interop-results.md` with its class and resolution. This list says which
entries above no longer describe the verifier, and why. Where the text was silent, the clarification went
into the specification or the ratification artifact first, and the verifier followed it.

* **A3 (tail) and A2.** The eligible tail is still `UNCOMMITTED_TAIL` (operational), but a head found by
  scanning, because the file does not end in its footer, is also a `FOOTER_INVALID` finding (R7 clarified).
* **A6 (what refuses).** A hash-verified head base image that fails R4 §3 refuses the head (D10 item 9
  clarified); the head's key envelopes out of order, repeated, or under two key IDs refuse it with
  `RECORD_INVALID` (R3 §7.1 already said "refused"; the cold session had made it a finding, against its own
  rule for commit-record violations).
* **A7 (what a refused summary contains), D1, D2.** The head's refusal code is a finding of the summary; the
  `object` of a finding is a stored-object hash only for a damaged data chunk, and a code that has such a row
  has no null row. Both are in `fixtures/golden/README.md`, "R8 summaries".
* **C3 (retained snapshots), E3 (keyless).** `snapshots` holds one entry per commit of the chain whose state
  opens; retention marks collectability, it does not remove a snapshot. An Encrypted archive read without a
  passphrase has `head.status` `keyless`.
* **D4 (dictionaries).** A chunk dependency that names an object the catalog does not hold is
  `REFERENCE_INVALID` (spec §24.2, the W10 clarification, which the cold session had missed); one that
  resolves to a catalog object stays `UNSUPPORTED_FEATURE`.
* **D5, G1 (double reports).** A damaged TAR framing chunk is reported once, as the stored-integrity failure,
  not again as a content failure (R7: `CONTENT_INTEGRITY_FAILED` means the stored bytes verified).
* **E2 (wrong passphrase).** The summary says `head` refused with `KEY_UNAVAILABLE`; the command line still
  exits 3 without `--summary`.
* **F3, F4 (parity).** `parity.commits` counts every commit the scan reaches. Findings come from the bytes as
  stored (spec B.2.11 item 12 clarified); the chain, the snapshots and the unhealed chunks come from the rebuilt
  view, so the verifier makes two passes.
* **Comparator.** `tools/compare_r8.py` read a passphrase and an anchor from sibling files that W16 does not
  write; it now reads them from `cases.json`, and a missing case, summary or directory fails.

## W20: the 1.0 flip (branch `claude/w20-1.0-flip`)

**Read for this change:** `docs/spec.md`: the status line, Annex B.2.14 (items 1 to 4), the descriptor key table
of B.2.2 (key 3), D24 in B.2, section 26 (the migration boundary paragraph) and section 28 (status 2026-10-15).
Also `fixtures/golden/r8/` (`cases.json`, `legacy/cases.json`, the summaries) and `fixtures/golden/b2/`
descriptor vectors, as data. No Rust source and no `gen_vectors.py`.

**Decisions:**

1. *Key 3 `null` is accepted; every number is refused.* B.2.2 key 3: "null in a ratified 1.0 archive ...
   A number is a pre-1.0 draft ... every other command refuses it ... with `UNSUPPORTED_FEATURE` (exit 4),
   naming the draft." The verifier is a read-only command other than `migrate`, so draft 2, draft 1, draft 3 and
   any other number all get `UNSUPPORTED_FEATURE`, with `draft identifier N` in the message. A non-integer,
   non-null key 3 stays `DESCRIPTOR_INVALID` (closed schema, `reject-descriptor-draft-type`).
2. *No `migrate` hint.* B.2.14 item 2 says only draft 2's message suggests `mochi migrate`. The verifier has no
   migrate command, and the refusal code is all agreement tests, so the message names the draft and nothing more.
3. *Ambiguity, strictest reading.* B.2.14 item 1 says "a build refuses every draft other than its own"; item 2
   says a 1.0 build refuses every draft. The 1.0 build has no draft of its own, so none is accepted.
4. *Refusal comes after the closed schema check,* as before: a malformed descriptor is `DESCRIPTOR_INVALID`
   before its draft is considered. Wire generation is still checked first (`gen != 2` is `UNSUPPORTED_FEATURE`).
5. *Legacy cases.* `r8/legacy/*.mochi` are not in `cases.json` or the summaries; the tests assert each is
   refused at the head with `UNSUPPORTED_FEATURE` naming draft 2, and that the R8 comparison (exact equality,
   D7, G14) covers every case of `r8/cases.json`.
6. The R5 layout vector's descriptor (draft identifier 1) remains refused; the test comment now says "1.0 reader".

## F1: compressed catalog images (payload encoding 2; branch `claude/f1-compressed-catalog`)

**Read for this change:** `docs/spec.md`: Annex B.2.2 (payload-encoding row, frame-kind table), B.2.3 (limits
table, "Decoded: Image"), B.2.10 item 5 (compress, then seal), B.2.15 (D25) items 1 to 9, and the status line;
`docs/ratification/R2-record-envelopes-draft.md` (the 2026-10-16 amendment, "Encoding 2", decode order);
`fixtures/golden/b2/vectors.txt` and the two new `.bin` vectors (as data); `fixtures/golden/r8/` summaries
(as data). No Rust source, no PR diff of Rust files.

**Done:** `envelope.decode_binary` registers encoding 2 (signature `28 b5 2f fd`) and lets `0x184D2A50` carry 2
or 0 and `0x184D2A5F` only 1 (B.2.2); any other value, and 2 in a parity frame, is `ENVELOPE_INVALID`.
New `envelope.decode_image(env)` runs after `bind` (B.2.15 item 3 order: stored-hash check, identity bind, then
decompress): payload is exactly one Zstandard frame (`MALFORMED_FRAME` otherwise) with
`Frame_Content_Size`, the checksum flag, no dictionary ID; the *declared* size is compared with *S* − 592 before
any output exists (`LIMIT_EXCEEDED`); the decoder is capped at declared + 1; a different size or a Zstandard
error is `CONTENT_INTEGRITY_FAILED` (item 2); then the decoded bytes must start `SQLite format 3\0` and their
`user_version` equals the envelope schema version (`ENVELOPE_INVALID`, R2). Encoding 0 returns the payload
unchanged (draft-2 and pre-decision 1.0 images, item 6; draft-2 *archives* are still refused at the
descriptor, W20). The existing catalog checks (R4 section 3, page size, application id) run on the decoded
bytes. Encrypted images unseal first, then take the same path (item 5). No new dependency: `zstandard` is
already pinned; the CI job is unchanged.

**Tests:** `tests/test_image_encoding.py` (round trip, encoding 0 still read, encoding 3, encoding 2 in a parity
frame, wrong envelope signature, decoded signature, decoded `user_version`, trailing bytes / two frames /
no content size / no checksum, declared size 2^40 refused before decoding, declared size ≠ actual);
`tests/test_golden.py` lists and decodes the two new b2 vectors.

**Gaps, all four now closed by the spec (commit 20cd681; B.2.15 items 1 to 3 and the R8 summary section of
`fixtures/golden/README.md`).** Re-read and aligned; my first choices that differed are marked.

1. *Fault codes for a non-conforming encoding-2 payload (B.2.15 item 1)* **[closed; first choice changed]**.
   `TRUNCATED`: empty or cut short inside the frame. `MALFORMED_FRAME`: not a Zstandard data-frame magic,
   bytes after the frame or a second frame, no `Frame_Content_Size`, no checksum flag. `CONTENT_INTEGRITY_FAILED`:
   non-zero dictionary ID, a decoding error (bad block, checksum mismatch), a size other than declared. Order:
   structure and window; `Frame_Content_Size` present; declared size against the bound (`LIMIT_EXCEEDED`);
   checksum flag; dictionary ID; decoding. So no checksum flag plus an over-bound size is `LIMIT_EXCEEDED`.
   *Envelope signature first (commit 4948c59, item 1):* an encoding-2 payload without the four magic bytes
   `28 B5 2F FD` is `ENVELOPE_INVALID` at envelope level, including an empty payload and one shorter than four
   bytes; `TRUNCATED` is only for a payload that has the magic and is cut short inside the frame. My earlier
   reading (letting empty and short payloads reach the frame walk) is removed.
2. *Window limit (item 2)* **[closed]**. `max-window-size` defaults to 134,217,728; the structural walk refuses a
   frame header declaring a larger window with `LIMIT_EXCEEDED` before the decoder exists (a single-segment
   frame's window is its content size, so the window is compared first), and the decoder is configured with the
   same maximum. *Not done:* the same limit for data objects ("exactly as for data objects"); this verifier's
   chunk path never applied it and no task asked for it, so chunks are unchanged.
3. *Order of checks (item 3)* **[closed]**. Stored hash; the envelope's intrinsic checks in R2's order (feature
   count, header and payload length, budget, encoding, features strictly increasing, each known, schema,
   signature); bind; the profile features check (`[1]` exactly in an Encrypted archive, else none;
   `RECORD_INVALID`); decompress; decoded-byte checks. This was already the code's order; tests now pin it.
4. *`parity` with a refused head and with no usable descriptor (R8 summary section)* **[closed]**. Parity does
   not depend on the head. It is null when the descriptor at offset 0 does not decode or declares no percentage
   (even if parity frames follow), when no footer is found, or when the scan fails. *First choice changed:* the
   verifier used to scan on parity frames after the head's commit when the descriptor was unreadable; that
   fallback is removed.

**Tests added for the closure:** every code and ordering row above in `tests/test_image_encoding.py` (empty, 1 to 3 byte prefixes, cuts at 4, 5, 10 and len-1 bytes, second frame and trailing byte, no content size, no checksum flag, non-zero dictionary ID,
corrupt checksum, size mismatch, over-bound before checksum flag, window above 128 MiB, features before decode,
bind before decode), and `test_parity_is_null_when_the_descriptor_does_not_decode` in `tests/test_golden.py`.
