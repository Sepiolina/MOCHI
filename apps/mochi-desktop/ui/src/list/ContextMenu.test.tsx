// The file list's right-click menu (owner request, 0.9.5): no browser menu,
// the in-app one on the list, and a drag that carries the whole selection.
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test, vi } from "vitest";

import type { ArchiveSummary, EntryRow } from "../ipc/browse";
import { Browser } from "../browser/Browser";
import { ForeignBrowser } from "../foreign/ForeignBrowser";
import { suppressNativeMenu } from "./ContextMenu";

const hex = (s: string) => Array.from(new TextEncoder().encode(s), (b) => b.toString(16).padStart(2, "0")).join("");
const HOSTILE = '<img src=x onerror="alert(1)">';

const SUMMARY: ArchiveSummary = {
  handle: 3,
  file_name: "music.mochi",
  archive_id: "ab".repeat(16),
  profile: { tar_compatible: false, encrypted: false, redundancy_percent: null },
  commits: [{ seq: 0, commit_id: "0".repeat(64), time: null, checkpoint: true, retained: true, expired: false, holds: [] }],
  retention_error: null,
};

const row = (name: string, kind: "file" | "directory" = "file"): EntryRow => ({
  key: hex(name),
  name: { text: name, verbatim: true },
  kind,
  size: "100",
  child_dirs: 0,
  child_files: 0,
});
const ROOT = [row("albums", "directory"), row("song.mp3"), row("track.flac"), row(HOSTILE)];

function backend() {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    if (cmd === "staged_sources") return { items: [], destination: null, default_destination: null, format: "mochi" };
    if (cmd === "watch_drops" || cmd === "watch_launches") return null;
    if (cmd === "recent_archives") return [];
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "open_archive":
        return { status: "opened", archive: SUMMARY };
      case "open_snapshot":
        return { seq: 0, commit_id: "0".repeat(64), entries: 4, root_dirs: 1, root_files: 3, catalog_source: { source: "image" } };
      case "list_children": {
        const dir = a.dir as string | null;
        const all = dir === null ? ROOT : dir === hex("albums") ? [row("x.mp3")] : [];
        const offset = Number(a.offset);
        return { total: all.length, offset, entries: all.slice(offset, offset + Number(a.limit)) };
      }
      case "entry_details":
        return {
          key: a.key,
          path: { text: "song.mp3", verbatim: true },
          kind: "file",
          size: "100",
          content_hash: "00",
          file_version_id: "11",
          attributes: { state: "recorded", posix: null, windows: null, mtime: null },
        };
      case "extract_wizard_setup":
        return { destination: "/home/me/Desktop", recent: [] };
      case "plan_extract":
        return { destination: "/home/me/out", entries: 2, existing: [], existing_total: 0, merged_folders: 0, conflicts: [], conflicts_total: 0, case_insensitive: false };
      case "open_foreign_archive":
        return {
          handle: 5,
          file_name: "songs.zip",
          format: "ZIP",
          entries: 2,
          complete: true,
          stop_reason: null,
          notes: [],
          checks: "CRC32 per entry",
          opened_with_password: false,
          weak_encryption: false,
          dimensions: { recoverability: "UNSUPPORTED", freshness: "UNSUPPORTED", retention: "UNSUPPORTED" },
        };
      case "foreign_children": {
        const entries = [
          { ...row("a.mp3"), size: "9", encryption: null, note: null },
          { ...row("b.mp3"), size: "9", encryption: null, note: null },
        ];
        return { total: 2, offset: 0, entries };
      }
      case "close_archive":
      case "close_foreign_archive":
      case "drag_out":
      case "drag_out_foreign":
        return null;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return calls;
}

afterEach(() => {
  clearMocks();
  vi.restoreAllMocks();
});

async function openIt() {
  const user = userEvent.setup();
  const calls = backend();
  render(<Browser foreign={<ForeignBrowser />} />);
  await user.click(screen.getByTestId("open-archive"));
  await screen.findAllByTestId("entry");
  return { user, calls };
}

const itemNames = () => screen.getAllByRole("menuitem").map((i) => i.textContent);

test("the webview's own menu is suppressed except in text fields", () => {
  const off = suppressNativeMenu();
  try {
    const plain = document.createElement("div");
    const field = document.createElement("input");
    field.type = "text";
    const button = document.createElement("input");
    button.type = "checkbox";
    document.body.append(plain, field, button);
    const fire = (el: Element) => {
      const e = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
      el.dispatchEvent(e);
      return e.defaultPrevented;
    };
    expect(fire(plain)).toBe(true);
    expect(fire(button)).toBe(true);
    expect(fire(field)).toBe(false);
    plain.remove();
    field.remove();
    button.remove();
  } finally {
    off();
  }
});

