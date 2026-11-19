import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import { Browser } from "../browser/Browser";
import type { ArchiveSummary } from "../ipc/browse";
import type { ExtractPlan, ExtractReport } from "../ipc/extract";
import type { JobEvent } from "../ipc/jobs";
import { Extractor } from "./Extractor";
import type { ExtractorProps } from "./Extractor";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () =>
  (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

const HOSTILE = '<img src=x onerror="alert(1)">';
const SELECTED = { key: "6162", name: { text: "docs", verbatim: true } };

function summary(handle = 3): ArchiveSummary {
  return {
    handle,
    file_name: "fake.mochi",
    archive_id: "ab".repeat(16),
    profile: { tar_compatible: false, encrypted: false, redundancy_percent: null },
    commits: [0, 1].map((seq) => ({
      seq,
      commit_id: String(seq).repeat(64),
      time: null,
      checkpoint: seq === 0,
      retained: true,
      expired: false,
      holds: [],
    })),
    retention_error: null,
  };
}

function plan(extra: Partial<ExtractPlan> = {}): ExtractPlan {
  return {
    destination: "/home/me/out",
    entries: 5,
    existing: [],
    existing_total: 0,
    merged_folders: 0,
    conflicts: [],
    conflicts_total: 0,
    case_insensitive: false,
    ...extra,
  };
}

function report(extra: Partial<ExtractReport> = {}): ExtractReport {
  return {
    destination: "/home/me/out",
    seq: 1,
    files: 3,
    directories: 1,
    bytes: 100,
    not_extracted: [],
    not_extracted_total: 0,
    attribute_notes: [],
    attributes_unavailable: null,
    durability_note: null,
    files_not_flushed: false,
    case_insensitive: false,
    opened_folder: false,
    replaced: 0,
    merged_folders: 0,
    ...extra,
  };
}

/** A fake backend: `plans` are answered in order; the test pushes job events. */
function backend(plans: (ExtractPlan | null | { error: { code: string; message: string } })[]) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let channel: Channel<JobEvent> | undefined;
  let index = 0;
  const next = new Map<number, number>();
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "plan_extract_default":
      case "plan_extract": {
        const p = plans[Math.min(index++, plans.length - 1)] ?? null;
        if (p !== null && "error" in p) throw p.error;
        return p;
      }
      case "discard_extract_folder":
      case "show_in_folder":
        return null;
      case "start_job":
        channel = a.onEvent as Channel<JobEvent>;
        return 7;
      case "cancel_job":
        return true;
      case "pause_job":
        return true;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return {
    calls,
    job: (m: JobEvent) => {
      if (!channel) throw new Error("no channel yet");
      const id = channel.id;
      const i = next.get(id) ?? 0;
      next.set(id, i + 1);
      act(() => internals().runCallback(id, { index: i, message: m }));
    },
    planCalls: () => calls.filter((c) => c.cmd === "plan_extract").map((c) => c.args),
    defaultCalls: () => calls.filter((c) => c.cmd === "plan_extract_default").map((c) => c.args),
    cmds: () => calls.map((c) => c.cmd),
    request: () => calls.find((c) => c.cmd === "start_job")?.args.request,
  };
}

afterEach(() => clearMocks());

function renderExtractor(props: Partial<ExtractorProps> = {}) {
  return render(<Extractor archive={summary()} seq={1} selected={[]} {...props} />);
}

const BANNED = ["backed up", "backup", "safe", "preserved", "secure"];

test("nothing can be extracted without an archive; with a selection, Extract takes the selection", () => {
  backend([plan()]);
  const { rerender } = render(<Extractor archive={null} seq={null} selected={[]} />);
  expect(screen.getByTestId("extract-all")).toBeDisabled();
  rerender(<Extractor archive={summary()} seq={1} selected={[]} />);
  expect(screen.getByTestId("extract-all")).toBeEnabled();
  expect(screen.getByTestId("extract-all")).toHaveAttribute("title", "Extract everything in this version");
  rerender(<Extractor archive={summary()} seq={1} selected={[SELECTED]} />);
  expect(screen.getByTestId("extract-all")).toHaveAttribute("title", "Extract the 1 selected");
});

