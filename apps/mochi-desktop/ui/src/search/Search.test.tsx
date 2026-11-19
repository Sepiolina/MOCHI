// The toolbar's search field, the list's multiple selection, and dragging
// entries out (owner request, 0.9.4).
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import type { ArchiveSummary, EntryRow } from "../ipc/browse";
import type { JobEvent } from "../ipc/jobs";
import { Browser } from "../browser/Browser";
import { ForeignBrowser } from "../foreign/ForeignBrowser";
import { ancestorsOf } from "../list/keys";
import { SEARCH_DELAY } from "./SearchBox";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () => (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;
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
  child_files: kind === "directory" ? 1 : 0,
});
const ROOT = [row("albums", "directory"), row("song.mp3"), row("track.flac"), row("notes.txt")];

function backend() {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let channel: Channel<JobEvent> | undefined;
  let n = 0;
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
        return { seq: 0, commit_id: "0".repeat(64), entries: 5, root_dirs: 1, root_files: 3, catalog_source: { source: "image" } };
      case "list_children": {
        const dir = a.dir as string | null;
        const all = dir === null ? ROOT : dir === hex("albums") ? [row("albums/x.mp3")] : [];
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
      case "start_job":
        channel = a.onEvent as Channel<JobEvent>;
        return 7;
      case "cancel_job":
      case "close_archive":
      case "close_foreign_archive":
        return true;
      case "drag_out":
      case "drag_out_foreign":
        return null;
      case "plan_extract_default":
        return { destination: "/home/me/music", entries: 2, existing: [], existing_total: 0, merged_folders: 0, conflicts: [], conflicts_total: 0, case_insensitive: false };
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
      case "foreign_children":
        return { total: 0, offset: 0, entries: [] };
      case "foreign_search":
        return {
          hits: [
            {
              row: { ...row("a.mp3"), key: hex("disc/a.mp3"), name: { text: "a.mp3", verbatim: true }, size: "9", encryption: null, note: null },
              path: { text: "disc/a.mp3", verbatim: true },
              parent: hex("disc"),
            },
          ],
          total_hits: 1,
        };
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return {
    calls,
    job: (m: JobEvent) => {
      if (!channel) throw new Error("no channel");
      const id = channel.id;
      act(() => internals().runCallback(id, { index: n++, message: m }));
    },
    requests: () => calls.filter((c) => c.cmd === "start_job").map((c) => c.args.request),
  };
}

afterEach(() => clearMocks());

async function openIt() {
  const user = userEvent.setup();
  const b = backend();
  render(<Browser foreign={<ForeignBrowser />} />);
  await user.click(screen.getByTestId("open-archive"));
  await screen.findAllByTestId("entry");
  return { user, b };
}

test("the search field is off until an archive is open", () => {
  backend();
  render(<Browser />);
  expect(screen.getByTestId("search-query")).toBeDisabled();
});

test("typing searches the names of the version on screen and lists the matches instead of the folder", async () => {
  const { user, b } = await openIt();
  await user.type(screen.getByTestId("search-query"), "mp3");
  await waitFor(() => expect(b.requests()).toHaveLength(1), { timeout: SEARCH_DELAY * 4 });
  expect(b.requests()[0]).toEqual({
    kind: "search_archive",
    handle: 3,
    query: "mp3",
    ignore_case: true,
    scope: { scope: "commit", seq: 0 },
    entry_kind: null,
  });
  expect(screen.getByTestId("search-running")).toBeInTheDocument();
  b.job({ event: "started", job_id: 7 });
  b.job({
    event: "finished",
    job_id: 7,
    outcome: {
      status: "completed",
      summary: "",
      search: {
        scope: { scope: "commit", seq: 0 },
        hits: [
          { seq: 0, key: hex("song.mp3"), parent: null, path: { text: "song.mp3", verbatim: true }, kind: "file", size: "100" },
          {
            seq: 0,
            key: hex(`albums/${HOSTILE}.mp3`),
            parent: hex("albums"),
            path: { text: `albums/${HOSTILE}.mp3`, verbatim: true },
            kind: "file",
            size: "5",
          },
        ],
        total_hits: 2,
        coverage: {
          complete: true,
          requested: [{ from: 0, to: 0 }],
          searched: [{ from: 0, to: 0 }],
          requested_count: 1,
          searched_count: 1,
          unavailable: [],
          opened_separately: [],
          entries_examined: 5,
          indexed_seq: 0,
          catalog_from_manifest: false,
          content_not_searched: true,
        },
      },
    },
  });
  const hits = await screen.findAllByTestId("search-hit");
  expect(hits).toHaveLength(2);
  expect(screen.queryByTestId("entry")).toBeNull();
  // Names are text, the folder is a column, and the count and the coverage are in the status bar.
  expect(hits[1]).toHaveTextContent(`${HOSTILE}.mp3`);
  expect(hits[1]?.querySelector(".folder")).toHaveTextContent(/^albums$/);
  expect(document.querySelector("img")).toBeNull();
  const status = screen.getByTestId("entry-count");
  expect(status).toHaveTextContent(/^2 matches$/);
  expect(status.getAttribute("title")).toContain("File contents were not searched");

  // A hit can be selected and extracted like any entry.
  await user.click(hits[1] as HTMLElement);
  await user.click(screen.getByTestId("extract-all"));
  await screen.findByTestId("extract-panel");
  expect(b.calls.find((c) => c.cmd === "plan_extract_default")?.args).toEqual({
    handle: 3,
    seq: 0,
    keys: [hex(`albums/${HOSTILE}.mp3`)],
  });
});

test("a new query cancels the search before it, and clearing the field shows the folder again", async () => {
  const { user, b } = await openIt();
  const field = screen.getByTestId("search-query");
  await user.type(field, "a{Enter}");
  await waitFor(() => expect(b.requests()).toHaveLength(1));
  b.job({ event: "started", job_id: 7 });
  await user.type(field, "b{Enter}");
  await waitFor(() => expect(b.calls.some((c) => c.cmd === "cancel_job")).toBe(true));
  await user.click(screen.getByTestId("search-clear"));
  expect(await screen.findAllByTestId("entry")).toHaveLength(4);
  expect(field).toHaveValue("");
});

test("double-clicking a hit opens the folder it is in", () => {
  // The folders above a hit come from the prefixes of its key that end before a "/".
  expect(ancestorsOf(hex("a/b/c.txt"), { text: "a/b/c.txt", verbatim: true })).toEqual([
    { key: hex("a"), name: { text: "a", verbatim: true } },
    { key: hex("a/b"), name: { text: "a/b".split("/")[1] ?? "", verbatim: true } },
  ]);
  expect(ancestorsOf(hex("top.txt"), { text: "top.txt", verbatim: true })).toEqual([]);
  // An escaped name whose split does not match the key gives no path rather than a wrong one.
  expect(ancestorsOf(hex("a/b"), { text: "a\\x2fb", verbatim: false })).toEqual([]);
});

test("Ctrl+A selects the whole folder and Extract takes all of it", async () => {
  const { user, b } = await openIt();
  const rows = screen.getAllByTestId("entry");
  (rows[0] as HTMLElement).focus();
  await user.keyboard("{Control>}a{/Control}");
  await waitFor(() => expect(screen.getByTestId("entry-count")).toHaveTextContent("4 items · 4 selected"));
  // Escape selects nothing again; Ctrl+A once more for the extract.
  await user.keyboard("{Escape}");
  await waitFor(() => expect(screen.getByTestId("entry-count")).toHaveTextContent(/^4 items$/));
  await user.keyboard("{Control>}a{/Control}");
  await waitFor(() => expect(screen.getByTestId("entry-count")).toHaveTextContent("4 items · 4 selected"));
  await user.click(screen.getByTestId("extract-all"));
  await screen.findByTestId("extract-panel");
  expect(b.calls.find((c) => c.cmd === "plan_extract_default")?.args.keys).toEqual(ROOT.map((r) => r.key));
});

test("Backspace goes up a folder, and Delete asks to remove the selection", async () => {
  const { user } = await openIt();
  await user.dblClick(screen.getAllByTestId("entry")[0] as HTMLElement);
  await waitFor(() => expect(screen.getAllByTestId("entry")).toHaveLength(1));
  (screen.getAllByTestId("entry")[0] as HTMLElement).focus();
  await user.keyboard("{Backspace}");
  await waitFor(() => expect(screen.getAllByTestId("entry")).toHaveLength(4));
  await user.click(screen.getAllByTestId("entry")[1] as HTMLElement);
  await user.keyboard("{Delete}");
  expect(await screen.findByTestId("delete-note")).toHaveTextContent("stays in the archive's history");
});

test("dragging a row out asks Rust to drag the selection (or the row), naming no path", async () => {
  const { user, b } = await openIt();
  const rows = screen.getAllByTestId("entry");
  await user.click(rows[1] as HTMLElement);
  await user.keyboard("{Control>}");
  await user.click(rows[2] as HTMLElement);
  await user.keyboard("{/Control}");
  // A press on the name text (not on the row's empty parts, which start a rubber band).
  const nameOf = (row: HTMLElement) => row.querySelector<HTMLElement>(".name") ?? row;
  const drag = (el: HTMLElement) => {
    act(() => {
      el.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, button: 0, buttons: 1, clientX: 10, clientY: 10, pointerId: 1, pointerType: "mouse" }));
      el.dispatchEvent(new PointerEvent("pointermove", { bubbles: true, button: 0, buttons: 1, clientX: 40, clientY: 12, pointerId: 1, pointerType: "mouse" }));
    });
  };
  drag(nameOf(rows[2] as HTMLElement));
  await waitFor(() => expect(b.calls.filter((c) => c.cmd === "drag_out")).toHaveLength(1));
  expect(b.calls.find((c) => c.cmd === "drag_out")?.args).toEqual({ handle: 3, seq: 0, keys: [hex("song.mp3"), hex("track.flac")] });
  // A row outside the selection drags alone.
  drag(nameOf(rows[3] as HTMLElement));
  await waitFor(() => expect(b.calls.filter((c) => c.cmd === "drag_out")).toHaveLength(2));
  expect(b.calls.filter((c) => c.cmd === "drag_out")[1]?.args).toEqual({ handle: 3, seq: 0, keys: [hex("notes.txt")] });
});

test("a foreign archive is searched in Rust, names only", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<Browser foreign={<ForeignBrowser />} />);
  await user.click(screen.getByTestId("toolbar-more"));
  await user.click(screen.getByTestId("menu-foreign"));
  await screen.findByTestId("foreign-meta");
  await user.type(screen.getByTestId("search-query"), "mp3{Enter}");
  const hits = await screen.findAllByTestId("search-hit");
  expect(hits).toHaveLength(1);
  expect(hits[0]).toHaveTextContent("a.mp3");
  expect(hits[0]?.querySelector(".folder")).toHaveTextContent(/^disc$/);
  expect(b.calls.find((c) => c.cmd === "foreign_search")?.args).toEqual({ handle: 5, query: "mp3", ignoreCase: true });
  expect(screen.getByTestId("foreign-count")).toHaveTextContent(/^1 match$/);
});

test("a partial search says so in its line (spec §23.3 #8), and a cut list says how many are shown", async () => {
  const { foundText } = await import("./useArchiveSearch");
  expect(foundText(2, 2, true)).toBe("2 matches");
  expect(foundText(1000, 4000, true)).toBe("4,000 matches (first 1,000 shown)");
  expect(foundText(0, 0, false)).toBe("0 matches · coverage PARTIAL");
});
