// Extract to... (the toolbar button and the list menu's entry): the window with its folder field,
// Browse..., recent folders and the new-folder choice, for MOCHI and for foreign archives; and the
// menu items and buttons that must be off in read-only archives. The window sends the folder text
// (Rust validates it); the job request still carries only handle, snapshot, keys.
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import { App } from "../App";
import type { ArchiveSummary, EntryRow } from "../ipc/browse";
import type { ExtractPlan, ExtractReport } from "../ipc/extract";
import type { ForeignArchiveSummary, ForeignRow } from "../ipc/foreign";
import type { JobEvent } from "../ipc/jobs";
import { Extractor } from "./Extractor";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () => (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

const hex = (s: string) => Array.from(new TextEncoder().encode(s), (b) => b.toString(16).padStart(2, "0")).join("");
const row = (name: string, kind: "file" | "directory" = "file"): EntryRow => ({
  key: hex(name),
  name: { text: name, verbatim: true },
  kind,
  size: "10",
  child_dirs: 0,
  child_files: 0,
});
const ROOT = [row("docs", "directory"), row("a.txt"), row("b.txt")];

function mochi(commits = 2, encrypted = false): ArchiveSummary {
  return {
    handle: 3,
    file_name: "tools.mochi",
    archive_id: "ab".repeat(16),
    profile: { tar_compatible: false, encrypted, redundancy_percent: null },
    commits: Array.from({ length: commits }, (_, seq) => ({
      seq,
      commit_id: String(seq).repeat(64),
      time: null,
      checkpoint: true,
      retained: true,
      expired: false,
      holds: [],
    })),
    retention_error: null,
  };
}

const FOREIGN: ForeignArchiveSummary = {
  handle: 9,
  file_name: "photos.tar.gz",
  format: "tar.gz",
  entries: 2,
  complete: true,
  stop_reason: null,
  notes: [],
  checks: "CRC32 per entry",
  opened_with_password: false,
  weak_encryption: false,
  dimensions: { recoverability: "UNSUPPORTED", freshness: "UNSUPPORTED", retention: "UNSUPPORTED" },
};
const FOREIGN_ROWS: ForeignRow[] = ["x.txt", "y.txt"].map((n) => ({ ...row(n), encryption: null, note: null }));

const PLAN: ExtractPlan = {
  destination: "/home/me/Desktop/tools",
  entries: 3,
  existing: [],
  existing_total: 0,
  merged_folders: 0,
  conflicts: [],
  conflicts_total: 0,
  case_insensitive: false,
};
const REPORT: ExtractReport = {
  destination: "/home/me/Desktop/tools",
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
};

interface Opts {
  commits?: number;
  /** What the window opens with. */
  setup?: { destination: string; recent: string[]; open_folder?: boolean };
  /** Holds the window's starting folder back until it resolves (a slow answer). */
  setupLate?: Promise<unknown>;
  /** What Rust says the typed folder would do; by default it exists. */
  preview?: (folder: string, newFolder: boolean) => unknown;
  /** The folder dialog's answers, one per Browse click. */
  browse?: (string | null)[];
  toPlan?: ExtractPlan | { error: { code: string; message: string } };
  foreignPlan?: { plan: ExtractPlan; skipped_kinds: number };
}

function backend(opts: Opts = {}) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let channel: Channel<JobEvent> | undefined;
  let sent = 0;
  let browsed = 0;
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "staged_sources":
        return { items: [], destination: null, default_destination: null, format: "mochi" };
      case "watch_drops":
      case "watch_launches":
        return null;
      case "recent_archives":
        return [];
      case "get_crash_settings":
        return { enabled: false, include_details: false, entries: 0, bytes: 0 };
      case "get_update_settings":
        return {
          check_on_start: false,
          current_version: "0.1.0",
          install_mode: "manual",
          download_page: "https://github.com/immerh8/MOCHI/releases/latest",
          needs_admin_prompt: false,
        };
    }
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "open_archive":
        return { status: "opened", archive: mochi(opts.commits ?? 2) };
      case "open_snapshot":
        return { seq: a.seq, commit_id: "c".repeat(64), entries: 437, root_dirs: 1, root_files: 2, catalog_source: { source: "image" } };
      case "list_children": {
        const pool = a.dir ? [] : ROOT;
        const offset = a.offset as number;
        return { total: pool.length, offset, entries: pool.slice(offset, offset + (a.limit as number)) };
      }
      case "entry_details":
        return {
          key: a.key,
          path: { text: "a.txt", verbatim: true },
          kind: "file",
          size: "10",
          content_hash: "d".repeat(64),
          file_version_id: "e".repeat(64),
          attributes: { state: "recorded", posix: null, windows: null, mtime: null },
        };
      case "extract_wizard_setup":
        if (opts.setupLate) return opts.setupLate;
        return { open_folder: true, ...(opts.setup ?? { destination: "/home/me/Desktop", recent: ["/home/me/Desktop"] }) };
      case "preview_extract_to":
        return opts.preview
          ? opts.preview(a.folder as string, a.newFolder as boolean)
          : { state: "ready", creates: false, target: a.folder, taken: null };
      case "set_open_folder_when_done":
        return a.open;
      case "browse_extract_folder":
        return (opts.browse ?? [null])[browsed++] ?? null;
      case "plan_extract_to": {
        const p = opts.toPlan ?? PLAN;
        if ("error" in p) throw p.error;
        return p;
      }
      case "plan_extract_foreign_to":
        return opts.foreignPlan ?? { plan: { ...PLAN, destination: "/home/me/Desktop/photos" }, skipped_kinds: 0 };
      case "start_job":
        channel = a.onEvent as Channel<JobEvent>;
        return 7;
      case "open_foreign_archive":
        return FOREIGN;
      case "foreign_children":
        return { total: FOREIGN_ROWS.length, offset: a.offset, entries: FOREIGN_ROWS };
      case "discard_extract_folder":
      case "show_in_folder":
        return null;
      case "close_archive":
      case "close_foreign_archive":
        return true;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return {
    calls,
    cmds: () => calls.map((c) => c.cmd),
    args: (cmd: string) => calls.find((c) => c.cmd === cmd)?.args,
    request: () => calls.find((c) => c.cmd === "start_job")?.args.request,
    job: (m: JobEvent) => {
      if (!channel) throw new Error("no channel yet");
      const id = channel.id;
      act(() => internals().runCallback(id, { index: sent++, message: m }));
    },
  };
}

