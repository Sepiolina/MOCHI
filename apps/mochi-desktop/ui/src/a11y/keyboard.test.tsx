// Plan D7 accessibility: the whole window is operable from the keyboard, focus
// is never lost or trapped, and results reach a screen reader. jsdom has no
// layout, so these tests pin the behaviour that does not depend on it: tab
// order, key handling, and where focus goes.
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import { App } from "../App";
import type { ArchiveSummary, EntryRow } from "../ipc/browse";

const hex = (s: string) =>
  Array.from(new TextEncoder().encode(s), (b) => b.toString(16).padStart(2, "0")).join("");

const MANY = 5_000;

function row(name: string, kind: "file" | "directory", extra: Partial<EntryRow> = {}): EntryRow {
  return {
    key: hex(name),
    name: { text: name, verbatim: true },
    kind,
    size: "10",
    child_dirs: 0,
    child_files: 0,
    ...extra,
  };
}

const ROOT = [row("many", "directory", { child_files: MANY }), row("a.txt", "file"), row("b.txt", "file")];

const SUMMARY: ArchiveSummary = {
  handle: 1,
  file_name: "keys.mochi",
  archive_id: "ab".repeat(16),
  profile: { tar_compatible: false, encrypted: false, redundancy_percent: null },
  commits: [{ seq: 0, commit_id: "0".repeat(64), time: null, checkpoint: true, retained: true, expired: false, holds: [] }],
  retention_error: null,
};

function backend(opts: { encrypted?: boolean } = {}) {
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "staged_sources":
        return { items: [], destination: null, default_destination: null, format: "mochi" };
      case "watch_drops":
      case "watch_launches":
        return null;
      case "get_crash_settings":
        return { enabled: false, include_details: false, entries: 0, bytes: 0 };
      case "open_archive":
        return opts.encrypted
          ? { status: "needs_passphrase", pending: 5, file_name: "vault.mochi" }
          : { status: "opened", archive: SUMMARY };
      case "unlock_archive":
        throw { code: "KEY_UNAVAILABLE", message: "no" };
      case "cancel_unlock":
      case "close_archive":
        return true;
      case "open_snapshot":
        return { seq: 0, commit_id: "c".repeat(64), entries: MANY + 3, root_dirs: 1, root_files: 2, catalog_source: { source: "image" } };
      case "list_children": {
        const dir = a.dir as string | null;
        const all =
          dir === null
            ? ROOT
            : dir === hex("many")
              ? Array.from({ length: MANY }, (_, i) => row(`f${String(i).padStart(5, "0")}`, "file"))
              : [];
        const pool = a.directoriesOnly ? all.filter((r) => r.kind === "directory") : all;
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
    }
    throw new Error(`unexpected command ${cmd}`);
  });
}

afterEach(() => clearMocks());

const focused = () => document.activeElement as HTMLElement;
const entryButtons = () => screen.queryAllByTestId("entry");

async function openArchive(user: ReturnType<typeof userEvent.setup>) {
  screen.getByTestId("open-archive").focus();
  await user.keyboard("{Enter}");
  await screen.findByText(/\d items/);
  await screen.findByText("a.txt");
}

test("opening an archive, choosing a file, and closing it need no pointer", async () => {
  backend();
  const user = userEvent.setup();
  render(<App />);
  await openArchive(user);

  // Roving focus: the list is one tab stop, not one per row.
  const stops = entryButtons().filter((b) => b.tabIndex === 0);
  expect(stops).toHaveLength(1);

  stops[0]?.focus();
  await user.keyboard("{ArrowDown}");
  expect(focused()).toBe(entryButtons()[1]);
  await user.keyboard("{Enter}");
  expect(await screen.findByTestId("details")).toBeInTheDocument();
  expect(focused()).toBe(entryButtons()[1]); // choosing keeps focus where the user is
  expect(entryButtons()[1]).toHaveAttribute("aria-pressed", "true");

  screen.getByTestId("close-archive").focus();
  await user.keyboard("{Enter}");
  expect(await screen.findByTestId("start-screen")).toBeInTheDocument();
});