test("extract all: a new folder next to the archive, no dialog, the plan is shown, and the job names no path", async () => {
  const user = userEvent.setup();
  const b = backend([plan()]);
  renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  const panel = await screen.findByTestId("extract-panel");
  expect(b.defaultCalls()).toEqual([{ handle: 3, seq: 1, keys: [] }]);
  expect(b.planCalls()).toEqual([]);
  expect(screen.getByTestId("extract-destination")).toHaveTextContent("/home/me/out");
  expect(screen.getByTestId("extract-notes")).toHaveTextContent("A new folder was made");
  expect(screen.getByTestId("extract-scope")).toHaveTextContent("Everything in version 1 (5 entries)");
  expect(panel).toHaveTextContent("Extract into a folder only you can modify.");
  expect(screen.getByTestId("extract-no-conflicts")).toBeInTheDocument();
  expect(screen.queryByTestId("extract-conflicts")).toBeNull();

  await user.click(screen.getByTestId("extract-confirm"));
  await screen.findByTestId("extract-running");
  // Only an archive handle, a snapshot, entry keys, and the conflict choice.
  expect(b.request()).toEqual({
    kind: "extract_archive",
    handle: 3,
    seq: 1,
    keys: [],
    on_conflict: "stop",
  });
});

test("extract selected sends the selected entry's key and says the folders above are created", async () => {
  const user = userEvent.setup();
  const b = backend([plan({ entries: 3 })]);
  renderExtractor({ selected: [SELECTED] });
  await user.click(screen.getByTestId("extract-all"));
  await screen.findByTestId("extract-panel");
  expect(b.defaultCalls()).toEqual([{ handle: 3, seq: 1, keys: ["6162"] }]);
  expect(screen.getByTestId("extract-scope")).toHaveTextContent("docs (3 entries)");
  expect(screen.getByTestId("extract-panel")).toHaveTextContent("folders above a selected entry are created too");
  await user.click(screen.getByTestId("extract-confirm"));
  await screen.findByTestId("extract-running");
  expect(b.request()).toMatchObject({ keys: ["6162"], on_conflict: "stop" });
});

test("cancelling the folder dialog (Choose a different folder) keeps the plan and starts nothing", async () => {
  const user = userEvent.setup();
  const b = backend([plan(), null]);
  renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  await screen.findByTestId("extract-panel");
  await user.click(screen.getByTestId("extract-change-folder"));
  await act(async () => undefined);
  expect(screen.getByTestId("extract-panel")).toBeInTheDocument();
  expect(screen.getByTestId("extract-destination")).toHaveTextContent("/home/me/out");
  expect(b.cmds()).toEqual(["plan_extract_default", "plan_extract"]);
});

test("a folder the engine refuses shows the error and no panel", async () => {
  const user = userEvent.setup();
  backend([{ error: { code: "UNSUPPORTED_FEATURE", message: "other users can modify this folder" } }]);
  renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  expect(await screen.findByTestId("extract-error")).toHaveTextContent(
    "Could not check the folder (UNSUPPORTED_FEATURE): other users can modify this folder",
  );
  expect(screen.queryByTestId("extract-panel")).toBeNull();
});

const CONFLICTS = plan({
  conflicts_total: 202,
  conflicts: [
    {
      path: { text: HOSTILE, verbatim: true },
      code: "NAME_COLLISION",
      reason: "an entry with this name is already in the folder",
      why: "other_kind",
    },
    {
      path: { text: "rtl\\u{202e}gpj.exe", verbatim: false },
      code: "NAME_UNSUPPORTED",
      reason: "this folder's file system cannot hold the name",
      why: "name_not_allowed",
    },
  ],
});

