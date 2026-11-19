import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import type { ForeignArchiveSummary, ForeignRow } from "../ipc/foreign";
import type { ExtractReport } from "../ipc/extract";
import type { JobEvent } from "../ipc/jobs";
import { Browser } from "../browser/Browser";
import { ForeignBrowser } from "./ForeignBrowser";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () =>
  (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

const hex = (s: string) => Array.from(new TextEncoder().encode(s), (b) => b.toString(16).padStart(2, "0")).join("");
const nth = <T,>(xs: T[], i: number): T => {
  const x = xs[i];
  if (x === undefined) throw new Error(`no item ${i}`);
  return x;
};

const HOSTILE = '<img src=x onerror="alert(1)">';
const BANNED = ["backed up", "backup", "safe", "preserved", "secure", "verified", "healthy"];

const DIMS = { recoverability: "UNSUPPORTED", freshness: "UNSUPPORTED", retention: "UNSUPPORTED" };

function summary(extra: Partial<ForeignArchiveSummary> = {}): ForeignArchiveSummary {
  return {
    handle: 5,
    file_name: "photos.7z",
    format: "7z",
    entries: 3,
    complete: true,
    stop_reason: null,
    notes: [],
    checks: "CRC32 per entry",
    opened_with_password: false,
    weak_encryption: false,
    dimensions: DIMS,
    ...extra,
  };
}

const row = (name: string, extra: Partial<ForeignRow> = {}): ForeignRow => ({
  key: hex(name),
  name: { text: name, verbatim: true },
  kind: "file",
  size: "600",
  child_dirs: 0,
  child_files: 0,
  encryption: null,
  note: null,
  ...extra,
});

const ROWS: ForeignRow[] = [
  row("docs", { kind: "directory", size: null, child_files: 2 }),
  row("a.txt"),
  row(HOSTILE),
  row("../evil", { key: null, note: "will not be extracted: the name contains \"..\" and would leave the destination" }),
];

function report(extra: Partial<ExtractReport> = {}): ExtractReport {
  return {
    destination: "/home/me/out",
    seq: 0,
    files: 2,
    directories: 1,
    bytes: 606,
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

type Err = { error: { code: string; message: string } };

/** A fake backend. `opens` are answered in order. */
function backend(opts: { opens?: (ForeignArchiveSummary | null | Err)[]; plan?: unknown } = {}) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let channel: Channel<JobEvent> | undefined;
  let nextOpen = 0;
  const seq = new Map<number, number>();
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    // The window's start-up calls (tested in their own files).
    if (cmd === "staged_sources") return { items: [], destination: null, default_destination: null, format: "mochi" };
    if (cmd === "watch_drops" || cmd === "watch_launches") return null;
    if (cmd === "recent_archives") return [];
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "open_foreign_archive": {
        const o = (opts.opens ?? [summary()])[Math.min(nextOpen++, (opts.opens ?? [1]).length - 1)];
        if (o !== null && o !== undefined && "error" in o) throw o.error;
        return o ?? null;
      }
      case "close_foreign_archive":
        return true;
      case "foreign_children": {
        const offset = Number(a.offset);
        const limit = Number(a.limit);
        return { total: ROWS.length, offset, entries: ROWS.slice(offset, offset + limit) };
      }
      case "plan_extract_foreign":
        return opts.plan ?? null;
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
    job: (m: JobEvent) => {
      if (!channel) throw new Error("no channel");
      const id = channel.id;
      const i = seq.get(id) ?? 0;
      seq.set(id, i + 1);
      act(() => internals().runCallback(id, { index: i, message: m }));
    },
    request: () => calls.find((c) => c.cmd === "start_job")?.args.request,
  };
}

afterEach(() => clearMocks());

/** The window, with a foreign archive opened from More, Other formats. */
function renderWindow() {
  return render(<Browser foreign={<ForeignBrowser />} />);
}

async function chooseOther(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByTestId("toolbar-more"));
  await user.click(screen.getByTestId("menu-foreign"));
}

