import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import { App } from "../App";
import type { ArchiveSummary } from "../ipc/browse";
import type { DropEvent } from "../ipc/write";
import type { RecentView } from "../ipc/start";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () =>
  (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

const HOSTILE = '<img src=x onerror="alert(1)">.mochi';

function summary(handle = 4, name = "photos.mochi"): ArchiveSummary {
  return {
    handle,
    file_name: name,
    archive_id: "ab".repeat(16),
    profile: { tar_compatible: false, encrypted: false, redundancy_percent: null },
    commits: [0].map((seq) => ({
      seq,
      commit_id: "0".repeat(64),
      time: null,
      checkpoint: true,
      retained: true,
      expired: false,
      holds: [],
    })),
    retention_error: null,
  };
}

function backend(opts: { recents?: RecentView[]; open?: () => unknown } = {}) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let dropChannel: Channel<DropEvent> | undefined;
  let dropIndex = 0;
  let recents = opts.recents ?? [];
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "staged_sources":
        return { items: [], destination: null, default_destination: null, format: "mochi" };
      case "watch_drops":
        dropChannel = a.onDrop as Channel<DropEvent>;
        return null;
      case "watch_launches":
      case "get_crash_settings":
        return cmd === "get_crash_settings"
          ? { enabled: false, include_details: false, entries: 0, bytes: 0 }
          : null;
      case "recent_archives":
        return recents;
      case "window_watch":
        return { maximized: false, focused: true };
      case "get_speed_settings":
        return { all_cores: true, cores: 4 };
      case "get_extract_settings":
        return { start: "desktop", custom: "", open_folder: true };
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
      case "forget_recent_archives":
        recents = [];
        return null;
      case "open_archive":
      case "open_recent":
      case "open_dropped_archive":
        return opts.open ? opts.open() : { status: "opened", archive: summary() };
      case "open_snapshot":
        return { seq: 0, commit_id: "c".repeat(64), entries: 0, root_dirs: 0, root_files: 0, catalog_source: { source: "image" } };
      case "list_children":
        return { total: 0, offset: 0, entries: [] };
      case "plan_extract_default":
        return { destination: "/home/me/photos", entries: 3, existing: [], existing_total: 0, merged_folders: 0, conflicts: [], conflicts_total: 0, case_insensitive: false };
      case "close_archive":
        return true;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return {
    calls,
    cmds: () => calls.map((c) => c.cmd),
    drop: (message: DropEvent) => {
      if (!dropChannel) throw new Error("no drop channel yet");
      const id = dropChannel.id;
      act(() => internals().runCallback(id, { index: dropIndex++, message }));
    },
  };
}

afterEach(() => clearMocks());

test("with no archive open, two plain actions are on screen and the honest limit is one click away in Help", async () => {
  const user = userEvent.setup();
  backend();
  render(<App />);
  const start = await screen.findByTestId("start-screen");
  expect(screen.getByRole("heading", { level: 2, name: "What would you like to do?" })).toBeInTheDocument();
  // Create and Open only: Extract is the toolbar's, and it waits for an archive.
  for (const name of ["Create archive", "Open archive"]) {
    expect(within(start).getByRole("button", { name: new RegExp(`^${name}`) })).toBeEnabled();
  }
  expect(within(start).queryByRole("button", { name: /^Extract/ })).toBeNull();
  expect(screen.getByTestId("extract-all")).toBeDisabled();
  // The honest limit (an archive is not a second copy) moved off the first screen into the Help
  // panel, which the toolbar's ? opens; it is stated once there, and does not use success wording.
  expect(screen.getByTestId("start-note")).not.toBeVisible();
  await user.click(screen.getByTestId("toolbar-help"));
  expect(screen.getByTestId("start-note")).toBeVisible();
  expect(screen.getByTestId("start-note")).toHaveTextContent("not a second copy of them");
  const text = (start.textContent ?? "").toLowerCase();
  for (const banned of ["backed up", "backup", "safe", "preserved", "secure"]) {
    expect(text).not.toContain(banned);
  }
  // Technical words are not on the first screen.
  for (const jargon of ["snapshot", "commit", "hash", "verification", "retention", "compaction"]) {
    expect(text).not.toContain(jargon);
  }
  // No recent list when there is nothing recent.
  expect(screen.queryByTestId("recent")).toBeNull();
});

test("the start screen has no tagline or wordmark, and the mascot is not an img element", async () => {
  backend();
  const { container } = render(<App />);
  const start = await screen.findByTestId("start-screen");
  expect(within(start).queryByText("Archive made simple")).toBeNull();
  expect(within(start).getByText("Drop files or an archive here")).toBeInTheDocument();
  // The mascot is decoration (a CSS background); the hostile-name checks rely on there being no img at all.
  expect(container.querySelectorAll("img")).toHaveLength(0);
  expect(container.querySelectorAll("[href]")).toHaveLength(0);
  expect(container.querySelectorAll(".mascot-face, .mascot-hero")).toHaveLength(2);
  for (const mascot of container.querySelectorAll(".mascot-face, .mascot-hero")) {
    expect(mascot).toHaveAttribute("aria-hidden", "true");
  }
});