// Owner decision 2026-11-11: when the folder already has some of the names, a short title, the
// names as a plain list, and three choices; what cannot be extracted at all is behind a closed
// disclosure, in plain words. Nothing is written before a choice.
const EXISTING = plan({
  destination: "/home/me/Desktop/out",
  existing_total: 3,
  existing: [
    { text: "docs/a.txt", verbatim: true },
    { text: HOSTILE, verbatim: true },
  ],
  merged_folders: 1,
});

test("files that already exist: a short title, their names, and Add only new files as the safe default", async () => {
  const user = userEvent.setup();
  const b = backend([EXISTING]);
  const { container } = renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  expect(await screen.findByTestId("extract-choices-title")).toHaveTextContent(
    "3 files already exist in “out”",
  );
  const items = screen.getAllByTestId("extract-existing-item");
  expect(items.map((i) => i.textContent)).toEqual(["docs/a.txt", HOSTILE]);
  // The folder part is quieter than the name; no error codes, no repeated sentence.
  expect(items[0]?.querySelector(".choices-dir")).toHaveTextContent("docs/");
  expect(screen.getByTestId("extract-existing")).toHaveTextContent("…and 1 more.");
  expect(screen.getByTestId("extract-existing")).not.toHaveTextContent("NAME_COLLISION");
  expect(screen.getByTestId("extract-merged-note")).toHaveTextContent(
    "New files are added to the folder that is already there.",
  );
  expect(screen.getByTestId("extract-choices")).toHaveTextContent("so you can get them back");
  // Archive text stays inert.
  expect(container.querySelector("img")).toBeNull();
  // The safe choice is the default one and has the focus.
  expect(screen.getByTestId("extract-add-new")).toHaveTextContent("Add only new files");
  expect(screen.getByTestId("extract-add-new")).toHaveClass("primary");
  expect(screen.getByTestId("extract-replace")).toHaveClass("destructive");
  expect(b.cmds()).not.toContain("start_job");
  await user.click(screen.getByTestId("extract-add-new"));
  await screen.findByTestId("extract-running");
  expect(b.request()).toMatchObject({ on_conflict: "add_new" });
});

test("Replace sends replace; the result says how many and where the old files went", async () => {
  const user = userEvent.setup();
  const b = backend([EXISTING]);
  renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  await user.click(await screen.findByTestId("extract-replace"));
  await screen.findByTestId("extract-running");
  expect(b.request()).toMatchObject({ on_conflict: "replace" });
  b.job({ event: "started", job_id: 7 });
  b.job({
    event: "finished",
    job_id: 7,
    outcome: { status: "completed", summary: "s", extract: report({ replaced: 2 }) },
  });
  expect(await screen.findByTestId("extract-headline")).toHaveTextContent("2 replaced");
  expect(screen.getByTestId("extract-status")).toHaveTextContent(/The files that were replaced are in the (Trash|Recycle Bin)\./);
});

test("what cannot be extracted at all is behind a closed disclosure, in plain words, with the full reason as a tooltip", async () => {
  const user = userEvent.setup();
  const b = backend([CONFLICTS]);
  renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  const box = await screen.findByTestId("extract-conflicts");
  expect(box.tagName).toBe("DETAILS");
  expect(box).not.toHaveAttribute("open");
  expect(box.querySelector("summary")).toHaveTextContent("202 items can't be extracted here");
  const items = within(box).getAllByTestId("extract-conflict");
  expect(items[0]).toHaveTextContent("Something of another kind is in the way");
  expect(items[1]).toHaveTextContent("This computer doesn't allow this name");
  expect(items[1]).toHaveAttribute("title", "this folder's file system cannot hold the name");
  expect(box).toHaveTextContent("…and 200 more.");
  // No files exist, so the only question is whether to extract the rest.
  expect(screen.queryByTestId("extract-replace")).toBeNull();
  expect(screen.getByTestId("extract-confirm")).toHaveTextContent("Extract the rest");
  await user.click(screen.getByTestId("extract-confirm"));
  await screen.findByTestId("extract-running");
  expect(b.request()).toMatchObject({ on_conflict: "add_new" });
});

