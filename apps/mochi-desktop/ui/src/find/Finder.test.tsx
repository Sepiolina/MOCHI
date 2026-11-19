// MOCHI Find: the folders, the scan's events, the matches, and above all what is said
// when not everything could be searched (spec §23.3 #8). Archive-supplied names are text.
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test, vi } from "vitest";

import { Browser } from "../browser/Browser";
import type { ArchiveSummary, EntryRow } from "../ipc/browse";
import type { FindCoverageView, FindEvent, FindHit, FindResultView, FindScopeView } from "../ipc/find";
import type { LaunchEvent } from "../ipc/launch";
import { Finder } from "./Finder";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () => (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;
const hex = (s: string) => Array.from(new TextEncoder().encode(s), (b) => b.toString(16).padStart(2, "0")).join("");

const HOSTILE = '<img src=x onerror="alert(1)">.txt';

const hit = (archive: number, path: string, over: Partial<FindHit> = {}): FindHit => ({
  run: 1,
  archive,
  archive_path: `/data/arch${archive}.mochi`,
  archive_modified_ms: 1_700_000_000_000,
  path: { text: path, verbatim: true },
  key: hex(path),
  kind: "file",
  size: "1536",
  terms: [0],
  ...over,
});

const coverage = (over: Partial<FindCoverageView> = {}): FindCoverageView => ({
  run: 1,
  state: "complete",
  complete: true,
  folders_visited: 3,
  folders_unreadable: 0,
  links_not_followed: 0,
  limit: null,
  archives_found: 3,
  archives_searched: 3,
  archives_pending: 0,
  archives_from_cache: 0,
  archives_skipped: 0,
  skipped: [],
  names_searched: 40,
  scopes: ["/data"],
  subfolders: true,
  failure: null,
  content_not_searched: true,
  ...over,
});

interface Setup {
  scopes?: FindScopeView[];
  chosen?: FindScopeView[] | null;
  /** The answer to a query, by its text. */
  answer?: (text: string) => FindResultView;
}

const scope = (index: number, path: string): FindScopeView => ({ index, path });

function backend(setup: Setup = {}) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let scopes = setup.scopes ?? [];
  let channel: Channel<FindEvent> | undefined;
  let n = 0;
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "find_scopes":
        return scopes;
      case "find_choose_folders":
        if (setup.chosen === null) return null;
        scopes = setup.chosen ?? [scope(0, "/data")];
        return scopes;
      case "find_remove_scope":
        scopes = scopes.filter((s) => s.index !== a.index);
        return scopes;
      case "find_start":
        channel = a.onEvent as Channel<FindEvent>;
        return 1;
      case "find_cancel":
        return true;
      case "find_query": {
        const text = String(a.text);
        return setup.answer ? setup.answer(text) : { hits: [], total_hits: 0, listed: 0, offset: 0, total_bytes: 0, archives_with_hits: 0, terms: [], coverage: coverage() };
      }
      case "find_show_folder":
        return null;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return {
    calls,
    count: (cmd: string) => calls.filter((c) => c.cmd === cmd).length,
    last: (cmd: string) => calls.filter((c) => c.cmd === cmd).at(-1)?.args,
    event: (m: FindEvent) => {
      if (!channel) throw new Error("no channel: the scan was not started");
      const id = channel.id;
      act(() => internals().runCallback(id, { index: n++, message: m }));
    },
  };
}

afterEach(() => {
  clearMocks();
  vi.restoreAllMocks();
});

test("the folders are listed, a folder is removed by its position, and Scan needs a folder", async () => {
  const user = userEvent.setup();
  const b = backend({ scopes: [scope(0, "/data/music"), scope(1, "/data/photos")] });
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  const items = await screen.findAllByTestId("find-scope");
  expect(items.map((i) => i.textContent)).toEqual(["/data/music", "/data/photos"]);
  expect(screen.getByTestId("find-scan")).toBeEnabled();
  await user.click(screen.getAllByTestId("find-remove-scope")[0] as HTMLElement);
  await waitFor(() => expect(screen.getAllByTestId("find-scope")).toHaveLength(1));
  expect(b.last("find_remove_scope")).toEqual({ index: 0 });
  await user.click(screen.getByTestId("find-remove-scope"));
  expect(await screen.findByTestId("find-no-folders")).toBeInTheDocument();
  expect(screen.getByTestId("find-scan")).toBeDisabled();
  // No path ever crosses from the window.
  for (const c of b.calls) expect(JSON.stringify(c.args)).not.toContain("/data");
});