async function openIt() {
  const user = userEvent.setup();
  renderWindow();
  await chooseOther(user);
  await screen.findByTestId("foreign-meta");
  return user;
}

test("an opened archive is labelled foreign, with no MOCHI health vocabulary", async () => {
  backend();
  await openIt();
  expect(screen.getByTestId("foreign-meta")).toHaveTextContent("photos.7z");
  // The format is a badge; its tooltip says what it is. No textbook in the window.
  expect(screen.getByTestId("foreign-meta").querySelector(".format-badge")).toHaveAttribute(
    "title",
    "7z archive (foreign format)",
  );
  expect(screen.getByTestId("foreign-meta")).toHaveTextContent("(read-only)");
  expect(screen.queryByText(/not a MOCHI archive/)).toBeNull();
  expect(screen.queryByText(/UNSUPPORTED/)).toBeNull();
  const text = document.body.textContent?.toLowerCase() ?? "";
  for (const w of BANNED) expect(text).not.toContain(w);
  // The test button does not say "verify". Its label is one word now (the toolbar's); what the check
  // is, and that it is not MOCHI's verification, is its tooltip (and the Info panel's note).
  const test = screen.getByTestId("foreign-test");
  expect(test).toHaveTextContent("Test");
  expect(test.textContent?.toLowerCase()).not.toContain("verif");
  expect(test).toHaveAttribute("title", expect.stringContaining("Reads every entry and runs the checks this format has"));
  expect(test).toHaveAttribute("title", expect.stringContaining("does not verify the archive the way MOCHI verifies its own"));
});

test("names are text only, and a refused name cannot be selected for extraction", async () => {
  backend();
  const user = await openIt();
  await screen.findAllByTestId("foreign-entry");
  expect(document.querySelector("img")).toBeNull();
  expect(screen.getByText(HOSTILE)).toBeInTheDocument();
  const entries = screen.getAllByTestId("foreign-entry");
  const refused = entries.find((e) => e.getAttribute("data-refused") === "true");
  expect(refused).toHaveTextContent("will not be extracted");
  await user.click(refused as HTMLElement);
  expect(refused).toHaveAttribute("aria-pressed", "false");
  expect(screen.getByTestId("foreign-count")).toHaveTextContent(/^4 items$/);
  await user.click(nth(entries, 1));
  expect(nth(entries, 1)).toHaveAttribute("aria-pressed", "true");
  expect(screen.getByTestId("foreign-count")).toHaveTextContent("4 items · 1 selected");
});

test("names that are encrypted ask for the password, which is sent once and cleared", async () => {
  const b = backend({
    opens: [
      { error: { code: "PASSWORD_REQUIRED", message: "a password is needed" } },
      { error: { code: "WRONG_PASSWORD", message: "no" } },
      summary({ opened_with_password: true }),
    ],
  });
  const user = userEvent.setup();
  renderWindow();
  await chooseOther(user);
  const note = await screen.findByTestId("foreign-password-note");
  expect(note).toHaveTextContent("names are encrypted");
  const input = screen.getByTestId("foreign-password-input") as HTMLInputElement;
  expect(input.type).toBe("password");
  await user.type(input, "first");
  await user.click(screen.getByTestId("foreign-password-submit"));
  await waitFor(() => expect(screen.getByTestId("foreign-password-note")).toHaveTextContent("did not open"));
  expect((screen.getByTestId("foreign-password-input") as HTMLInputElement).value).toBe("");
  await user.type(screen.getByTestId("foreign-password-input"), "second");
  await user.click(screen.getByTestId("foreign-password-submit"));
  await screen.findByTestId("foreign-meta");
  expect(screen.getByTestId("foreign-meta")).toHaveTextContent("opened with a password");
  const opens = b.calls.filter((c) => c.cmd === "open_foreign_archive").map((c) => c.args);
  expect(opens).toMatchObject([
    { retry: false, password: null },
    { retry: true, password: "first" },
    { retry: true, password: "second" },
  ]);
  expect(screen.queryByTestId("foreign-password")).toBeNull();
  // The password is nowhere in the document afterwards.
  expect(document.body.textContent).not.toContain("second");
});

