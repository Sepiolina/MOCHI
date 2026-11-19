// Plan D6, owner decision 2026-10-16: the compact window of the shell's quick verbs. It hears
// events from Rust, asks for a passphrase once, and never names a path (spec §23.3 wording:
// a new archive is a local commit, never "backed up" or "safe").
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, beforeEach, expect, test } from "vitest";

import { resetQuickForTests, type QuickEvent } from "../ipc/quick";
import { QuickWindow } from "./QuickWindow";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () =>
  (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

function backend(answer: (cmd: string, args: Record<string, unknown>) => unknown = () => null) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let channel: Channel<QuickEvent> | undefined;
  let index = 0;
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    if (cmd === "watch_quick") {
      channel = (args as { onEvent: Channel<QuickEvent> }).onEvent;
      return null;
    }
    return answer(cmd, a);
  });
  return {
    calls,
    send: (message: QuickEvent) => {
      if (!channel) throw new Error("the window never asked to hear the job");
      const id = channel.id;
      act(() => {
        internals().runCallback(id, { index: index++, message });
      });
    },
  };
}

const name = (text: string) => ({ text, verbatim: true });

beforeEach(() => resetQuickForTests());
afterEach(() => clearMocks());

test("an extraction shows its progress, then a plain result with Show in folder and Close", async () => {
  const b = backend();
  render(<QuickWindow />);
  b.send({ event: "started", action: "extract_here", subject: name("photos.mochi"), items: 1, open_folder: true });
  expect(await screen.findByTestId("quick-title")).toHaveTextContent("Extracting photos.mochi");
  b.send({ event: "progress", phase: "restore", completed: 3, total: 10 });
  expect(await screen.findByTestId("quick-progress-text")).toHaveTextContent("3");
  b.send({
    event: "done",
    action: "extract_here",
    name: name("Pictures"),
    location: name("/home/me/Pictures"),
    files: 3,
    directories: 1,
    bytes: 12,
    skipped: 0,
    note: null,
    status: null,
    durability_note: null,
    format: "mochi",
    output_bytes: 0,
    opened_folder: false,
    movable: false,
  });
  const result = await screen.findByTestId("quick-result");
  expect(result).toHaveTextContent(/^Extracted 3 files · 1 folder · 12 bytes into Pictures$/);
  // The rest is behind a small Details disclosure, closed by default.
  const toggle = screen.getByRole("button", { name: "Details" });
  expect(toggle).toHaveAttribute("aria-expanded", "false");
  expect(screen.getByTestId("quick-details")).not.toBeVisible();
  await userEvent.click(toggle);
  expect(toggle).toHaveAttribute("aria-expanded", "true");
  expect(screen.getByTestId("quick-details")).toBeVisible();
  expect(screen.getByTestId("quick-details")).toHaveTextContent("/home/me/Pictures");
  expect(screen.getByTestId("quick-details")).toHaveTextContent("Extracting is not a verification");
  expect(result).toHaveAttribute("data-state", "finished");
  await userEvent.click(screen.getByTestId("quick-show-folder"));
  await userEvent.click(screen.getByTestId("quick-close"));
  const cmds = b.calls.map((c) => c.cmd);
  expect(cmds).toEqual(["watch_quick", "quick_show_folder", "quick_close"]);
  // Neither call carries a path.
  expect(b.calls.filter((c) => c.cmd !== "watch_quick").map((c) => c.args)).toEqual([{}, {}]);
});

