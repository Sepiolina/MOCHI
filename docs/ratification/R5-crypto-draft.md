# R5 — Cryptographic profile (FROZEN 2026-10-11)

> **Status 2026-10-11: frozen** (owner-approved). Implemented (plan C11, `docs/c11-encrypted.md`); the vectors below are checked in `mochi-format`'s tests and the golden set `fixtures/golden/c11/` adds byte-exact envelope and sealed-object vectors and sixteen rejects. Gate G10's evidence is complete, and the review pass the gate asks for ran on 2026-10-11; §12 records what it checked, what it found, and the call on each open item, and the owner approved the freeze. The file keeps its name so that existing links hold.

Ratification artifact R5 for the Encrypted profile. Written with the design (spec
Annex B.2.10, D20) before the code, because a crypto layout mistake is permanent once
archives exist, and frozen after the implementation passed gate G10 and the review.
Where this file and the spec text of B.2.10 disagree, that is a defect in one of
them: raise it, do not pick.

**What frozen means.** The byte layouts, identifiers, and algorithms of §1 to §4 and
§6 are fixed for suite 1, key-envelope schema 0, sealed-object version 0, commit
record schema 2 keys 11 and 12, and recovery-manifest schema 3 key 13 and the
provenance `reason`. A change to any of them is a **new** identifier (suite 2,
envelope schema 1, sealed version 1, a new required feature) with its own vectors,
never an edit of this file. The reader limits of §1 are reader policy, not wire
format, and may change through `--limit` or a later default. The rest of commit
schema 2 and manifest schema 3 is restated from R3's schemas and freezes with R3.
The format as a whole stays "experimental / draft-compatible" until 1.0 (spec §28):
freezing R5 means no draft-era change to this topic, not that 1.0 has shipped.

Sources: `docs/spec.md` §7.3, §14, Annex B D3, D10, D11, D12, **B.2.10 (D20)**;
`docs/schemas/key-envelope-v0.cddl`, `commit-record-v2.cddl`,
`recovery-manifest-v3.cddl`. Vectors: `docs/ratification/R5-vectors/` (`gen_vectors.py`
makes `vectors.json`; both are in the repository, and the output is byte-for-byte
reproducible).

## 1. Algorithms and parameters

| Item | Value |
|---|---|
| AEAD | XChaCha20-Poly1305, draft-irtf-cfrg-xchacha: HChaCha20 over the first 16 nonce bytes gives the subkey; ChaCha20-Poly1305 (RFC 8439) runs with the nonce `00 00 00 00 ‖ last 8 nonce bytes` |
| Key / nonce / tag | 32 bytes / 24 bytes, fresh random per encryption / 16 bytes |
| Suite identifier | 1 (the single suite of required feature 1) |
| Required-feature identifier | 1 |
| KDF | Argon2id, RFC 9106, version 0x13 (19), 32-byte output, no secret, no associated data, a 16-byte random salt per envelope |
| Writer KDF defaults | m = 65,536 KiB, t = 3, p = 4 (threads = lanes) |
| Reader KDF limits | m ≤ 1,048,576 KiB, t ≤ 16, p ≤ 16, else `LIMIT_EXCEEDED`; m ≥ 8·p, t ≥ 1, p ≥ 1, else `RECORD_INVALID` |
| Randomness | The OS CSPRNG for the DEK, key ID, envelope ID, salts, and every nonce. Nothing is derived from content, position, or a counter |
| Passphrase | UTF-8 of the NFC-normalized text; 1 to 4,096 bytes after normalization |
| Hash | BLAKE3-256; stored-object scope for stored hashes (unchanged) |

## 2. Domain strings

Defined once, in `mochi-format/src/digest.rs` (new in the implementation). They are
AEAD associated-data prefixes, not BLAKE3 scopes; the implementation's
"separators are distinct and none is a prefix of another" test includes them.

| Name | Bytes | Length | Hex |
|---|---|---|---|
| `KEY_WRAP_DOMAIN` | `MOCHI2-KEY-WRAP` ‖ 0x00 | 16 | `4d4f434849322d4b45592d5752415000` |
| `OBJECT_SEAL_DOMAIN` | `MOCHI2-OBJECT-SEAL` ‖ 0x00 | 19 | `4d4f434849322d4f424a4543542d5345414c00` |

## 3. Key envelope

