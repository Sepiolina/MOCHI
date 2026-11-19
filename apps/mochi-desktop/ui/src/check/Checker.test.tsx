import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import type { ArchiveSummary } from "../ipc/browse";
import type { CheckResult, HealthView, RepairSetupView, SubjectView } from "../ipc/check";
import type { JobEvent } from "../ipc/jobs";
import { Checker } from "./Checker";
import { planView, report, resultView } from "./fixtures";
import { BANNED, freshnessNote, healthNote, keylessNote } from "./messages";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () => (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

const HOSTILE = '<img src=x onerror="alert(1)">';

function summary(): ArchiveSummary {
  return {
    handle: 3,
    file_name: "open.mochi",
    archive_id: "ab".repeat(16),
    profile: { tar_compatible: false, encrypted: false, redundancy_percent: null },
    commits: [],
    retention_error: null,
  };
}

interface Backend {
  subject?: SubjectView | null;
  health?: HealthView;
  setup?: (action: { action: string }) => RepairSetupView;
  exported?: string | null;
}

/** A fake backend; the test pushes job events over the latest channel. */
function backend(b: Backend = {}) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let channel: Channel<JobEvent> | undefined;
  const next = new Map<number, number>();
  let jobs = 0;
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "choose_check_subject":
        return b.subject === undefined ? { id: 5, file_name: "chosen.mochi", encrypted: false, unlocked: false } : b.subject;
      case "health_report":
        return b.health;
      case "export_report":
        return b.exported === undefined ? "out.json" : b.exported;
      case "repair_setup":
        return b.setup ? b.setup(a.action as { action: string }) : { copies: [], output: "new.mochi" };
      case "start_job":
        channel = a.onEvent as Channel<JobEvent>;
        jobs += 1;
        return jobs;
      case "cancel_job":
        return true;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  const send = (m: JobEvent) => {
    if (!channel) throw new Error("no channel yet");
    const id = channel.id;
    const i = next.get(id) ?? 0;
    next.set(id, i + 1);
    act(() => internals().runCallback(id, { index: i, message: m }));
  };
  return {
    calls,
    send,
    finish: (check: CheckResult) => {
      send({ event: "started", job_id: jobs });
      send({ event: "progress", job_id: jobs, phase: "verify-objects", completed: 1, total: 2 });
      send({ event: "finished", job_id: jobs, outcome: { status: "completed", summary: "done", check } });
    },
    requests: () => calls.filter((c) => c.cmd === "start_job").map((c) => c.args.request),
    of: (cmd: string) => calls.filter((c) => c.cmd === cmd).map((c) => c.args),
  };
}

afterEach(() => clearMocks());

async function chosen(archive: ArchiveSummary | null = null) {
  const user = userEvent.setup();
  render(<Checker archive={archive} />);
  await user.click(screen.getByTestId("check-choose"));
  await screen.findByText("chosen.mochi");
  return user;
}

/** Every badge: success styling exactly when the status is PASS. */
function assertBadges() {
  const badges = screen.queryAllByTestId("status-badge");
  for (const b of badges) {
    expect(b.classList.contains("status-pass")).toBe(b.dataset.status === "PASS");
  }
  return badges;
}

function assertNoBannedWords() {
  const text = document.body.textContent.toLowerCase();
  for (const w of BANNED) expect(text).not.toContain(w);
}

test("nothing runs before an archive is chosen; choosing names no path", async () => {
  const b = backend();
  const user = userEvent.setup();
  const { rerender } = render(<Checker archive={null} />);
  expect(screen.getByTestId("check-test")).toBeDisabled();
  expect(screen.getByTestId("repair-plan")).toBeDisabled();
  expect(screen.getByTestId("check-use-open")).toBeDisabled();
  rerender(<Checker archive={summary()} />);
  await user.click(screen.getByTestId("check-use-open"));
  await screen.findByText("chosen.mochi");
  await user.click(screen.getByTestId("check-choose"));
  expect(b.of("choose_check_subject")).toMatchObject([{ from: { from: "open_archive", handle: 3 } }, { from: { from: "dialog" } }]);
  expect(screen.getByTestId("check-test")).toBeEnabled();
});

