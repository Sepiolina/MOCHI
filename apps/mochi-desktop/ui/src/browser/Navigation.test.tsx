import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";

import { setLocale, t } from "../i18n";
import type { ArchiveSummary, EntryRow } from "../ipc/browse";
import { ForeignBrowser } from "../foreign/ForeignBrowser";
import { Browser } from "./Browser";

// Folder navigation regression: A/{fileA, fileB, B/C}. The keys are the real
// shape (the full stored path, hex), so a folder's key differs from its name.
const hex = (s: string) =>
  Array.from(new TextEncoder().encode(s), (b) => b.toString(16).padStart(2, "0")).join("");

function row(path: string, kind: "file" | "directory", dirs = 0, files = 0): EntryRow {
  return {
    key: hex(path),
    name: { text: path.split("/").at(-1) ?? path, verbatim: true },
    kind,
    size: "10",
    child_dirs: dirs,
    child_files: files,
  };
}

const TREE: Record<string, EntryRow[]> = {
  "": [row("A", "directory", 1, 2), row("top.txt", "file")],
  A: [row("A/B", "directory", 1, 0), row("A/fileA", "file"), row("A/fileB", "file")],
  "A/B": [row("A/B/C", "directory", 0, 1)],
  "A/B/C": [row("A/B/C/deep.txt", "file")],
};
const KEYS = new Map(Object.keys(TREE).map((p) => [hex(p), p]));

const SUMMARY: ArchiveSummary = {
  handle: 3,
  file_name: "nested.mochi",
  archive_id: "ab".repeat(16),
  profile: { tar_compatible: false, encrypted: false, redundancy_percent: null },
  commits: [{ seq: 0, commit_id: "0".repeat(64), time: null, checkpoint: true, retained: null, expired: null, holds: [] }],
  retention_error: null,
};

let calls: { cmd: string; args: Record<string, unknown> }[] = [];

function fake() {
  calls = [];
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    if (cmd === "extract_wizard_setup") return { destination: "/home/me/Desktop", recent: ["/home/me/Desktop"] };
    if (cmd === "plan_extract_to")
      return { destination: "/home/me/Desktop", entries: 3, existing: [], existing_total: 0, merged_folders: 0, conflicts: [], conflicts_total: 0, case_insensitive: false };
    if (cmd === "start_job") return 7;
    if (cmd === "staged_sources") return { items: [], destination: null, default_destination: null, format: "mochi" };
    if (cmd === "watch_drops" || cmd === "watch_launches") return null;
    if (cmd === "recent_archives") return [];
    switch (cmd) {
      case "open_archive":
        return { status: "opened", archive: SUMMARY };
      case "close_archive":
        return true;
      case "open_snapshot":
        return { seq: a.seq, commit_id: "c".repeat(64), entries: 7, root_dirs: 1, root_files: 1, catalog_source: { source: "image" } };
      case "list_children": {
        const dir = a.dir === null ? "" : KEYS.get(a.dir as string);
        if (dir === undefined) throw { code: "INVALID_ARGUMENT", message: "no such directory" };
        const all = TREE[dir] ?? [];
        const pool = a.directoriesOnly ? all.filter((r) => r.kind === "directory") : all;
        const off = a.offset as number;
        return { total: pool.length, offset: off, entries: pool.slice(off, off + (a.limit as number)) };
      }
      case "entry_details":
        throw { code: "INVALID_ARGUMENT", message: "n/a" };
    }
    throw new Error(`unexpected command ${cmd}`);
  });
}

afterEach(() => {
  clearMocks();
  setLocale("en");
});

const names = () => screen.getAllByTestId("entry").map((e) => e.querySelector(".name")?.textContent);

async function into(user: ReturnType<typeof userEvent.setup>, name: string, expected: string[]) {
  const target = (await screen.findAllByTestId("entry")).find((e) => e.querySelector(".name")?.textContent === name);
  await user.dblClick(target as HTMLElement);
  await waitFor(() => expect(names()).toEqual(expected));
}

async function openDeep() {
  const user = userEvent.setup();
  fake();
  render(<Browser />);
  await user.click(screen.getByTestId("open-archive"));
  await screen.findByText(/^2 items/);
  await into(user, "A", ["B", "fileA", "fileB"]);
  await into(user, "B", ["C"]);
  await into(user, "C", ["deep.txt"]);
  return user;
}