Frame `0x184D2A5C`, payload = deterministic CBOR of `key-envelope-v0.cddl`
(keys 0–10: schema 0; archive ID; sequence and transaction ID of the writing commit;
required features `[1]`; envelope ID; key ID; suite 1; the KDF map; wrap nonce; wrapped
DEK with tag).

**Wrap.** `KEK = Argon2id(NFC(passphrase), salt, m, t, p)`;
`wrapped = XChaCha20-Poly1305-seal(KEK, wrap_nonce, DEK, wrap_aad)` (48 bytes).

**Wrap associated data** (fixed width, last item variable):

| Offset | Size | Field |
|---|---|---|
| 0 | 16 | `KEY_WRAP_DOMAIN` |
| 16 | 32 | archive ID |
| 48 | 16 | envelope ID |
| 64 | 16 | key ID |
| 80 | 2 | suite, u16 little-endian |
| 82 | to end | the exact bytes of the KDF map (key 8), as it is encoded in the payload |

**Unwrap (reader).** After the frame's stored hash verified (commit key 11): decode
the CBOR under the closed schema and re-encode check; check the identity (archive ID
equals the commit's; sequence not after the commit's); check the KDF values against the
reader limits **before allocating**; derive the KEK; open. A failed open is
`KEY_UNAVAILABLE`, never a damage finding, because the stored hash already ruled out
corruption of these bytes. The DEK is trusted only after a first sealed object opens
under it.

## 4. Sealed object

Frame `0x184D2A59`. Payload (little-endian, every byte defined):

| Offset | Size | Field | Rule |
|---|---|---|---|
| 0 | 2 | version | 0 |
| 2 | 2 | suite | 1 |
| 4 | 4 | kind | 0 data chunk, 1 catalog image, 2 delta manifest, 3 snapshot manifest |
| 8 | 16 | key ID | equals the envelopes' key ID |
| 24 | 24 | nonce | fresh per encryption |
| 48 | n | ciphertext | n ≥ 1 |
| 48 + n | 16 | tag | |

**Object associated data:**

| Offset | Size | Field |
|---|---|---|
| 0 | 19 | `OBJECT_SEAL_DOMAIN` |
| 19 | 32 | archive ID |
| 51 | 24 | payload bytes 0–23 (version, suite, kind, key ID) |
| 75 | 32, or 8 + 16 | binding: kind 0 → the 32-byte object ID; kinds 1–3 → commit sequence (u64) ‖ transaction ID (16) |

**Plaintext.** Kind 0: the whole Zstandard frame (one frame, `Frame_Content_Size`
and checksum present, O21). Kind 1: the binary envelope v0 header and the SQLite
image, exactly as in a Core `0x184D2A50` payload. Kinds 2–3: the canonical CBOR of the
manifest (schema 3), exactly as in a Core `0x184D2A58` payload.

**Open (reader).** The stored hash of the whole frame is verified first (by the
referencing record, or, for chunks, by the manifest or the data region). Then: parse
the 48-byte header (unknown version, suite, or kind → `UNSUPPORTED_FEATURE`; key ID
mismatch → `RECORD_INVALID`); build the associated data; open. A tag failure after a
verified hash is `CONTENT_INTEGRITY_FAILED`. Only then is the plaintext handed to the
decoder that Core uses, with its D11 obligations unchanged (features `[1]`, identity).

## 5. Where the key is needed

| Action | Needs the key? |
|---|---|
| Find the head, validate footers and commit records, walk frames | No |
| Hash the descriptor, key envelopes, manifests, images, and each commit's data region | No (`stored_integrity` level) |
| Read file names, sizes, content, chunk hashes; replay manifests; restore; compact; gc; repair; `rekey` (except `--list`) | Yes |
| `verify` content levels, Recoverability, Key availability = `PASS` | Yes |

**Which passphrases reach the key.** Every passphrase whose envelope is anywhere in
the file reaches the data key, including one removed by a rewrap: its envelope frame
stays in the file, and a rewrap keeps the data key. That key opens every sealed object
under the key ID, **including those written after the removal**. The implementation
refuses a removed passphrase at the head (`open_head` is strict), but that is the
tool's policy, not a cryptographic barrier: another reader, or this library through a
historical commit, gets the same key. Only a rewrite (`rekey --reencrypt`, `compact`,
`gc apply`, `repair apply`) seals content under a new data key, in a new file, that a
passphrase not given again cannot reach. Copies of the old file keep the old key.
`c11_rekey.rs` asserts this
(`a_removed_passphrase_still_reads_content_appended_after_the_removal`).