test("Test archive runs the chosen level as a job and shows a plain verdict", async () => {
  const b = backend();
  const user = await chosen();
  await user.selectOptions(screen.getByTestId("check-level"), "stored_integrity");
  await user.click(screen.getByTestId("check-deep"));
  await user.click(screen.getByTestId("check-test"));
  expect(b.requests()).toEqual([{ kind: "test_archive", subject: 5, level: "stored_integrity", deep: true }]);
  b.send({ event: "started", job_id: 1 });
  b.send({ event: "progress", job_id: 1, phase: "verify-objects", completed: 1, total: 4 });
  expect(screen.getByTestId("check-progress")).toHaveTextContent("1 of 4");
  expect(screen.getByTestId("check-test")).toBeDisabled();
  b.send({ event: "finished", job_id: 1, outcome: { status: "completed", summary: "", check: { kind: "test", report: report() } } });
  const r = screen.getByTestId("report");
  expect(r.dataset.overall).toBe("UNKNOWN");
  // Overall is UNKNOWN: the verdict says what ran and is not styled as success.
  expect(within(r).getByTestId("report-headline")).toHaveTextContent("No problems found in the checks that ran");
  expect(within(r).getByTestId("report-verdict").className).not.toContain("verdict-pass");
  expect(within(r).getByTestId("report-scope")).toHaveTextContent("a.mochi: Read all data");
  // The default view lists only the checks that apply; the rest are one line under Details.
  const rows = within(r).getAllByTestId("dimension");
  expect(rows.map((x) => [x.dataset.dimension, x.dataset.status])).toEqual([
    ["integrity", "PASS"],
    ["recoverability", "PASS"],
  ]);
  expect(within(r).getAllByTestId("dimension-quiet").map((x) => x.dataset.dimension)).toEqual([
    "durability",
    "freshness",
  ]);
  const details = within(r).getByTestId("report-details");
  expect(details).not.toHaveAttribute("open");
  expect(within(details).getByTestId("report-quiet")).toHaveTextContent("2 checks do not apply to this archive.");
  expect(within(details).getByTestId("report-freshness-note")).toBeInTheDocument();
  const badges = assertBadges();
  expect(badges.filter((x) => x.classList.contains("status-pass"))).toHaveLength(2);
  // The overall indicator is UNKNOWN, so it is not styled as success.
  expect(within(screen.getByTestId("report-overall")).getByTestId("status-badge")).not.toHaveClass("status-pass");
  expect(screen.getByTestId("report-freshness-note")).toHaveTextContent(freshnessNote());
  expect(screen.queryByTestId("report-repair")).toBeNull();
  expect(screen.getByTestId("report-json").textContent).toBe(report().json);
  expect(screen.queryByTestId("report-keyless-note")).toBeNull();
  assertNoBannedWords();
});

test("a failing report shows FAIL overall and names the failing dimension", async () => {
  const b = backend();
  const user = await chosen();
  await user.click(screen.getByTestId("check-test"));
  b.finish({
    kind: "test",
    report: report({
      dimensions: [
        { dimension: "integrity", status: "FAIL", required: true },
        { dimension: "recoverability", status: "DEGRADED", required: true },
        { dimension: "searchability", status: "UNSUPPORTED", required: false },
        { dimension: "freshness", status: "OVERDUE", required: false },
      ],
      overall_status: "FAIL",
      policy_result: "FAIL",
      exit_code: 1,
      findings: [
        {
          code: "STORED_INTEGRITY_FAILED",
          severity: "error",
          message: { text: HOSTILE, verbatim: true },
          expected: null,
          observed: null,
          affected: { first: 2, last: 2 },
        },
      ],
    }),
  });
  expect(screen.getByTestId("report").dataset.overall).toBe("FAIL");
  expect(screen.getByTestId("report-headline")).toHaveTextContent("Problems found");
  expect(screen.getByTestId("report-verdict-line")).toHaveTextContent("Failed: Integrity.");
  expect(screen.getByTestId("report-verdict").className).toContain("verdict-fail");
  // The failing and the reduced dimensions are in the default view; none is folded away.
  const shown = within(screen.getByTestId("report")).getAllByTestId("dimension");
  expect(shown.map((x) => x.dataset.dimension)).toEqual(["integrity", "recoverability", "freshness"]);
  expect(screen.getByTestId("report-quiet")).toHaveTextContent("1 check does not apply");
  expect(screen.getByTestId("report-overall")).toHaveTextContent("Failing: Integrity");
  expect(screen.getByTestId("report-overall")).toHaveTextContent("exit code 1");
  const badges = assertBadges();
  expect(badges.some((x) => x.classList.contains("status-pass"))).toBe(false);
  expect(badges.filter((x) => x.classList.contains("status-fail")).length).toBe(2);
  expect(screen.getByTestId("finding-code")).toHaveTextContent("STORED_INTEGRITY_FAILED");
  // Archive text is text: the markup is shown, never parsed.
  expect(screen.getByTestId("finding")).toHaveTextContent(HOSTILE);
  expect(document.querySelector("img")).toBeNull();
});

