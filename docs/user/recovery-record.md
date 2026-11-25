# Recovery record (Redundancy profile)

Sources: spec §7.4, §15.1, §15.2, §22 step 6; `docs/ratification/R6-erasure-coding-draft.md`
(frozen 2026-10-12); `docs/cli.md`; `docs/dev/performance.md#parity`.

A recovery record is **extra bytes that can repair bounded damage to the same file**. It
is a repair aid. It is not a second copy, and an archive that carries one is not
protected against losing the file. Parity repairs bounded damage; it does not replace
independent copies (spec §1.1).

```bash
mochi create --recovery-record 10 rr.mochi big.bin   # PERCENT: 1 to 100
```

Fixed at creation; every later commit in that archive writes its own parity at the same
percentage.

## What it is

Every commit writes Reed-Solomon parity (Cauchy matrix over GF(2^8), suite 1) over the
stored bytes it adds, between its commit record and its footer. The file grows by about
the percentage you chose, plus a per-frame overhead that is proportionally larger for small
commits (R6 §9 item 3). In the example above, 2,000,000 bytes of input gave a 2,313,712-byte
file at 10 %, which includes the data object, metadata, and parity.

## What it can repair: the stated capacity

Parity is computed over groups of shards. In each group, **up to *m* damaged shards can be
rebuilt** (R6 §7); the number *m* is about the percentage of the group's size. A
contiguous burst of damage is spread across groups. It is never promised beyond that:

* If a group has more damage than intact parity, `PARITY_CAPACITY_EXCEEDED` is reported,
  nothing in that group is changed, and `verify` fails.
* A rebuilt shard is used only if it hashes to the value recorded for it; every
  other check still runs against the commit chain's hashes. Parity is never the proof that
  data is right.

What parity does and does not cover (R6 §7):

| Covered | Not covered |
|---|---|
| Data, catalog images, manifests, key envelopes, commit records of a commit, the descriptor (by commit 0), each earlier footer (by the next commit) | The **newest footer** (the head's own), and the parity frames themselves |

Truncation removes the newest parity first (R6 §9 item 2), so damage at the end of the
file is the weakest part.

## What you see

Damage inside capacity, example run on a 10 % archive with four bytes overwritten:

```
integrity             FAIL         required
recoverability        DEGRADED     required
result: FAIL (exit 1)
```

The stored bytes really are damaged, so integrity fails; recoverability is `DEGRADED`
because parity can rebuild them. `verify` rebuilds only in memory and never edits the
file. To get a repaired archive, use the reviewed two-step repair:

```bash
mochi repair plan rr.mochi --output rr.plan   # "step 6 parity: used (… damaged shards: 1, rebuilt: 1 …)"
mochi repair apply rr.mochi --plan rr.plan --output rr.fixed
mochi verify rr.fixed                         # PASS
```

`rr.mochi` is unchanged. The new archive keeps the recovery record. If a parity frame itself
is damaged, `verify` reports `PARITY_DAMAGED`, integrity is unaffected, and the result is
degraded (exit 2) (R6 §8).

Use it together with real copies, not instead of them: independent copies can also
supply bytes to `repair` (`--copy PATH`), and parity runs first.

## With other options

Encrypted archives may carry a recovery record. Parity is over sealed bytes and needs no
key to check, but `repair` of an encrypted archive needs the passphrase
([encrypted.md](encrypted.md)). TAR-compatible archives can carry one too.
