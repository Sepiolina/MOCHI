// What the app says about tests, health, and repairs (spec §20.4, §22.2,
// §23.3). Status values are the report's own (§20.4), each dimension is shown
// on its own, and only PASS may look like success: UNKNOWN, OVERDUE,
// UNSUPPORTED, and DEGRADED never do (§23.3 #2). A repair is labelled by what
// its report establishes; nothing here calls an archive "repaired", "safe",
// "backed up", or "preserved", or says it was "recovered completely".
import { formatSize } from "../browser/format";
import { phaseLabel, t, tn } from "../i18n";
import type { JobOutcome } from "../ipc/jobs";
import type {
  CheckLevel,
  RepairOutcomeView,
  RepairPlanView,
  RepairResultView,
  ReportView,
  StatusView,
} from "../ipc/check";

/** Words the app never uses about an archive or a repair (§23.3 #4, #6). */
export const BANNED = [
  "backed up",
  "backup",
  "safe",
  "preserved",
  "secure",
  "repaired",
  "recovered completely",
  "fully recovered",
];

export const STATUSES: StatusView[] = ["PASS", "FAIL", "DEGRADED", "UNKNOWN", "OVERDUE", "UNSUPPORTED"];

/** A status value as the report spells it (§20.4). */
export function statusLabel(s: StatusView): string {
  return t(`status.${s}`);
}

/** The glyph that goes with a status, so colour is never the only signal. */
export function statusMark(s: StatusView): string {
  return t(`status.mark.${s}`);
}

/** Spec §20.4, in words. */
export const statusMeaning = (): Record<StatusView, string> => ({
  PASS: t("status.meaning.PASS"),
  FAIL: t("status.meaning.FAIL"),
  DEGRADED: t("status.meaning.DEGRADED"),
  UNKNOWN: t("status.meaning.UNKNOWN"),
  OVERDUE: t("status.meaning.OVERDUE"),
  UNSUPPORTED: t("status.meaning.UNSUPPORTED"),
});

/**
 * The CSS classes of a status. Only PASS gets the success class; FAIL gets
 * the failure class; everything else is neutral and marked as needing
 * attention, never success.
 */
export function statusClass(s: StatusView): string {
  switch (s) {
    case "PASS":
      return "status status-pass";
    case "FAIL":
      return "status status-fail";
    case "DEGRADED":
    case "UNKNOWN":
    case "OVERDUE":
    case "UNSUPPORTED":
      return "status status-attention";
  }
}

/** A dimension's name for display; an unknown one as the report spells it. */
export function dimensionLabel(name: string): string {
  switch (name) {
    case "durability":
    case "integrity":
    case "recoverability":
    case "searchability":
    case "freshness":
    case "retention_compliance":
    case "key_availability":
      return t(`dimension.${name}`);
    default:
      return name;
  }
}

export const levels = (): { level: CheckLevel; label: string; detail: string }[] => [
  { level: "restoration", label: t("level.restoration"), detail: t("level.restorationDetail") },
  { level: "content_integrity", label: t("level.content_integrity"), detail: t("level.content_integrityDetail") },
  { level: "stored_integrity", label: t("level.stored_integrity"), detail: t("level.stored_integrityDetail") },
  { level: "referential", label: t("level.referential"), detail: t("level.referentialDetail") },
  { level: "structural", label: t("level.structural"), detail: t("level.structuralDetail") },
];

/** Shown with every test: what "Test archive" is. */
export const testNote = () => t("check.testNote");

/** Explains UNKNOWN freshness the first time it appears (spec §5.7, D8). */
export const freshnessNote = () => t("check.freshnessNote");

/** Shown when an Encrypted archive was checked without its key. */
export const keylessNote = () => t("check.keylessNote");

/** Shown with every health summary (plan D4 "Gap"). */
export const healthNote = () => t("check.healthNote");

/** Shown with every repair plan. */
export const planNote = () => t("check.planNote");

/** Shown with the approval. */
export const approvalNote = () => t("check.approvalNote");

/** The overall line of a report: never hides a failing dimension. */
export function overallLine(r: ReportView): string {
  const failing = r.dimensions.filter((d) => d.status === "FAIL").map((d) => dimensionLabel(d.dimension));
  const base = t("check.overall", { status: r.overall_status, exit: r.exit_code, policy: r.policy_result });
  return failing.length === 0
    ? base
    : t("check.overallFailing", { base, failing: failing.join(t("check.failingSeparator")) });
}