test("an encrypted archive's keyless test says the check needs the key", async () => {
  const b = backend();
  const user = await chosen();
  await user.click(screen.getByTestId("check-test"));
  b.finish({
    kind: "test",
    report: report({
      keyless: true,
      dimensions: [{ dimension: "key_availability", status: "UNKNOWN", required: false }],
      overall_status: "UNKNOWN",
      exit_code: 2,
    }),
  });
  expect(screen.getByTestId("report-keyless-note")).toHaveTextContent(keylessNote());
  expect(screen.getByTestId("report-headline")).toHaveTextContent("Only part of this archive was checked");
  expect(screen.getByTestId("report-verdict").className).not.toContain("verdict-pass");
  expect(screen.getByTestId("report").dataset.overall).not.toBe("PASS");
  assertBadges();
});

test("a passing report says no problems; an unfinished one says it could not finish", async () => {
  const b = backend();
  const user = await chosen();
  await user.click(screen.getByTestId("check-test"));
  b.finish({
    kind: "test",
    report: report({
      dimensions: [
        { dimension: "integrity", status: "PASS", required: true },
        { dimension: "recoverability", status: "PASS", required: true },
        { dimension: "retention_compliance", status: "UNSUPPORTED", required: false },
        { dimension: "searchability", status: "UNSUPPORTED", required: false },
        { dimension: "key_availability", status: "UNKNOWN", required: false },
      ],
      overall_status: "PASS",
    }),
  });
  expect(screen.getByTestId("report-headline")).toHaveTextContent("No problems found");
  expect(screen.getByTestId("report-headline")).not.toHaveTextContent("in the checks that ran");
  expect(screen.getByTestId("report-verdict").className).toContain("verdict-pass");
  expect(screen.getAllByTestId("dimension-quiet")).toHaveLength(3);
  expect(screen.getByTestId("report-quiet")).toHaveTextContent("3 checks do not apply to this archive.");
  // Nothing shown by default is UNSUPPORTED or UNKNOWN.
  for (const row of screen.getAllByTestId("dimension")) expect(row.dataset.status).toBe("PASS");
  assertBadges();

  await user.click(screen.getByTestId("check-test"));
  b.finish({
    kind: "test",
    report: report({ overall_status: "UNKNOWN", operational_error: true, exit_code: 3, dimensions: [] }),
  });
  expect(screen.getByTestId("report-headline")).toHaveTextContent("Could not finish the check");
  expect(screen.getByTestId("report-verdict").className).not.toContain("verdict-pass");
  assertNoBannedWords();
});

test("a required check without an answer is shown, not folded away", async () => {
  const b = backend();
  const user = await chosen();
  await user.click(screen.getByTestId("check-test"));
  b.finish({
    kind: "test",
    report: report({
      dimensions: [
        { dimension: "integrity", status: "UNKNOWN", required: true },
        { dimension: "searchability", status: "UNSUPPORTED", required: true },
      ],
      overall_status: "UNKNOWN",
    }),
  });
  expect(screen.getByTestId("report-headline")).toHaveTextContent("Not everything could be checked");
  expect(screen.getAllByTestId("dimension")).toHaveLength(2);
  expect(screen.queryByTestId("report-quiet")).toBeNull();
});

test("problems offer Repair, which moves to the repair section", async () => {
  const b = backend();
  const user = await chosen();
  await user.click(screen.getByTestId("check-test"));
  b.finish({
    kind: "test",
    report: report({
      dimensions: [{ dimension: "integrity", status: "FAIL", required: true }],
      overall_status: "FAIL",
      exit_code: 1,
    }),
  });
  await user.click(screen.getByTestId("report-repair"));
  expect(within(screen.getByTestId("repair")).getByTestId("repair-add-copy")).toHaveFocus();
});

