// The wording of MOCHI Find's status and coverage (spec §23.3 #8). Pure: the
// component renders what these return. Archive-supplied text (names, the
// core's error sentences) is only ever a parameter.
import { formatNumber, t, tc, tn } from "../i18n";
import type { FindCoverageView, FindResultView, FindSkippedView } from "../ipc/find";

/** Why one archive could not be searched, in a sentence. */
export function skipText(s: FindSkippedView): string {
  const message = s.message?.text ?? "";
  const code = s.code ?? "";
  switch (s.reason) {
    case "encrypted":
      return t("find.skipEncrypted");
    case "not_mochi":
      return t("find.skipNotMochi");
    case "unsupported":
      return t("find.skipUnsupported", { code });
    case "damaged":
      return t("find.skipDamaged", { code, message });
    case "too_large":
      return t("find.skipTooLarge");
    case "unreadable":
      return t("find.skipUnreadable", { message });
  }
}

export interface Incomplete {
  /** "Search incomplete: 3 archives could not be searched", or the next most important reason. */
  headline: string;
  /** Every other reason, one sentence each. */
  notes: string[];
}

/**
 * Whether anything was not searched, and what. `null` when nothing is known
 * to be missing: a scan that is still running with nothing skipped yet is not
 * "incomplete", it is not finished (its progress says so).
 */
export function incomplete(c: FindCoverageView): Incomplete | null {
  const notes: string[] = [];
  if (c.folders_unreadable > 0) {
    notes.push(tc(c.folders_unreadable, "find.noteFoldersOne", "find.noteFoldersMany"));
  }
  if (c.links_not_followed > 0) {
    notes.push(tc(c.links_not_followed, "find.noteLinksOne", "find.noteLinksMany"));
  }
  if (c.limit === "depth") notes.push(t("find.noteLimitDepth"));
  else if (c.limit === "folders") notes.push(t("find.noteLimitFolders"));
  else if (c.limit === "archives") notes.push(t("find.noteLimitArchives"));
  if (c.state === "cancelled") notes.push(t("find.noteStopped"));
  if (c.state === "failed") notes.push(t("find.noteFailed", { message: c.failure ?? "" }));
  if ((c.state === "cancelled" || c.state === "failed") && c.archives_pending > 0) {
    notes.push(t("find.notePending", { n: formatNumber(c.archives_pending) }));
  }

  let headline: string | null = null;
  if (c.archives_skipped > 0) {
    headline = tc(c.archives_skipped, "find.incompleteArchivesOne", "find.incompleteArchivesMany");
  } else if (c.folders_unreadable > 0) {
    headline = tc(c.folders_unreadable, "find.incompleteFoldersOne", "find.incompleteFoldersMany");
  } else if (c.state === "failed") {
    headline = t("find.incompleteFailed");
  } else if (c.state === "cancelled") {
    headline = t("find.incompleteStopped");
  } else if (notes.length > 0) {
    headline = t("find.incompleteOther");
  }
  return headline === null ? null : { headline, notes };
}

/** The line under the search box. */
export function statusText(r: FindResultView | null, query: string): string {
  if (r === null || r.coverage.state === "idle") return t("find.notScanned");
  const c = r.coverage;
  if (query === "") {
    return c.archives_searched === 0
      ? t("find.notScanned")
      : tn(c.archives_searched, "find.readyOne", "find.readyMany", {
          names: formatNumber(c.names_searched),
          n: formatNumber(c.archives_searched),
        });
  }
  if (r.total_hits > 0) {
    // Every match can be reached by scrolling: the list loads the rest as it goes.
    return tc(r.total_hits, "search.foundOne", "search.foundMany");
  }
  // Zero matches. "No matches" only when everything was searched and the scan is over.
  if (c.state === "running") return t("find.searching");
  return c.complete
    ? tc(c.archives_searched, "find.noMatchesOne", "find.noMatchesMany")
    : t("find.noMatchesIncomplete");
}

/** The scan's progress while it runs. */
export function progressText(c: FindCoverageView, walking: boolean): string {
  const found = formatNumber(c.archives_found);
  const searched = formatNumber(c.archives_searched + c.archives_skipped);
  return walking
    ? t("find.walking", { found, searched })
    : t("find.opening", { found, searched });
}
