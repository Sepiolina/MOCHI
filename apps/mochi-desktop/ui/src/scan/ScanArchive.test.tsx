// Scan for viruses (owner request 2026-11-14): the found scanners, the program and
// parameters, and the scanner's report in its own words. Never "safe" (spec §23.3).
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import type { ArchiveSummary } from "../ipc/browse";
import type { JobEvent } from "../ipc/jobs";
import type { ScanReportView, ScanSetup } from "../ipc/scan";
import { audit } from "../test/audit";
import { ScanArchive, verdictText } from "./ScanArchive";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () => (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

const archive: ArchiveSummary = {
  handle: 4,
  file_name: "a.mochi",
  archive_id: "cd".repeat(16),
  profile: { tar_compatible: false, encrypted: false, redundancy_percent: null },
  commits: [],
  retention_error: null,
};

const defender = {
  name: "Microsoft Defender",
  program: "C:\\Program Files\\Windows Defender\\MpCmdRun.exe",
  arguments: '-Scan -ScanType 3 -File "%f" -DisableRemediation',
};

const report = (over: Partial<ScanReportView> = {}): ScanReportView => ({
  scanner: "Microsoft Defender",
  seq: 1,
  files: 52,
  folders: 18,
  not_scanned: 0,
  exit_code: 0,
  verdict: "none_reported",
  output: "Scan finished.\n<script>x</script>",
  output_truncated: false,
  cleanup_pending: false,
  ...over,
});

function backend(setup: ScanSetup, picked: string | null = null) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let channel: Channel<JobEvent> | undefined;
  let index = 0;
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "scan_setup":
        return setup;
      case "pick_scanner_program":
        return picked;
      case "start_job":
        channel = a.onEvent as Channel<JobEvent>;
        return 9;
      case "cancel_job":
        return true;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return {
    calls,
    emit: (message: JobEvent) => {
      if (!channel) throw new Error("no job");
      const id = channel.id;
      act(() => internals().runCallback(id, { index: index++, message }));
    },
  };
}

afterEach(() => clearMocks());

test("the scanner's words, never a verdict of MOCHI's own", () => {
  const base = { scanner: "ClamAV" };
  expect(verdictText({ ...base, verdict: "none_reported", exit_code: 0 })).toBe("ClamAV reported no threats.");
  expect(verdictText({ ...base, verdict: "threats_reported", exit_code: 1 })).toContain("reported threats");
  expect(verdictText({ ...base, verdict: "scanner_failed", exit_code: 2 })).toContain("exit code 2");
  expect(verdictText({ ...base, verdict: "unknown", exit_code: 5 })).toContain("exit code 5");
  expect(verdictText({ ...base, verdict: "unknown", exit_code: null })).toContain("stopped");
  for (const v of ["none_reported", "threats_reported", "scanner_failed", "unknown"] as const) {
    expect(verdictText({ ...base, verdict: v, exit_code: 0 }).toLowerCase()).not.toMatch(/\bsafe\b|\bclean\b/);
  }
});