test("Export report JSON asks Rust to write that report, and says where", async () => {
  const b = backend({ exported: "a-test-report.json" });
  const user = await chosen();
  await user.click(screen.getByTestId("check-test"));
  b.finish({ kind: "test", report: report({ report_id: 42 }) });
  await user.click(screen.getByTestId("report-export"));
  expect(b.of("export_report")).toMatchObject([{ report: 42 }]);
  expect(await screen.findByTestId("report-exported")).toHaveTextContent("a-test-report.json");
});

test("the health view shows its own report with the evidence note", async () => {
  const health: HealthView = {
    report: report({ kind: "health", report_id: 9, overall_status: "UNKNOWN" }),
    records: 1,
  };
  const b = backend({ health });
  const user = await chosen();
  await user.click(screen.getByTestId("check-health"));
  expect(b.of("health_report")).toEqual([{ subject: 5 }]);
  const h = await screen.findByTestId("health");
  expect(within(h).getByTestId("health-note")).toHaveTextContent(healthNote());
  expect(within(h).getByTestId("report").dataset.kind).toBe("health");
  assertBadges();
  assertNoBannedWords();
});

test("repair: plan, review, choose a new file, approve, write, re-verify, test the new file", async () => {
  const b = backend();
  const user = await chosen();
  await user.click(screen.getByTestId("repair-plan"));
  expect(b.requests()).toEqual([{ kind: "plan_repair", subject: 5, salvage: false, archive_id: null }]);
  b.finish({
    kind: "plan",
    plan: planView({
      omitted: planView().omitted.map((o) => ({ ...o, path: { text: HOSTILE, verbatim: true } })),
    }),
  });
  const view = screen.getByTestId("repair-plan-view");
  expect(view.dataset.outcome).toBe("partial");
  expect(screen.getByTestId("repair-plan-line")).toHaveTextContent("Plan: PARTIAL.");
  expect(screen.getByTestId("repair-omitted")).toHaveTextContent(HOSTILE);
  expect(document.querySelector("img")).toBeNull();
  expect(screen.getAllByTestId("ladder-step").map((s) => s.dataset.status)).toEqual(["used", "not_attempted"]);
  // No approval and no new file yet: nothing can be written.
  expect(screen.getByTestId("repair-apply")).toBeDisabled();
  await user.click(screen.getByTestId("repair-approve"));
  expect(screen.getByTestId("repair-apply")).toBeDisabled();
  await user.click(screen.getByTestId("repair-choose-output"));
  expect(b.of("repair_setup")).toMatchObject([{ action: { action: "choose_output" } }]);
  expect(await screen.findByTestId("repair-output")).toHaveTextContent("new.mochi");
  expect(screen.getByTestId("repair-apply")).toBeEnabled();
  // Withdrawing the approval disables writing again.
  await user.click(screen.getByTestId("repair-approve"));
  expect(screen.getByTestId("repair-apply")).toBeDisabled();
  await user.click(screen.getByTestId("repair-approve"));
  await user.click(screen.getByTestId("repair-apply"));
  expect(b.requests()[1]).toEqual({ kind: "apply_repair", subject: 5, plan: 4, accept_retention_loss: false });
  b.finish({ kind: "repair", result: resultView() });
  const result = screen.getByTestId("repair-result");
  expect(result.dataset.outcome).toBe("partial");
  expect(result).toHaveTextContent("Repair result: PARTIAL");
  expect(screen.getByTestId("repair-reverification").dataset.status).toBe("UNKNOWN");
  expect(screen.queryByTestId("repair-plan-view")).toBeNull();
  assertBadges();
  assertNoBannedWords();
  await user.click(screen.getByTestId("repair-test-new"));
  expect(b.of("choose_check_subject").at(-1)).toMatchObject({ from: { from: "repair_output" } });
  await screen.findByText("chosen.mochi");
  expect(b.requests()[2]).toEqual({ kind: "test_archive", subject: 5, level: "restoration", deep: false });
});

