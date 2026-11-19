import { expect, test } from "vitest";

import type { StatusView } from "../ipc/check";
import { t, type MessageKey } from "../i18n";
import { planView, report, resultView } from "./fixtures";
import * as m from "./messages";

const ALL: StatusView[] = ["PASS", "FAIL", "DEGRADED", "UNKNOWN", "OVERDUE", "UNSUPPORTED"];

test("only PASS gets the success class; FAIL the failure class; the rest neither", () => {
  expect(m.STATUSES).toEqual(ALL);
  for (const s of ALL) {
    const c = m.statusClass(s);
    expect(c.split(" ").includes("status-pass")).toBe(s === "PASS");
    expect(c.split(" ").includes("status-fail")).toBe(s === "FAIL");
    if (s !== "PASS" && s !== "FAIL") expect(c).toBe("status status-attention");
  }
});

test("the overall line never hides a failing dimension", () => {
  const r = report({
    dimensions: [
      { dimension: "integrity", status: "FAIL", required: true },
      { dimension: "freshness", status: "UNKNOWN", required: false },
    ],
    overall_status: "FAIL",
    policy_result: "FAIL",
    exit_code: 1,
  });
  expect(m.overallLine(r)).toBe("Overall FAIL (exit code 1); policy result FAIL. Failing: Integrity.");
  expect(m.overallLine(report())).not.toContain("Failing");
  expect(m.headline(report({ deep: true }))).toBe("Test of a.mochi, level restoration, deep (fsck): UNKNOWN.");
  expect(m.headline(report({ kind: "health" }))).toBe("Health of a.mochi: UNKNOWN.");
});

test("plans and results are labelled by their outcome; partial salvage says so", () => {
  expect(m.planLine(planView())).toBe(
    "Plan: PARTIAL. 0 snapshots would not be written and 1 entry would be left out.",
  );
  expect(m.planLine(planView({ salvage_used: true }))).toMatch(/^Plan: PARTIAL SALVAGE\./);
  expect(m.planLine(planView({ outcome: "complete", omitted: [], omitted_total: "0" }))).toMatch(/^Plan: COMPLETE\./);
  expect(m.planLine(planView({ outcome: "nothing_recoverable" }))).toMatch(/nothing was changed/);
  expect(m.resultLine(resultView())).toMatch(/Outcome PARTIAL: 0 snapshot\(s\) not written, 1 entry left out\.$/);
  expect(m.resultLine(resultView({ salvage_used: true }))).toMatch(/PARTIAL SALVAGE/);
  expect(m.resultLine(resultView({ outcome: "complete", omitted: [], omitted_total: "0" }))).toMatch(
    /Outcome COMPLETE: the plan left nothing out\.$/,
  );
  expect(m.reverificationLine(resultView())).toBe(
    "Re-verified before it was published (level restoration): overall UNKNOWN, policy result PASS, exit code 0, 0 finding(s).",
  );
});

test("no message uses a banned word", () => {
  const texts: string[] = [
    m.testNote(),
    m.freshnessNote(),
    m.keylessNote(),
    m.healthNote(),
    m.planNote(),
    m.approvalNote(),
    m.sourceKeptNote(),
    ...Object.values(m.statusMeaning()),
    ...Object.values(m.outcomeLabel()),
    ...m.levels().flatMap((l) => [l.label, l.detail]),
    m.overallLine(report()),
    m.headline(report()),
    m.scopeLine(report({ deep: true })),
    m.scopeLine(report({ kind: "health" })),
    m.coverageLine(report()) ?? "",
  ];
  for (const k of ["ok", "problems", "degraded", "incomplete", "keyless", "failed", "ranOk"]) {
    const tt = t as (key: MessageKey, ...a: unknown[]) => string;
    texts.push(tt(`verdict.${k}` as MessageKey), tt(`verdict.${k}Line` as MessageKey, { where: "x" }));
  }
  for (const outcome of ["complete", "partial", "nothing_recoverable"] as const) {
    for (const salvage_used of [false, true]) {
      texts.push(m.planLine(planView({ outcome, salvage_used })));
      texts.push(m.resultLine(resultView({ outcome, salvage_used })));
    }
  }
  texts.push(m.reverificationLine(resultView()));
  for (const kind of ["test", "plan", "apply"] as const) {
    texts.push(m.failedLine(kind, { status: "cancelled" }) ?? "");
    texts.push(m.failedLine(kind, { status: "failed", code: "IO_ERROR", message: "x" }) ?? "");
  }
  for (const t of texts) {
    for (const word of m.BANNED) expect(t.toLowerCase()).not.toContain(word);
  }
});