test("a new archive is reported as a local commit and never as backed up or safe", async () => {
  const b = backend();
  render(<QuickWindow />);
  b.send({ event: "started", action: "add", subject: name("notes.txt"), items: 3, open_folder: true });
  expect(await screen.findByTestId("quick-title")).toHaveTextContent("Adding notes.txt and 2 more to a new archive");
  b.send({
    event: "done",
    action: "add",
    name: name("notes.mochi"),
    location: name("/home/me/notes.mochi"),
    files: 3,
    directories: 0,
    bytes: 1536,
    skipped: 1,
    note: "x: not stored",
    status: "LOCAL_COMMITTED",
    durability_note: null,
    format: "mochi",
    output_bytes: 0,
    opened_folder: false,
    movable: false,
  });
  const result = await screen.findByTestId("quick-result");
  expect(result).toHaveTextContent("Archive created: notes.mochi · 3 files · 1.5 KiB · 1 not added");
  expect(result).not.toHaveTextContent("LOCAL_COMMITTED");
  // Something was not stored: the line is a warning and the Details button carries a badge.
  expect(result.querySelector("p")).toHaveClass("warning");
  const toggle = screen.getByRole("button", { name: "Details, 1 item needs attention" });
  await userEvent.click(toggle);
  const details = screen.getByTestId("quick-details");
  expect(details).toHaveTextContent("LOCAL_COMMITTED");
  expect(details).toHaveTextContent("First reason: x: not stored");
  const text = (document.body.textContent ?? "");
  for (const word of ["backed up", "safe", "preserved", "verified"]) {
    expect(text.toLowerCase()).not.toContain(word);
  }
});

test("an encrypted archive asks for its passphrase once, empties the field, and asks again if it was wrong", async () => {
  const b = backend();
  render(<QuickWindow />);
  b.send({ event: "started", action: "extract_to_folder", subject: name("secret.mochi"), items: 1, open_folder: true });
  b.send({ event: "need_passphrase", wrong: false });
  const input = await screen.findByTestId("quick-passphrase-input");
  expect(input).toHaveAttribute("type", "password");
  await userEvent.type(input, "hunter2{Enter}");
  const sent = b.calls.filter((c) => c.cmd === "quick_passphrase");
  expect(sent.map((c) => c.args)).toEqual([{ passphrase: "hunter2" }]);
  expect(await screen.findByTestId("quick-progress")).toBeInTheDocument();
  b.send({ event: "need_passphrase", wrong: true });
  expect(await screen.findByTestId("quick-passphrase-note")).toHaveTextContent("did not open secret.mochi");
  expect(screen.getByTestId("quick-passphrase-input")).toHaveValue("");
});

test("a collision explains that nothing was written and where the plan is", async () => {
  const b = backend();
  render(<QuickWindow />);
  b.send({ event: "started", action: "extract_here", subject: name("a.mochi"), items: 1, open_folder: true });
  b.send({ event: "fallback", conflicts: 2, destination: name("/home/me") });
  const result = await screen.findByTestId("quick-result");
  expect(result).toHaveTextContent("Nothing was written: 2 entries");
  expect(result).toHaveTextContent("main window");
  expect(screen.queryByTestId("quick-show-folder")).toBeNull();
});

test("a failure is text with its code, hostile markup inert, and cancel is sent to Rust", async () => {
  const b = backend();
  render(<QuickWindow />);
  b.send({ event: "started", action: "extract_here", subject: name("<img src=x onerror=alert(1)>.mochi"), items: 1, open_folder: true });
  expect(await screen.findByTestId("quick-title")).toHaveTextContent("<img src=x onerror=alert(1)>.mochi");
  expect(document.querySelector("img")).toBeNull();
  await userEvent.click(await screen.findByTestId("quick-cancel"));
  expect(b.calls.map((c) => c.cmd)).toContain("quick_cancel");
  b.send({ event: "failed", code: "IO_ERROR", message: "<b>no space</b>" });
  const result = await screen.findByTestId("quick-result");
  expect(result).toHaveAttribute("data-state", "failed");
  expect(result).toHaveTextContent("Failed (IO_ERROR): <b>no space</b>");
  expect(result.querySelector("b")).toBeNull();
});

test("a window that mounts after the job began still hears all of it", async () => {
  const b = backend();
  const first = render(<QuickWindow />);
  b.send({ event: "started", action: "add", subject: name("x"), items: 1, open_folder: true });
  b.send({ event: "cancelled", action: "add" });
  first.unmount();
  render(<QuickWindow />);
  expect(await screen.findByTestId("quick-result")).toHaveTextContent("Cancelled. No archive was created.");
});