test("unresolved retention needs its own acceptance; nothing recoverable offers no approval", async () => {
  const b = backend();
  const user = await chosen();
  await user.click(screen.getByTestId("repair-plan"));
  b.finish({
    kind: "plan",
    plan: planView({ retention: { state: "unresolved", code: "NO_VALID_HEAD", reason: { text: "head lost", verbatim: true } } }),
  });
  await user.click(screen.getByTestId("repair-choose-output"));
  await screen.findByText(/New file: new.mochi/);
  await user.click(screen.getByTestId("repair-approve"));
  expect(screen.getByTestId("repair-apply")).toBeDisabled();
  await user.click(screen.getByTestId("repair-accept-retention"));
  await user.click(screen.getByTestId("repair-apply"));
  expect(b.requests().at(-1)).toMatchObject({ kind: "apply_repair", accept_retention_loss: true });

  b.finish({ kind: "plan", plan: planView({ outcome: "nothing_recoverable", plan_id: 6 }) });
  expect(screen.getByTestId("repair-plan-line")).toHaveTextContent("NOTHING RECOVERABLE");
  expect(screen.queryByTestId("repair-approve-panel")).toBeNull();
});

test("salvage is an explicit choice and its partial result is labelled", async () => {
  const b = backend();
  const user = await chosen();
  await user.click(screen.getByTestId("repair-salvage"));
  await user.type(screen.getByTestId("repair-archive-id"), "ab".repeat(32));
  await user.click(screen.getByTestId("repair-plan"));
  expect(b.requests()).toEqual([{ kind: "plan_repair", subject: 5, salvage: true, archive_id: "ab".repeat(32) }]);
  b.finish({ kind: "plan", plan: planView({ salvage_used: true }) });
  expect(screen.getByTestId("repair-plan-line")).toHaveTextContent("PARTIAL SALVAGE");
});

test("adding a copy drops the plan; copies are listed by name", async () => {
  let copies: string[] = [];
  const b = backend({
    setup: (a) => {
      if (a.action === "add_copy") copies = [...copies, "copy.mochi"];
      return { copies, output: null };
    },
  });
  const user = await chosen();
  await user.click(screen.getByTestId("repair-plan"));
  b.finish({ kind: "plan", plan: planView() });
  expect(screen.getByTestId("repair-plan-view")).toBeInTheDocument();
  await user.click(screen.getByTestId("repair-add-copy"));
  expect(await screen.findByTestId("repair-copy")).toHaveTextContent("copy.mochi");
  expect(screen.queryByTestId("repair-plan-view")).toBeNull();
});

test("cancel asks Rust to cancel and the result says nothing was written", async () => {
  const b = backend();
  const user = await chosen();
  await user.click(screen.getByTestId("repair-plan"));
  b.send({ event: "started", job_id: 1 });
  b.send({ event: "progress", job_id: 1, phase: "repair-check", completed: 3, total: 100 });
  expect(screen.getByTestId("check-progress")).toHaveTextContent("Reading file versions: 3 of 100");
  await user.click(screen.getByTestId("check-cancel"));
  expect(b.of("cancel_job")).toEqual([{ jobId: 1 }]);
  expect(screen.getByTestId("check-cancel")).toBeDisabled();
  b.send({ event: "finished", job_id: 1, outcome: { status: "cancelled" } });
  expect(screen.getByTestId("check-status")).toHaveTextContent("Planning was cancelled before it finished. Nothing was written.");
  expect(screen.queryByTestId("check-running")).toBeNull();
  expect(screen.getByTestId("repair-plan")).toBeEnabled();
});

test("a failed job says why, and a dialog cancelled changes nothing", async () => {
  const b = backend({ subject: null });
  const user = userEvent.setup();
  render(<Checker archive={null} />);
  await user.click(screen.getByTestId("check-choose"));
  expect(screen.getByTestId("check-subject")).toHaveTextContent("No archive chosen");
  clearMocks();
  const c = backend();
  await user.click(screen.getByTestId("check-choose"));
  await screen.findByText("chosen.mochi");
  await user.click(screen.getByTestId("repair-plan"));
  c.send({
    event: "finished",
    job_id: 1,
    outcome: { status: "failed", code: "KEY_UNAVAILABLE", message: "this archive is encrypted" },
  });
  expect(screen.getByTestId("check-status")).toHaveTextContent("Planning failed (KEY_UNAVAILABLE): this archive is encrypted");
  expect(b.calls.length).toBe(1);
});
