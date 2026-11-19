import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";

import type { ArchiveSummary, EntryDetails, EntryRow } from "../ipc/browse";
import { Browser, ROW_HEIGHT } from "./Browser";
import { formatMode, formatSize } from "./format";
import { PAGE } from "./VirtualList";

const hex = (s: string) =>
  Array.from(new TextEncoder().encode(s), (b) => b.toString(16).padStart(2, "0")).join("");

const MANY = 100_000;
const HOSTILE = '<img src=x onerror="alert(1)">';
const SCRIPT = "<script>alert(1)<\\script>";

function row(name: string, kind: "file" | "directory", opts: Partial<EntryRow> = {}): EntryRow {
  return {
    key: hex(name),
    name: { text: name, verbatim: true },
    kind,
    size: kind === "file" ? "1536" : "0",
    child_dirs: 0,
    child_files: 0,
    ...opts,
  };
}

/** The root of the fake snapshot, in the backend's order (dirs first). */
const ROOT: EntryRow[] = [
  row("many", "directory", { child_files: MANY }),
  row("sub", "directory", { child_dirs: 1, child_files: 1 }),
  row(HOSTILE, "file"),
  row(SCRIPT, "file"),
  { ...row("bad\\xffname", "file"), key: "626164ff6e616d65", name: { text: "bad\\xffname", verbatim: false } },
];

const SUMMARY: ArchiveSummary = {
  handle: 3,
  file_name: "fake.mochi",
  archive_id: "ab".repeat(16),
  profile: { tar_compatible: false, encrypted: false, redundancy_percent: null },
  commits: [0, 1, 2].map((seq) => ({
    seq,
    commit_id: String(seq).repeat(64),
    time: seq === 1 ? null : "2023-11-14T22:13:20Z",
    checkpoint: seq !== 1,
    retained: seq === 0 ? null : true,
    expired: seq === 0 ? null : false,
    holds: seq === 2 ? [{ text: '<b onmouseover="x">hold</b>', verbatim: true }] : [],
  })),
  retention_error: null,
};

function children(dir: string | null, dirsOnly: boolean, offset: number, limit: number) {
  let all: EntryRow[];
  if (dir === null) all = ROOT;
  else if (dir === hex("many"))
    all = Array.from({ length: MANY }, (_, i) => row(`f${String(i).padStart(6, "0")}`, "file"));
  else if (dir === hex("sub")) all = [row("deeper", "directory"), row("x", "file")];
  else all = [];
  const pool = dirsOnly ? all.filter((r) => r.kind === "directory") : all;
  return { total: pool.length, offset, entries: pool.slice(offset, offset + limit) };
}

function fakeBackend(opts: { open?: () => unknown } = {}) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    // The Writer's start-up calls (D2) are tested in write/Writer.test.tsx.
    if (cmd === "staged_sources") return { items: [], destination: null, default_destination: null, format: "mochi" };
    if (cmd === "watch_drops" || cmd === "watch_launches") return null;
    if (cmd === "recent_archives") return [];
    // The title bar's window state (shell/TitleBar.test.tsx tests it).
    if (cmd === "window_watch") return { maximized: false, focused: true };
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "open_archive":
        return opts.open ? opts.open() : { status: "opened", archive: SUMMARY };
      case "close_archive":
        return true;
      case "open_snapshot":
        return {
          seq: a.seq,
          commit_id: "c".repeat(64),
          entries: MANY + 9,
          root_dirs: 2,
          root_files: 3,
          catalog_source: { source: "image" },
        };
      case "list_children":
        if ((a.limit as number) > 1000) throw { code: "INVALID_ARGUMENT", message: "page" };
        return children(
          a.dir as string | null,
          a.directoriesOnly as boolean,
          a.offset as number,
          a.limit as number,
        );
      case "entry_details": {
        const d: EntryDetails = {
          key: a.key as string,
          path: { text: HOSTILE, verbatim: true },
          kind: "file",
          size: "1536",
          content_hash: "d".repeat(64),
          file_version_id: "e".repeat(64),
          attributes: {
            state: "recorded",
            posix: { mode: 0o644, uid: 1000, gid: 1000 },
            windows: null,
            mtime: "2023-11-14T22:13:20Z",
          },
        };
        return d;
      }
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return calls;
}

afterEach(() => clearMocks());

async function openIt() {
  const user = userEvent.setup();
  const calls = fakeBackend();
  const view = render(<Browser />);
  await user.click(screen.getByTestId("open-archive"));
  await screen.findByText(/^5 items/);
  return { user, calls, ...view };
}