afterEach(() => clearMocks());

async function openMochi(opts: Opts = {}) {
  const user = userEvent.setup();
  const b = backend(opts);
  render(<App />);
  await user.click(await screen.findByTestId("open-archive"));
  await screen.findByText(/^3 items/);
  return { user, b };
}

async function openForeign(opts: Opts = {}) {
  const user = userEvent.setup();
  const b = backend(opts);
  render(<App />);
  await screen.findByTestId("start-screen");
  await user.click(screen.getByTestId("toolbar-more"));
  await user.click(screen.getByTestId("menu-foreign"));
  await screen.findByTestId("foreign-meta");
  await waitFor(() => expect(screen.getByTestId("foreign-count")).toHaveTextContent(/^2 items$/));
  return { user, b };
}

// ---- The toolbar button, MOCHI --------------------------------------------------------------

async function openWizard(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByTestId("extract-to"));
  return screen.findByTestId("extract-to-dialog");
}

const field = () => screen.getByTestId("extract-to-folder") as HTMLInputElement;

test("the button is off with nothing open, and says to open an archive", async () => {
  backend();
  render(<App />);
  await screen.findByTestId("start-screen");
  const button = screen.getByTestId("extract-to");
  expect(button).toBeDisabled();
  expect(button).toHaveAttribute("title", "Open an archive first");
  expect(button).toHaveTextContent("Extract to…");
});