test("a partial listing and ZipCrypto are said plainly", async () => {
  backend({
    opens: [
      summary({
        format: "ZIP",
        complete: false,
        stop_reason: { text: "entry count 5 exceeds the limit 2", verbatim: true },
        weak_encryption: true,
        notes: ["LIMIT_ENTRY_COUNT: limit 2, observed 5"],
      }),
    ],
  });
  await openIt();
  expect(screen.getByTestId("foreign-partial")).toHaveTextContent("Partial listing");
  expect(screen.getByTestId("foreign-weak")).toHaveTextContent("ZipCrypto");
  expect(screen.getByTestId("foreign-notes")).toHaveTextContent("LIMIT_ENTRY_COUNT");
});

test("a read-and-check job reports the checks that ran in the format's words", async () => {
  const b = backend();
  const user = await openIt();
  await user.click(screen.getByTestId("foreign-test"));
  await waitFor(() => expect(b.request()).toEqual({ kind: "test_foreign", handle: 5 }));
  b.job({ event: "started", job_id: 9 });
  b.job({ event: "progress", job_id: 9, phase: "foreign_test", completed: 2, total: 3 });
  expect(await screen.findByTestId("foreign-progress")).toHaveTextContent("Reading: 2 of 3 entries");
  b.job({
    event: "finished",
    job_id: 9,
    outcome: {
      status: "completed",
      summary: "read 3 entries",
      foreign_test: {
        format: "7z",
        entries: 3,
        checks: [
          { what: "CRC32 matched", count: 2 },
          { what: "CRC32 did NOT match", count: 1 },
        ],
        problems: [{ path: { text: "b.txt", verbatim: true }, what: { text: "CRC32 did NOT match", verbatim: true } }],
        problems_total: 1,
        stream_check: "no whole-stream check in this format",
        complete: true,
        stop_reason: null,
        bytes_read: 606,
        dimensions: DIMS,
      },
    },
  });
  const status = await screen.findByTestId("foreign-status");
  expect(within(status).getByTestId("foreign-headline")).toHaveTextContent(
    "Read 3 entries of the 7z archive: 1 problem.",
  );
  // The checks that ran, and what failed, are in Details.
  await user.click(screen.getByTestId("foreign-details-toggle"));
  const details = screen.getByTestId("foreign-details");
  const counts = within(details).getAllByTestId("foreign-check-count").map((e) => e.textContent);
  expect(counts).toEqual(["2 × CRC32 matched", "1 × CRC32 did NOT match"]);
  expect(within(details).getByTestId("foreign-problem")).toHaveTextContent("b.txt");
  expect(details).toHaveTextContent("does not verify the archive the way MOCHI verifies its own");
  expect(status).not.toHaveTextContent(/\bPASS\b/);
  const text = document.body.textContent?.toLowerCase() ?? "";
  for (const w of ["backed up", "backup", "preserved", "secure"]) expect(text).not.toContain(w);
});