test("a degraded directory entry is a note in the details, not in the line, and gets no success styling", async () => {
  const b = backend();
  const { container } = render(<QuickWindow />);
  b.send({ event: "started", action: "add", subject: name("a.txt"), items: 1, open_folder: true });
  b.send({
    event: "done",
    action: "add",
    name: name("a.mochi"),
    location: name("/home/me/a.mochi"),
    files: 1,
    directories: 0,
    bytes: 10,
    skipped: 0,
    note: null,
    status: "LOCAL_COMMITTED",
    durability_note: "directory flush ran, but Windows does not document it as durable",
    format: "mochi",
    output_bytes: 0,
    opened_folder: false,
    movable: false,
  });
  const result = await screen.findByTestId("quick-result");
  expect(result).toHaveTextContent("Archive created: a.mochi · 1 file · 10 bytes");
  expect(result).not.toHaveTextContent(/degraded|durable/i);
  expect(result.querySelector("p")).not.toHaveClass("warning");
  expect(screen.getByTestId("quick-attention")).toHaveClass("sr-only");
  expect(result).toContainElement(screen.getByTestId("quick-attention"));
  expect(screen.getByTestId("quick-attention")).toHaveTextContent("1 item needs attention, see Details");
  await userEvent.click(screen.getByRole("button", { name: "Details, 1 item needs attention" }));
  expect(screen.getByTestId("quick-details")).toHaveTextContent("Not confirmed");
  expect(screen.getByTestId("quick-details")).toHaveTextContent("Windows does not document it as durable");
  expect(container.querySelector(".success, .pass, [data-status='PASS']")).toBeNull();
});

// ---- the Add form (owner decision 2026-10-20) ------------------------------

const prompt = (items = 1, subject = "notes.txt"): QuickEvent => ({
  event: "add_prompt",
  items,
  subject: name(subject),
  name: "notes.mochi",
  folder: name("/home/me/docs"),
  format: "mochi",
});

test("Add to archive... opens a form with the suggested name, the folder as text, and the .mochi defaults", async () => {
  const b = backend();
  render(<QuickWindow />);
  b.send(prompt(3));
  expect(await screen.findByTestId("quick-title")).toHaveTextContent("Add notes.txt and 2 more to an archive");
  expect(screen.getByTestId("quick-name")).toHaveValue("notes.mochi");
  expect(screen.getByTestId("quick-folder")).toHaveTextContent("In folder: /home/me/docs");
  expect(screen.getByTestId("quick-format")).toHaveValue("mochi");
  expect(screen.getByTestId("quick-level")).toHaveValue("balanced");
  expect(screen.getByTestId("quick-mochi-options")).toBeInTheDocument();
  expect(screen.getByTestId("quick-encrypt")).not.toBeChecked();
  expect(screen.getByTestId("quick-recovery")).toHaveValue("");
  // Five formats, .mochi first.
  const options = within(screen.getByTestId("quick-format")).getAllByRole("option");
  expect(options.map((o) => (o as HTMLOptionElement).value)).toEqual(["mochi", "zip", "tar", "tar_gz", "tar_zst"]);
  // The name is the thing to type: focus is there, with the extension left out of the selection.
  expect(screen.getByTestId("quick-name")).toHaveFocus();
  // Nothing was asked of Rust but the stream.
  expect(b.calls.map((c) => c.cmd)).toEqual(["watch_quick"]);
});