test("arrow keys, Page Up/Down, Home and End move through a 5,000-row folder", async () => {
  backend();
  const user = userEvent.setup();
  render(<App />);
  await openArchive(user);

  // Enter on a folder opens it and puts focus in the new listing.
  entryButtons()[0]?.focus();
  await user.keyboard("{Enter}");
  await screen.findByText("f00000");
  await waitFor(() => expect(focused()).toHaveTextContent("f00000"));

  await user.keyboard("{End}");
  await waitFor(() => expect(focused()).toHaveTextContent("f04999"));
  expect(screen.getByTestId("entries").getAttribute("data-total")).toBe(String(MANY));
  // Still only one tab stop, and it is the row that has focus.
  const stops = entryButtons().filter((b) => b.tabIndex === 0);
  expect(stops).toEqual([focused()]);

  await user.keyboard("{Home}");
  await waitFor(() => expect(focused()).toHaveTextContent("f00000"));
  await user.keyboard("{PageDown}");
  await waitFor(() => expect(focused()).not.toHaveTextContent("f00000"));
  const afterPage = focused().textContent ?? "";
  await user.keyboard("{PageUp}");
  await waitFor(() => expect(focused()).toHaveTextContent("f00000"));
  expect(afterPage).toMatch(/f000\d\d/);

  // The keys do nothing unmodified keys should not: Ctrl+Down is left to the browser.
  await user.keyboard("{Control>}{ArrowDown}{/Control}");
  expect(focused()).toHaveTextContent("f00000");
});

test("Tab visits every control once and comes back round: nothing traps focus", async () => {
  backend();
  const user = userEvent.setup();
  render(<App />);
  await openArchive(user);
  await user.click(screen.getByTestId("new-archive"));
  await screen.findByTestId("write-panel");

  focused().blur(); // start from the top of the document
  const visited: Element[] = [];
  for (let i = 0; i < 400; i++) {
    await user.tab();
    const el = document.activeElement;
    if (el === null || el === document.body) break; // wrapped out of the document
    expect(visited, `stuck on ${el.outerHTML.slice(0, 80)}`).not.toContain(el);
    visited.push(el);
  }
  expect(visited.length, "a window this size has many tab stops").toBeGreaterThan(15);
  expect(visited.length, "and the cycle ends").toBeLessThan(400);
  // Everything reached is something a person can operate and is not disabled or hidden.
  for (const el of visited) {
    expect((el as HTMLButtonElement).disabled).not.toBe(true);
    expect(el.closest("[hidden],[aria-hidden=true]")).toBeNull();
  }

  // And back the other way, one step at a time, without getting stuck.
  const back: Element[] = [];
  for (let i = 0; i < visited.length + 5; i++) {
    await user.tab({ shift: true });
    const el = document.activeElement;
    if (el === null || el === document.body) break;
    expect(back).not.toContain(el);
    back.push(el);
  }
  const name = (e: Element) => e.getAttribute('data-testid') ?? e.tagName;
  expect(back.map(name).reverse()).toEqual(visited.map(name));
});

test("an opened panel takes focus, and closing it gives focus back to the button that opened it", async () => {
  backend();
  const user = userEvent.setup();
  render(<App />);
  const opener = screen.getByTestId("start-create");
  opener.focus();
  await user.keyboard("{Enter}");
  const panel = await screen.findByRole("group", { name: "Create a new archive" });
  expect(panel.contains(focused())).toBe(true);
  expect(focused().tagName).toBe("H3");

  // The panel is inline: Tab goes on into its controls, not round a trap.
  await user.tab();
  expect(panel.contains(focused())).toBe(true);

  screen.getByTestId("write-close").focus();
  await user.keyboard("{Enter}");
  await waitFor(() => expect(screen.queryByTestId("write-panel")).toBeNull());
  // The start screen steps aside while the panel is open and comes back; focus goes to its button.
  await waitFor(() => expect(focused()).toBe(screen.getByTestId("start-create")));
});

