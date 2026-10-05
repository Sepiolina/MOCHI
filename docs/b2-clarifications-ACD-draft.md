# Draft: Annex B.2 wording for clarifications A, C, D

**Status: APPLIED to `docs/spec.md` on 2026-10-04 [delegated]**, as drafted, after an adversarial self-review (every claim checked against the code; see the decision log). Kept as the record of the wording's rationale.
The implementation decisions are settled (checklist Q16, Q18, Q19); this
document proposes only their normative wording. Approving a draft below
also ratifies the error code it names, which is today provisional.

Convention, following the 2026-10-03 D10.6 amendment: the spec states the
outcome normatively and names the reference error-registry code in
parentheses. The spec defines only the new B.2 codes; the others live in
`crates/mochi-core/src/error.rs`.

---

## A. One descriptor per replay segment (Q16)

**Amends:** D10.6 (adds a sentence after the base rule). Cross-reference
from D12.

**Current text:** D12 says "Every commit references it by hash" and lists
"missing, damaged, or mismatched descriptor" outcomes. Nothing says *when*
a reader checks that commits agree, or what a disagreement inside a replay
segment means.

**Proposed text (D10.6, new paragraph after the base rule):**

> **One descriptor per segment.** Every commit in a replay segment, from
> the base *b* through the head *h*, MUST reference the same descriptor:
> identical offset, stored length, and stored-object hash (commit key 10).
> A reader checks this while validating the segment, before applying any
> delta manifest. A difference is a mismatched descriptor (D12): the head
> is not interpreted, no earlier state is returned, and the open fails as
> an invalid descriptor (`DESCRIPTOR_INVALID`).

**Proposed text (D12, last bullet "Placement", append):**

> Because there is exactly one descriptor, at offset 0, two commits of one
> archive that reference different descriptors cannot both be valid; D10.6
> checks this for every commit a replay depends on.

**Notes for review.**
* Implemented: `segment::check_segment`; tests
  `t11_descriptor_differing_within_the_segment_is_descriptor_invalid`,
  golden `reject-archive-segment-descriptor-differs.mochi`.
* The rule is checked over the segment only, not over the whole history:
  D10.6 forbids reading outside the segment for an open. Whole-history
  agreement is a `verify` concern (not drafted here).
* **Ratifies** `DESCRIPTOR_INVALID` for this case (provisional today).

---

## C. Unknown operation kind under a supported schema (Q19)

**Amends:** D10.4, the bullet "An unknown operation kind or required
feature causes refusal."

**Why:** "refusal" reads like the capability refusal of §26
(`UNSUPPORTED`), which would invite readers to treat an unknown operation
kind as a feature they lack. Under D10.3, new operation kinds arrive only
with a new manifest schema version, and an unknown schema version is
already refused (D11). So under a schema version the reader supports, an
operation kind that version does not define is a malformed record.

**Proposed text (replaces the bullet):**

> - An unknown required feature (manifest key 9) causes refusal as an
>   unsupported feature (`UNSUPPORTED_FEATURE`; §26).
> - Under a supported manifest schema version, an operation kind that the
>   version does not define is a schema violation, not an unsupported
>   capability: the manifest is invalid (`RECORD_INVALID`). New operation
>   kinds are introduced only by a new manifest schema version (item 3),
>   and an unknown schema version is refused (D11).
> - In both cases the commit whose manifest it is, and every snapshot whose
>   replay segment contains that commit, cannot be opened; no earlier state
>   is returned (item 9).

**Notes for review.**
* Implemented: manifest decoder (`manifest.rs`, `decode_op`) and
  `check_required_features`; tests
  `t12_unknown_operation_kind_mid_segment_is_record_invalid`,
  `t12_unknown_required_feature_mid_segment_is_unsupported`, golden
  `reject-archive-unknown-feature.mochi`.
* **Ratifies** `RECORD_INVALID` for an unknown operation kind.

---

## D. Introduction versus reference during replay (Q18)

**Amends:** D10.4, the bullet "An object ID or file-version ID introduced
twice is corruption (O19)." Relationship to §10.2 stated explicitly.

**Why:** §10.2 makes only *conflicting* records corruption ("conflicting
immutable IDs with different record content"). D10.4 is stricter: during
replay, a second introduction is corruption even when the records are
identical. Without a statement of which rule governs replay, and of what
"introduced" means, a reader could accept an identical reintroduction
under §10.2, or reject legitimate reuse by reference.

**Proposed text (replaces the bullet):**

> - **Introduction.** A delta manifest *introduces* the object IDs it
>   lists as chunks and the file-version IDs it lists as file versions. An
>   introduced ID MUST NOT already exist in the replay state (introduced by
>   the base checkpoint or by an earlier delta of the segment), and MUST
>   NOT be introduced twice within one manifest. This holds even when the
>   two records are identical: during replay, a second introduction is
>   corruption (O19), and the manifest is invalid (`RECORD_INVALID`).
>   For replay this rule governs; it is stricter than, and does not relax,
>   §10.2's rule for conflicting immutable IDs.
> - **Reference.** Naming an existing ID without listing it (a `PUT` of an
>   existing file-version ID, or an extent of an existing chunk) is reuse
>   by reference, not introduction, and is valid. A `PUT` that names a
>   file-version ID existing neither in the replay state nor among the
>   manifest's own file versions is an invalid namespace operation
>   (`NAMESPACE_INVALID`); validity is judged against the state after the
>   manifest, so a reference to a version the same manifest introduces is
>   valid.

**Notes for review.**
* Implemented: `SegmentApplier::check_introductions` (introduction);
  the Q26 reference check in `SegmentApplier::apply` (reference). Tests:
  `reintroducing_an_id_is_record_invalid_identical_or_not`,
  `reuse_by_reference_is_not_reintroduction`,
  `t12_reintroduced_version_is_record_invalid`,
  `a_put_of_a_version_introduced_by_the_same_delta_is_valid`,
  `q26_put_of_an_unknown_version_is_namespace_invalid`; golden
  `reject-archive-duplicate-version.mochi`.
* The **Reference** bullet includes Q26 (decided 2026-10-04). Strike its
  last sentence if Q26 should stay out of the normative text for now.
* **Ratifies** `RECORD_INVALID` (reintroduction) and `NAMESPACE_INVALID`
  (unknown reference).

---

## Proposed decision-log updates (apply only on approval)

In `docs/b2-implementation-checklist.md`:

* Q16: append "**Spec text approved <date>** (D10.6 'One descriptor per
  segment'; D12 Placement note). Code ratified: `DESCRIPTOR_INVALID`."
* Q18: append "**Spec text approved <date>** (D10.4 'Introduction' and
  'Reference'). Codes ratified: `RECORD_INVALID`, `NAMESPACE_INVALID`."
* Q19: append "**Spec text approved <date>** (D10.4 unknown feature /
  operation kind). Code ratified: `RECORD_INVALID`."
* "For spec clarification": mark A, C, D **Resolved (spec text written)**,
  as B is; remove them from the G2 blocker line.
* Annex B.2.5 ("Specification text amended by this batch"): add D10.4 and
  D10.6 (A) entries alongside §8.3 and §12.2.