test("choosing another format changes the extension of the name and hides the .mochi-only options", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<QuickWindow />);
  b.send(prompt());
  await screen.findByTestId("quick-form");
  await user.selectOptions(screen.getByTestId("quick-format"), "zip");
  expect(screen.getByTestId("quick-name")).toHaveValue("notes.zip");
  expect(screen.queryByTestId("quick-mochi-options")).toBeNull();
  expect(screen.queryByTestId("quick-encrypt")).toBeNull();
  expect(screen.queryByTestId("quick-recovery")).toBeNull();
  expect(screen.getByTestId("quick-plain-note")).toBeInTheDocument();
  await user.selectOptions(screen.getByTestId("quick-format"), "tar_gz");
  expect(screen.getByTestId("quick-name")).toHaveValue("notes.tar.gz");
  expect(screen.getByTestId("quick-level")).toBeEnabled();
  await user.selectOptions(screen.getByTestId("quick-format"), "tar");
  expect(screen.getByTestId("quick-name")).toHaveValue("notes.tar");
  expect(screen.getByTestId("quick-level")).toBeDisabled();
  expect(screen.getByTestId("quick-no-level")).toBeInTheDocument();
  await user.selectOptions(screen.getByTestId("quick-format"), "tar_zst");
  expect(screen.getByTestId("quick-name")).toHaveValue("notes.tar.zst");
  await user.selectOptions(screen.getByTestId("quick-format"), "mochi");
  expect(screen.getByTestId("quick-name")).toHaveValue("notes.mochi");
  expect(screen.getByTestId("quick-mochi-options")).toBeInTheDocument();
  // A name the user typed keeps its stem.
  await user.clear(screen.getByTestId("quick-name"));
  await user.type(screen.getByTestId("quick-name"), "Holiday 2026");
  await user.selectOptions(screen.getByTestId("quick-format"), "zip");
  expect(screen.getByTestId("quick-name")).toHaveValue("Holiday 2026.zip");
});

test("leaving .mochi drops its password and repair data: they are not carried to a ZIP", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<QuickWindow />);
  b.send(prompt());
  await screen.findByTestId("quick-form");
  await user.click(screen.getByTestId("quick-encrypt"));
  await user.type(screen.getByTestId("quick-new-passphrase"), "hunter2");
  await user.type(screen.getByTestId("quick-new-passphrase-again"), "hunter2");
  await user.selectOptions(screen.getByTestId("quick-recovery"), "10");
  await user.selectOptions(screen.getByTestId("quick-format"), "zip");
  await user.click(screen.getByTestId("quick-ok"));
  const sent = b.calls.find((c) => c.cmd === "quick_add_submit");
  expect(sent?.args).toEqual({ request: { name: "notes.zip", format: "zip", level: "balanced" } });
});

test("the level list follows the format: three for ZIP, seven for tar.zst", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<QuickWindow />);
  b.send(prompt());
  await screen.findByTestId("quick-form");
  const level = screen.getByTestId("quick-level");
  const options = () => [...level.querySelectorAll("option")].map((o) => o.getAttribute("value"));
  await user.selectOptions(screen.getByTestId("quick-format"), "tar_zst");
  expect(options()).toHaveLength(7);
  await user.selectOptions(level, "extreme");
  await user.selectOptions(screen.getByTestId("quick-format"), "zip");
  expect(options()).toEqual(["fast", "balanced", "maximum"]);
  expect(level).toHaveValue("maximum");
});

test("OK sends the name, the format and the level, and no path", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<QuickWindow />);
  b.send(prompt());
  await screen.findByTestId("quick-form");
  await user.selectOptions(screen.getByTestId("quick-format"), "tar_zst");
  await user.selectOptions(screen.getByTestId("quick-level"), "ultra");
  await user.click(screen.getByTestId("quick-ok"));
  expect(b.calls.filter((c) => c.cmd === "quick_add_submit").map((c) => c.args)).toEqual([
    { request: { name: "notes.tar.zst", format: "tar_zst", level: "ultra" } },
  ]);
  // The job's events take over the window: the form is replaced by its progress.
  b.send({ event: "started", action: "add", subject: name("notes.txt"), items: 1, open_folder: true });
  expect(await screen.findByTestId("quick-progress")).toBeInTheDocument();
  expect(screen.queryByTestId("quick-form")).toBeNull();
});