test("a right-click on a row selects it and opens the menu with its items", async () => {
  await openIt();
  const rows = screen.getAllByTestId("entry");
  fireEvent.contextMenu(rows[1] as HTMLElement);
  const menu = await screen.findByRole("menu");
  expect(itemNames()).toEqual(["Select all", "Copy full names", "Extract to...", "Add files...", "Delete"]);
  expect(menu).toContainElement(screen.getByTestId("menu-delete"));
  expect(rows[1]).toHaveAttribute("aria-pressed", "true");
  expect(screen.getByTestId("menu-delete")).not.toHaveAttribute("aria-disabled");
  // Escape closes it and gives focus back.
  await userEvent.keyboard("{Escape}");
  expect(screen.queryByRole("menu")).toBeNull();
  // The selection survives the menu's Escape.
  expect(rows[1]).toHaveAttribute("aria-pressed", "true");
});

test("on an empty selection the items that need one are disabled", async () => {
  await openIt();
  fireEvent.contextMenu(screen.getByTestId("entries"));
  await screen.findByRole("menu");
  expect(screen.getByTestId("menu-select-all")).not.toHaveAttribute("aria-disabled");
  for (const id of ["copy-names", "delete"]) {
    expect(screen.getByTestId(`menu-${id}`)).toHaveAttribute("aria-disabled", "true");
  }
});

test("Select all selects the folder", async () => {
  const { user } = await openIt();
  fireEvent.contextMenu(screen.getAllByTestId("entry")[0] as HTMLElement);
  await user.click(await screen.findByTestId("menu-select-all"));
  await waitFor(() => expect(screen.getByTestId("entry-count")).toHaveTextContent("4 selected"));
});

test("Copy full names puts the in-archive paths on the clipboard, newline-separated, as text", async () => {
  const { user } = await openIt();
  await user.click(screen.getAllByTestId("entry")[0] as HTMLElement); // albums
  await user.dblClick(screen.getAllByTestId("entry")[0] as HTMLElement);
  await waitFor(() => expect(screen.getAllByTestId("entry")).toHaveLength(1));
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
  fireEvent.contextMenu(screen.getAllByTestId("entry")[0] as HTMLElement);
  await user.click(await screen.findByTestId("menu-copy-names"));
  expect(writeText).toHaveBeenCalledWith("albums/x.mp3");
});

test("Copy full names with several selected joins them with newlines (hostile names stay text)", async () => {
  const { user } = await openIt();
  const rows = screen.getAllByTestId("entry");
  await user.click(rows[1] as HTMLElement);
  await user.keyboard("{Control>}");
  await user.click(rows[3] as HTMLElement);
  await user.keyboard("{/Control}");
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
  fireEvent.contextMenu(rows[3] as HTMLElement);
  await user.click(await screen.findByTestId("menu-copy-names"));
  expect(writeText).toHaveBeenCalledWith(`song.mp3\n${HOSTILE}`);
  expect(document.querySelector("img")).toBeNull();
});

test("Extract to... opens the window for the selection", async () => {
  const { user, calls } = await openIt();
  const rows = screen.getAllByTestId("entry");
  await user.click(rows[1] as HTMLElement);
  await user.keyboard("{Control>}");
  await user.click(rows[2] as HTMLElement);
  await user.keyboard("{/Control}");
  fireEvent.contextMenu(rows[2] as HTMLElement);
  await user.click(await screen.findByTestId("menu-extract-to"));
  expect(await screen.findByTestId("extract-to-dialog")).toBeInTheDocument();
  expect(screen.getByTestId("extract-to-what")).toHaveTextContent("The 2 selected entries will be extracted.");
  expect(calls.some((c) => c.cmd === "plan_extract_to")).toBe(false);
});

test("the keyboard opens the menu, arrows skip nothing enabled, Enter chooses", async () => {
  const { user } = await openIt();
  const rows = screen.getAllByTestId("entry");
  await user.click(rows[1] as HTMLElement);
  rows[1]?.focus();
  await user.keyboard("{Shift>}{F10}{/Shift}");
  await screen.findByRole("menu");
  expect(screen.getByTestId("menu-select-all")).toHaveFocus();
  await user.keyboard("{ArrowDown}");
  expect(screen.getByTestId("menu-copy-names")).toHaveFocus();
  await user.keyboard("{End}");
  expect(screen.getByTestId("menu-delete")).toHaveFocus();
  await user.keyboard("{Home}{Enter}");
  expect(screen.queryByRole("menu")).toBeNull();
  await waitFor(() => expect(screen.getByTestId("entry-count")).toHaveTextContent("4 selected"));
});

test("the menu closes on a click outside it", async () => {
  await openIt();
  fireEvent.contextMenu(screen.getAllByTestId("entry")[1] as HTMLElement);
  await screen.findByRole("menu");
  fireEvent.pointerDown(document.body);
  expect(screen.queryByRole("menu")).toBeNull();
});