test("choose a different folder asks Rust to pick again, with the same selection", async () => {
  const user = userEvent.setup();
  const b = backend([CONFLICTS, plan({ destination: "/home/me/other" })]);
  renderExtractor({ selected: [SELECTED] });
  await user.click(screen.getByTestId("extract-all"));
  await screen.findByTestId("extract-conflicts");

  await user.click(screen.getByTestId("extract-change-folder"));
  await screen.findByTestId("extract-no-conflicts");
  // A folder the user picked is not "a new folder made for this".
  expect(screen.getByTestId("extract-notes")).not.toHaveTextContent("A new folder was made");
  expect(screen.getByTestId("extract-destination")).toHaveTextContent("/home/me/other");
  expect(screen.getByTestId("extract-confirm")).toHaveTextContent(/^Extract$/);
  expect(b.defaultCalls()).toHaveLength(1);
  expect(b.planCalls().map((c) => c.pick)).toEqual([true]);
  expect(b.planCalls().map((c) => c.keys)).toEqual([["6162"]]);

  await user.click(screen.getByTestId("extract-close"));
  expect(screen.queryByTestId("extract-panel")).toBeNull();
  expect(b.calls.map((c) => c.cmd)).not.toContain("start_job");
  // The folder was picked by the user, so there is no made folder to tidy up.
  expect(b.cmds()).not.toContain("discard_extract_folder");
});

test("closing the plan of a new folder asks Rust to remove it if it is still empty", async () => {
  const user = userEvent.setup();
  const b = backend([plan()]);
  renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  await screen.findByTestId("extract-panel");
  await user.click(screen.getByTestId("extract-close"));
  expect(screen.queryByTestId("extract-panel")).toBeNull();
  expect(b.cmds().at(-1)).toBe("discard_extract_folder");
});

test("a finished extraction offers Show in folder, which names no path", async () => {
  const user = userEvent.setup();
  const b = backend([plan()]);
  renderExtractor();
  expect(screen.queryByTestId("extract-show-folder")).toBeNull();
  await user.click(screen.getByTestId("extract-all"));
  await user.click(await screen.findByTestId("extract-confirm"));
  await screen.findByTestId("extract-running");
  b.job({ event: "started", job_id: 7 });
  b.job({ event: "finished", job_id: 7, outcome: { status: "completed", summary: "s", extract: report() } });
  await user.click(screen.getByTestId("extract-show-folder"));
  const shown = b.calls.find((c) => c.cmd === "show_in_folder");
  expect(shown?.args).toEqual({ what: { kind: "extract_destination" } });
});

test("progress and cancel: the job is cancelled by id, and the result says what stays", async () => {
  const user = userEvent.setup();
  const b = backend([plan()]);
  renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  await user.click(await screen.findByTestId("extract-confirm"));
  await screen.findByTestId("extract-running");
  b.job({ event: "started", job_id: 7 });
  b.job({ event: "progress", job_id: 7, phase: "restore", completed: 2, total: 5 });
  expect(screen.getByTestId("extract-progress")).toHaveTextContent("Extracting: 2 of 5 entries");
  expect(screen.getByTestId("extract-all")).toBeDisabled();
  expect(screen.getByTestId("extract-confirm")).toBeDisabled();

  await user.click(screen.getByTestId("extract-cancel"));
  expect(b.calls.at(-1)).toEqual({ cmd: "cancel_job", args: { jobId: 7 } });
  expect(screen.getByTestId("extract-progress")).toHaveTextContent("(cancelling…)");
  expect(screen.getByTestId("extract-cancel")).toBeDisabled();

  b.job({ event: "finished", job_id: 7, outcome: { status: "cancelled" } });
  const status = screen.getByTestId("extract-status");
  expect(status).toHaveAttribute("data-state", "cancelled");
  expect(status).toHaveAttribute("data-complete", "false");
  expect(status).toHaveTextContent("Cancelled. The extraction stopped before it finished.");
  expect(status).toHaveTextContent("each one complete");
  expect(screen.queryByTestId("extract-running")).toBeNull();
  // The panel stays, so the user can try again.
  expect(screen.getByTestId("extract-panel")).toBeInTheDocument();
  expect(screen.getByTestId("extract-all")).toBeEnabled();
});