test("a .mochi archive with a password and a recovery record sends both once, and empties the fields", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<QuickWindow />);
  b.send(prompt());
  await screen.findByTestId("quick-form");
  await user.click(screen.getByTestId("quick-encrypt"));
  // The two must match.
  await user.type(screen.getByTestId("quick-new-passphrase"), "correct horse");
  await user.type(screen.getByTestId("quick-new-passphrase-again"), "correct hose");
  await user.click(screen.getByTestId("quick-ok"));
  expect(await screen.findByTestId("quick-form-error")).toHaveTextContent("The two passphrases differ");
  expect(b.calls.some((c) => c.cmd === "quick_add_submit")).toBe(false);
  await user.clear(screen.getByTestId("quick-new-passphrase-again"));
  await user.type(screen.getByTestId("quick-new-passphrase-again"), "correct horse");
  await user.selectOptions(screen.getByTestId("quick-recovery"), "20");
  await user.click(screen.getByTestId("quick-ok"));
  expect(b.calls.filter((c) => c.cmd === "quick_add_submit").map((c) => c.args)).toEqual([
    {
      request: {
        name: "notes.mochi",
        format: "mochi",
        level: "balanced",
        passphrase: "correct horse",
        recovery_percent: 20,
      },
    },
  ]);
  expect(screen.getByTestId("quick-new-passphrase")).toHaveValue("");
  expect(screen.getByTestId("quick-new-passphrase-again")).toHaveValue("");
});

test("an empty passphrase is refused before anything is sent", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<QuickWindow />);
  b.send(prompt());
  await screen.findByTestId("quick-form");
  await user.click(screen.getByTestId("quick-encrypt"));
  await user.click(screen.getByTestId("quick-ok"));
  expect(await screen.findByTestId("quick-form-error")).toHaveTextContent("Enter a passphrase");
  expect(b.calls.some((c) => c.cmd === "quick_add_submit")).toBe(false);
});

test("Browse asks Rust for a place and shows only the folder text and the file name", async () => {
  const user = userEvent.setup();
  const b = backend((cmd) =>
    cmd === "quick_choose_location" ? { folder: name("/home/me/Elsewhere"), name: "Chosen.zip" } : null,
  );
  render(<QuickWindow />);
  b.send(prompt());
  await screen.findByTestId("quick-form");
  await user.selectOptions(screen.getByTestId("quick-format"), "zip");
  await user.click(screen.getByTestId("quick-browse"));
  expect(await screen.findByDisplayValue("Chosen.zip")).toBeInTheDocument();
  expect(screen.getByTestId("quick-folder")).toHaveTextContent("In folder: /home/me/Elsewhere");
  expect(b.calls.filter((c) => c.cmd === "quick_choose_location").map((c) => c.args)).toMatchObject([
    { name: "notes.zip", format: "zip" },
  ]);
});

test("cancelling the dialog from Browse changes nothing", async () => {
  const user = userEvent.setup();
  const b = backend(() => null);
  render(<QuickWindow />);
  b.send(prompt());
  await screen.findByTestId("quick-form");
  await user.click(screen.getByTestId("quick-browse"));
  await waitFor(() => expect(b.calls.some((c) => c.cmd === "quick_choose_location")).toBe(true));
  expect(screen.getByTestId("quick-name")).toHaveValue("notes.mochi");
  expect(screen.getByTestId("quick-folder")).toHaveTextContent("/home/me/docs");
});

test("a name Rust refuses is shown and the form stays open for another", async () => {
  const user = userEvent.setup();
  const refusal = {
    code: "DESTINATION_EXISTS",
    message: "notes.mochi already exists in that folder. Choose another name; MOCHI does not replace files.",
  };
  let refuse = true;
  const b = backend((cmd) => {
    if (cmd === "quick_add_submit" && refuse) {
      refuse = false;
      return Promise.reject(refusal);
    }
    return null;
  });
  render(<QuickWindow />);
  b.send(prompt());
  await screen.findByTestId("quick-form");
  await user.click(screen.getByTestId("quick-ok"));
  expect(await screen.findByTestId("quick-form-error")).toHaveTextContent("does not replace files");
  expect(screen.getByTestId("quick-form")).toBeInTheDocument();
  expect(screen.getByTestId("quick-ok")).toBeEnabled();
  await user.clear(screen.getByTestId("quick-name"));
  await user.type(screen.getByTestId("quick-name"), "other");
  await user.click(screen.getByTestId("quick-ok"));
  await waitFor(() => expect(b.calls.filter((c) => c.cmd === "quick_add_submit")).toHaveLength(2));
  expect(b.calls.filter((c) => c.cmd === "quick_add_submit").at(-1)?.args).toEqual({
    request: { name: "other", format: "mochi", level: "balanced" },
  });
});