## 6. Nonce discipline

Every encryption (each sealed object, each key wrap) draws 24 fresh bytes from the OS
CSPRNG. A retry after an interrupted write draws again; a crash and reopen never
reuses a counter because there is none; restoring a backup cannot repeat a nonce under
a key because nonces are not state. For *q* encryptions under one key, the chance that
any two nonces coincide is at most *q*² / 2¹⁹³: for *q* = 2⁴⁰ that is below 2⁻¹¹³. The
DEK is per archive, and every rewrite creates a new one, so *q* is one archive's number
of sealed objects. A KEK is used once per envelope (each envelope has its own salt).
The nonce-uniqueness property test of gate G10 checks the implementation's draws, not
this bound.

## 7. Errors

| Condition | Code | Exit |
|---|---|---|
| No passphrase supplied for a command that needs the key; no envelope opens | `KEY_UNAVAILABLE` (new) | 3 |
| Tag failure on a sealed object whose stored hash verified | `CONTENT_INTEGRITY_FAILED` | 1 |
| Stored hash mismatch (envelope, sealed object, data region) | `STORED_INTEGRITY_FAILED` | 1 |
| KDF m, t, or p over a reader limit | `LIMIT_EXCEEDED` | 3 |
| Malformed envelope; m < 8·p; wrong salt length; key ID mismatch; features or schema inconsistent between records | `RECORD_INVALID` | 3 |
| Unknown suite, KDF, Argon2 version, sealed version or kind, required feature | `UNSUPPORTED_FEATURE` | 4 |
| In-place conversion to or from Encrypted | `PROFILE_CHANGE_UNSUPPORTED` | 4 |
| Empty or over-long passphrase; Encrypted with `--tar-compatible` | `INVALID_ARGUMENT` | 3 |
| Descriptor lists feature 1 and `tar_compatible` | `DESCRIPTOR_INVALID` | 1 |