test("a finished extraction lists what was not extracted and which attributes were not applied, as text", async () => {
  const user = userEvent.setup();
  const b = backend([plan()]);
  const { container } = renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  await user.click(await screen.findByTestId("extract-confirm"));
  await screen.findByTestId("extract-running");
  b.job({ event: "started", job_id: 7 });
  b.job({
    event: "finished",
    job_id: 7,
    outcome: {
      status: "completed",
      summary: "s",
      extract: report({
        not_extracted_total: 3,
        not_extracted: [
          {
            path: { text: HOSTILE, verbatim: true },
            code: "NAME_UNSUPPORTED",
            reason: { text: "this folder's file system cannot hold the name", verbatim: true },
          },
          {
            path: { text: "line\\u{a}break", verbatim: false },
            code: "NAME_COLLISION",
            reason: { text: "an entry with this name is already in the folder", verbatim: true },
          },
        ],
        attribute_notes: [
          {
            attribute: "owner and group",
            count: 3,
            first_path: { text: "a/b", verbatim: true },
            reason: { text: "changing the owner needs privilege", verbatim: true },
          },
          {
            attribute: "setuid and setgid bits",
            count: 1,
            first_path: { text: "tool", verbatim: true },
            reason: { text: "setuid/setgid are restored only on request", verbatim: true },
          },
        ],
      }),
    },
  });
  const status = screen.getByTestId("extract-status");
  expect(status).toHaveAttribute("data-state", "completed");
  expect(status).toHaveAttribute("data-complete", "false");
  // The line is partial and says so; the list is in the Details panel.
  expect(screen.getByTestId("extract-headline")).toHaveTextContent(
    "Extracted 3 files · 1 folder · 100 bytes · 3 entries not extracted",
  );
  expect(status).toHaveClass("warning");
  expect(screen.getByTestId("extract-details")).not.toBeVisible();
  await user.click(screen.getByRole("button", { name: "Details, 2 items need attention" }));
  expect(screen.getByTestId("extract-details")).toBeVisible();
  const items = screen.getAllByTestId("extract-not-extracted-item");
  expect(items[0]).toHaveTextContent(`${HOSTILE} (NAME_UNSUPPORTED)`);
  expect(items[1]).toHaveTextContent("line\\u{a}break (escaped) (NAME_COLLISION)");
  expect(screen.getByTestId("extract-details")).toHaveTextContent("…and 1 more.");
  const notes = screen.getAllByTestId("extract-attribute-note");
  expect(notes[0]).toHaveTextContent("Not applied: owner and group, on 3 entries. First: a/b.");
  expect(notes[1]).toHaveTextContent("setuid and setgid bits");
  expect(screen.getByTestId("extract-details")).toHaveTextContent("not a verification of the archive");
  expect(container.querySelector("img")).toBeNull();
  expect(container.querySelector("[src], a[href]")).toBeNull();
});

test("a clean extraction is plain text: no success styling, no backed-up, safe, or preserved", async () => {
  const user = userEvent.setup();
  const b = backend([plan()]);
  const { container } = renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  await user.click(await screen.findByTestId("extract-confirm"));
  await screen.findByTestId("extract-running");
  b.job({ event: "started", job_id: 7 });
  b.job({ event: "finished", job_id: 7, outcome: { status: "completed", summary: "s", extract: report() } });
  expect(screen.getByTestId("extract-headline")).toHaveTextContent(/^Extracted 3 files · 1 folder · 100 bytes$/);
  expect(screen.getByTestId("extract-status")).not.toHaveClass("warning");
  expect(screen.queryByTestId("extract-badge")).toBeNull();
  expect(screen.getByTestId("extract-status")).toHaveAttribute("data-complete", "true");
  // The plan is stale once what it described has been written.
  expect(screen.queryByTestId("extract-panel")).toBeNull();
  const text = (container.textContent ?? "").toLowerCase();
  for (const banned of BANNED) expect(text).not.toContain(banned);
  expect(container.querySelector(".success, .pass, [data-status='PASS']")).toBeNull();
});