/** What was checked, in one line. */
export function headline(r: ReportView): string {
  if (r.kind === "health") return t("check.headlineHealth", { file: r.file_name, status: r.overall_status });
  return t(r.deep ? "check.headlineTestDeep" : "check.headlineTest", {
    file: r.file_name,
    level: r.level,
    status: r.overall_status,
  });
}

export function coverageLine(r: ReportView): string | null {
  const c = r.coverage;
  if (c.expected_objects === null || c.checked_objects === null) return null;
  const bytes =
    c.checked_bytes !== null && c.expected_bytes !== null
      ? t("check.coverageBytes", { checked: formatSize(c.checked_bytes), expected: formatSize(c.expected_bytes) })
      : "";
  return t("check.coverage", { checked: c.checked_objects, expected: c.expected_objects, bytes });
}

export const outcomeLabel = (): Record<RepairOutcomeView, string> => ({
  complete: t("check.outcome.complete"),
  partial: t("check.outcome.partial"),
  nothing_recoverable: t("check.outcome.nothing_recoverable"),
});

/** The plan's verdict, in one line. Partial salvage is labelled as such. */
export function planLine(p: RepairPlanView): string {
  switch (p.outcome) {
    case "complete":
      return t("check.planComplete");
    case "partial": {
      const lost = lostSummary(p.lost.length, p.omitted_total);
      return p.salvage_used
        ? t("check.planPartialSalvage", { lost })
        : t("check.planPartial", { lost });
    }
    case "nothing_recoverable":
      return t("check.planNothing");
  }
}

function lostSummary(lost: number, omitted: string): string {
  return t("check.lostJoin", {
    snapshots: tn(lost, "check.lostSnapshotOne", "check.lostSnapshotMany", { n: lost }),
    entries: tn(omitted === "1" ? 1 : 2, "check.lostEntryOne", "check.lostEntryMany", { n: omitted }),
  });
}

/** What a written repair is, in one line, from its report only. */
export function resultLine(r: RepairResultView): string {
  const wrote = t("check.resultWrote", {
    file: r.file_name,
    size: formatSize(r.new_len),
    commits: tn(r.commits.length, "check.commitOne", "check.commitMany", { n: r.commits.length }),
  });
  const entries = t(r.omitted_total === "1" ? "check.entryOne" : "check.entryMany");
  switch (r.outcome) {
    case "complete":
      return t("check.resultComplete", { wrote });
    case "partial":
      return r.salvage_used
        ? t("check.resultPartialSalvage", { wrote, lost: r.lost.length, omitted: r.omitted_total, entries })
        : t("check.resultPartial", { wrote, lost: r.lost.length, omitted: r.omitted_total, entries });
    case "nothing_recoverable":
      return t("check.resultNothing", { wrote });
  }
}

/** The re-verification, in one line (spec §22.2). */
export function reverificationLine(r: RepairResultView): string {
  const v = r.reverification;
  return t("check.reverification", {
    level: v.level,
    status: v.overall_status,
    policy: v.policy_result,
    exit: v.exit_code,
    findings: v.findings,
  });
}

export const sourceKeptNote = () => t("check.sourceKept");

/** The job a note is about: each has its own full sentences in the catalogue. */
export type JobKind = "test" | "plan" | "apply";

/** A job that ended without a result, in words. */
export function failedLine(kind: JobKind, outcome: JobOutcome): string | null {
  switch (outcome.status) {
    case "completed":
      return null;
    case "cancelled":
      return t(
        kind === "test" ? "check.cancelledTest" : kind === "plan" ? "check.cancelledPlan" : "check.cancelledApply",
      );
    case "failed": {
      const params = { code: outcome.code, message: outcome.message };
      return kind === "test"
        ? t("check.failedTest", params)
        : kind === "plan"
          ? t("check.failedPlan", params)
          : t("check.failedApply", params);
    }
    case "aborted": {
      const params = { message: outcome.message };
      return kind === "test"
        ? t("check.stoppedTest", params)
        : kind === "plan"
          ? t("check.stoppedPlan", params)
          : t("check.stoppedApply", params);
    }
  }
}

