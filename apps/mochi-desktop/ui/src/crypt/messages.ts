// What the app says about passphrases, encryption, the recovery record, and
// search coverage (spec §23.3; AGENTS.md "Encrypted profile" and "Redundancy
// profile"). Nothing here may say an archive is "safe", "secure", "backed
// up", or "preserved", or that a passphrase was "revoked": removing one is
// not revocation, and a recovery record is a repair aid, not a second copy.
import { formatNumber, t, tn } from "../i18n";
import type { RekeyReport, SearchCoverageView } from "../ipc/crypt";

// These are fixed for the one shipped locale; a second locale would turn them
// into functions (the screens only import the names).

/** Shown whenever a passphrase is being chosen. */
export const lostPassphrase = () => t("crypt.lostPassphrase");

/** Shown whenever encryption is chosen: deduplication is always on. */
export const dedupLeak = () => t("crypt.dedupLeak");

export const hiddenWithKey = () => t("crypt.hiddenWithKey");

/** What stays visible to anyone holding the file (docs/dev/encrypted-profile.md). */
export const visibleWithoutKey = () => [
  t("crypt.visible1"),
  t("crypt.visible2"),
  t("crypt.visible3"),
  t("crypt.visible4"),
];

export const notSigned = () => t("crypt.notSigned");

export const noGenericTools = () => t("crypt.noGenericTools");

/** The wrong-passphrase answer: an operational error, not a finding. */
export const wrongPassphrase = () => t("crypt.wrongPassphrase");
export const wrongPassphraseNote = () => t("crypt.wrongPassphraseNote");

export const removeNotRevocation = () => t("crypt.removeNotRevocation");

export const reencryptNote = () => t("crypt.reencryptNote");

export const rekeyNeedsUnlock = () => t("crypt.rekeyNeedsUnlock");

/** Recovery-record choices (percent of each commit's bytes). */
export const RECOVERY_CHOICES = [5, 10, 20, 50] as const;

export const recoveryNotes = () => [
  t("crypt.recovery1"),
  t("crypt.recovery2"),
  t("crypt.recovery3"),
  t("crypt.recovery4"),
  t("crypt.recovery5"),
];

export function recoveryLabel(percent: number): string {
  return t("crypt.recoveryLabel", { percent });
}

/** The result of a rekey job as lines; the first is the headline. */
export function rekeyLines(r: RekeyReport): string[] {
  const lines: string[] = [];
  const committed = {
    seq: r.seq ?? t("crypt.rekeyUnknownSeq"),
    status: r.status ?? t("crypt.rekeyDefaultStatus"),
    file: r.file_name,
  };
  switch (r.action) {
    case "added":
      lines.push(t("crypt.rekeyAdded", committed));
      break;
    case "removed":
      lines.push(t("crypt.rekeyRemoved", committed), removeNotRevocation());
      break;
    case "reencrypted":
      lines.push(
        tn(r.snapshots, "crypt.rekeyReencryptedOne", "crypt.rekeyReencryptedMany", {
          file: r.file_name,
          n: r.snapshots,
        }),
        reencryptNote(),
      );
      break;
  }
  if (r.durability_note) {
    lines.push(t("crypt.rekeyDegraded", { note: r.durability_note }));
  }
  if (r.reopen_error) {
    lines.push(
      t("crypt.rekeyReopenError", { code: r.reopen_error.code, message: r.reopen_error.message }),
    );
  }
  return lines;
}

function rangeText(ranges: { from: number; to: number }[]): string {
  if (ranges.length === 0) return t("crypt.rangeNone");
  return ranges
    .map((r) => (r.from === r.to ? `${r.from}` : `${r.from}–${r.to}`))
    .join(t("crypt.rangeSeparator"));
}

/**
 * Search coverage as lines (spec §19.3, §23.3 #8): what was searched, what
 * was not, and what "no matches" can and cannot mean.
 */
export function coverageLines(c: SearchCoverageView): string[] {
  const lines = [
    tn(c.requested_count, "crypt.coverageSearchedOne", "crypt.coverageSearchedMany", {
      searched: c.searched_count,
      requested: c.requested_count,
      ranges: rangeText(c.searched),
      examined: formatNumber(c.entries_examined),
    }),
  ];
  lines.push(t(c.complete ? "crypt.coverageComplete" : "crypt.coveragePartial"));
  for (const u of c.unavailable) {
    lines.push(t("crypt.coverageUnavailable", { seq: u.seq, code: u.code, message: u.message.text }));
  }
  if (c.opened_separately.length > 0) {
    lines.push(t("crypt.coverageOpenedSeparately", { ranges: rangeText(c.opened_separately) }));
  }
  if (c.catalog_from_manifest) {
    lines.push(t("crypt.coverageFromManifest"));
  }
  if (c.content_not_searched) {
    lines.push(t("crypt.coverageContentNotSearched"));
  }
  return lines;
}