test("the drop zone is one line; what dropping does is its tooltip, which adds to the line instead of repeating it", async () => {
  backend();
  render(<App />);
  const zone = await screen.findByTestId("start-drop-hint");
  expect(zone).toHaveTextContent("Drop files or an archive here");
  expect(zone.querySelector(".note")).toBeNull();
  const hint = zone.getAttribute("title");
  expect(hint).not.toBeNull();
  expect(hint).not.toMatch(/drag|drop/i);
  expect(hint).toContain("an archive opens.");
});

test("Settings opens in its own window over the list and takes focus", async () => {
  const user = userEvent.setup();
  backend();
  render(<App />);
  await screen.findByTestId("start-screen");
  const sheet = screen.getByTestId("tool-sheet");
  expect(sheet).not.toBeVisible();
  await user.click(screen.getByTestId("toolbar-more"));
  await user.click(screen.getByTestId("start-settings"));
  expect(sheet).toBeVisible();
  expect(document.activeElement).toBe(document.getElementById("settings-heading"));
});

test("the tools are in a closed tool window, one at a time, and the page never scrolls to them", async () => {
  backend();
  render(<App />);
  await screen.findByTestId("start-screen");
  expect(screen.queryByTestId("advanced")).toBeNull();
  const sheet = screen.getByTestId("tool-sheet");
  expect(sheet).not.toBeVisible();
  for (const id of ["rekey-heading", "check-heading", "settings-heading", "help-heading"]) {
    expect(sheet.querySelector(`#${id}`), id).not.toBeNull();
  }
  // Create and Extract are not hidden in there.
  expect(sheet.querySelector('[data-testid="start-create"]')).toBeNull();
});

test("Open archive shows the archive and replaces the start screen", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<App />);
  await user.click(await screen.findByTestId("open-archive"));
  await screen.findByTestId("archive-meta");
  expect(screen.queryByTestId("start-screen")).toBeNull();
  expect(b.cmds().slice(0, 2)).toEqual(["open_archive", "open_snapshot"]);
  expect(screen.getByTestId("archive-meta")).toHaveTextContent(/^photos\.mochi$/);
  // The archive's own actions: extract everything, extract a selection, add, remove.
  expect(screen.getByTestId("extract-all")).toBeEnabled();
  expect(screen.getByTestId("add-to-archive")).toBeEnabled();
});

test("Extract archive: pick an archive, then a plan into a new folder next to it, with no dialog for the folder", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<App />);
  await user.click(await screen.findByTestId("start-extract"));
  const panel = await screen.findByTestId("extract-panel");
  expect(b.cmds().slice(0, 2)).toEqual(["open_archive", "open_snapshot"]);
  const plan = b.calls.find((c) => c.cmd === "plan_extract_default");
  expect(plan?.args).toEqual({ handle: 4, seq: 0, keys: [] });
  expect(b.cmds()).not.toContain("plan_extract");
  expect(within(panel).getByTestId("extract-destination")).toHaveTextContent("/home/me/photos");
  expect(within(panel).getByTestId("extract-notes")).toHaveTextContent("A new folder was made");
  expect(within(panel).getByTestId("extract-confirm")).toHaveTextContent("Extract");
  // Nothing is extracted until the user confirms.
  expect(b.cmds()).not.toContain("start_job");
});

test("a cancelled Extract archive dialog leaves the start screen as it was", async () => {
  const user = userEvent.setup();
  backend({ open: () => null });
  render(<App />);
  await user.click(await screen.findByTestId("start-extract"));
  expect(screen.getByTestId("start-screen")).toBeInTheDocument();
  expect(screen.queryByTestId("extract-panel")).toBeNull();
  expect(screen.getByTestId("start-extract")).toBeEnabled();
});

test("Create archive opens the guided panel, the start screen steps aside, and Cancel brings it back", async () => {
  const user = userEvent.setup();
  backend();
  render(<App />);
  await user.click(await screen.findByTestId("start-create"));
  const panel = await screen.findByRole("group", { name: "Create a new archive" });
  expect(screen.queryByTestId("start-screen")).toBeNull();
  expect(panel.contains(document.activeElement)).toBe(true);
  expect(within(panel).getByTestId("create-step-files")).toBeInTheDocument();
  expect(within(panel).getByTestId("create-step-where")).toBeInTheDocument();
  // Options are collapsed, and Create is disabled until something is chosen.
  expect(within(panel).getByTestId("create-options")).not.toHaveAttribute("open");
  expect(within(panel).getByTestId("create-confirm")).toBeDisabled();
  await user.click(within(panel).getByTestId("write-close"));
  expect(await screen.findByTestId("start-screen")).toBeInTheDocument();
  expect(document.activeElement).toBe(screen.getByTestId("start-create"));
});