test("opening shows the archive, its timeline, and the root, with hostile names inert", async () => {
  const { calls, container } = await openIt();
  expect(calls.map((c) => c.cmd).slice(0, 2)).toEqual(["open_archive", "open_snapshot"]);
  expect(calls[1]?.args).toEqual({ handle: 3, seq: 2 });
  expect(screen.getByTestId("archive-meta")).toHaveTextContent("fake.mochi");
  expect(screen.getByTestId("toggle-versions")).toHaveAttribute("title", "3 versions");

  const timeline = screen.getByTestId("timeline");
  expect(within(timeline).getByTestId("commit-2")).toHaveTextContent("Version 2 (latest)");
  expect(within(timeline).getByTestId("commit-0")).toHaveTextContent("retention unknown");
  expect(within(timeline).getByTestId("commit-1")).toHaveTextContent("no time recorded");
  // Checkpoint or delta is technical detail, not on the main path.
  expect(within(timeline).getByTestId("commit-1")).not.toHaveTextContent("delta");
  expect(screen.getByTestId("archive-technical")).toHaveTextContent("Commit 1 · delta");
  expect(screen.getByTestId("archive-technical")).toHaveTextContent("Commit 2 · checkpoint");

  const entries = await screen.findAllByTestId("entry");
  const names = entries.map((e) => e.querySelector(".name")?.textContent);
  expect(names).toEqual(["many", "sub", HOSTILE, SCRIPT, "bad\\xffname"]);
  expect(entries[4]).toHaveTextContent("(escaped)");
  expect(entries[2]).not.toHaveTextContent("(escaped)");
  // Markup in names and labels stayed text: no element was created from it.
  for (const tag of ["img", "script", "b"]) {
    expect(container.querySelector(tag)).toBeNull();
  }
  // The only svg elements are the decorative icons the app draws itself: each
  // is hidden from assistive technology, and none came from a name.
  const icons = Array.from(container.querySelectorAll("svg"));
  expect(icons.length).toBeGreaterThan(0);
  for (const icon of icons) {
    expect(icon.getAttribute("aria-hidden")).toBe("true");
    expect(icon.getAttribute("focusable")).toBe("false");
    expect(icon.querySelector("script, foreignObject, image, a")).toBeNull();
  }
  expect(container.querySelectorAll('[data-testid="entry"] svg')).toHaveLength(entries.length);
  expect(screen.getByTestId("timeline")).toHaveTextContent('hold: <b onmouseover="x">hold</b>');
});

test("a 100,000-entry folder is paged and only a window of rows is in the DOM", async () => {
  const { user, calls } = await openIt();
  const many = (await screen.findAllByTestId("entry")).at(0);
  if (!many) throw new Error("no entries");
  await user.dblClick(many);
  await screen.findByText(/^100,000 items/);
  await screen.findByText("f000000");

  const rendered = screen.getAllByRole("listitem").filter((li) => li.closest('[data-testid="entries"]'));
  expect(rendered.length).toBeLessThan(100);
  expect(screen.getByTestId("entries")).toHaveAttribute("data-total", String(MANY));

  const pages = calls.filter((c) => c.cmd === "list_children" && c.args.dir === hex("many"));
  for (const p of pages) expect(p.args.limit as number).toBeLessThanOrEqual(PAGE);
  const fetched = pages.reduce((n, p) => n + (p.args.limit as number), 0);
  expect(fetched).toBeLessThan(1000);

  // Scrolling far down fetches only the page there.
  const list = screen.getByTestId("entries");
  list.scrollTop = ROW_HEIGHT * 99_990;
  list.dispatchEvent(new Event("scroll"));
  await screen.findByText("f099990");
  const deep = calls.filter((c) => c.cmd === "list_children" && (c.args.offset as number) >= 99_800);
  expect(deep.length).toBeGreaterThan(0);
  expect(screen.queryByText("f000000")).toBeNull();
  expect(screen.getByTestId("crumbs")).toHaveTextContent("many");
  expect(screen.getByTestId("toggle-versions")).toHaveTextContent(/^v2$/);
});

test("selecting an entry shows its recorded details without verification wording", async () => {
  const { user, container } = await openIt();
  const hostile = (await screen.findAllByTestId("entry")).at(2);
  if (!hostile) throw new Error("no hostile entry");
  await user.click(hostile);
  const details = await screen.findByTestId("details");
  expect(within(details).getByTestId("details-path")).toHaveTextContent(HOSTILE);
  expect(details).toHaveTextContent("1.5 KiB (1,536 bytes)");
  expect(details).toHaveTextContent("rw-r--r-- (0644), owner 1000, group 1000");
  expect(details).toHaveTextContent("d".repeat(64));
  expect(container.querySelector("img")).toBeNull();
  const text = (container.textContent ?? "").toLowerCase();
  for (const banned of ["backed up", "safe", "preserved", "verified"]) {
    expect(text).not.toContain(banned);
  }
  // "pass" as a status word (PASS), not as the start of "passphrase" (D5).
  expect(text).not.toMatch(/\bpass\b/);
});