test("the window opens prefilled: the folder Rust chose, the recent folders newest first, a new folder ticked", async () => {
  const { user, b } = await openMochi({
    setup: { destination: "/data/out", recent: ["/data/out", "/home/me/Desktop", "/mnt/usb"] },
  });
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/data/out"));
  expect(screen.getByTestId("extract-to-new-folder")).toBeChecked();
  expect(screen.getAllByTestId("extract-to-recent-item").map((e) => e.textContent)).toEqual([
    "/data/out",
    "/home/me/Desktop",
    "/mnt/usb",
  ]);
  expect(screen.getByTestId("extract-to-what")).toHaveTextContent("Everything in this version");
  // Opening it extracts nothing and plans nothing.
  expect(b.cmds()).not.toContain("plan_extract_to");
  expect(b.cmds()).not.toContain("start_job");
});

test("with no history the window starts on the Desktop Rust resolved, with no recent list", async () => {
  const { user } = await openMochi({ setup: { destination: "/home/me/Desktop", recent: [] } });
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  expect(screen.queryByTestId("extract-to-recent")).toBeNull();
});

test("Enter confirms: everything is extracted into a new folder at once, with no plan panel", async () => {
  const { user, b } = await openMochi();
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  expect(field()).toHaveFocus();
  await user.keyboard("{Enter}");
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  expect(b.args("plan_extract_to")).toEqual({
    handle: 3,
    seq: 1,
    keys: [],
    folder: "/home/me/Desktop",
    newFolder: true,
  });
  expect(b.request()).toEqual({ kind: "extract_archive", handle: 3, seq: 1, keys: [], on_conflict: "stop" });
  for (const other of ["plan_extract", "plan_extract_default"]) expect(b.cmds()).not.toContain(other);
  expect(screen.queryByTestId("extract-panel")).toBeNull();
  expect(screen.queryByTestId("extract-to-dialog")).toBeNull();
});

test("a typed folder, and the new-folder choice, are what Rust is asked to plan", async () => {
  const { user, b } = await openMochi();
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.clear(field());
  await user.type(field(), "/srv/exports");
  await user.click(screen.getByTestId("extract-to-new-folder"));
  await user.click(screen.getByTestId("extract-to-ok"));
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  expect(b.args("plan_extract_to")).toMatchObject({ folder: "/srv/exports", newFolder: false });
});

test("a recent folder fills the field", async () => {
  const { user, b } = await openMochi({
    setup: { destination: "/data/out", recent: ["/data/out", "/mnt/usb"] },
  });
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/data/out"));
  await user.click(screen.getAllByTestId("extract-to-recent-item")[1] as HTMLElement);
  expect(field()).toHaveValue("/mnt/usb");
  await user.click(screen.getByTestId("extract-to-ok"));
  await waitFor(() => expect(b.args("plan_extract_to")).toMatchObject({ folder: "/mnt/usb" }));
});

test("Browse... fills the field from the folder dialog, starting where the field points; cancelling keeps it", async () => {
  const { user, b } = await openMochi({ browse: ["/picked/folder", null] });
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.click(screen.getByTestId("extract-to-browse"));
  await waitFor(() => expect(field()).toHaveValue("/picked/folder"));
  expect(b.args("browse_extract_folder")).toMatchObject({ start: "/home/me/Desktop" });
  await user.click(screen.getByTestId("extract-to-browse"));
  await waitFor(() => expect(b.calls.filter((c) => c.cmd === "browse_extract_folder")).toHaveLength(2));
  expect(field()).toHaveValue("/picked/folder");
  expect(b.cmds()).not.toContain("plan_extract_to");
});