test("another item joining the form changes the count, not what the user typed", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<QuickWindow />);
  b.send(prompt(1));
  await screen.findByTestId("quick-form");
  await user.clear(screen.getByTestId("quick-name"));
  await user.type(screen.getByTestId("quick-name"), "mine.mochi");
  await user.selectOptions(screen.getByTestId("quick-format"), "zip");
  b.send({ ...prompt(2), name: "something-else.mochi" } as QuickEvent);
  await waitFor(() => expect(screen.getByTestId("quick-title")).toHaveTextContent("Add notes.txt and 1 more to an archive"));
  expect(screen.getByTestId("quick-name")).toHaveValue("mine.zip");
  expect(screen.getByTestId("quick-format")).toHaveValue("zip");
});

test("Cancel closes the window and starts nothing", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<QuickWindow />);
  b.send(prompt());
  await screen.findByTestId("quick-form");
  await user.click(screen.getByTestId("quick-form-cancel"));
  expect(b.calls.map((c) => c.cmd)).toEqual(["watch_quick", "quick_close"]);
});

test("a hostile file name in the title and the folder is text, never markup", async () => {
  const b = backend();
  const { container } = render(<QuickWindow />);
  const hostile = '<img src=x onerror="alert(1)">';
  b.send({ ...prompt(), subject: name(hostile), folder: name(`/tmp/${hostile}`) } as QuickEvent);
  await screen.findByTestId("quick-form");
  expect(screen.getByTestId("quick-title")).toHaveTextContent(hostile);
  expect(screen.getByTestId("quick-folder")).toHaveTextContent(hostile);
  expect(container.querySelector("img")).toBeNull();
});

test("a window that opens after the form was sent still shows it, once", async () => {
  const b = backend();
  const first = render(<QuickWindow />);
  b.send(prompt(2));
  b.send(prompt(3));
  first.unmount();
  render(<QuickWindow />);
  expect(await screen.findByTestId("quick-title")).toHaveTextContent("and 2 more");
  expect(screen.getAllByTestId("quick-form")).toHaveLength(1);
});

test("a plain ZIP or tar file is reported as what was written: no commit, no status, no claim", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<QuickWindow />);
  b.send({ event: "started", action: "add", subject: name("notes.txt"), items: 2, open_folder: true });
  b.send({
    event: "done",
    action: "add",
    name: name("trip.zip"),
    location: name("/home/me/trip.zip"),
    files: 12,
    directories: 3,
    bytes: 3_400_000,
    skipped: 1,
    note: "link: a symbolic link is not stored; it was not followed",
    status: null,
    durability_note: null,
    format: "zip",
    output_bytes: 1_200_000,
    opened_folder: false,
    movable: false,
  });
  const result = await screen.findByTestId("quick-result");
  expect(result).toHaveTextContent("Wrote 12 files · 3 folders · 3.2 MiB to trip.zip · 1 not added");
  await user.click(screen.getByRole("button", { name: /^Details/ }));
  const details = screen.getByTestId("quick-details");
  expect(details).toHaveTextContent("ZIP (.zip)");
  expect(details).toHaveTextContent("/home/me/trip.zip");
  expect(details).toHaveTextContent("File size");
  expect(details).toHaveTextContent("did not check the file afterwards");
  const text = (result.textContent + (details.textContent ?? "")).toLowerCase();
  for (const word of ["local_committed", "committed", "backed up", "safe", "preserved", "healthy", "verified"]) {
    expect(text, word).not.toContain(word);
  }
  expect(screen.getByTestId("quick-show-folder")).toBeInTheDocument();
});