test("a wrong passphrase leaves focus in the passphrase field with the error attached to it", async () => {
  backend({ encrypted: true });
  const user = userEvent.setup();
  render(<App />);
  screen.getByTestId("open-archive").focus();
  await user.keyboard("{Enter}");
  const panel = await screen.findByTestId("unlock-panel");
  expect(focused().textContent).toBe("vault.mochi is encrypted");
  expect(panel.contains(focused())).toBe(true);

  const input = screen.getByTestId("unlock-input");
  await user.click(input);
  // jsdom keeps focus on a field that gets disabled, a browser does not, so
  // watch for the call that gives it back rather than only for where it ends.
  const focusCalls: unknown[] = [];
  const original = HTMLElement.prototype.focus;
  HTMLElement.prototype.focus = function (this: HTMLElement, ...args: Parameters<typeof original>) {
    focusCalls.push(this);
    return original.apply(this, args);
  };
  try {
    await user.keyboard("nope{Enter}");
    await screen.findByTestId("unlock-error");
    await waitFor(() => expect(focusCalls).toContain(input));
  } finally {
    HTMLElement.prototype.focus = original;
  }
  const alert = await screen.findByTestId("unlock-error");
  expect(alert).toHaveAttribute("role", "alert");
  await waitFor(() => expect(focused()).toBe(input));
  expect(input).toHaveAttribute("aria-invalid", "true");
  expect(input).toHaveAccessibleDescription(/did not open this archive/);

  // Cancelling returns focus to where the prompt came from.
  await user.keyboard("{Tab}{Tab}");
  await user.keyboard("{Enter}");
  await waitFor(() => expect(screen.queryByTestId("unlock-panel")).toBeNull());
});

test("a tool from the More menu opens in its own window and takes focus on its heading", async () => {
  backend();
  const user = userEvent.setup();
  render(<App />);
  screen.getByTestId("toolbar-more").focus();
  await user.keyboard("{Enter}");
  screen.getByTestId("menu-repair").focus();
  await user.keyboard("{Enter}");
  expect(screen.getByTestId("tool-sheet")).toBeVisible();
  expect(focused()).toBe(document.getElementById("check-heading"));
  expect(focused()).toHaveTextContent("Test, health, and repair");
  // Escape closes it and gives focus back to More. No anchors at all.
  await user.keyboard("{Escape}");
  expect(screen.getByTestId("tool-sheet")).not.toBeVisible();
  expect(focused()).toBe(screen.getByTestId("toolbar-more"));
  expect(document.querySelectorAll("a[href]")).toHaveLength(0);
});

test("results and progress have live regions that exist before they are used", async () => {
  backend();
  const user = userEvent.setup();
  render(<App />);
  const regions = ["write-status", "check-status", "report-exported", "crash-status"];
  // Errors are alerts, which assistive technology announces at once.
  expect(screen.queryAllByRole("alert")).toHaveLength(0);
  // The extract section exists once an archive is open.
  expect(screen.queryByTestId("extract-status")).toBeNull();
  await openArchive(user);
  for (const id of [...regions, "extract-status"]) {
    const region = screen.getByTestId(id);
    expect(region, id).toHaveAttribute("role", "status");
    expect(region, id).toHaveAttribute("aria-live", "polite");
    expect(region, id).toBeEmptyDOMElement();
  }
});