test("with a selection it extracts the selection", async () => {
  const { user, b } = await openMochi();
  await user.click((await screen.findAllByTestId("entry"))[1] as HTMLElement);
  await waitFor(() => expect(screen.getByTestId("extract-to")).toHaveAttribute("title", expect.stringContaining("1 selected")));
  await openWizard(user);
  expect(screen.getByTestId("extract-to-what")).toHaveTextContent("The selected entry will be extracted.");
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.keyboard("{Enter}");
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  expect(b.args("plan_extract_to")).toMatchObject({ keys: [hex("a.txt")] });
  expect(b.request()).toMatchObject({ kind: "extract_archive", keys: [hex("a.txt")] });
});

test("Cancel and Escape close the window and plan nothing", async () => {
  const { user, b } = await openMochi();
  await openWizard(user);
  await user.click(screen.getByTestId("extract-to-cancel"));
  expect(screen.queryByTestId("extract-to-dialog")).toBeNull();
  await openWizard(user);
  await user.keyboard("{Escape}");
  expect(screen.queryByTestId("extract-to-dialog")).toBeNull();
  expect(b.cmds()).not.toContain("plan_extract_to");
});

test("a folder Rust refuses is explained in the window, which stays open; nothing starts", async () => {
  const { user, b } = await openMochi({
    toPlan: { error: { code: "INVALID_ARGUMENT", message: "that folder does not exist; choose one that does" } },
  });
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.keyboard("{Enter}");
  const error = await screen.findByTestId("extract-to-error");
  expect(error).toHaveTextContent("Could not use that folder");
  expect(error).toHaveTextContent("does not exist");
  expect(screen.getByTestId("extract-to-dialog")).toBeInTheDocument();
  expect(b.cmds()).not.toContain("start_job");
  expect(screen.getByTestId("extract-to-ok")).toBeEnabled();
});

test("files that already exist are listed first; Add only new files extracts the rest and keeps them", async () => {
  const { user, b } = await openMochi({
    toPlan: { ...PLAN, existing_total: 1, existing: [{ text: "a.txt", verbatim: true }] },
  });
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.click(screen.getByTestId("extract-to-new-folder"));
  await user.keyboard("{Enter}");
  await screen.findByTestId("extract-panel");
  expect(screen.getByTestId("extract-choices-title")).toHaveTextContent("“a.txt” already exists in “tools”");
  expect(screen.getByTestId("extract-existing")).toHaveTextContent("a.txt");
  expect(b.cmds()).not.toContain("start_job");
  await user.click(screen.getByTestId("extract-add-new"));
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  expect(b.request()).toMatchObject({ on_conflict: "add_new" });
});

test("Extract to a new folder instead plans a new folder named after the archive inside the same one", async () => {
  const { user, b } = await openMochi({
    toPlan: { ...PLAN, existing_total: 1, existing: [{ text: "a.txt", verbatim: true }] },
  });
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.click(screen.getByTestId("extract-to-new-folder"));
  await user.keyboard("{Enter}");
  await user.click(await screen.findByTestId("extract-new-folder"));
  await waitFor(() => expect(b.calls.filter((c) => c.cmd === "plan_extract_to")).toHaveLength(2));
  expect(b.calls.filter((c) => c.cmd === "plan_extract_to").at(-1)?.args).toMatchObject({
    folder: "/home/me/Desktop/tools",
    newFolder: true,
  });
});
test("the normal result follows, with a way to open the folder (which names no path)", async () => {
  const { user, b } = await openMochi();
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.keyboard("{Enter}");
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  b.job({ event: "started", job_id: 7 });
  b.job({ event: "finished", job_id: 7, outcome: { status: "completed", summary: "s", extract: REPORT } });
  expect(screen.getByTestId("extract-headline")).toHaveTextContent(/^Extracted 3 files · 1 folder · 100 bytes$/);
  await user.click(screen.getByTestId("extract-show-folder"));
  expect(b.args("show_in_folder")).toEqual({ what: { kind: "extract_destination" } });
  expect(b.cmds()).not.toContain("discard_extract_folder");
});