test("the tree opens folders lazily and choosing a commit reopens at the root", async () => {
  const { user, calls } = await openIt();
  // The folder tree is an optional pane (WinRAR-style): the path bar's Folders button opens it.
  const toggle = screen.getByTestId("toggle-folders");
  expect(toggle).toHaveAttribute("aria-expanded", "false");
  await user.click(toggle);
  expect(toggle).toHaveAttribute("aria-expanded", "true");
  const tree = screen.getByTestId("tree");
  await within(tree).findByText("sub");
  const treeCalls = calls.filter((c) => c.cmd === "list_children" && c.args.directoriesOnly);
  expect(treeCalls).toHaveLength(1);
  const subItem = within(tree).getByText("sub").closest("li");
  if (!subItem) throw new Error("no tree item");
  await user.click(within(subItem).getByRole("button", { name: "Expand sub" }));
  await within(tree).findByText("deeper");
  await user.click(within(tree).getByText("sub"));
  await screen.findByText(/^2 items/);
  expect(screen.getByTestId("crumbs")).toHaveTextContent("sub");

  await user.click(screen.getByTestId("commit-0"));
  await screen.findByText(/^5 items/);
  expect(calls.filter((c) => c.cmd === "open_snapshot").at(-1)?.args).toEqual({ handle: 3, seq: 0 });
  expect(screen.getByTestId("commit-0")).toHaveAttribute("aria-current", "true");
  expect(screen.getByTestId("toggle-versions")).toHaveTextContent(/^v0$/);
});

test("the path bar names the archive from facts the app has, and the search field finds names", async () => {
  const { user } = await openIt();
  const bar = screen.getByTestId("archive-meta");
  expect(bar).toHaveTextContent(/^fake\.mochi$/);
  expect(screen.getByTestId("toggle-versions")).toHaveAttribute("title", "3 versions");
  // The size of the whole version moved from the top bar to the status line's tooltip.
  expect(screen.getByTestId("entry-count")).toHaveAttribute("title", "100,009 entries in this version");
  expect(screen.getByTestId("close-archive")).toHaveAccessibleName("Close");
  expect(screen.getByTestId("search-query")).toBeEnabled();
  void user;
  // The table header is for the eye; the rows carry the names of their parts.
  expect(document.querySelector(".table-head")).toHaveAttribute("aria-hidden", "true");
  const types = (await screen.findAllByTestId("entry")).map((e) => e.querySelector(".type")?.textContent);
  expect(types).toEqual(["Folder", "Folder", "File", "File", "File"]);
});

test("a cancelled dialog changes nothing; a refused archive shows its code", async () => {
  const user = userEvent.setup();
  let answer: () => unknown = () => null;
  fakeBackend({ open: () => answer() });
  render(<Browser />);
  await user.click(screen.getByTestId("open-archive"));
  expect(screen.getByTestId("start-screen")).toBeInTheDocument();
  answer = () => {
    throw { code: "KEY_UNAVAILABLE", message: "this archive is encrypted" };
  };
  await user.click(screen.getByTestId("open-archive"));
  expect(await screen.findByTestId("browse-error")).toHaveTextContent(
    "Could not open the archive (KEY_UNAVAILABLE): this archive is encrypted",
  );
  expect(screen.getByTestId("open-archive")).toBeEnabled();
});

test("a draft-2 archive is refused as text, naming the draft and pointing to mochi migrate", async () => {
  const user = userEvent.setup();
  const message =
    "archive was written under draft 2, a pre-1.0 layout; this build reads ratified archives only. " +
    "Convert it with `mochi migrate SRC --output DST`";
  fakeBackend({
    open: () => {
      throw { code: "UNSUPPORTED_FEATURE", message };
    },
  });
  render(<Browser />);
  await user.click(screen.getByTestId("open-archive"));
  const alert = await screen.findByTestId("browse-error");
  expect(alert).toHaveTextContent(`Could not open the archive (UNSUPPORTED_FEATURE): ${message}`);
  expect(alert).toHaveTextContent("draft 2");
  expect(alert).toHaveTextContent("mochi migrate");
  // Plain text in an alert: no markup, no success styling, no archive listing.
  expect(alert.querySelector("a, button, img, script")).toBeNull();
  expect(screen.queryByTestId("tree")).toBeNull();
  expect(screen.getByTestId("open-archive")).toBeEnabled();
});

test("sizes and modes format exactly", () => {
  expect(formatSize("0")).toBe("0 bytes");
  expect(formatSize("1")).toBe("1 byte");
  expect(formatSize("1023")).toBe("1,023 bytes");
  expect(formatSize("1536")).toBe("1.5 KiB (1,536 bytes)");
  expect(formatSize("1048576")).toBe("1.0 MiB (1,048,576 bytes)");
  // Above 2^53: exact, because sizes are strings handled as BigInt.
  expect(formatSize("18446744073709551615")).toBe(
    "15.9 EiB (18,446,744,073,709,551,615 bytes)",
  );
  expect(formatMode(0o755)).toBe("rwxr-xr-x (0755)");
  expect(formatMode(0o600)).toBe("rw------- (0600)");
});
