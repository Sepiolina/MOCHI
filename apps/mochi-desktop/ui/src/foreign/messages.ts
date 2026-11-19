// What the app says about a foreign archive (plan D8 rule 6, spec §23.3).
// Nothing here uses MOCHI's verification vocabulary or health dimensions: a
// foreign "test" names the checks that ran, recoverability, freshness and
// retention are UNSUPPORTED, and an extraction is never a verification.
import { formatSize } from "../browser/format";
import type { DetailRow, ResultDetails } from "../result/details";
import { formatNumber, t, tc } from "../i18n";
import type { ExtractReport } from "../ipc/extract";
import type { ForeignTestReport } from "../ipc/foreign";
import type { JobOutcome } from "../ipc/jobs";

/** The only thing "Read and check" claims. */
export const testNote = () => t("foreign.testNote");

/** The rule every extraction follows (plan D8 rule 4). */
export const extractNote = () => t("foreign.extractNote");

export const weakNote = () => t("foreign.weakNote");

/** Password errors the open flow handles by asking. */
export function needsPassword(code: string): "ask" | "wrong" | null {
  if (code === "PASSWORD_REQUIRED") return "ask";
  if (code === "WRONG_PASSWORD") return "wrong";
  return null;
}

/** One line for the test result, in the format's own words. */
export function testHeadline(r: ForeignTestReport): string {
  return t(r.complete ? "foreign.testHeadline" : "foreign.testHeadlineEarly", {
    entries: tc(r.entries, "count.entryOne", "count.entryMany"),
    format: r.format,
    problems: tc(r.problems_total, "count.problemOne", "count.problemMany"),
  });
}

/** How a finished job reads (extract or test): one line, and what Details holds. */
export interface ForeignResult {
  headline: string;
  notes: string[];
  /** Part of the job did not happen, or the test found problems. */
  warn: boolean;
  details: ResultDetails | null;
}

/** The Details panel of a test: the checks that ran, in the format's words, and what failed. */
function testDetails(r: ForeignTestReport): ResultDetails {
  const rows: DetailRow[] = [
    {
      label: t("foreign.whatChecked"),
      items: r.checks.map((c) => ({ text: t("foreign.checkCount", { count: formatNumber(c.count), what: c.what }) })),
      itemTestId: "foreign-check-count",
      testId: "foreign-test-report",
    },
    { label: t("foreign.streamCheckLabel"), value: r.stream_check, testId: "foreign-stream-check" },
  ];
  if (r.stop_reason) {
    rows.push({ label: t("foreign.stoppedEarlyLabel"), value: r.stop_reason.text, caution: true });
  }
  if (r.problems.length > 0) {
    rows.push({
      label: t("foreign.problems"),
      caution: true,
      items: r.problems.map((p) => ({ name: p.path, text: p.what.text })),
      more: r.problems_total - r.problems.length,
      testId: "foreign-problems",
      itemTestId: "foreign-problem",
    });
  }
  return { rows, notes: [testNote()] };
}

/** The Details panel of an extraction: what was not extracted, and why. */
function extractDetails(r: ExtractReport, summary: string): ResultDetails {
  const rows: DetailRow[] = [{ label: t("foreign.folder"), value: r.destination }];
  if (r.not_extracted.length > 0) {
    rows.push({
      label: t("foreign.notExtractedList"),
      caution: true,
      items: r.not_extracted.map((e) => ({ name: e.path, code: e.code, text: e.reason.text })),
      more: r.not_extracted_total - r.not_extracted.length,
      testId: "foreign-not-extracted",
      itemTestId: "foreign-not-extracted-item",
    });
  }
  return { rows, notes: [summary, extractNote()] };
}

export function describeOutcome(outcome: JobOutcome): ForeignResult {
  switch (outcome.status) {
    case "completed": {
      const test = outcome.foreign_test;
      if (test) {
        return {
          headline: testHeadline(test),
          notes: [],
          warn: test.problems_total > 0 || !test.complete,
          details: testDetails(test),
        };
      }
      const r = outcome.extract;
      if (!r) return { headline: t("common.completed", { summary: outcome.summary }), notes: [], warn: false, details: null };
      return {
        headline: extractedLine(r),
        notes: [],
        warn: r.not_extracted_total > 0,
        details: extractDetails(r, outcome.summary),
      };
    }
    case "cancelled":
      return { headline: t("foreign.cancelled"), notes: [t("foreign.cancelledNote")], warn: false, details: null };
    case "failed":
      return {
        headline: t("common.failed", { code: outcome.code, message: outcome.message }),
        notes: [t("foreign.failedNote")],
        warn: true,
        details: null,
      };
    case "aborted":
      return {
        headline: t("common.stoppedUnexpectedly", { message: outcome.message }),
        notes: [],
        warn: true,
        details: null,
      };
  }
}

function extractedLine(r: ExtractReport): string {
  const params = {
    files: tc(r.files, "count.fileOne", "count.fileMany"),
    size: formatSize(String(r.bytes)),
    folders: tc(r.directories, "count.folderOne", "count.folderMany"),
    dest: r.destination,
  };
  if (r.not_extracted_total === 0) return t("foreign.extracted", params);
  const n = formatNumber(r.not_extracted_total);
  return r.not_extracted_total === 1
    ? t("foreign.extractedSkippedOne", { ...params, n })
    : t("foreign.extractedSkippedMany", { ...params, n });
}

export function progressText(phase: string, completed: number, total: number | null): string {
  const reading = phase === "foreign_test";
  if (total === null) {
    return t(reading ? "foreign.progressReadingNoTotal" : "foreign.progressExtractingNoTotal", { completed });
  }
  return t(reading ? "foreign.progressReading" : "foreign.progressExtracting", { completed, total });
}