test("a failed job shows its code and that nothing was written; a job that cannot start shows the error", async () => {
  const user = userEvent.setup();
  const b = backend([plan()]);
  renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  await user.click(await screen.findByTestId("extract-confirm"));
  await screen.findByTestId("extract-running");
  b.job({ event: "started", job_id: 7 });
  b.job({
    event: "finished",
    job_id: 7,
    outcome: { status: "failed", code: "NAME_COLLISION", message: "1 entry cannot be restored here" },
  });
  const status = screen.getByTestId("extract-status");
  expect(status).toHaveTextContent("Failed (NAME_COLLISION): 1 entry cannot be restored here");
  expect(status).toHaveTextContent("Nothing was written.");
  expect(status).toHaveAttribute("data-state", "failed");
  expect(screen.getByTestId("extract-panel")).toBeInTheDocument();
});

test("a start that is refused shows the error and leaves the panel usable", async () => {
  const user = userEvent.setup();
  mockIPC((cmd) => {
    if (cmd === "plan_extract_default") return plan();
    if (cmd === "start_job") throw { code: "INVALID_ARGUMENT", message: "choose the folder to extract into first" };
    throw new Error(cmd);
  });
  renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  await user.click(await screen.findByTestId("extract-confirm"));
  expect(await screen.findByTestId("extract-error")).toHaveTextContent(
    "Could not start (INVALID_ARGUMENT): choose the folder to extract into first",
  );
  expect(screen.getByTestId("extract-panel")).toBeInTheDocument();
  expect(screen.queryByTestId("extract-running")).toBeNull();
});

test("showing another snapshot drops the plan and the last result", async () => {
  const user = userEvent.setup();
  const b = backend([plan()]);
  const { rerender } = renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  await screen.findByTestId("extract-panel");
  rerender(<Extractor archive={summary()} seq={0} selected={[]} />);
  expect(screen.queryByTestId("extract-panel")).toBeNull();
  expect(b.calls.map((c) => c.cmd)).not.toContain("start_job");
});

test("the browser's Extract takes the list's selection", async () => {
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "watch_drops":
        return null;
      case "recent_archives":
        return [];
      case "staged_sources":
        return { items: [], destination: null, default_destination: null, format: "mochi" };
      case "open_archive":
        return { status: "opened", archive: summary() };
      case "open_snapshot":
        return {
          seq: a.seq,
          commit_id: "c".repeat(64),
          entries: 1,
          root_dirs: 0,
          root_files: 1,
          catalog_source: { source: "image" },
        };
      case "list_children":
        return {
          total: 1,
          offset: 0,
          entries: [
            { key: "6162", name: { text: "ab", verbatim: true }, kind: "file", size: "3", child_dirs: 0, child_files: 0 },
          ],
        };
      case "entry_details":
        return {
          key: "6162",
          path: { text: "ab", verbatim: true },
          kind: "file",
          size: "3",
          content_hash: "00",
          file_version_id: "11",
          attributes: { state: "recorded", posix: null, windows: null, mtime: null },
        };
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  const user = userEvent.setup();
  render(<Browser />);
  // No archive: the toolbar's Extract is there but off (the Extract section itself is not mounted).
  expect(screen.getByTestId("extract-all")).toBeDisabled();
  await user.click(screen.getByTestId("open-archive"));
  await screen.findByTestId("archive-meta");
  expect(screen.getByTestId("extract-all")).toBeEnabled();
  expect(screen.getByTestId("extract-all")).toHaveAttribute("title", "Extract everything in this version");
  await user.click(await screen.findByTestId("entry"));
  expect(screen.getByTestId("extract-all")).toHaveAttribute("title", "Extract the 1 selected");
});