(`RECORD_INVALID` exits 3 in `exit_code_for` as of this draft; the exit-code table is
R7's.)

## 8. Visible without the key

Archive ID; wire generation and draft ID; number of commits, their sequences,
parent links, and footer offsets; the count, kind, and stored size of every object
(hence the number of chunks and their compressed sizes, and roughly how many files a
commit changed); the data region of each commit; envelope IDs, the key ID, KDF
parameters, and salts. **Not visible:** names, paths, attributes, structure, file
sizes, content hashes, the dedup relation, content. No commit times are recorded.
Compressed chunk lengths are visible (compress-then-encrypt). The dedup-equality leak
is in B.2.10 item 8.

## 9. Vectors

`R5-vectors/vectors.json`, generated by `R5-vectors/gen_vectors.py` from fixed inputs.
Each vector was produced by a library independent of the Rust crates the
implementation uses, and the generator asserts the agreements below before writing:

| Vector | Checks | Independent oracle |
|---|---|---|
| `primitives.xchacha20poly1305_draft_irtf_cfrg_xchacha_a31` | The AEAD, from the CFRG draft Appendix A.3.1 | libsodium (PyNaCl), and OpenSSL ChaCha20-Poly1305 fed with the generator's own HChaCha20 |
| `primitives.hchacha20_…_221` | HChaCha20, draft §2.2.1 | the generator's implementation, equal to the published value |
| `primitives.argon2id_rfc9106_53` | Argon2id, RFC 9106 §5.3 (uses secret and associated data, which MOCHI does not) | the Argon2 reference C library |
| `primitives.kdf_nfc` | NFC and NFD of the same passphrase give different raw inputs; the rule fixes the NFC one | Argon2 reference library |
| `primitives.kdf_writer_defaults` | The default parameters on a fixed passphrase and salt | Argon2 reference library |
| `key_envelope_small_params`, `key_envelope_writer_defaults` | KEK, wrap AAD, wrapped DEK, the canonical CBOR payload, the frame, its stored hash | libsodium, Argon2 reference library, `cbor2` canonical mode, `blake3` |
| `sealed_data_chunk` | A kind-0 object (plaintext is a real Zstandard frame), AAD with the object ID, the frame, its stored hash; plus a **reject** case (another object ID fails) | libsodium, `python-zstandard`, `blake3` |
| `sealed_manifest` | A kind-2 object bound to sequence 7 and a transaction ID | libsodium, `blake3` |
| `commit_layout` | A commit 0 of an Encrypted archive: descriptor, key envelope, two sealed chunks (the data region), sealed delta manifest (schema 3 plaintext with key 13), two placeholder sealed objects, and the commit record v2 with its ID | the generator's deterministic CBOR (checked against `cbor2`), `blake3` |

The vector set is self-contained: it stores the Zstandard frame and every hex string, so
a reader of the vectors does not need to compress anything. Regenerate with
`pip install pynacl cryptography argon2-cffi blake3 zstandard cbor2` and
`python3 docs/ratification/R5-vectors/gen_vectors.py > docs/ratification/R5-vectors/vectors.json`.
A change to `vectors.json` is a deliberate format change with an explained diff
(AGENTS.md). **Not covered by vectors:** a full archive with a footer (the
implementation's golden archive covers that), and any hostile-input case beyond the
one reject vector (the implementation's reject fixtures and fuzz targets cover those).

## 10. Implementation constraints

* Crates (MIT or Apache-2.0): `chacha20poly1305` (the `XChaCha20Poly1305` type),
  `argon2`, `zeroize`, `unicode-normalization`, and `rpassword` in the CLI. Versions
  are pinned by `Cargo.lock`. The versions the vectors and the G10 evidence ran with
  (recorded at the freeze, 2026-10-11):

  | Crate | Version | Role |
  |---|---|---|
  | `chacha20poly1305` | 0.11.0 (`chacha20` 0.10.2, `poly1305` 0.9.1) | AEAD |
  | `argon2` | 0.6.0 | KDF |
  | `unicode-normalization` | 0.1.25, **Unicode 17.0.0** | NFC of passphrases (open item d) |
  | `zeroize` | 1.9.1 | wiping secrets |
  | `getrandom` | 0.3.4 | the OS CSPRNG |
  | `blake3` | 1.8.7 | stored-object hashes |
  | `rpassword` | 7.5.4 | prompt without echo (CLI only) |

  An update of any of these needs the R5 vectors to pass unchanged. An update of
  `unicode-normalization` that changes its Unicode version also updates this table.
* `#![forbid(unsafe_code)]` stays in `mochi-format` and `mochi-core`.
* Secret types (passphrase, KEK, DEK) have no `Display`, `Clone`, byte accessor
  outside `mochi-format`, or serialization; they zeroize on drop. Their `Debug` prints
  a fixed placeholder (`DataKey(redacted)`) and never the bytes, so a struct holding
  one can still derive `Debug` (accepted at the freeze; the draft said "no `Debug`").
  `compile_fail` doctests guard the missing `Clone` and byte accessor; a unit test
  checks the placeholder. The test log is searched for the test passphrase and key bytes.
* Randomness: one function in `mochi-core`, used for every draw above; a test double
  exists only behind `test-controls` and cannot be built into the shipped binary.
* The sealing pipeline is stage 2 of the existing object codec (`Protection::Aead`,
  today `Unsupported`); the representation types of AGENTS.md (decoded, encoded
  plaintext, stored payload, stored object) stay distinct, and no conversion that erases
  the distinction is added.

## 11. Open items

The list in B.2.10 ("Open items recorded here", a to h) is this file's list. In
addition: (i) the Argon2 thread count equals the lane count in the vectors; a reader
on a machine with fewer cores gets the same bytes, only slower (lanes, not threads,
determine the result). (j) The key-envelope identity fields (sequence and transaction
ID of the writing commit) are informational beyond the archive-ID check; a later
version may bind them.

Each item was decided at the freeze; §12 gives the calls.

## 12. Freeze review (2026-10-11)

The review pass gate G10 asks for. It read this file, spec B.2.10, the three schemas,
`docs/c11-encrypted.md`, and the code in `mochi-format` (`seal.rs`, `kdf.rs`,
`secret.rs`, `digest.rs`) and `mochi-core` (`keys.rs`, `rekey.rs`). The owner
approved the freeze and the calls below.

**Checked.**

* **The vectors reproduce.** `gen_vectors.py` run in a clean environment (PyNaCl 1.6.2,
  argon2-cffi 25.1.0, cryptography 50.0.2, blake3 1.0.11, zstandard 0.25.0, cbor2
  6.1.5) gives a `vectors.json` byte-identical to the committed one, and every oracle
  agreement it asserts holds.
* **The code is the layout.** The domain strings, the wrap and object associated data
  (field order, widths, little-endian integers), the 48-byte sealed header, the
  header-before-open order (version, suite, kind, key ID, then the AEAD), the KDF
  checks before allocation, and the error mapping match §2 to §4 and §7.
* **The construction.** XChaCha20-Poly1305 with 192-bit random nonces under a key that
  is never shared between archives; Argon2id at RFC 9106's second recommended option;
  the stored hash checked before any header is parsed; a tag failure after a verified
  hash reported as content, not stored, damage; a wrong passphrase operational, not
  damage. The associated data binds archive, kind, key ID, and object or commit, so a
  sealed frame cannot be moved, re-kinded, or replayed across commits without the
  open failing. No objection.
* **Gate G10's evidence**, item by item: present, with the gap below now closed.

**Found and fixed at the freeze** (immerh8/MOCHI#21; no stored byte changed).

1. **Removal was worded too weakly** (B.2.10 item 10, the CLI's `rekey` output and
   help, `docs/c11-encrypted.md`). They said a copy made *before* the removal still
   opens. In fact the removed passphrase still reaches the data key from the *same*
   file, and the key opens content appended *after* the removal (§5). The wording now
   says so and points to `--reencrypt`; a test asserts it.
2. **The nonce property test had no crash.** G10 names "retries, crash and reopen";
   the test covered reopen only. It now has a step that tears a commit before its
   footer, truncates the tail on reopen, and writes the same content again, and it
   checks the lost attempt's nonces too.
3. **§10 named no versions** although it said the implementation would record them.
   The table in §10 records them, with the Unicode version.
4. **§10 said "no `Debug`"** for secrets; the code prints a fixed placeholder. The text
   now describes the code, which is the better choice.
5. **Stale status text** ("Nothing here is implemented") in spec B.2.10 and §7.3, in
   `docs/ratification/README.md`, and in the schema headers is updated.

**Calls on the open items** (B.2.10 a to h, this file i and j). None changes a byte.

| Item | Call | Why |
|---|---|---|
| (a) data region, commit key 12 | **Keep.** | Without it, keyless verification would cover metadata only, and "stored integrity needs no key" would be false for most of the bytes. Its cost is one hash per commit and a contiguity rule the writer already follows. |
| (b) the archive ID in the associated data | **Keep.** | A rewrite needing the passphrase is the right cost: it is also what gives every rewrite a new data key, the only way a removed passphrase is actually cut off (§5). Binding the key ID alone would let sealed frames move between archives. |
| (c) no minimum KDF cost on readers | **Keep: no reader floor.** The finding B.2.10 item 14 promises is defined now and is not wire format: `verify` (keyless or keyed) reports, for each envelope of the head whose `m` < 65,536 KiB or `t` < 3 (below the writer defaults), one informational item naming the declared and the default values. It changes no dimension's status and no exit code, and says "below the MOCHI writer default", never "weak" or "insecure". | A floor would refuse archives other writers made legitimately, and the writer defaults are the only threshold this project can defend without inventing one. Implementing the item is a C11 follow-up (plan), not a freeze blocker: no stored byte depends on it. |
| (d) NFC and the Unicode version | **Keep NFC; the version is recorded** (§10, Unicode 17.0.0). | Unicode's normalization stability policy fixes NFC for every assigned code point, so a later Unicode version can change only passphrases that use code points unassigned in 17.0.0, which pass through unchanged today. |
| (e) no key retirement in 1.0 | **Keep.** | Removal is a rewrap and re-encryption is a rewrite; a retirement that keeps retained snapshots under another key needs per-snapshot keys, a post-1.0 design. |
| (f) every envelope damaged | **Keep.** | Several passphrases mean several envelope frames, and an older commit's envelopes still yield the key (§5). The Redundancy profile (C12) covers the rest. |
| (g) the v2 manifest schema lacks key 12 | **Keep for R3**, as written. | It is R3's file; the v3 schema draws the key. |
| (h) no profile conversion by rewrite | **Keep.** | D12; `--reencrypt` stays Encrypted to Encrypted. |
| (i) threads = lanes | **Accepted as written.** | Lanes, not threads, determine the output. |
| (j) envelope sequence and transaction ID unbound | **Keep.** | An envelope can only move within its own archive, where every envelope wraps the same key; binding them gains nothing a later envelope schema could not add. |

**Implementation decisions** (`docs/c11-encrypted.md`, 1 to 6): all six accepted, with
decision 1 read as §5 says (the strict head open is policy).