test("a cancelled run removes the empty new folder it made, but never an existing folder", async () => {
  const { user, b } = await openMochi();
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.keyboard("{Enter}");
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  b.job({ event: "started", job_id: 7 });
  b.job({ event: "finished", job_id: 7, outcome: { status: "cancelled" } });
  await waitFor(() => expect(b.cmds()).toContain("discard_extract_folder"));
  expect(screen.getByTestId("extract-status")).toHaveAttribute("data-state", "cancelled");
});

test("with the new folder unticked a cancelled run discards nothing", async () => {
  const { user, b } = await openMochi();
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.click(screen.getByTestId("extract-to-new-folder"));
  await user.keyboard("{Enter}");
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  b.job({ event: "started", job_id: 7 });
  b.job({ event: "finished", job_id: 7, outcome: { status: "cancelled" } });
  await screen.findByTestId("extract-status");
  expect(b.cmds()).not.toContain("discard_extract_folder");
});

test("the Extractor draws the button in place when it is used alone", async () => {
  const b = backend();
  const user = userEvent.setup();
  render(<Extractor archive={mochi()} seq={1} selected={[]} />);
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.keyboard("{Enter}");
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  expect(b.args("plan_extract_to")).toMatchObject({ handle: 3, seq: 1, keys: [] });
});

// ---- The list menu --------------------------------------------------------------------------

test("the list menu has Extract to..., on with nothing selected (it takes the whole archive)", async () => {
  const { user, b } = await openMochi();
  fireEvent.contextMenu(screen.getByTestId("entries"));
  const item = await screen.findByTestId("menu-extract-to");
  expect(item).not.toHaveAttribute("aria-disabled");
  expect(screen.queryByTestId("menu-extract-desktop")).toBeNull();
  await user.click(item);
  await screen.findByTestId("extract-to-dialog");
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.keyboard("{Enter}");
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  expect(b.args("plan_extract_to")).toMatchObject({ handle: 3, seq: 1, keys: [] });
});

test("the list menu's Extract to... takes the right-clicked selection", async () => {
  const { user, b } = await openMochi();
  fireEvent.contextMenu((await screen.findAllByTestId("entry"))[2] as HTMLElement);
  await user.click(await screen.findByTestId("menu-extract-to"));
  await screen.findByTestId("extract-to-dialog");
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.keyboard("{Enter}");
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  expect(b.args("plan_extract_to")).toMatchObject({ keys: [hex("b.txt")] });
});

// ---- Foreign archives -----------------------------------------------------------------------

test("a foreign archive extracts through the same window and its own plan, and shows the folder afterwards", async () => {
  const { user, b } = await openForeign();
  const button = screen.getByTestId("foreign-extract-to");
  expect(button).toBeEnabled();
  await user.click(button);
  await screen.findByTestId("extract-to-dialog");
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.keyboard("{Enter}");
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  expect(b.args("plan_extract_foreign_to")).toEqual({
    handle: 9,
    keys: [],
    folder: "/home/me/Desktop",
    newFolder: true,
  });
  expect(b.request()).toEqual({ kind: "extract_foreign", handle: 9, keys: [], on_conflict: "stop" });
  expect(b.cmds()).not.toContain("plan_extract_foreign");
  expect(screen.queryByTestId("foreign-plan")).toBeNull();
  b.job({ event: "started", job_id: 7 });
  b.job({ event: "finished", job_id: 7, outcome: { status: "completed", summary: "s", extract: REPORT } });
  await user.click(await screen.findByTestId("foreign-show-folder"));
  expect(b.args("show_in_folder")).toEqual({ what: { kind: "extract_destination" } });
});