test("dropping files with nothing open goes to the create panel", async () => {
  const b = backend();
  render(<App />);
  await screen.findByTestId("start-screen");
  b.drop({
    event: "dropped",
    staged: { items: [{ name: { text: HOSTILE, verbatim: true }, kind: "file" }], destination: null, default_destination: "/home/me/x.mochi", format: "mochi" },
    problems: [],
    from_shell: false,
  });
  const panel = await screen.findByRole("group", { name: "Create a new archive" });
  expect(within(panel).getByTestId("staged-item")).toHaveTextContent(HOSTILE);
  expect(within(panel).getByTestId("save-location")).toHaveTextContent("/home/me/x.mochi");
  expect(panel.querySelector("img")).toBeNull();
});

test("dropping one .mochi file opens it, by asking Rust: the page names no path", async () => {
  const b = backend();
  render(<App />);
  await screen.findByTestId("start-screen");
  b.drop({ event: "archive", name: "photos.mochi", foreign: false });
  await screen.findByTestId("archive-meta");
  expect(b.calls.find((c) => c.cmd === "open_dropped_archive")?.args).toEqual({});
  expect(screen.queryByTestId("start-screen")).toBeNull();
});

test("recent archives are shown by name, as text, and opened by position", async () => {
  const user = userEvent.setup();
  const recents: RecentView[] = [
    { index: 0, name: HOSTILE, folder: "/home/me/<b>docs</b>" },
    { index: 1, name: "older.mochi", folder: "/home/me" },
  ];
  const b = backend({ recents });
  const { container } = render(<App />);
  const list = await screen.findByTestId("recent");
  expect(within(list).getAllByTestId("recent-open")).toHaveLength(2);
  expect(list).toHaveTextContent(HOSTILE);
  expect(container.querySelector("img, b")).toBeNull();

  await user.click(within(list).getAllByTestId("recent-open")[1] as HTMLElement);
  await screen.findByTestId("archive-meta");
  expect(b.calls.find((c) => c.cmd === "open_recent")?.args).toEqual({ index: 1 });
});

test("the recent list can be cleared", async () => {
  const user = userEvent.setup();
  const b = backend({ recents: [{ index: 0, name: "a.mochi", folder: "/x" }] });
  render(<App />);
  await user.click(await screen.findByTestId("recent-forget"));
  await screen.findByTestId("start-screen");
  expect(b.cmds()).toContain("forget_recent_archives");
  await act(async () => undefined);
  expect(screen.queryByTestId("recent")).toBeNull();
});

test("the start screen is operable by keyboard alone", async () => {
  const user = userEvent.setup();
  backend();
  render(<App />);
  const create = await screen.findByTestId("start-create");
  create.focus();
  await user.tab();
  expect(document.activeElement).toBe(screen.getByTestId("open-archive"));
  // "Open and extract" is in the toolbar's More menu: open it by keyboard, take the item.
  screen.getByTestId("toolbar-more").focus();
  await user.keyboard("{Enter}");
  expect(screen.getByTestId("toolbar-more")).toHaveAttribute("aria-expanded", "true");
  // New archive comes first; Extract, Remove and Info are off with nothing open, so they are skipped.
  await user.tab();
  expect(document.activeElement).toBe(screen.getByTestId("new-archive"));
  await user.tab();
  expect(document.activeElement).toBe(screen.getByTestId("start-extract"));
  await user.keyboard("{Enter}");
  await screen.findByTestId("extract-panel");
});

test("an encrypted archive opened for extraction asks for its passphrase first", async () => {
  const user = userEvent.setup();
  backend({ open: () => ({ status: "needs_passphrase", pending: 2, file_name: "vault.mochi" }) });
  render(<App />);
  await user.click(await screen.findByTestId("start-extract"));
  expect(await screen.findByTestId("unlock-panel")).toBeInTheDocument();
  expect(screen.queryByTestId("extract-panel")).toBeNull();
  // The start screen is not shown under the prompt.
  expect(screen.queryByTestId("start-screen")).toBeNull();
});

test("opening the archive this window already has keeps it open and shows it", async () => {
  // Rust answers with the same handle when this window already holds the file (its page may
  // have lost it): the window must show it, not close the handle it was just given.
  const user = userEvent.setup();
  const b = backend({ open: () => ({ status: "opened", archive: summary(7, "Test.mochi") }) });
  render(<App />);
  await user.click(await screen.findByTestId("open-archive"));
  await screen.findByTestId("archive-meta");
  await user.click(screen.getByTestId("toolbar-open"));
  await act(async () => undefined);
  expect(b.cmds().filter((c) => c === "open_archive")).toHaveLength(2);
  expect(b.cmds()).not.toContain("close_archive");
  expect(screen.getByTestId("archive-meta")).toBeInTheDocument();
});