test("a cancelled export says no archive was created", async () => {
  const b = backend();
  render(<QuickWindow />);
  b.send({ event: "started", action: "add", subject: name("big.bin"), items: 1, open_folder: true });
  b.send({ event: "progress", phase: "export", completed: 1024, total: 4096 });
  expect(await screen.findByTestId("quick-progress-text")).toHaveTextContent("1.0 KiB of 4.0 KiB");
  b.send({ event: "cancelled", action: "add" });
  expect(await screen.findByTestId("quick-result")).toHaveTextContent("Cancelled. No archive was created.");
});

test("the compact window has the same title bar, with Minimize and Close only; Close is the window's own close", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<QuickWindow />);
  const bar = screen.getByTestId("titlebar");
  expect(within(bar).getByTestId("window-title")).toHaveTextContent(/^MOCHI$/);
  const buttons = within(within(bar).getByRole("group", { name: "Window" })).getAllByRole("button");
  expect(buttons.map((x) => x.getAttribute("aria-label"))).toEqual(["Minimize", "Close"]);
  // Its only heading is the job's; the bar's title is plain text.
  expect(within(bar).queryByRole("heading")).toBeNull();
  await user.click(within(bar).getByRole("button", { name: "Close" }));
  // The same request as the system's close button (Rust stops the job when the window goes).
  expect(b.calls.map((c) => c.cmd)).toContain("window_close");
  // The compact window is granted no maximize and no watch of the window.
  expect(b.calls.map((c) => c.cmd)).not.toContain("window_watch");
});

test("Pause in the compact window sends quick_pause, shows Paused, and Resume sends false", async () => {
  const b = backend((cmd) => (cmd === "quick_pause" ? true : null));
  render(<QuickWindow />);
  b.send({ event: "started", action: "extract_here", subject: name("photos.mochi"), items: 1, open_folder: true });
  b.send({ event: "progress", phase: "restore", completed: 3, total: 10 });
  const card = await screen.findByTestId("quick-progress");
  expect(card).toHaveAttribute("data-state", "running");
  expect(card).toHaveTextContent("30% complete");

  await userEvent.click(screen.getByTestId("quick-pause"));
  expect(b.calls.at(-1)).toEqual({ cmd: "quick_pause", args: { paused: true } });
  expect(card).toHaveAttribute("data-state", "paused");
  expect(card).toHaveTextContent("Paused");
  expect(screen.getByTestId("quick-pause")).toHaveAccessibleName("Resume");
  // The window's own Cancel still works while paused.
  expect(screen.getByTestId("quick-cancel")).toBeEnabled();

  await userEvent.click(screen.getByTestId("quick-pause"));
  expect(b.calls.at(-1)).toEqual({ cmd: "quick_pause", args: { paused: false } });
  expect(card).toHaveAttribute("data-state", "running");
});

// Owner request 2026-11-09: after a quick extraction the folder opens in the file manager unless
// the user turned that off, and the folder "Extract to new folder" made can be renamed or moved.
const extracted = (over: Partial<Extract<QuickEvent, { event: "done" }>> = {}): QuickEvent => ({
  event: "done",
  action: "extract_to_folder",
  name: name("docs"),
  location: name("/home/me/Desktop/docs"),
  files: 2,
  directories: 0,
  bytes: 10,
  skipped: 0,
  note: null,
  status: null,
  durability_note: null,
  format: "mochi",
  output_bytes: 0,
  opened_folder: true,
  movable: true,
  ...over,
});