test("a degraded folder entry is a caution in the details: badge on the button, a neutral line", async () => {
  const user = userEvent.setup();
  const b = backend([plan()]);
  const { container } = renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  await user.click(await screen.findByTestId("extract-confirm"));
  await screen.findByTestId("extract-running");
  b.job({ event: "started", job_id: 7 });
  b.job({
    event: "finished",
    job_id: 7,
    outcome: { status: "completed", summary: "s", extract: report({ durability_note: "no directory sync" }) },
  });
  const status = screen.getByTestId("extract-status");
  // The visible line carries no durability wording; a hidden pointer is in the live region.
  expect(screen.getByTestId("extract-headline")).toHaveTextContent(/^Extracted 3 files · 1 folder · 100 bytes$/);
  expect(screen.getByTestId("extract-attention")).toHaveClass("sr-only");
  expect(screen.getByTestId("extract-attention")).toHaveTextContent("1 item needs attention, see Details");
  expect(status).toContainElement(screen.getByTestId("extract-attention"));
  expect(status).not.toHaveClass("warning");
  const toggle = screen.getByRole("button", { name: "Details, 1 item needs attention" });
  expect(screen.getByTestId("extract-badge")).toBeInTheDocument();
  await user.click(toggle);
  expect(screen.getByTestId("extract-details")).toHaveTextContent("Folder entries on disk");
  expect(screen.getByTestId("extract-details")).toHaveTextContent("Not confirmed");
  expect(screen.getByTestId("extract-details")).toHaveTextContent("no directory sync");
  expect(container.querySelector(".success, .pass, [data-status='PASS']")).toBeNull();
});

test("pausing an extraction sends pause_job by id, shows Paused, and Resume sends false", async () => {
  const user = userEvent.setup();
  const b = backend([plan()]);
  renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  await user.click(await screen.findByTestId("extract-confirm"));
  await screen.findByTestId("extract-running");
  b.job({ event: "started", job_id: 7 });
  b.job({ event: "progress", job_id: 7, phase: "restore", completed: 2, total: 5 });
  expect(screen.getByTestId("extract-running")).toHaveTextContent("Extracting…");
  expect(screen.getByTestId("extract-running")).toHaveTextContent("40% complete");

  await user.click(screen.getByTestId("extract-pause"));
  expect(b.calls.at(-1)).toEqual({ cmd: "pause_job", args: { jobId: 7, paused: true } });
  expect(screen.getByTestId("extract-running")).toHaveAttribute("data-state", "paused");
  expect(screen.getByTestId("extract-pause")).toHaveAccessibleName("Resume");

  await user.click(screen.getByTestId("extract-pause"));
  expect(b.calls.at(-1)).toEqual({ cmd: "pause_job", args: { jobId: 7, paused: false } });
  expect(screen.getByTestId("extract-running")).toHaveAttribute("data-state", "running");
});

test("a folder MOCHI will not add to says so, not that something else is in the way", async () => {
  const user = userEvent.setup();
  backend([
    plan({
      conflicts_total: 2,
      conflicts: [
        { path: { text: "docs", verbatim: true }, code: "NAME_COLLISION", reason: "r1", why: "folder_not_entered" },
        { path: { text: "X", verbatim: true }, code: "NAME_COLLISION", reason: "r2", why: "same_name_in_archive" },
      ],
    }),
  ]);
  renderExtractor();
  await user.click(screen.getByTestId("extract-all"));
  const items = await screen.findAllByTestId("extract-conflict");
  expect(items[0]).toHaveTextContent("A folder MOCHI won't add to");
  expect(items[1]).toHaveTextContent("The archive has another item with this name");
});