test("on a foreign (read-only) archive Delete is disabled", async () => {
  const user = userEvent.setup();
  backend();
  render(<Browser foreign={<ForeignBrowser />} />);
  await user.click(screen.getByTestId("toolbar-more"));
  await user.click(screen.getByTestId("menu-foreign"));
  const rows = await screen.findAllByTestId("foreign-entry");
  await user.click(rows[0] as HTMLElement);
  fireEvent.contextMenu(rows[0] as HTMLElement);
  await screen.findByRole("menu");
  expect(screen.getByTestId("menu-delete")).toHaveAttribute("aria-disabled", "true");
  expect(screen.getByTestId("menu-copy-names")).not.toHaveAttribute("aria-disabled");
});

test("dragging one of several selected rows carries every selected path", async () => {
  const { user, calls } = await openIt();
  const rows = screen.getAllByTestId("entry");
  await user.click(rows[0] as HTMLElement);
  await user.keyboard("{Control>}");
  await user.click(rows[1] as HTMLElement);
  await user.click(rows[2] as HTMLElement);
  await user.keyboard("{/Control}");
  act(() => {
    const el = rows[1] as HTMLElement;
    el.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, button: 0, buttons: 1, clientX: 10, clientY: 10, pointerId: 1, pointerType: "mouse" }));
    el.dispatchEvent(new PointerEvent("pointermove", { bubbles: true, button: 0, buttons: 1, clientX: 40, clientY: 12, pointerId: 1, pointerType: "mouse" }));
  });
  await waitFor(() => expect(calls.filter((c) => c.cmd === "drag_out")).toHaveLength(1));
  expect(calls.find((c) => c.cmd === "drag_out")?.args).toEqual({
    handle: 3,
    seq: 0,
    keys: [hex("albums"), hex("song.mp3"), hex("track.flac")],
  });
});

test("selecting past one page loads every row in backend-sized pages", async () => {
  const { useSelection, LOAD_CHUNK } = await import("./selection");
  const { renderHook, act: hookAct } = await import("@testing-library/react");
  const asked: number[] = [];
  const load = (offset: number, limit: number) => {
    asked.push(limit);
    return Promise.resolve(Array.from({ length: limit }, (_, i) => ({ key: String(offset + i) })));
  };
  const { result } = renderHook(() => useSelection(load, () => undefined));
  hookAct(() => result.current.all(2500));
  await waitFor(() => expect(result.current.rows).toHaveLength(2500));
  expect(asked).toEqual([LOAD_CHUNK, LOAD_CHUNK, 500]);
});

// Owner request 2026-11-10: the selection's own actions beside "N selected", one click away.
test("a selection shows its actions in the status bar: Extract to…, Remove, Info, and clear", async () => {
  const { user, calls } = await openIt();
  expect(screen.queryByTestId("selection-bar")).toBeNull();
  const rows = screen.getAllByTestId("entry");
  await user.click(rows[1] as HTMLElement);
  await user.keyboard("{Control>}");
  await user.click(rows[2] as HTMLElement);
  await user.keyboard("{/Control}");
  const bar = await screen.findByRole("group", { name: "Selected items" });
  expect(bar).toHaveAttribute("data-testid", "selection-bar");
  expect(screen.getByTestId("selection-extract")).toHaveAccessibleName("Extract to…");
  expect(screen.getByTestId("selection-remove")).toBeEnabled();
  expect(screen.getByTestId("selection-clear")).toHaveAccessibleName("Clear the selection (Esc)");

  // Extract to… is the same window as the list's menu, for the same selection; nothing is planned yet.
  await user.click(screen.getByTestId("selection-extract"));
  expect(await screen.findByTestId("extract-to-dialog")).toBeInTheDocument();
  expect(screen.getByTestId("extract-to-what")).toHaveTextContent("The 2 selected entries will be extracted.");
  expect(calls.some((c) => c.cmd === "plan_extract_to")).toBe(false);
  await user.keyboard("{Escape}");
  await waitFor(() => expect(screen.queryByTestId("extract-to-dialog")).toBeNull());

  // Info shows and hides the details pane, and says which it is.
  const info = screen.getByTestId("selection-info");
  expect(info).toHaveAttribute("aria-pressed", "false");
  await user.click(info);
  expect(info).toHaveAttribute("aria-pressed", "true");
  expect(screen.getByTestId("info-panel")).toBeVisible();
  await user.click(info);
  expect(screen.getByTestId("info-panel")).not.toBeVisible();

  // Clear empties the selection, and the bar goes with it.
  await user.click(screen.getByTestId("selection-clear"));
  await waitFor(() => expect(screen.queryByTestId("selection-bar")).toBeNull());
  expect(screen.getByTestId("entry-count")).not.toHaveTextContent("selected");
});

test("the selection bar's Remove opens the same remove panel as the Delete key", async () => {
  const { user } = await openIt();
  await user.click(screen.getAllByTestId("entry")[1] as HTMLElement);
  await user.click(await screen.findByTestId("selection-remove"));
  expect(await screen.findByTestId("delete-note")).toBeInTheDocument();
});