test("'Open the folder when done' shows the saved choice, is sent to Rust when changed, and the result says the folder opened", async () => {
  const b = backend((cmd, args) => (cmd === "quick_set_open_folder" ? args.open : null));
  render(<QuickWindow />);
  b.send({ event: "started", action: "extract_to_folder", subject: name("docs.mochi"), items: 1, open_folder: true });
  const box = await screen.findByTestId("quick-open-when-done");
  expect(box).toBeChecked();
  expect(screen.getByTestId("quick-open-when-done-label")).toHaveTextContent("Open the folder when done");
  await userEvent.click(box);
  expect(box).not.toBeChecked();
  expect(b.calls.filter((c) => c.cmd === "quick_set_open_folder").map((c) => c.args)).toEqual([{ open: false }]);
  b.send(extracted({ opened_folder: false }));
  await screen.findByTestId("quick-result");
  expect(screen.queryByTestId("quick-opened")).toBeNull();
  // Still there after the job, to turn it back on for the next one.
  expect(screen.getByTestId("quick-open-when-done")).not.toBeChecked();

  b.send({ event: "started", action: "extract_here", subject: name("a.mochi"), items: 1, open_folder: true });
  b.send(extracted({ action: "extract_here", movable: false }));
  expect(await screen.findByTestId("quick-opened")).toHaveTextContent("opened in the file manager");
  // Extract here wrote into the archive's own folder: nothing to rename or move.
  expect(screen.queryByTestId("quick-rename")).toBeNull();
});

test("an add never shows the open-folder box or the rename controls", async () => {
  const b = backend();
  render(<QuickWindow />);
  b.send({ event: "started", action: "add", subject: name("n.txt"), items: 1, open_folder: true });
  expect(await screen.findByTestId("quick-progress")).toBeInTheDocument();
  expect(screen.queryByTestId("quick-open-when-done")).toBeNull();
  b.send(extracted({ action: "add", name: name("n.mochi"), status: "LOCAL_COMMITTED", movable: false, opened_folder: false }));
  await screen.findByTestId("quick-result");
  expect(screen.queryByTestId("quick-open-when-done")).toBeNull();
  expect(screen.queryByTestId("quick-rename")).toBeNull();
});

test("the new folder is renamed by name only, a refusal is shown, and Move to… lets Rust pick the place", async () => {
  let renames = 0;
  const b = backend((cmd, args) => {
    if (cmd === "quick_rename_folder") {
      renames += 1;
      if (renames === 1) throw { code: "DESTINATION_EXISTS", message: "taken already exists" };
      const n = String(args.name);
      return { name: name(n), location: name(`/home/me/Desktop/${n}`) };
    }
    if (cmd === "quick_move_folder") return { name: name("Report"), location: name("/home/me/Documents/Report") };
    return null;
  });
  render(<QuickWindow />);
  b.send({ event: "started", action: "extract_to_folder", subject: name("docs.mochi"), items: 1, open_folder: true });
  b.send(extracted());
  const field = await screen.findByTestId("quick-folder-name");
  expect(field).toHaveValue("docs");
  await userEvent.clear(field);
  await userEvent.type(field, "taken{Enter}");
  expect(await screen.findByTestId("quick-problem")).toHaveTextContent("DESTINATION_EXISTS");
  expect(screen.queryByTestId("quick-moved")).toBeNull();
  await userEvent.clear(field);
  expect(screen.getByTestId("quick-rename-submit")).toBeDisabled();
  await userEvent.type(field, "Report");
  await userEvent.click(screen.getByTestId("quick-rename-submit"));
  expect(await screen.findByTestId("quick-moved")).toHaveTextContent("Renamed. The folder is now /home/me/Desktop/Report");
  expect(screen.queryByTestId("quick-problem")).toBeNull();
  await userEvent.click(screen.getByTestId("quick-move"));
  await waitFor(() =>
    expect(screen.getByTestId("quick-moved")).toHaveTextContent("Moved. The folder is now /home/me/Documents/Report"),
  );
  const sent = b.calls.filter((c) => c.cmd === "quick_rename_folder" || c.cmd === "quick_move_folder");
  // Only a folder name crosses for a rename; the move names no path at all (Rust shows the dialog).
  expect(sent.map((c) => [c.cmd, c.args.name])).toEqual([
    ["quick_rename_folder", "taken"],
    ["quick_rename_folder", "Report"],
    ["quick_move_folder", undefined],
  ]);
  expect(Object.keys(sent[2]?.args ?? {})).toEqual(["text"]);
});