test("extraction shows what cannot be extracted first, then skips it and lists what was skipped", async () => {
  const b = backend({
    plan: {
      plan: {
        destination: "/home/me/out",
        entries: 3,
        existing: [],
        existing_total: 0,
        merged_folders: 0,
        conflicts: [
          { path: { text: "../evil", verbatim: true }, code: "NAME_UNSUPPORTED", reason: "the name contains \"..\"", why: "name_not_allowed" },
        ],
        conflicts_total: 1,
        case_insensitive: false,
      },
      skipped_kinds: 2,
    },
  });
  const user = await openIt();
  await user.click(screen.getByTestId("foreign-extract-all"));
  await screen.findByTestId("foreign-plan");
  // The folder is named in the question; its whole path is the tooltip.
  expect(screen.getByTestId("foreign-choices-title")).toHaveTextContent("Some items can't be extracted to “out”");
  expect(screen.getByTestId("foreign-choices-title")).toHaveAttribute("title", "/home/me/out");
  expect(screen.getByTestId("foreign-skipped-kinds")).toHaveTextContent("2 entries are a link or special file");
  expect(screen.getByTestId("foreign-conflict")).toHaveTextContent("This computer doesn't allow this name");
  expect(screen.getByTestId("foreign-plan")).toHaveTextContent("Nothing is overwritten");
  await user.click(screen.getByTestId("foreign-extract-confirm"));
  await waitFor(() =>
    expect(b.request()).toEqual({ kind: "extract_foreign", handle: 5, keys: [], on_conflict: "add_new" }),
  );
  b.job({ event: "started", job_id: 9 });
  b.job({
    event: "finished",
    job_id: 9,
    outcome: {
      status: "completed",
      summary: "extracted 2 files",
      extract: report({
        not_extracted: [
          { path: { text: "../evil", verbatim: true }, code: "NAME_UNSUPPORTED", reason: { text: "not extracted: the name contains \"..\"", verbatim: true } },
        ],
        not_extracted_total: 1,
      }),
    },
  });
  const status = await screen.findByTestId("foreign-status");
  expect(status).toHaveTextContent("Extracted 2 files");
  expect(status).toHaveTextContent("1 entry was not extracted");
  expect(screen.getByTestId("foreign-not-extracted-item")).toHaveTextContent("../evil");
});

test("extracting the selected entry sends its key; cancelling is offered while it runs", async () => {
  const b = backend({
    plan: {
      plan: { destination: "/o", entries: 1, existing: [], existing_total: 0, merged_folders: 0, conflicts: [], conflicts_total: 0, case_insensitive: false },
      skipped_kinds: 0,
    },
  });
  const user = await openIt();
  const entries = await screen.findAllByTestId("foreign-entry");
  await user.click(nth(entries, 1));
  await user.click(screen.getByTestId("foreign-extract-all"));
  await screen.findByTestId("foreign-plan");
  await user.click(screen.getByTestId("foreign-extract-confirm"));
  await waitFor(() =>
    expect(b.request()).toEqual({
      kind: "extract_foreign",
      handle: 5,
      keys: [hex("a.txt")],
      on_conflict: "stop",
    }),
  );
  b.job({ event: "started", job_id: 9 });
  b.job({ event: "progress", job_id: 9, phase: "restore", completed: 1, total: 4 });
  expect(await screen.findByTestId("foreign-progress")).toHaveTextContent("Extracting: 1 of 4 entries");
  await user.click(screen.getByTestId("foreign-cancel"));
  expect(b.calls.some((c) => c.cmd === "cancel_job")).toBe(true);
  b.job({ event: "finished", job_id: 9, outcome: { status: "cancelled" } });
  expect(await screen.findByTestId("foreign-headline")).toHaveTextContent("Cancelled");
});

test("an opened archive fills the window, and rows get icons from a fixed set", async () => {
  backend();
  const user = userEvent.setup();
  const { container } = renderWindow();
  await chooseOther(user);
  await screen.findAllByTestId("foreign-entry");
  // The archive is drawn in the window's own area; the start screen steps aside.
  expect(screen.queryByTestId("start-screen")).toBeNull();
  const entries = screen.getAllByTestId("foreign-entry");
  expect(entries.map((e) => e.querySelector(".type")?.textContent)).toEqual(["Folder", "TXT", "File", "File"]);
  expect(container.querySelector("img")).toBeNull();
  for (const icon of container.querySelectorAll("svg")) expect(icon).toHaveAttribute("aria-hidden", "true");
  expect(container.querySelectorAll('[data-testid="foreign-entry"] svg')).toHaveLength(entries.length);
});