test("a foreign archive's existing files are listed before anything is written, and Replace is offered", async () => {
  const { user, b } = await openForeign({
    foreignPlan: {
      plan: {
        ...PLAN,
        existing_total: 1,
        existing: [{ text: "x.txt", verbatim: true }],
      },
      skipped_kinds: 0,
    },
  });
  await user.click(screen.getByTestId("foreign-extract-to"));
  await screen.findByTestId("extract-to-dialog");
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.click(screen.getByTestId("extract-to-new-folder"));
  await user.keyboard("{Enter}");
  await screen.findByTestId("foreign-plan");
  expect(screen.getByTestId("foreign-existing")).toHaveTextContent("x.txt");
  expect(b.cmds()).not.toContain("start_job");
  await user.click(screen.getByTestId("foreign-replace"));
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  expect(b.request()).toMatchObject({ kind: "extract_foreign", on_conflict: "replace" });
});

test("a foreign archive's list menu: Extract to... is on, Add files and Delete are off and say why", async () => {
  const { user, b } = await openForeign();
  await user.click((await screen.findAllByTestId("foreign-entry"))[0] as HTMLElement);
  fireEvent.contextMenu(screen.getByTestId("foreign-entries"));
  await screen.findByRole("menu");
  expect(screen.getByTestId("menu-extract-to")).not.toHaveAttribute("aria-disabled");
  for (const id of ["add", "delete"]) {
    const item = screen.getByTestId(`menu-${id}`);
    expect(item, id).toHaveAttribute("aria-disabled", "true");
    expect(item, id).toHaveAttribute("title", "Archives in other formats are read-only");
  }
  // Choosing a disabled item does nothing.
  await user.click(screen.getByTestId("menu-add"));
  expect(b.cmds()).not.toContain("stage_sources");
  expect(screen.getByRole("menu")).toBeInTheDocument();
  await user.click(screen.getByTestId("menu-extract-to"));
  await screen.findByTestId("extract-to-dialog");
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.keyboard("{Enter}");
  await waitFor(() => expect(b.args("plan_extract_foreign_to")).toMatchObject({ handle: 9, keys: [hex("x.txt")] }));
});

// ---- The rubber band, in the real browsers (the mechanics are in list/marquee.test.tsx) -------

/** jsdom has no layout: give the scrolling list a size (200 x 300) and a position. */
function sizeList(list: HTMLElement) {
  Object.defineProperty(list, "clientWidth", { configurable: true, value: 185 });
  Object.defineProperty(list, "clientHeight", { configurable: true, value: 300 });
  list.getBoundingClientRect = () =>
    ({ left: 0, top: 0, right: 200, bottom: 300, width: 200, height: 300, x: 0, y: 0, toJSON: () => ({}) }) as DOMRect;
}

function pointer(target: Element, type: string, y: number, init: PointerEventInit = {}) {
  act(() => {
    target.dispatchEvent(
      new PointerEvent(type, {
        bubbles: true,
        cancelable: true,
        button: 0,
        buttons: type === "pointerup" ? 0 : 1,
        clientX: 60,
        clientY: y,
        pointerId: 5,
        pointerType: "mouse",
        ...init,
      }),
    );
  });
}

const pressed = () => screen.getAllByTestId("entry").map((e) => e.getAttribute("aria-pressed") === "true");

test("dragging a band over the MOCHI list selects the rows it touches; Ctrl toggles; a click on empty space clears", async () => {
  await openMochi();
  const list = screen.getByTestId("entries");
  sizeList(list);
  // The three rows end at 78 px; 200 px is empty space below them.
  pointer(list, "pointerdown", 200);
  pointer(list, "pointermove", 40);
  await waitFor(() => expect(pressed()).toEqual([false, true, true]));
  expect(screen.getByTestId("entry-count")).toHaveTextContent("3 items · 2 selected");
  pointer(list, "pointerup", 40);
  expect(screen.queryByTestId("marquee-band")).toBeNull();
  // Ctrl + a band over the first two rows: the second was selected, so it is toggled off.
  pointer(list, "pointerdown", 200, { ctrlKey: true });
  pointer(list, "pointermove", 10, { ctrlKey: true });
  await waitFor(() => expect(pressed()).toEqual([true, false, false]));
  pointer(list, "pointerup", 10, { ctrlKey: true });
  // A plain click on empty space selects nothing.
  pointer(list, "pointerdown", 250);
  pointer(list, "pointerup", 250);
  await waitFor(() => expect(pressed()).toEqual([false, false, false]));
  expect(screen.getByTestId("entry-count")).toHaveTextContent(/^3 items$/);
});