test("Backspace climbs C -> B -> A -> root, each landing on the parent", async () => {
  const user = await openDeep();
  const up = async (expected: string[]) => {
    (screen.getAllByTestId("entry")[0] as HTMLElement).focus();
    await user.keyboard("{Backspace}");
    await waitFor(() => expect(names()).toEqual(expected));
  };
  await up(["C"]);
  await up(["B", "fileA", "fileB"]);
  await up(["A", "top.txt"]);
});

test("the path bar goes to each ancestor", async () => {
  const user = await openDeep();
  const crumbs = () => within(screen.getByTestId("crumbs"));
  await user.click(crumbs().getByRole("button", { name: "B" }));
  await waitFor(() => expect(names()).toEqual(["C"]));
  await user.click(crumbs().getByRole("button", { name: "A" }));
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  await user.click(crumbs().getByTestId("crumb-root"));
  await waitFor(() => expect(names()).toEqual(["A", "top.txt"]));
});

const selectedNames = () =>
  screen
    .getAllByTestId("entry")
    .filter((e) => e.getAttribute("aria-pressed") === "true")
    .map((e) => e.querySelector(".name")?.textContent);

test("going up selects the folder you came out of, as a file manager does", async () => {
  const user = await openDeep();
  (screen.getAllByTestId("entry")[0] as HTMLElement).focus();
  await user.keyboard("{Backspace}");
  await waitFor(() => expect(names()).toEqual(["C"]));
  await waitFor(() => expect(selectedNames()).toEqual(["C"]));
  await user.keyboard("{Backspace}");
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  await waitFor(() => expect(selectedNames()).toEqual(["B"]));
  // The path bar does the same.
  await user.click(screen.getByTestId("crumb-root"));
  await waitFor(() => expect(selectedNames()).toEqual(["A"]));
});

test("Alt+Left and Alt+Right walk the folder history, Alt+Up goes to the parent", async () => {
  const user = await openDeep();
  (screen.getAllByTestId("entry")[0] as HTMLElement).focus();
  await user.keyboard("{Alt>}{ArrowUp}{/Alt}");
  await waitFor(() => expect(names()).toEqual(["C"]));
  await user.keyboard("{Alt>}{ArrowLeft}{/Alt}");
  await waitFor(() => expect(names()).toEqual(["deep.txt"]));
  await user.keyboard("{Alt>}{ArrowLeft}{/Alt}");
  await waitFor(() => expect(names()).toEqual(["C"]));
  await user.keyboard("{Alt>}{ArrowLeft}{/Alt}");
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  await user.keyboard("{Alt>}{ArrowRight}{/Alt}");
  await waitFor(() => expect(names()).toEqual(["C"]));
});

test("the folder tree goes to each ancestor", async () => {
  const user = await openDeep();
  await user.click(screen.getByTestId("toggle-folders"));
  const tree = within(screen.getByTestId("tree"));
  await user.click(await tree.findByRole("button", { name: "Expand A" }));
  await user.click(await tree.findByRole("button", { name: "Expand B" }));
  await user.click(await tree.findByRole("button", { name: "C" }));
  await waitFor(() => expect(names()).toEqual(["deep.txt"]));
  await user.click(tree.getByRole("button", { name: "B" }));
  await waitFor(() => expect(names()).toEqual(["C"]));
  await user.click(tree.getByRole("button", { name: "A" }));
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  await user.click(tree.getByRole("button", { name: "Archive root" }));
  await waitFor(() => expect(names()).toEqual(["A", "top.txt"]));
});