test("nothing is asked of Rust until Find is shown", async () => {
  const b = backend({ scopes: [scope(0, "/data")] });
  const { rerender } = render(<Finder active={false} launchToken={0} onOpen={() => undefined} />);
  await new Promise((r) => setTimeout(r, 300));
  expect(b.calls).toEqual([]);
  rerender(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  expect(b.count("find_scopes")).toBe(1);
});

test("adding folders asks Rust's dialog and scans; the scan's events show progress, then the end", async () => {
  const user = userEvent.setup();
  const b = backend({
    answer: () => ({ hits: [], total_hits: 0, listed: 0, offset: 0, total_bytes: 0, archives_with_hits: 0, terms: [], coverage: coverage({ archives_searched: 3, names_searched: 40 }) }),
  });
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await user.click(screen.getByTestId("find-add-folders"));
  await screen.findAllByTestId("find-scope");
  await waitFor(() => expect(b.count("find_start")).toBe(1));
  expect(b.last("find_choose_folders")).toEqual({ text: { title: "Choose folders to search for MOCHI archives" } });
  expect(b.last("find_start")?.fresh).toBe(false);

  b.event({ event: "started", run: 1 });
  expect(await screen.findByTestId("find-stop")).toBeInTheDocument();
  b.event({ event: "progress", run: 1, completed: 1, total: null });
  b.event({ event: "progress", run: 1, completed: 2, total: 3 });
  b.event({ event: "finished", run: 1, state: "complete" });
  await waitFor(() => expect(screen.queryByTestId("find-stop")).not.toBeInTheDocument());
  expect(screen.getByTestId("find-scan")).toHaveTextContent("Scan again");
  await waitFor(() => expect(screen.getByTestId("find-status")).toHaveTextContent("Ready: 40 names in 3 archives. Type to search."));
  expect(screen.queryByTestId("find-incomplete")).not.toBeInTheDocument();
});

test("a cancelled dialog changes nothing, and the shell's launch scans the folders Rust holds", async () => {
  const user = userEvent.setup();
  const b = backend({ scopes: [scope(0, "/from/shell")], chosen: null });
  const { rerender } = render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  await user.click(screen.getByTestId("find-add-folders"));
  await waitFor(() => expect(b.count("find_choose_folders")).toBe(1));
  expect(b.count("find_start")).toBe(0);
  rerender(<Finder active launchToken={1} onOpen={() => undefined} />);
  await waitFor(() => expect(b.count("find_start")).toBe(1));
});

test("Stop cancels the scan and the button says it is stopping", async () => {
  const user = userEvent.setup();
  const b = backend({ scopes: [scope(0, "/data")] });
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  await user.click(screen.getByTestId("find-scan"));
  b.event({ event: "started", run: 1 });
  await user.click(await screen.findByTestId("find-stop"));
  expect(b.count("find_cancel")).toBe(1);
  expect(screen.getByTestId("find-stop")).toBeDisabled();
  expect(screen.getByTestId("find-stop")).toHaveTextContent("Stopping…");
  b.event({ event: "finished", run: 1, state: "cancelled" });
  await waitFor(() => expect(screen.queryByTestId("find-stop")).not.toBeInTheDocument());
});

test("typing asks Rust and lists the matches; archive-supplied names are text only", async () => {
  const user = userEvent.setup();
  const hits = [hit(0, HOSTILE), hit(1, "docs/<b>bold</b>.pdf", { archive_modified_ms: null }), hit(2, "photos", { kind: "directory", size: "0" })];
  const b = backend({
    scopes: [scope(0, "/data")],
    answer: (text) =>
      text === "" ? { hits: [], total_hits: 0, listed: 0, offset: 0, total_bytes: 0, archives_with_hits: 0, terms: [], coverage: coverage() } : { hits, total_hits: 3, listed: 3, offset: 0, total_bytes: 0, archives_with_hits: 0, terms: [], coverage: coverage() },
  });
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  await user.type(screen.getByTestId("find-query"), "txt");
  const rows = await screen.findAllByTestId("find-hit");
  expect(rows).toHaveLength(3);
  expect(b.last("find_query")).toEqual({ text: "txt" });
  expect(screen.getByTestId("find-status")).toHaveTextContent("3 matches");
  // The hostile name is a text node: no element was made from it.
  expect(rows[0]).toHaveTextContent(HOSTILE);
  expect(document.querySelector("img")).toBeNull();
  expect(document.querySelector("b")).toBeNull();
  expect(rows[1]).toHaveTextContent("docs/");
  expect(rows[0]).toHaveTextContent("arch0.mochi");
  expect(rows[0]).toHaveTextContent("1.5 KiB");
  expect(rows[1]).toHaveTextContent("unknown");
  expect(rows[2]).toHaveAttribute("data-kind", "directory");
  // The row shows the archive's file name; the full path is the tooltip and the detail line.
  await user.click(rows[0] as HTMLElement);
  expect(screen.getByTestId("find-selected")).toHaveTextContent("Archive: /data/arch0.mochi");
  expect(screen.getByTestId("find-selected")).toHaveTextContent(`Entry: ${HOSTILE}`);
  // Clearing the box clears the list.
  await user.click(screen.getByTestId("find-clear"));
  await waitFor(() => expect(screen.queryByTestId("find-hit")).not.toBeInTheDocument());
});

test("an escaped name says so, and a long list says how many matched in all", async () => {
  const user = userEvent.setup();
  backend({
    scopes: [scope(0, "/data")],
    answer: (text) =>
      text === ""
        ? { hits: [], total_hits: 0, listed: 0, offset: 0, total_bytes: 0, archives_with_hits: 0, terms: [], coverage: coverage() }
        : { hits: [hit(0, "a\\u{202e}b", { path: { text: "a\\u{202e}b", verbatim: false } })], total_hits: 4_321, listed: 4_321, offset: 0, total_bytes: 0, archives_with_hits: 0, terms: [], coverage: coverage() },
  });
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  await user.type(screen.getByTestId("find-query"), "a");
  await screen.findByTestId("find-hit");
  expect(screen.getByTestId("find-status")).toHaveTextContent(/^4,321 matches$/);
  expect(screen.getByTestId("find-hit").querySelector(".escaped")).not.toBeNull();
});

test("zero matches with archives that could not be searched is incomplete, never No matches", async () => {
  const user = userEvent.setup();
  const incomplete = coverage({
    complete: false,
    archives_found: 5,
    archives_searched: 3,
    archives_skipped: 2,
    skipped: [
      { archive: 3, path: "/data/secret.mochi", reason: "encrypted", code: null, message: null },
      {
        archive: 4,
        path: "/data/<b>broken</b>.mochi",
        reason: "damaged",
        code: "FOOTER_INVALID",
        message: { text: "no valid footer <i>here</i>", verbatim: true },
      },
    ],
  });
  backend({
    scopes: [scope(0, "/data")],
    answer: (text) => ({ hits: [], total_hits: 0, listed: 0, offset: 0, total_bytes: 0, archives_with_hits: 0, terms: [], coverage: text === "" ? coverage() : incomplete }),
  });
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  await user.type(screen.getByTestId("find-query"), "invoice");
  const head = await screen.findByTestId("find-incomplete-head");
  expect(head).toHaveTextContent("Search incomplete: 2 archives could not be searched");
  const status = screen.getByTestId("find-status");
  expect(status).toHaveTextContent("No matches found, but the search is incomplete.");
  expect(status.textContent).not.toMatch(/^No matches\b(?! found)/);
  expect(screen.queryByText(/No matches in the/)).not.toBeInTheDocument();
  // Which archives, and why. The encrypted one says its names need the passphrase.
  const items = screen.getAllByTestId("find-skipped-item");
  expect(items).toHaveLength(2);
  expect(items[0]).toHaveTextContent("/data/secret.mochi");
  expect(items[0]).toHaveTextContent("Encrypted: the names are not searchable without the passphrase.");
  expect(items[1]).toHaveTextContent("Damaged or unreadable (FOOTER_INVALID): no valid footer <i>here</i>");
  expect(document.querySelector("i")).toBeNull();
  expect(document.querySelector("b")).toBeNull();
});

test("one archive that could not be searched, and the other reasons, are each said", async () => {
  const user = userEvent.setup();
  backend({
    scopes: [scope(0, "/data")],
    answer: (text) => ({
      hits: [],
      total_hits: 0,
      listed: 0,
      offset: 0,
      total_bytes: 0,
      archives_with_hits: 0,
      terms: [],
      coverage:
        text === ""
          ? coverage()
          : coverage({
              complete: false,
              state: "cancelled",
              archives_found: 9,
              archives_searched: 4,
              archives_pending: 4,
              archives_skipped: 1,
              skipped: [{ archive: 0, path: "/data/x.mochi", reason: "not_mochi", code: null, message: null }],
              folders_unreadable: 2,
              links_not_followed: 1,
              limit: "depth",
            }),
    }),
  });
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  await user.type(screen.getByTestId("find-query"), "x");
  expect(await screen.findByTestId("find-incomplete-head")).toHaveTextContent(
    "Search incomplete: 1 archive could not be searched",
  );
  const box = screen.getByTestId("find-incomplete");
  expect(box).toHaveTextContent("2 folders could not be read");
  expect(box).toHaveTextContent("1 linked folder or archive was not followed.");
  expect(box).toHaveTextContent("depth limit");
  expect(box).toHaveTextContent("The scan was stopped before it finished.");
  expect(box).toHaveTextContent("4 archives found were not opened yet.");
  expect(box).toHaveTextContent("Not a MOCHI archive");
});

test("only a finished scan with nothing left out may say No matches", async () => {
  const user = userEvent.setup();
  backend({
    scopes: [scope(0, "/data")],
    answer: () => ({ hits: [], total_hits: 0, listed: 0, offset: 0, total_bytes: 0, archives_with_hits: 0, terms: [], coverage: coverage() }),
  });
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  await user.type(screen.getByTestId("find-query"), "nothing");
  await waitFor(() =>
    expect(screen.getByTestId("find-status")).toHaveTextContent("No matches in the 3 archives that were searched."),
  );
  expect(screen.queryByTestId("find-incomplete")).not.toBeInTheDocument();
});

test("a scan that failed is incomplete and says why", async () => {
  const user = userEvent.setup();
  backend({
    scopes: [scope(0, "/data")],
    answer: (text) => ({
      hits: [],
      total_hits: 0,
      listed: 0,
      offset: 0,
      total_bytes: 0,
      archives_with_hits: 0,
      terms: [],
      coverage: text === "" ? coverage() : coverage({ complete: false, state: "failed", failure: "disk went away" }),
    }),
  });
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  await user.type(screen.getByTestId("find-query"), "x");
  expect(await screen.findByTestId("find-incomplete-head")).toHaveTextContent("Search incomplete: the scan did not finish");
  expect(screen.getByTestId("find-incomplete")).toHaveTextContent("The scan failed: disk went away");
});

test("Enter and a double click open the hit; the menu opens the folder and copies paths", async () => {
  const user = userEvent.setup();
  const onOpen = vi.fn();
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
  const b = backend({
    scopes: [scope(0, "/data")],
    answer: (text) =>
      text === ""
        ? { hits: [], total_hits: 0, listed: 0, offset: 0, total_bytes: 0, archives_with_hits: 0, terms: [], coverage: coverage() }
        : { hits: [hit(0, "a/one.txt"), hit(1, "b/two.txt")], total_hits: 2, listed: 2, offset: 0, total_bytes: 0, archives_with_hits: 0, terms: [], coverage: coverage() },
  });
  render(<Finder active launchToken={0} onOpen={onOpen} />);
  await screen.findAllByTestId("find-scope");
  await user.type(screen.getByTestId("find-query"), "txt");
  const rows = await screen.findAllByTestId("find-hit");
  // A single click only selects.
  await user.click(rows[0] as HTMLElement);
  expect(onOpen).not.toHaveBeenCalled();
  await user.dblClick(rows[1] as HTMLElement);
  expect(onOpen).toHaveBeenCalledTimes(1);
  expect(onOpen.mock.calls[0]?.[0]).toMatchObject({ archive: 1, run: 1 });
  (rows[0] as HTMLElement).focus();
  await user.keyboard("{Enter}");
  expect(onOpen).toHaveBeenCalledTimes(2);
  expect(onOpen.mock.calls[1]?.[0]).toMatchObject({ archive: 0 });
  // Arrow keys move between rows.
  await user.keyboard("{ArrowDown}");
  expect(rows[1]).toHaveFocus();

  fireEvent.contextMenu(rows[1] as HTMLElement, { clientX: 20, clientY: 20 });
  const menu = await screen.findByTestId("context-menu");
  expect(within(menu).getAllByRole("menuitem").map((i) => i.textContent)).toEqual([
    "Open archive",
    "Open containing folder",
    "Copy archive path",
    "Copy entry path",
  ]);
  await user.click(within(menu).getByText("Open containing folder"));
  await waitFor(() => expect(b.last("find_show_folder")).toEqual({ run: 1, archive: 1 }));
  fireEvent.contextMenu(rows[1] as HTMLElement, { clientX: 20, clientY: 20 });
  await user.click(within(await screen.findByTestId("context-menu")).getByText("Copy archive path"));
  expect(writeText).toHaveBeenLastCalledWith("/data/arch1.mochi");
  fireEvent.contextMenu(rows[1] as HTMLElement, { clientX: 20, clientY: 20 });
  await user.click(within(await screen.findByTestId("context-menu")).getByText("Copy entry path"));
  expect(writeText).toHaveBeenLastCalledWith("b/two.txt");
  fireEvent.contextMenu(rows[0] as HTMLElement, { clientX: 20, clientY: 20 });
  await user.click(within(await screen.findByTestId("context-menu")).getByText("Open archive"));
  expect(onOpen).toHaveBeenCalledTimes(3);
});

// ---- inside the window ---------------------------------------------------

const SUMMARY: ArchiveSummary = {
  handle: 8,
  file_name: "arch0.mochi",
  archive_id: "ab".repeat(16),
  profile: { tar_compatible: false, encrypted: false, redundancy_percent: null },
  commits: [{ seq: 0, commit_id: "0".repeat(64), time: null, checkpoint: true, retained: true, expired: false, holds: [] }],
  retention_error: null,
};

const row = (name: string, kind: "file" | "directory" = "file"): EntryRow => ({
  key: hex(name),
  name: { text: name.split("/").at(-1) ?? name, verbatim: true },
  kind,
  size: "100",
  child_dirs: 0,
  child_files: 0,
});

function windowBackend() {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let launch: Channel<LaunchEvent> | undefined;
  let finder: Channel<FindEvent> | undefined;
  let n = 0;
  let m = 0;
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "watch_launches":
        launch = a.onLaunch as Channel<LaunchEvent>;
        return null;
      case "watch_drops":
        return null;
      case "recent_archives":
        return [];
      case "staged_sources":
        return { items: [], destination: null, default_destination: null, format: "mochi" };
      case "find_scopes":
        return [scope(0, "/from/shell")];
      case "find_start":
        finder = a.onEvent as Channel<FindEvent>;
        return 1;
      case "find_query":
        return a.text === ""
          ? { hits: [], total_hits: 0, listed: 0, offset: 0, total_bytes: 0, archives_with_hits: 0, terms: [], coverage: coverage() }
          : { hits: [hit(0, "music/albums/song.mp3")], total_hits: 1, listed: 1, offset: 0, total_bytes: 0, archives_with_hits: 0, terms: [], coverage: coverage() };
      case "find_open_hit":
        // Rust opens the archive and tells the window which entry to show (a launch event).
        setTimeout(() => {
          if (!launch) return;
          const id = launch.id;
          act(() =>
            internals().runCallback(id, {
              index: n++,
              message: {
                event: "reveal",
                handle: SUMMARY.handle,
                key: String(a.key),
                path: { text: "music/albums/song.mp3", verbatim: true },
                kind: "file",
              } satisfies LaunchEvent,
            }),
          );
        }, 0);
        return { status: "opened", archive: SUMMARY };
      case "open_snapshot":
        return { seq: 0, commit_id: "0".repeat(64), entries: 5, root_dirs: 1, root_files: 0, catalog_source: { source: "image" } };
      case "list_children": {
        const dir = a.dir as string | null;
        const all =
          dir === null
            ? [row("music", "directory")]
            : dir === hex("music")
              ? [row("music/albums", "directory")]
              : dir === hex("music/albums")
                ? [row("music/albums/intro.mp3"), row("music/albums/song.mp3"), row("music/albums/zed.mp3")]
                : [];
        const offset = Number(a.offset);
        return { total: all.length, offset, entries: all.slice(offset, offset + Number(a.limit)) };
      }
      case "entry_details":
        return {
          key: a.key,
          path: { text: "music/albums/song.mp3", verbatim: true },
          kind: "file",
          size: "100",
          content_hash: "00",
          file_version_id: "11",
          attributes: { state: "recorded", posix: null, windows: null, mtime: null },
        };
      case "close_archive":
        return true;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return {
    calls,
    launch: (e: LaunchEvent) => {
      if (!launch) throw new Error("no launch channel");
      const id = launch.id;
      act(() => internals().runCallback(id, { index: n++, message: e }));
    },
    finder: (e: FindEvent) => {
      if (!finder) throw new Error("no find channel");
      const id = finder.id;
      act(() => internals().runCallback(id, { index: m++, message: e }));
    },
    count: (cmd: string) => calls.filter((c) => c.cmd === cmd).length,
  };
}

test("the title bar's Find opens Find; a hit opens its archive at the entry's folder with the entry selected", async () => {
  const user = userEvent.setup();
  const b = windowBackend();
  render(<Browser />);
  await user.click(screen.getByTestId("toolbar-find"));
  expect(await screen.findByRole("heading", { name: "Find in archives" })).toBeVisible();
  // Opened from the menu, it does not scan on its own.
  expect(b.count("find_start")).toBe(0);
  await screen.findAllByTestId("find-scope");
  await user.type(screen.getByTestId("find-query"), "song");
  await user.dblClick(await screen.findByTestId("find-hit"));
  expect(b.calls.find((c) => c.cmd === "find_open_hit")?.args).toEqual({
    run: 1,
    archive: 0,
    key: hex("music/albums/song.mp3"),
  });
  // The sheet closes, the archive is shown at music / albums, and song.mp3 is selected.
  await screen.findByTestId("archive-meta");
  const entries = await screen.findAllByTestId("entry");
  expect(entries.map((e) => e.textContent)).toEqual(expect.arrayContaining([expect.stringContaining("song.mp3")]));
  await waitFor(() => {
    const selected = screen.getAllByTestId("entry").filter((e) => e.getAttribute("aria-pressed") === "true");
    expect(selected).toHaveLength(1);
    expect(selected[0]).toHaveTextContent("song.mp3");
  });
  const crumbs = screen.getByTestId("crumbs");
  expect(crumbs).toHaveTextContent("music");
  expect(crumbs).toHaveTextContent("albums");
});

test("a --find launch opens Find and scans the folders Rust holds", async () => {
  const b = windowBackend();
  render(<Browser />);
  await waitFor(() => expect(b.calls.some((c) => c.cmd === "watch_launches")).toBe(true));
  b.launch({ event: "find", folders: 1 });
  expect(await screen.findByRole("heading", { name: "Find in archives" })).toBeVisible();
  await waitFor(() => expect(b.count("find_start")).toBe(1));
  b.finder({ event: "started", run: 1 });
  b.finder({ event: "finished", run: 1, state: "complete" });
  await waitFor(() => expect(screen.getByTestId("find-status")).toHaveTextContent("Ready: 40 names in 3 archives"));
});

test("Ctrl+Shift+F opens Find", async () => {
  windowBackend();
  render(<Browser />);
  expect(screen.getByTestId("tool-sheet")).toHaveAttribute("hidden");
  fireEvent.keyDown(document, { key: "F", ctrlKey: true, shiftKey: true });
  expect(await screen.findByRole("heading", { name: "Find in archives" })).toBeVisible();
});