type Internals = { runCallback: (id: number, data: unknown) => void };
const tauri = () => (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

/**
 * A backend whose `start_job` hands the test the channel, to drive the job by
 * hand. The jobs here are the repair plan of the Test and repair tool.
 */
function jobBackend() {
  let channel: { id: number } | undefined;
  let index = 0;
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    if (cmd === "staged_sources") return { items: [], destination: null, default_destination: null, format: "mochi" };
    if (cmd === "watch_drops" || cmd === "watch_launches") return null;
    if (cmd === "recent_archives") return [];
    if (cmd === "get_crash_settings") return { enabled: false, include_details: false, entries: 0, bytes: 0 };
    if (cmd === "choose_check_subject") return { id: 5, file_name: "chosen.mochi", encrypted: false, unlocked: false };
    if (cmd === "start_job") {
      channel = a.onEvent as { id: number };
      return 1;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return (message: unknown) => {
    act(() => tauri().runCallback(channel?.id ?? 0, { index: index++, message }));
  };
}

/** Open Test and repair from the More menu and choose an archive, as a person would. */
async function openCheck(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByTestId("toolbar-more"));
  await user.click(screen.getByTestId("menu-repair"));
  await user.click(screen.getByTestId("check-choose"));
  await screen.findByText("chosen.mochi");
}

test("a job's progress bar has a name and a spoken value", async () => {
  const send = jobBackend();
  const user = userEvent.setup();
  render(<App />);
  await openCheck(user);
  await user.click(screen.getByTestId("repair-plan"));
  send({ event: "started", job_id: 1 });
  send({ event: "progress", job_id: 1, phase: "repair-check", completed: 3, total: 50 });
  const bar = await screen.findByRole("progressbar");
  expect(bar).toHaveAccessibleName("Progress");
  expect(bar).toHaveAttribute("aria-valuetext", "Reading file versions: 3 of 50");
});

/** Put focus on the page body, as a browser does when the focused control becomes disabled. */
function loseFocus() {
  const heading = document.getElementById("check-heading");
  heading?.focus(); // moves focus off the (disabled) button, which jsdom will not blur directly
  heading?.blur();
  expect(document.activeElement).toBe(document.body);
}

/** Record which elements are given focus by code. */
function spyOnFocus() {
  const given: unknown[] = [];
  const original = HTMLElement.prototype.focus;
  HTMLElement.prototype.focus = function (this: HTMLElement, ...args: Parameters<typeof original>) {
    given.push(this);
    return original.apply(this, args);
  };
  return { given, stop: () => (HTMLElement.prototype.focus = original) };
}

test("when a job ends, focus goes back to the button that started it", async () => {
  const finish = jobBackend();
  const user = userEvent.setup();
  render(<App />);
  await openCheck(user);
  const start = screen.getByTestId("repair-plan");
  start.focus();
  const spy = spyOnFocus();
  try {
    await user.keyboard("{Enter}");
    finish({ event: "started", job_id: 1 });
    expect(start).toBeDisabled();
    loseFocus(); // a browser drops focus from a control that becomes disabled; jsdom does not
    finish({ event: "finished", job_id: 1, outcome: { status: "cancelled" } });
    await waitFor(() => expect(screen.getByTestId("repair-plan")).toBeEnabled());
    await waitFor(() => expect(spy.given).toContain(start));
    expect(focused()).toBe(start);
  } finally {
    spy.stop();
  }
});

test("…but focus the user moved elsewhere in the meantime is left alone", async () => {
  const finish = jobBackend();
  const user = userEvent.setup();
  render(<App />);
  await openCheck(user);
  const start = screen.getByTestId("repair-plan");
  start.focus();
  await user.keyboard("{Enter}");
  finish({ event: "started", job_id: 1 });
  loseFocus();
  const link = screen.getByTestId("tool-sheet-close");
  link.focus();
  const spy = spyOnFocus();
  try {
    finish({ event: "finished", job_id: 1, outcome: { status: "cancelled" } });
    await waitFor(() => expect(screen.getByTestId("repair-plan")).toBeEnabled());
    expect(spy.given).not.toContain(start);
    expect(focused()).toBe(link);
  } finally {
    spy.stop();
  }
});