test("starts with the found scanner, runs the job on the version on screen, and shows the report as text", async () => {
  const b = backend({ detected: [defender], chosen: defender });
  render(<ScanArchive archive={archive} seq={1} onClose={() => {}} />);
  await waitFor(() => expect(screen.getByTestId("scan-program")).toHaveValue(defender.program));
  expect(screen.getByTestId("scan-arguments")).toHaveValue(defender.arguments);
  expect(screen.getByTestId("scan-found")).toHaveDisplayValue("Microsoft Defender");
  expect(audit()).toEqual([]);
  await userEvent.click(screen.getByTestId("scan-start"));
  await waitFor(() => expect(b.calls.some((c) => c.cmd === "start_job")).toBe(true));
  expect(b.calls.find((c) => c.cmd === "start_job")?.args.request).toEqual({
    kind: "scan_archive",
    handle: 4,
    seq: 1,
    program: defender.program,
    arguments: defender.arguments,
  });
  b.emit({ event: "started", job_id: 9 });
  expect(screen.getByTestId("scan-running")).toHaveTextContent("Extracting files");
  b.emit({ event: "progress", job_id: 9, phase: "scan", completed: 0, total: null });
  expect(screen.getByTestId("scan-running")).toHaveTextContent("Microsoft Defender is scanning");
  b.emit({ event: "finished", job_id: 9, outcome: { status: "completed", summary: "", scan: report() } });
  expect(await screen.findByTestId("scan-verdict")).toHaveTextContent("Microsoft Defender reported no threats.");
  expect(screen.getByTestId("scan-scope")).toHaveTextContent("Scanned 52 files in 18 folders of version 1.");
  expect(screen.getByTestId("scan-output").textContent).toBe("Scan finished.\n<script>x</script>");
  expect(document.querySelector("script")).toBeNull();
  expect(screen.queryByTestId("scan-partial")).toBeNull();
  expect(screen.getByTestId("scan-result").textContent?.toLowerCase()).not.toMatch(/\bsafe\b/);
});

test("entries that could not be extracted make the scan partial, said in words", async () => {
  const b = backend({ detected: [defender], chosen: defender });
  render(<ScanArchive archive={archive} seq={1} onClose={() => {}} />);
  await waitFor(() => expect(screen.getByTestId("scan-program")).toHaveValue(defender.program));
  await userEvent.click(screen.getByTestId("scan-start"));
  await waitFor(() => expect(b.calls.some((c) => c.cmd === "start_job")).toBe(true));
  b.emit({
    event: "finished",
    job_id: 9,
    outcome: { status: "completed", summary: "", scan: report({ not_scanned: 3, verdict: "threats_reported", exit_code: 2 }) },
  });
  expect(await screen.findByTestId("scan-partial")).toHaveTextContent("Partial: 3 entries");
  expect(screen.getByTestId("scan-verdict")).toHaveClass("scan-attention");
});

test("with no scanner found, Browse fills in a program and a refusal is shown with its code", async () => {
  const b = backend({ detected: [], chosen: { name: "", program: "", arguments: '"%f"' } }, "/opt/av/scan");
  render(<ScanArchive archive={archive} seq={1} onClose={() => {}} />);
  await waitFor(() => expect(screen.getByTestId("scan-found")).toHaveDisplayValue("No scanner was found on this computer"));
  expect(screen.getByTestId("scan-start")).toBeDisabled();
  await userEvent.click(screen.getByTestId("scan-browse"));
  await waitFor(() => expect(screen.getByTestId("scan-program")).toHaveValue("/opt/av/scan"));
  await userEvent.click(screen.getByTestId("scan-start"));
  await waitFor(() => expect(b.calls.some((c) => c.cmd === "start_job")).toBe(true));
  b.emit({ event: "finished", job_id: 9, outcome: { status: "failed", code: "INVALID_ARGUMENT", message: "put %f in the parameters" } });
  expect(await screen.findByTestId("scan-error")).toHaveTextContent("INVALID_ARGUMENT");
  // The form is back, with what was typed.
  expect(screen.getByTestId("scan-program")).toHaveValue("/opt/av/scan");
});

test("Cancel stops the job", async () => {
  const b = backend({ detected: [defender], chosen: defender });
  render(<ScanArchive archive={archive} seq={1} onClose={() => {}} />);
  await waitFor(() => expect(screen.getByTestId("scan-program")).toHaveValue(defender.program));
  await userEvent.click(screen.getByTestId("scan-start"));
  await waitFor(() => expect(screen.getByTestId("scan-cancel")).toBeEnabled());
  await userEvent.click(screen.getByTestId("scan-cancel"));
  expect(b.calls.find((c) => c.cmd === "cancel_job")?.args).toEqual({ jobId: 9 });
  b.emit({ event: "finished", job_id: 9, outcome: { status: "cancelled" } });
  expect(await screen.findByText("The scan was cancelled. The temporary folder was deleted.")).toBeInTheDocument();
});