test("a band on a foreign list selects rows too", async () => {
  await openForeign();
  const list = screen.getByTestId("foreign-entries");
  sizeList(list);
  pointer(list, "pointerdown", 200);
  pointer(list, "pointermove", 5);
  await waitFor(() => expect(screen.getByTestId("foreign-count")).toHaveTextContent("2 items · 2 selected"));
  pointer(list, "pointerup", 5);
});

// ---- Menus and buttons that cannot work are off (and still visible) --------------------------

test("an older version is read-only: Add and Remove are off in the toolbar and the list menu, with the reason", async () => {
  const { user } = await openMochi();
  await user.click(screen.getByTestId("toggle-versions"));
  await user.click(screen.getByTestId("commit-0"));
  await waitFor(() => expect(screen.getByTestId("toggle-versions")).toHaveTextContent(/^v0$/));
  await user.click((await screen.findAllByTestId("entry"))[1] as HTMLElement);
  for (const id of ["add-to-archive", "remove-entry"]) {
    expect(screen.getByTestId(id), id).toBeDisabled();
    expect(screen.getByTestId(id), id).toHaveAttribute("title", "Only the latest version can be changed");
  }
  fireEvent.contextMenu(screen.getByTestId("entries"));
  await screen.findByRole("menu");
  for (const id of ["add", "delete"]) {
    expect(screen.getByTestId(`menu-${id}`), id).toHaveAttribute("aria-disabled", "true");
  }
  for (const id of ["extract-to", "copy-names", "select-all"]) {
    expect(screen.getByTestId(`menu-${id}`), id).not.toHaveAttribute("aria-disabled");
  }
});

test("in the latest version Add is on in the menu and opens the same panel as the toolbar's", async () => {
  const { user } = await openMochi();
  fireEvent.contextMenu(screen.getByTestId("entries"));
  const add = await screen.findByTestId("menu-add");
  expect(add).not.toHaveAttribute("aria-disabled");
  await user.click(add);
  expect(await screen.findByRole("group", { name: "Add to archive" })).toBeInTheDocument();
});

test("the Delete key does nothing in a read-only archive; Passphrases waits for an encrypted MOCHI archive", async () => {
  const { user, b } = await openForeign();
  await user.click((await screen.findAllByTestId("foreign-entry"))[0] as HTMLElement);
  await user.keyboard("{Delete}");
  expect(screen.queryByTestId("delete-panel")).toBeNull();
  expect(b.cmds()).not.toContain("start_job");
  await user.click(screen.getByTestId("toolbar-more"));
  expect(screen.getByTestId("menu-passphrases")).toBeDisabled();
});

// ---- Owner request 2026-11-10: the Desktop by default, a folder that does not exist is
// created, what OK will do is said as the folder is typed, and the folder opens when done.