test("a foreign archive climbs the same way, including folders that exist only as prefixes", async () => {
  const user = userEvent.setup();
  // Only A/B/C/deep.txt is in the archive: A, B and C are implied by the path.
  const foreignRow = (path: string, kind: "file" | "directory") => ({
    ...row(path, kind, kind === "directory" ? 1 : 0, 0),
    size: kind === "file" ? "5" : null,
    encryption: null,
    note: null,
  });
  const FTREE: Record<string, ReturnType<typeof foreignRow>[]> = {
    "": [foreignRow("A", "directory")],
    A: [foreignRow("A/B", "directory"), foreignRow("A/fileA", "file")],
    "A/B": [foreignRow("A/B/C", "directory")],
    "A/B/C": [foreignRow("A/B/C/deep.txt", "file")],
  };
  const fkeys = new Map(Object.keys(FTREE).map((p) => [hex(p), p]));
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    if (cmd === "staged_sources") return { items: [], destination: null, default_destination: null, format: "mochi" };
    if (cmd === "watch_drops" || cmd === "watch_launches") return null;
    if (cmd === "recent_archives") return [];
    switch (cmd) {
      case "open_foreign_archive":
        return {
          handle: 5,
          file_name: "deep.zip",
          format: "ZIP",
          entries: 3,
          complete: true,
          stop_reason: null,
          notes: [],
          checks: "CRC32 per entry",
          opened_with_password: false,
          weak_encryption: false,
          dimensions: { recoverability: "UNSUPPORTED", freshness: "UNSUPPORTED", retention: "UNSUPPORTED" },
        };
      case "foreign_children": {
        const dir = a.dir === null ? "" : fkeys.get(a.dir as string);
        if (dir === undefined) throw { code: "INVALID_ARGUMENT", message: "no such folder" };
        const all = FTREE[dir] ?? [];
        return { total: all.length, offset: 0, entries: all };
      }
      case "close_foreign_archive":
        return true;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  render(<Browser foreign={<ForeignBrowser />} />);
  await user.click(screen.getByTestId("toolbar-more"));
  await user.click(screen.getByTestId("menu-foreign"));
  const fnames = () => screen.getAllByTestId("foreign-entry").map((e) => e.querySelector(".name")?.textContent);
  const down = async (name: string, expected: string[]) => {
    const target = (await screen.findAllByTestId("foreign-entry")).find(
      (e) => e.querySelector(".name")?.textContent === name,
    );
    await user.dblClick(target as HTMLElement);
    await waitFor(() => expect(fnames()).toEqual(expected));
  };
  await waitFor(() => expect(fnames()).toEqual(["A"]));
  await down("A", ["B", "fileA"]);
  await down("B", ["C"]);
  await down("C", ["deep.txt"]);
  const selected = () =>
    screen
      .getAllByTestId("foreign-entry")
      .filter((e) => e.getAttribute("aria-pressed") === "true")
      .map((e) => e.querySelector(".name")?.textContent);
  (screen.getAllByTestId("foreign-entry")[0] as HTMLElement).focus();
  await user.keyboard("{Backspace}");
  await waitFor(() => expect(fnames()).toEqual(["C"]));
  await waitFor(() => expect(selected()).toEqual(["C"]));
  await user.keyboard("{Backspace}");
  await waitFor(() => expect(fnames()).toEqual(["B", "fileA"]));
  await waitFor(() => expect(selected()).toEqual(["B"]));
  await user.click(screen.getByTestId("crumb-root"));
  await waitFor(() => expect(fnames()).toEqual(["A"]));
});

// ---- Back, Forward, Up and the ".." row -------------------------------------------------------

const btn = (id: string) => screen.getByTestId(id) as HTMLButtonElement;
const upRow = () => screen.queryByTestId("up-row");

test("the Up button climbs C -> B -> A -> root and is disabled at the root", async () => {
  const user = await openDeep();
  expect(btn("nav-up")).toBeEnabled();
  await user.click(btn("nav-up"));
  await waitFor(() => expect(names()).toEqual(["C"]));
  await user.click(btn("nav-up"));
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  await user.click(btn("nav-up"));
  await waitFor(() => expect(names()).toEqual(["A", "top.txt"]));
  expect(btn("nav-up")).toBeDisabled();
  expect(upRow()).toBeNull();
});

test("the .. row climbs by double click and by Enter, and is absent at the root", async () => {
  const user = await openDeep();
  expect(upRow()).not.toBeNull();
  expect(upRow()).toHaveTextContent("..");
  await user.dblClick(upRow() as HTMLElement);
  await waitFor(() => expect(names()).toEqual(["C"]));
  upRow()?.focus();
  await user.keyboard("{Enter}");
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  // The first row of the listing is the .. row for the keyboard too: Up arrow from row 0, Down arrow back.
  (screen.getAllByTestId("entry")[0] as HTMLElement).focus();
  await user.keyboard("{ArrowUp}");
  expect(upRow()).toHaveFocus();
  await user.keyboard("{ArrowDown}");
  expect(screen.getAllByTestId("entry")[0]).toHaveFocus();
  await user.dblClick(upRow() as HTMLElement);
  await waitFor(() => expect(names()).toEqual(["A", "top.txt"]));
  expect(upRow()).toBeNull();
});

test("Back and Forward walk the history across the folders; disabled with none", async () => {
  const user = userEvent.setup();
  fake();
  render(<Browser />);
  await user.click(screen.getByTestId("open-archive"));
  await screen.findByText(/^2 items/);
  expect(btn("nav-back")).toBeDisabled();
  expect(btn("nav-forward")).toBeDisabled();
  expect(btn("nav-up")).toBeDisabled();
  await into(user, "A", ["B", "fileA", "fileB"]);
  await into(user, "B", ["C"]);
  expect(btn("nav-back")).toBeEnabled();
  expect(btn("nav-forward")).toBeDisabled();
  await user.click(btn("nav-up"));
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  await user.click(btn("nav-back"));
  await waitFor(() => expect(names()).toEqual(["C"]));
  await user.click(btn("nav-back"));
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  await user.click(btn("nav-back"));
  await waitFor(() => expect(names()).toEqual(["A", "top.txt"]));
  expect(btn("nav-back")).toBeDisabled();
  expect(btn("nav-forward")).toBeEnabled();
  await user.click(btn("nav-forward"));
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  await user.click(btn("nav-forward"));
  await waitFor(() => expect(names()).toEqual(["C"]));
  await user.click(btn("nav-forward"));
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  expect(btn("nav-forward")).toBeDisabled();
});

test("going somewhere new after Back clears Forward", async () => {
  const user = await openDeep();
  await user.click(btn("nav-back"));
  await waitFor(() => expect(names()).toEqual(["C"]));
  await user.click(btn("nav-back"));
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  expect(btn("nav-forward")).toBeEnabled();
  await user.click(within(screen.getByTestId("crumbs")).getByTestId("crumb-root"));
  await waitFor(() => expect(names()).toEqual(["A", "top.txt"]));
  expect(btn("nav-forward")).toBeDisabled();
  expect(btn("nav-back")).toBeEnabled();
  await user.click(btn("nav-back"));
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  expect(btn("nav-forward")).toBeEnabled();
});

test("disabled Back, Forward and Up ignore clicks and Alt+arrows at the root", async () => {
  const user = userEvent.setup();
  fake();
  render(<Browser />);
  await user.click(screen.getByTestId("open-archive"));
  await screen.findByText(/^2 items/);
  const top = (await screen.findAllByTestId("entry"))[1] as HTMLElement;
  await user.click(top);
  expect(top).toHaveAttribute("aria-pressed", "true");
  const listed = calls.filter((c) => c.cmd === "list_children").length;
  for (const id of ["nav-back", "nav-forward", "nav-up"]) {
    expect(btn(id)).toBeDisabled();
    await user.click(btn(id));
  }
  top.focus();
  await user.keyboard("{Alt>}{ArrowLeft}{ArrowRight}{ArrowUp}{/Alt}");
  expect(names()).toEqual(["A", "top.txt"]);
  expect(calls.filter((c) => c.cmd === "list_children").length).toBe(listed);
  // Nothing moved, so the selection stays.
  expect(screen.getAllByTestId("entry")[1]).toHaveAttribute("aria-pressed", "true");
});

test("closing and opening an archive again starts with no history", async () => {
  const user = await openDeep();
  expect(btn("nav-back")).toBeEnabled();
  await user.click(btn("close-archive"));
  await user.click(await screen.findByTestId("open-archive"));
  await screen.findByText(/^2 items/);
  expect(btn("nav-back")).toBeDisabled();
  expect(btn("nav-forward")).toBeDisabled();
  expect(btn("nav-up")).toBeDisabled();
});

test("Back, Forward and Up are off while search results are on screen", async () => {
  const user = await openDeep();
  await user.click(btn("nav-back"));
  await waitFor(() => expect(names()).toEqual(["C"]));
  await user.type(screen.getByTestId("search-query"), "deep");
  await waitFor(() => expect(screen.queryByTestId("entries")).toBeNull(), { timeout: 3000 });
  expect(btn("nav-back")).toBeDisabled();
  expect(btn("nav-forward")).toBeDisabled();
  expect(btn("nav-up")).toBeDisabled();
  await user.clear(screen.getByTestId("search-query"));
  await waitFor(() => expect(names()).toEqual(["C"]));
  expect(btn("nav-back")).toBeEnabled();
  expect(btn("nav-forward")).toBeEnabled();
  expect(btn("nav-up")).toBeEnabled();
});

test("Close is the last control of the path bar", async () => {
  await openDeep();
  const close = btn("close-archive");
  expect(close.parentElement).toHaveClass("pathbar");
  expect(close.parentElement?.lastElementChild).toBe(close);
  expect(close).toHaveClass("pathbar-close");
});

test("the buttons name themselves and their shortcuts", async () => {
  const user = await openDeep();
  expect(btn("nav-back")).toHaveAttribute("aria-label", "Back");
  expect(btn("nav-back")).toHaveAttribute("title", expect.stringContaining("Alt+Left"));
  expect(btn("nav-forward")).toHaveAttribute("title", expect.stringContaining("Alt+Right"));
  expect(btn("nav-up")).toHaveAttribute("title", expect.stringContaining("Alt+Up"));
  expect(btn("nav-up")).toHaveAttribute("title", expect.stringContaining("Backspace"));
  expect(btn("nav-up")).toHaveAttribute("aria-label", "Up to the parent folder");
  expect(user).toBeDefined();
});

test(".. is never selected: Ctrl+A, a click and the status bar count only real entries", async () => {
  const user = await openDeep();
  await user.click(upRow() as HTMLElement);
  expect(upRow()).not.toHaveAttribute("aria-pressed");
  await user.dblClick(upRow() as HTMLElement);
  await waitFor(() => expect(names()).toEqual(["C"]));
  await user.dblClick(upRow() as HTMLElement);
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  (screen.getAllByTestId("entry")[0] as HTMLElement).focus();
  await user.keyboard("{Control>}a{/Control}");
  await waitFor(() => expect(screen.getByTestId("entry-count")).toHaveTextContent("3 items · 3 selected"));
  expect(upRow()).not.toHaveAttribute("aria-pressed");
  expect(upRow()).not.toHaveClass("selected");
});

test(".. is not caught by the rubber band and is never extracted", async () => {
  const user = await openDeep();
  await user.dblClick(upRow() as HTMLElement);
  await waitFor(() => expect(names()).toEqual(["C"]));
  await user.dblClick(upRow() as HTMLElement);
  await waitFor(() => expect(names()).toEqual(["B", "fileA", "fileB"]));
  expect(names()).toHaveLength(3);
  const list = screen.getByTestId("entries");
  Object.defineProperty(list, "clientWidth", { configurable: true, value: 185 });
  Object.defineProperty(list, "clientHeight", { configurable: true, value: 300 });
  list.getBoundingClientRect = () =>
    ({ left: 0, top: 0, right: 200, bottom: 300, width: 200, height: 300, x: 0, y: 0, toJSON: () => ({}) }) as DOMRect;
  const pointer = (type: string, y: number) =>
    act(() => {
      list.dispatchEvent(
        new PointerEvent(type, {
          bubbles: true,
          cancelable: true,
          button: 0,
          buttons: type === "pointerup" ? 0 : 1,
          clientX: 60,
          clientY: y,
          pointerId: 5,
          pointerType: "mouse",
        }),
      );
    });
  pointer("pointerdown", 200);
  pointer("pointermove", 0);
  await waitFor(() => expect(screen.getByTestId("entry-count")).toHaveTextContent("3 items · 3 selected"));
  pointer("pointerup", 0);
  expect(upRow()).not.toHaveAttribute("aria-pressed");
  // Extract to...: the plan names the three entries and nothing else.
  await user.click(screen.getByTestId("extract-to"));
  await screen.findByTestId("extract-to-dialog");
  await waitFor(() => expect(screen.getByTestId("extract-to-folder")).toHaveValue("/home/me/Desktop"));
  await user.keyboard("{Enter}");
  await waitFor(() => expect(calls.some((c) => c.cmd === "plan_extract_to")).toBe(true));
  const plan = calls.find((c) => c.cmd === "plan_extract_to");
  expect([...((plan?.args.keys as string[]) ?? [])].sort()).toEqual(
    [hex("A/B"), hex("A/fileA"), hex("A/fileB")].sort(),
  );
});

test(".. does not appear in search results", async () => {
  const user = await openDeep();
  expect(upRow()).not.toBeNull();
  await user.type(screen.getByTestId("search-query"), "deep");
  // The search may finish before a "running" state can be observed: wait
  // for the folder listing (and its ".." row) to give way to the search.
  await waitFor(() => expect(screen.queryByTestId("entries")).toBeNull(), { timeout: 3000 });
  expect(upRow()).toBeNull();
});

test("the root crumb shows the archive name; each crumb goes to exactly its folder with a full-name tooltip", async () => {
  const user = await openDeep();
  const bar = within(screen.getByTestId("crumbs"));
  const root = bar.getByTestId("crumb-root");
  expect(root).toHaveTextContent("nested.mochi");
  expect(root).toHaveAttribute("title", "nested.mochi (archive root)");
  const crumbs = bar.getAllByTestId("crumb");
  expect(crumbs.map((c) => c.getAttribute("title"))).toEqual(["A", "B", "C"]);
  expect(crumbs[2]).toHaveAttribute("aria-current", "location");
  expect(root).not.toHaveAttribute("aria-current");
  await user.click(crumbs[1] as HTMLElement);
  await waitFor(() => expect(names()).toEqual(["C"]));
  expect(within(screen.getByTestId("crumbs")).getAllByTestId("crumb")).toHaveLength(2);
  await user.click(within(screen.getByTestId("crumbs")).getByTestId("crumb-root"));
  await waitFor(() => expect(names()).toEqual(["A", "top.txt"]));
  expect(within(screen.getByTestId("crumbs")).queryAllByTestId("crumb")).toHaveLength(0);
  expect(within(screen.getByTestId("crumbs")).getByTestId("crumb-root")).toHaveAttribute("aria-current", "location");
});

test("the folder tree marks the current folder", async () => {
  const user = await openDeep();
  await user.click(screen.getByTestId("toggle-folders"));
  const tree = within(screen.getByTestId("tree"));
  await user.click(await tree.findByRole("button", { name: "Expand A" }));
  await user.click(await tree.findByRole("button", { name: "Expand B" }));
  const current = () => tree.getAllByRole("button").filter((b) => b.getAttribute("aria-current") === "true");
  await waitFor(() => expect(current().map((b) => b.textContent)).toEqual(["C"]));
  await user.click(btn("nav-up"));
  await waitFor(() => expect(current().map((b) => b.textContent)).toEqual(["B"]));
  await user.click(btn("nav-up"));
  await waitFor(() => expect(current().map((b) => b.textContent)).toEqual(["A"]));
  await user.click(btn("nav-up"));
  await waitFor(() => expect(current().map((b) => b.textContent)).toEqual(["Archive root"]));
});

test("the labels exist in Thai and Simplified Chinese", () => {
  const keys = [
    "browser.navGroup",
    "browser.navBack",
    "browser.navBackTip",
    "browser.navForward",
    "browser.navForwardTip",
    "browser.navUp",
    "browser.navUpTip",
    "browser.upRowLabel",
  ] as const;
  const seen = new Set<string>();
  for (const locale of ["th", "zh-CN"] as const) {
    setLocale(locale);
    for (const k of keys) {
      const text = t(k);
      expect(text, `${locale} ${k}`).not.toEqual("");
      expect(text, `${locale} ${k}`).not.toEqual(k);
      seen.add(text);
    }
    expect(t("browser.navBack")).not.toEqual("Back");
    expect(t("browser.navUp")).not.toEqual("Up to the parent folder");
    expect(t("browser.crumbRoot", { name: "x.mochi" })).toContain("x.mochi");
  }
  setLocale("en");
  expect(t("browser.navBack")).toBe("Back");
});

test("a foreign archive has the same buttons, .. row, crumbs and disabled states", async () => {
  const user = userEvent.setup();
  const frow = (path: string, kind: "file" | "directory") => ({
    ...row(path, kind, kind === "directory" ? 1 : 0, 0),
    size: kind === "file" ? "5" : null,
    encryption: null,
    note: null,
  });
  const FTREE: Record<string, ReturnType<typeof frow>[]> = {
    "": [frow("A", "directory")],
    A: [frow("A/B", "directory"), frow("A/fileA", "file")],
    "A/B": [frow("A/B/deep.txt", "file")],
  };
  const fkeys = new Map(Object.keys(FTREE).map((p) => [hex(p), p]));
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    if (cmd === "staged_sources") return { items: [], destination: null, default_destination: null, format: "mochi" };
    if (cmd === "watch_drops" || cmd === "watch_launches") return null;
    if (cmd === "recent_archives") return [];
    switch (cmd) {
      case "open_foreign_archive":
        return {
          handle: 5,
          file_name: "deep.zip",
          format: "ZIP",
          entries: 3,
          complete: true,
          stop_reason: null,
          notes: [],
          checks: "CRC32 per entry",
          opened_with_password: false,
          weak_encryption: false,
          dimensions: { recoverability: "UNSUPPORTED", freshness: "UNSUPPORTED", retention: "UNSUPPORTED" },
        };
      case "foreign_children": {
        const dir = a.dir === null ? "" : fkeys.get(a.dir as string);
        if (dir === undefined) throw { code: "INVALID_ARGUMENT", message: "no such folder" };
        const all = FTREE[dir] ?? [];
        return { total: all.length, offset: 0, entries: all };
      }
      case "close_foreign_archive":
        return true;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  render(<Browser foreign={<ForeignBrowser />} />);
  await user.click(screen.getByTestId("toolbar-more"));
  await user.click(screen.getByTestId("menu-foreign"));
  const fnames = () => screen.getAllByTestId("foreign-entry").map((e) => e.querySelector(".name")?.textContent);
  await waitFor(() => expect(fnames()).toEqual(["A"]));
  const fb = (id: string) => screen.getByTestId(id) as HTMLButtonElement;
  expect(fb("foreign-nav-back")).toBeDisabled();
  expect(fb("foreign-nav-forward")).toBeDisabled();
  expect(fb("foreign-nav-up")).toBeDisabled();
  expect(fb("foreign-close").parentElement?.lastElementChild).toBe(fb("foreign-close"));
  expect(screen.queryByTestId("foreign-up-row")).toBeNull();
  expect(within(screen.getByTestId("foreign-crumbs")).getByTestId("crumb-root")).toHaveTextContent("deep.zip");
  const dive = async (name: string, expected: string[]) => {
    const target = (await screen.findAllByTestId("foreign-entry")).find(
      (e) => e.querySelector(".name")?.textContent === name,
    );
    await user.dblClick(target as HTMLElement);
    await waitFor(() => expect(fnames()).toEqual(expected));
  };
  await dive("A", ["B", "fileA"]);
  await dive("B", ["deep.txt"]);
  expect(fb("foreign-nav-back")).toBeEnabled();
  expect(fb("foreign-nav-up")).toBeEnabled();
  await user.dblClick(screen.getByTestId("foreign-up-row"));
  await waitFor(() => expect(fnames()).toEqual(["B", "fileA"]));
  // Ctrl+A selects the two entries, not the .. row.
  (screen.getAllByTestId("foreign-entry")[0] as HTMLElement).focus();
  await user.keyboard("{Control>}a{/Control}");
  await waitFor(() =>
    expect(screen.getAllByTestId("foreign-entry").map((e) => e.getAttribute("aria-pressed"))).toEqual(["true", "true"]),
  );
  expect(screen.getByTestId("foreign-up-row")).not.toHaveAttribute("aria-pressed");
  await user.click(fb("foreign-nav-up"));
  await waitFor(() => expect(fnames()).toEqual(["A"]));
  expect(screen.queryByTestId("foreign-up-row")).toBeNull();
  expect(fb("foreign-nav-up")).toBeDisabled();
  await user.click(fb("foreign-nav-back"));
  await waitFor(() => expect(fnames()).toEqual(["B", "fileA"]));
  await user.click(fb("foreign-nav-forward"));
  await waitFor(() => expect(fnames()).toEqual(["A"]));
  expect(fb("foreign-nav-forward")).toBeDisabled();
});
