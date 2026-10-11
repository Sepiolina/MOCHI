# R5 — Cryptographic profile (DRAFT, not frozen)

> **Status 2026-10-09.** Implemented (plan C11, `docs/c11-encrypted.md`); the vectors below are checked in `mochi-format`'s tests and the golden set `fixtures/golden/c11/` adds byte-exact envelope and sealed-object vectors and sixteen rejects. Still a draft until gate G10: the owner's read of this file has not happened.

Ratification artifact R5 for the Encrypted profile. **Draft for review**: written
with the design (spec Annex B.2.10, D20) before the code, because a crypto layout
mistake is permanent once archives exist. It becomes frozen only after the
implementation (plan C11) passes gate G10 and the owner (or a dedicated review pass)
has read it. Where this file and the spec text of B.2.10 disagree, that is a defect
in one of them: raise it, do not pick.

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
  are pinned by `Cargo.lock`, and the implementation PR records each crate's version
  and the Unicode version in this file's table before it merges.
* `#![forbid(unsafe_code)]` stays in `mochi-format` and `mochi-core`.
* Secret types (passphrase, KEK, DEK) have no `Debug`, `Display`, `Clone`-by-default, or
  serialization; they zeroize on drop. `compile_fail` doctests guard the missing impls.
  The test log is searched for the test passphrase and key bytes.
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