test("the line under the field says, as the folder is typed, whether it exists, will be created, or gets a numbered new folder", async () => {
  const { user, b } = await openMochi({
    preview: (folder, newFolder) => {
      if (folder === "") return { state: "invalid", reason: "enter the folder to extract into" };
      if (folder === "/home/me/Desktop/new") return { state: "ready", creates: true, target: folder, taken: null };
      if (newFolder) return { state: "ready", creates: false, target: `${folder}/tools (2)`, taken: "tools" };
      return { state: "ready", creates: false, target: folder, taken: null };
    },
  });
  await openWizard(user);
  const line = () => screen.getByTestId("extract-to-preview");
  // A new folder named after the archive, whose name is taken on the Desktop: it gets a number.
  await waitFor(() =>
    expect(line()).toHaveTextContent("“tools” already exists here, so files go into a new folder: “tools (2)”."),
  );
  expect(b.args("preview_extract_to")).toEqual({
    folder: "/home/me/Desktop",
    newFolder: true,
    archive: { kind: "mochi", handle: 3 },
  });
  // Straight into the folder.
  await user.click(screen.getByTestId("extract-to-new-folder"));
  await waitFor(() => expect(line()).toHaveTextContent("Files go straight into this folder."));
  // The whole path is the tooltip.
  expect(line()).toHaveAttribute("title", "/home/me/Desktop");
  // A folder that does not exist yet: OK creates it.
  await user.type(field(), "/new");
  await waitFor(() =>
    expect(line()).toHaveTextContent("This folder doesn't exist yet. OK creates it."),
  );
  expect(line()).toHaveAttribute("aria-live", "polite");
  // Text that will not do says why, before OK.
  await user.clear(field());
  await waitFor(() => expect(line()).toHaveTextContent("enter the folder to extract into"));
  expect(line()).toHaveAttribute("data-state", "invalid");
  // Looking is not planning: nothing was made or started.
  expect(b.cmds()).not.toContain("plan_extract_to");
  expect(b.cmds()).not.toContain("start_job");
});

test("a folder that does not exist yet is sent as typed; Rust creates it when it plans", async () => {
  const { user, b } = await openMochi({
    preview: (folder) => ({ state: "ready", creates: folder !== "/home/me/Desktop", target: folder, taken: null }),
  });
  await openWizard(user);
  await waitFor(() => expect(field()).toHaveValue("/home/me/Desktop"));
  await user.clear(field());
  await user.type(field(), "/home/me/Desktop/Trip photos");
  await user.click(screen.getByTestId("extract-to-new-folder"));
  await user.click(screen.getByTestId("extract-to-ok"));
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  expect(b.args("plan_extract_to")).toMatchObject({ folder: "/home/me/Desktop/Trip photos", newFolder: false });
});

test("Open the folder when done shows the saved choice, saves a change, and the result says the folder opened", async () => {
  const { user, b } = await openMochi({ setup: { destination: "/home/me/Desktop", recent: [], open_folder: false } });
  await openWizard(user);
  const box = screen.getByTestId("extract-to-open-folder");
  await waitFor(() => expect(box).not.toBeChecked());
  await user.click(box);
  expect(box).toBeChecked();
  expect(b.calls.filter((c) => c.cmd === "set_open_folder_when_done").map((c) => c.args)).toEqual([{ open: true }]);
  await user.click(screen.getByTestId("extract-to-ok"));
  await waitFor(() => expect(b.cmds()).toContain("start_job"));
  b.job({ event: "started", job_id: 7 });
  b.job({
    event: "finished",
    job_id: 7,
    outcome: { status: "completed", summary: "s", extract: { ...REPORT, opened_folder: true } },
  });
  expect(await screen.findByText("The folder was opened in the file manager.")).toBeInTheDocument();
  // Rust opened it: the window asked for nothing more.
  expect(b.cmds()).not.toContain("show_in_folder");
});

test("a foreign archive's preview names that archive", async () => {
  const { user, b } = await openForeign();
  await user.click(screen.getByTestId("foreign-extract-to"));
  await screen.findByTestId("extract-to-dialog");
  await waitFor(() => expect(b.args("preview_extract_to")).toMatchObject({ archive: { kind: "foreign", handle: 9 } }));
});

test("a folder typed before the starting folder arrives is kept, not replaced", async () => {
  let release: (v: unknown) => void = () => undefined;
  const late = new Promise((r) => {
    release = r;
  });
  const { user } = await openMochi({ setupLate: late });
  await user.click(screen.getByTestId("extract-to"));
  await screen.findByTestId("extract-to-dialog");
  await user.type(field(), "/srv/mine");
  await act(async () => {
    release({ destination: "/home/me/Desktop", recent: [], open_folder: true });
    await late;
  });
  expect(field()).toHaveValue("/srv/mine");
});