test("freshness UNKNOWN is explained as not a failure and never a pass", () => {
  expect(m.freshnessNote()).toMatch(/UNKNOWN/);
  expect(m.freshnessNote()).toMatch(/not a failure/);
  expect(m.freshnessNote()).toMatch(/never shown as a pass/);
  expect(m.keylessNote()).toMatch(/without its passphrase/);
  expect(m.keylessNote()).toMatch(/never PASS overall/);
});

test("the verdict never reads better than the report", () => {
  const dims = (d: [string, StatusView, boolean][]) =>
    d.map(([dimension, status, required]) => ({ dimension, status, required }));
  const ok = m.verdict(report({ overall_status: "PASS", dimensions: dims([["integrity", "PASS", true]]) }));
  expect([ok.kind, ok.tone, ok.title]).toEqual(["ok", "pass", "No problems found"]);
  // Not required and unknown: nothing to show but the report's own overall (UNKNOWN) is not success.
  const ran = m.verdict(report());
  expect([ran.kind, ran.tone]).toEqual(["ranOk", "attention"]);
  // A failing dimension wins over everything, even a PASS overall or a keyless report.
  for (const extra of [{ overall_status: "PASS" as const }, { keyless: true }, { operational_error: true }]) {
    const v = m.verdict(report({ ...extra, dimensions: dims([["searchability", "FAIL", false]]) }));
    expect([v.kind, v.tone]).toEqual(["problems", "fail"]);
    expect(v.line).toBe("Failed: Searchability.");
  }
  // Keyless is never ok, even if the report claims PASS.
  expect(m.verdict(report({ keyless: true, overall_status: "PASS", dimensions: [] })).kind).toBe("keyless");
  expect(m.verdict(report({ dimensions: dims([["durability", "DEGRADED", true]]) })).kind).toBe("degraded");
  expect(m.verdict(report({ dimensions: dims([["freshness", "OVERDUE", false]]) })).kind).toBe("degraded");
  expect(m.verdict(report({ operational_error: true })).title).toBe("Could not finish the check");
});

test("only checks that say nothing about this archive are folded away", () => {
  expect(m.isQuiet({ status: "UNSUPPORTED", required: false })).toBe(true);
  expect(m.isQuiet({ status: "UNKNOWN", required: false })).toBe(true);
  for (const status of ["PASS", "FAIL", "DEGRADED", "OVERDUE"] as StatusView[]) {
    expect(m.isQuiet({ status, required: false })).toBe(false);
  }
  for (const status of ALL) expect(m.isQuiet({ status, required: true })).toBe(false);
});

test("keys are unique and stable for repeated items", () => {
  expect(m.keyed(["a", "b", "a"], (x) => x).map((k) => k.key)).toEqual(["a", "b", "a#2"]);
  expect(m.keyed(["a", "b"], (x) => x).map((k) => k.index)).toEqual([0, 1]);
  expect(m.dimensionLabel("key_availability")).toBe("Key availability");
  expect(m.dimensionLabel("new_thing")).toBe("new_thing");
  expect(m.progressText("repair-check", 3, 10)).toBe("Reading file versions: 3 of 10");
  expect(m.coverageLine(report({ coverage: { ...report().coverage, checked_objects: null } }))).toBeNull();
});