/** How a running test or repair reads. */
export function progressText(phase: string, completed: number, total: number | null): string {
  const n = total === null ? `${completed}` : t("check.progressOf", { completed, total });
  const names: Record<string, string> = {
    "repair-survey": t("check.progressSurvey"),
    "repair-check": t("check.progressCheck"),
    "repair-salvage": t("check.progressSalvage"),
    "repair-copy": t("check.progressCopy"),
    "repair-verify": t("check.progressVerify"),
  };
  const label = names[phase] ?? phaseLabel(phase);
  return t("check.progressLine", { label, n });
}

/**
 * A check that says nothing about this archive: not required by the policy
 * and either UNSUPPORTED or UNKNOWN. These are folded into one line under
 * Details. A required check, or any other status, is never folded away, so
 * a failing (or degraded, overdue) dimension is always in the default view.
 */
export function isQuiet(d: { status: StatusView; required: boolean }): boolean {
  return !d.required && (d.status === "UNSUPPORTED" || d.status === "UNKNOWN");
}

export type VerdictKind = "ok" | "ranOk" | "problems" | "degraded" | "incomplete" | "keyless" | "failed";

export interface Verdict {
  kind: VerdictKind;
  /** Only "ok" (overall PASS) looks like success; "problems" looks like a failure. */
  tone: "pass" | "fail" | "attention";
  title: string;
  line: string;
}

/** One line on what was checked, from the level the report carries. */
export function scopeLine(r: ReportView): string {
  const byLevel: Record<string, string> = {
    restoration: t("verdict.scope.restoration"),
    content_integrity: t("verdict.scope.content_integrity"),
    stored_integrity: t("verdict.scope.stored_integrity"),
    referential: t("verdict.scope.referential"),
    structural: t("verdict.scope.structural"),
  };
  const what = r.kind === "health" ? t("verdict.health") : (byLevel[r.level] ?? t("verdict.scope.other"));
  return t(r.deep && r.kind !== "health" ? "verdict.checkedDeep" : "verdict.checked", { file: r.file_name, what });
}

/**
 * The plain-language verdict. It never reads better than the report: the
 * success tone needs an overall PASS; a failing dimension always gives
 * "problems"; keyless checks of an encrypted archive are never "ok"; and
 * a report with UNKNOWN overall says what ran, not that all is well.
 */
export function verdict(r: ReportView): Verdict {
  const sep = t("verdict.whereJoin");
  const names = (pred: (d: ReportView["dimensions"][number]) => boolean) =>
    r.dimensions
      .filter(pred)
      .map((d) => dimensionLabel(d.dimension))
      .join(sep);
  const failing = names((d) => d.status === "FAIL");
  if (failing !== "" || r.overall_status === "FAIL") {
    return {
      kind: "problems",
      tone: "fail",
      title: t("verdict.problems"),
      line: t("verdict.problemsLine", { where: failing || r.overall_status }),
    };
  }
  if (r.operational_error) {
    return { kind: "failed", tone: "attention", title: t("verdict.failed"), line: t("verdict.failedLine") };
  }
  if (r.keyless) {
    return { kind: "keyless", tone: "attention", title: t("verdict.keyless"), line: t("verdict.keylessLine") };
  }
  const reduced = names((d) => d.status === "DEGRADED" || d.status === "OVERDUE");
  if (reduced !== "") {
    return {
      kind: "degraded",
      tone: "attention",
      title: t("verdict.degraded"),
      line: t("verdict.degradedLine", { where: reduced }),
    };
  }
  const open = names((d) => d.required && d.status !== "PASS");
  if (open !== "") {
    return {
      kind: "incomplete",
      tone: "attention",
      title: t("verdict.incomplete"),
      line: t("verdict.incompleteLine", { where: open }),
    };
  }
  if (r.overall_status === "PASS") {
    return { kind: "ok", tone: "pass", title: t("verdict.ok"), line: t("verdict.okLine") };
  }
  return { kind: "ranOk", tone: "attention", title: t("verdict.ranOk"), line: t("verdict.ranOkLine") };
}

/**
 * Each item with a stable React key made from `text` (repeats get "#2", "#3",
 * … in order) and its position, for lists whose items need not be unique.
 */
export function keyed<T>(items: readonly T[], text: (item: T) => string): { key: string; item: T; index: number }[] {
  const seen = new Map<string, number>();
  return items.map((item, index) => {
    const base = text(item);
    const n = (seen.get(base) ?? 0) + 1;
    seen.set(base, n);
    return { key: n === 1 ? base : `${base}#${n}`, item, index };
  });
}
