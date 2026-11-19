import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import { Browser } from "../browser/Browser";
import { Checker } from "../check/Checker";
import type { ArchiveSummary } from "../ipc/browse";
import type { EnvelopeView } from "../ipc/crypt";
import type { JobEvent, JobOutcome } from "../ipc/jobs";
import { Rekey } from "./Rekey";
import { Writer } from "../write/Writer";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () =>
  (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

const SECRET = "hunter2 correct horse 4471";
const WRONG = "not the right one 9981";

function summary(encrypted: boolean, handle = 3): ArchiveSummary {
  return {
    handle,
    file_name: "vault.mochi",
    archive_id: "ab".repeat(16),
    profile: { tar_compatible: false, encrypted, redundancy_percent: null },
    commits: [0, 1].map((seq) => ({
      seq,
      commit_id: String(seq).repeat(64),
      time: "2023-11-14T22:13:20Z",
      checkpoint: seq === 0,
      retained: true,
      expired: false,
      holds: [],
    })),
    retention_error: null,
  };
}

function envelope(id: string, mine: boolean): EnvelopeView {
  return { envelope_id: id.repeat(32).slice(0, 32), written_by_commit: 0, kdf: "argon2id m=64 MiB t=3 p=4", opened_with: mine };
}

type Call = { cmd: string; args: Record<string, unknown> };

/** A fake backend that records every call and lets the test push job events. */
function backend(handlers: Record<string, (a: Record<string, unknown>) => unknown> = {}) {
  const calls: Call[] = [];
  let jobChannel: Channel<JobEvent> | undefined;
  const next = new Map<number, number>();
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    if (handlers[cmd]) return handlers[cmd](a);
    switch (cmd) {
      case "staged_sources":
        return { items: [{ name: { text: "notes.txt", verbatim: true }, kind: "file" }], destination: null, default_destination: null, format: "mochi" };
      case "watch_drops":
        return null;
      case "recent_archives":
        return [];
      case "choose_destination":
        return { items: [{ name: { text: "notes.txt", verbatim: true }, kind: "file" }], destination: "new.mochi", default_destination: null, format: "mochi" };
      case "start_job":
        jobChannel = a.onEvent as Channel<JobEvent>;
        return 9;
      case "cancel_job":
        return true;
      case "close_archive":
        return true;
      case "open_snapshot":
        return { seq: a.seq, commit_id: "c".repeat(64), entries: 0, root_dirs: 0, root_files: 0, catalog_source: { source: "image" } };
      case "list_children":
        return { total: 0, offset: 0, entries: [] };
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  const job = (m: JobEvent) => {
    if (!jobChannel) throw new Error("no job channel yet");
    const id = jobChannel.id;
    const index = next.get(id) ?? 0;
    next.set(id, index + 1);
    act(() => internals().runCallback(id, { index, message: m }));
  };
  const finish = (outcome: JobOutcome) => job({ event: "finished", job_id: 9, outcome });
  return {
    calls,
    job,
    finish,
    request: () => calls.filter((c) => c.cmd === "start_job").at(-1)?.args.request as Record<string, unknown>,
    cmds: () => calls.map((c) => c.cmd),
    /** Every argument ever sent over IPC, as one string. */
    wire: () => JSON.stringify(calls.map((c) => ({ ...c.args, onEvent: undefined, onDrop: undefined }))),
  };
}

afterEach(() => {
  clearMocks();
});

// ---- the prompt when opening an encrypted archive -------------------------------------------

test("an encrypted archive asks for its passphrase, warns about loss, and a wrong one is not damage", async () => {
  const user = userEvent.setup();
  let attempts = 0;
  const b = backend({
    open_archive: () => ({ status: "needs_passphrase", pending: 7, file_name: "vault.mochi" }),
    unlock_archive: (a) => {
      attempts += 1;
      if (a.passphrase !== SECRET) {
        throw { code: "KEY_UNAVAILABLE", message: "the passphrase did not open this archive" };
      }
      return summary(true);
    },
  });
  const { container } = render(<Browser />);
  await user.click(screen.getByTestId("open-archive"));
  const panel = await screen.findByTestId("unlock-panel");
  expect(panel).toHaveTextContent("is encrypted");
  expect(within(panel).getByTestId("lost-passphrase")).toHaveTextContent("contents are lost");
  expect(b.cmds()).not.toContain("open_snapshot");

  const input = screen.getByTestId("unlock-input");
  expect(input).toHaveAttribute("type", "password");
  await user.type(input, WRONG);
  await user.click(screen.getByTestId("unlock-submit"));
  const err = await screen.findByTestId("unlock-error");
  expect(err).toHaveTextContent("The passphrase did not open this archive.");
  expect(err.textContent?.toLowerCase()).not.toMatch(/damage|corrupt/);
  // The field was emptied on submit; the text is nowhere on the page.
  expect(screen.getByTestId("unlock-input")).toHaveValue("");
  expect(container.textContent).not.toContain(WRONG);
  expect(container.innerHTML).not.toContain(WRONG);
  // The archive keeps waiting: nothing opened.
  expect(b.cmds()).not.toContain("open_snapshot");

  await user.type(screen.getByTestId("unlock-input"), SECRET);
  await user.click(screen.getByTestId("unlock-submit"));
  await screen.findByTestId("archive-meta");
  expect(screen.getByTestId("archive-meta")).toHaveTextContent("Encrypted");
  expect(screen.queryByTestId("unlock-panel")).toBeNull();
  expect(container.innerHTML).not.toContain(SECRET);
  expect(container.textContent).not.toContain(SECRET);
  // The passphrase crossed IPC exactly once per attempt, in unlock_archive
  // only (never in any other command's arguments).
  expect(attempts).toBe(2);
  const carrying = b.calls.filter((c) => JSON.stringify(c.args).includes(SECRET));
  expect(carrying.map((c) => c.cmd)).toEqual(["unlock_archive"]);
  const wrongCarrying = b.calls.filter((c) => JSON.stringify(c.args).includes(WRONG));
  expect(wrongCarrying.map((c) => c.cmd)).toEqual(["unlock_archive"]);
  // What stays visible without the key is explained.
  expect(screen.getByTestId("encrypted-note").getAttribute("title")).toContain("Compressed sizes");
});

test("cancelling the prompt tells Rust to forget the archive", async () => {
  const user = userEvent.setup();
  const b = backend({
    open_archive: () => ({ status: "needs_passphrase", pending: 7, file_name: "vault.mochi" }),
    cancel_unlock: () => true,
  });
  render(<Browser />);
  await user.click(screen.getByTestId("open-archive"));
  await user.click(await screen.findByTestId("unlock-cancel"));
  expect(screen.queryByTestId("unlock-panel")).toBeNull();
  expect(b.calls.find((c) => c.cmd === "cancel_unlock")?.args).toEqual({ pending: 7 });
});

test("an empty passphrase is refused on the page and never sent", async () => {
  const user = userEvent.setup();
  const b = backend({
    open_archive: () => ({ status: "needs_passphrase", pending: 7, file_name: "vault.mochi" }),
  });
  render(<Browser />);
  await user.click(screen.getByTestId("open-archive"));
  await user.click(await screen.findByTestId("unlock-submit"));
  expect(screen.getByTestId("unlock-error")).toHaveTextContent("Enter a passphrase");
  expect(b.cmds()).not.toContain("unlock_archive");
});

// ---- create: encryption and recovery record ---------------------------------------------------

function renderWriter() {
  return render(<Writer archive={null} viewingHead={false} selected={[]} onCommitted={() => undefined} />);
}

test("choosing encryption shows the loss, deduplication, and visibility warnings in plain text", async () => {
  const user = userEvent.setup();
  backend();
  renderWriter();
  await user.click(screen.getByTestId("new-archive"));
  expect(screen.queryByTestId("encrypt-panel")).toBeNull();
  await user.click(screen.getByTestId("encrypt"));
  const p = screen.getByTestId("encrypt-panel");
  expect(within(p).getByTestId("lost-passphrase")).toHaveTextContent("There is no recovery");
  expect(within(p).getByTestId("dedup-leak")).toHaveTextContent("stored once");
  expect(within(p).getByTestId("visible-without-key")).toHaveTextContent("key envelopes");
  // Encrypted and TAR-compatible exclude each other.
  expect(screen.getByTestId("tar-compatible")).toBeDisabled();
  await user.click(screen.getByTestId("encrypt"));
  await user.click(screen.getByTestId("tar-compatible"));
  expect(screen.getByTestId("encrypt")).toBeDisabled();
});

test("the passphrase crosses once in the create request and the page keeps no copy", async () => {
  const user = userEvent.setup();
  const b = backend();
  const { container } = renderWriter();
  await user.click(screen.getByTestId("new-archive"));
  await user.click(screen.getByTestId("encrypt"));
  await user.type(screen.getByTestId("create-passphrase"), SECRET);
  await user.type(screen.getByTestId("create-passphrase-again"), SECRET);
  await user.click(screen.getByTestId("recovery-10"));
  await user.click(screen.getByTestId("create-confirm"));
  await screen.findByTestId("write-running");
  expect(b.request()).toEqual({
    kind: "create_archive",
    preset: "balanced",
    tar_compatible: false,
    passphrase: SECRET,
    recovery_percent: 10,
  });
  expect(b.calls.filter((c) => JSON.stringify(c.args).includes(SECRET)).map((c) => c.cmd)).toEqual(["start_job"]);
  // The inputs were emptied once the request left.
  expect(screen.getByTestId("create-passphrase")).toHaveValue("");
  expect(container.innerHTML).not.toContain(SECRET);
});

test("a new archive does not inherit the encryption or recovery choice of the last one", async () => {
  const user = userEvent.setup();
  backend();
  renderWriter();
  await user.click(screen.getByTestId("new-archive"));
  await user.click(screen.getByTestId("encrypt"));
  await user.click(screen.getByTestId("recovery-20"));
  await user.click(screen.getByTestId("write-close"));
  await user.click(screen.getByTestId("new-archive"));
  expect(screen.getByTestId("encrypt")).not.toBeChecked();
  expect(screen.getByTestId("recovery-none")).toBeChecked();
});

test("two different passphrases start nothing, and an empty one is refused", async () => {
  const user = userEvent.setup();
  const b = backend();
  renderWriter();
  await user.click(screen.getByTestId("new-archive"));
  await user.click(screen.getByTestId("encrypt"));
  await user.click(screen.getByTestId("create-confirm"));
  expect(screen.getByTestId("write-error")).toHaveTextContent("Enter a passphrase");
  await user.type(screen.getByTestId("create-passphrase"), SECRET);
  await user.type(screen.getByTestId("create-passphrase-again"), WRONG);
  await user.click(screen.getByTestId("create-confirm"));
  expect(screen.getByTestId("write-error")).toHaveTextContent("The two passphrases differ");
  expect(b.cmds()).not.toContain("start_job");
  expect(b.cmds()).not.toContain("choose_destination");
});

test("a core create sends no passphrase and no recovery record unless asked", async () => {
  const user = userEvent.setup();
  const b = backend();
  renderWriter();
  await user.click(screen.getByTestId("new-archive"));
  await user.click(screen.getByTestId("create-confirm"));
  await screen.findByTestId("write-running");
  expect(b.request()).toEqual({ kind: "create_archive", preset: "balanced", tar_compatible: false });
});

test("the recovery record choices state the capacity honestly and never say safe or backed up", async () => {
  const user = userEvent.setup();
  backend();
  renderWriter();
  await user.click(screen.getByTestId("new-archive"));
  const notes = screen.getByTestId("recovery-notes");
  expect(notes).toHaveTextContent("not a second copy");
  expect(notes).toHaveTextContent("Damage beyond that is reported and left as it is");
  expect(notes).toHaveTextContent("newest commit's footer");
  expect(screen.getByTestId("recovery-none")).toBeChecked();
  for (const p of [5, 10, 20, 50]) expect(screen.getByTestId(`recovery-${p}`)).toBeInTheDocument();
  const text = (screen.getByTestId("write-panel").textContent ?? "").toLowerCase();
  for (const word of ["safe", "backed up", "secure", "preserved"]) expect(text).not.toContain(word);
});

// ---- rekey --------------------------------------------------------------------------------------

const ENVS = [envelope("a", true), envelope("b", false)];

test("the key envelopes are listed, and the one that opened the archive is marked", async () => {
  backend({ list_envelopes: () => ({ envelopes: ENVS }) });
  render(<Rekey archive={summary(true)} onChanged={() => undefined} />);
  const items = await screen.findAllByTestId("envelope");
  expect(items).toHaveLength(2);
  expect(items[0]).toHaveTextContent("opened this archive in this session");
  expect(items[1]).not.toHaveTextContent("opened this archive");
});

test("a Core archive has nothing to rekey and the last passphrase cannot be removed", async () => {
  backend({ list_envelopes: () => ({ envelopes: [ENVS[0]] }) });
  const { unmount } = render(<Rekey archive={summary(false)} onChanged={() => undefined} />);
  expect(screen.getByTestId("rekey-unavailable")).toHaveTextContent("not encrypted");
  expect(screen.queryByTestId("rekey-add")).toBeNull();
  unmount();
  render(<Rekey archive={summary(true)} onChanged={() => undefined} />);
  await screen.findAllByTestId("envelope");
  expect(screen.getByTestId("rekey-remove")).toBeDisabled();
});

test("adding a passphrase sends it once, confirms it twice, and reports a local commit", async () => {
  const user = userEvent.setup();
  let changed: ArchiveSummary | null | undefined;
  const b = backend({ list_envelopes: () => ({ envelopes: ENVS }) });
  const { container } = render(<Rekey archive={summary(true)} onChanged={(a) => (changed = a)} />);
  await screen.findAllByTestId("envelope");
  await user.click(screen.getByTestId("rekey-add"));
  await user.type(screen.getByTestId("rekey-add-form-input"), SECRET);
  await user.type(screen.getByTestId("rekey-add-form-confirm"), WRONG);
  await user.click(screen.getByTestId("rekey-add-form-submit"));
  expect(screen.getByTestId("rekey-add-form-error")).toHaveTextContent("differ");
  expect(b.cmds()).not.toContain("start_job");
  await user.clear(screen.getByTestId("rekey-add-form-input"));
  await user.clear(screen.getByTestId("rekey-add-form-confirm"));
  await user.type(screen.getByTestId("rekey-add-form-input"), SECRET);
  await user.type(screen.getByTestId("rekey-add-form-confirm"), SECRET);
  await user.click(screen.getByTestId("rekey-add-form-submit"));
  await screen.findByTestId("rekey-running");
  expect(b.request()).toEqual({ kind: "add_passphrase", handle: 3, passphrase: SECRET });
  expect(b.calls.filter((c) => JSON.stringify(c.args).includes(SECRET)).map((c) => c.cmd)).toEqual(["start_job"]);
  const after = summary(true);
  b.finish({
    status: "completed",
    summary: "s",
    rekey: {
      action: "added",
      seq: 2,
      status: "LOCAL_COMMITTED",
      file_name: "vault.mochi",
      snapshots: 0,
      new_archive_id: null,
      durability_note: null,
      archive: after,
      reopen_error: null,
    },
  });
  const status = await screen.findByTestId("rekey-status");
  expect(status).toHaveTextContent("LOCAL_COMMITTED");
  expect(status).toHaveTextContent("data key and the data are unchanged");
  expect(changed).toEqual(after);
  expect(container.innerHTML).not.toContain(SECRET);
});

test("removing a passphrase says first and after that it is not revocation", async () => {
  const user = userEvent.setup();
  const b = backend({ list_envelopes: () => ({ envelopes: ENVS }) });
  render(<Rekey archive={summary(true)} onChanged={() => undefined} />);
  await screen.findAllByTestId("envelope");
  await user.click(screen.getByTestId("rekey-remove"));
  const warning = screen.getByTestId("not-revocation");
  expect(warning).toHaveTextContent("Removing a passphrase is not revocation");
  expect(warning).toHaveTextContent("kept a copy of the file can still open that copy");
  await user.click(screen.getAllByTestId("remove-envelope")[1] as HTMLElement);
  await screen.findByTestId("rekey-running");
  expect(b.request()).toEqual({ kind: "remove_passphrase", handle: 3, envelope_id: ENVS[1]?.envelope_id });
  b.finish({
    status: "completed",
    summary: "s",
    rekey: {
      action: "removed",
      seq: 2,
      status: "LOCAL_COMMITTED",
      file_name: "vault.mochi",
      snapshots: 0,
      new_archive_id: null,
      durability_note: null,
      archive: summary(true),
      reopen_error: null,
    },
  });
  expect(await screen.findByTestId("rekey-status")).toHaveTextContent("not revocation");
});

test("re-encrypting chooses a new file first, never writes in place, and keeps the source", async () => {
  const user = userEvent.setup();
  let changed = false;
  const b = backend({ list_envelopes: () => ({ envelopes: ENVS }) });
  render(<Rekey archive={summary(true)} onChanged={() => (changed = true)} />);
  await screen.findAllByTestId("envelope");
  await user.click(screen.getByTestId("rekey-reencrypt"));
  expect(b.cmds()).toContain("choose_destination");
  expect(await screen.findByTestId("reencrypt-note")).toHaveTextContent("The original file is not changed");
  await user.type(screen.getByTestId("reencrypt-form-input"), SECRET);
  await user.type(screen.getByTestId("reencrypt-form-confirm"), SECRET);
  await user.click(screen.getByTestId("reencrypt-form-submit"));
  await screen.findByTestId("rekey-running");
  expect(b.request()).toEqual({ kind: "reencrypt", handle: 3, passphrase: SECRET });
  b.finish({
    status: "completed",
    summary: "s",
    rekey: {
      action: "reencrypted",
      seq: null,
      status: null,
      file_name: "new.mochi",
      snapshots: 2,
      new_archive_id: "f".repeat(64),
      durability_note: null,
      archive: null,
      reopen_error: null,
    },
  });
  const status = await screen.findByTestId("rekey-status");
  expect(status).toHaveTextContent("Wrote new.mochi: 2 snapshots under a new data key");
  expect(status).toHaveTextContent("Copies already made are not recalled");
  // The open archive is the untouched source; the page is not told to reload it.
  expect(changed).toBe(false);
});

test("a refused rekey shows its code and says nothing was changed", async () => {
  const user = userEvent.setup();
  const b = backend({ list_envelopes: () => ({ envelopes: ENVS }) });
  render(<Rekey archive={summary(true)} onChanged={() => undefined} />);
  await screen.findAllByTestId("envelope");
  await user.click(screen.getByTestId("rekey-remove"));
  await user.click(screen.getAllByTestId("remove-envelope")[0] as HTMLElement);
  await screen.findByTestId("rekey-running");
  b.finish({ status: "failed", code: "INVALID_ARGUMENT", message: "that would leave no passphrase" });
  const status = await screen.findByTestId("rekey-status");
  expect(status).toHaveTextContent("Failed (INVALID_ARGUMENT)");
  expect(status).toHaveTextContent("Nothing was changed");
});

// ---- test and repair of an encrypted archive ------------------------------------------------------

test("an encrypted archive chosen for testing asks for the passphrase, once, and then says it is held", async () => {
  const user = userEvent.setup();
  let attempts = 0;
  const b = backend({
    choose_check_subject: () => ({ id: 5, file_name: "vault.mochi", encrypted: true, unlocked: false }),
    unlock_check_subject: (a) => {
      attempts += 1;
      if (a.passphrase !== SECRET) {
        throw { code: "KEY_UNAVAILABLE", message: "the passphrase did not open this archive" };
      }
      return { id: 5, file_name: "vault.mochi", encrypted: true, unlocked: true };
    },
  });
  const { container } = render(<Checker archive={null} />);
  await user.click(screen.getByTestId("check-choose"));
  const box = await screen.findByTestId("check-unlock");
  expect(box).toHaveTextContent("never");
  await user.type(screen.getByTestId("check-unlock-form-input"), WRONG);
  await user.click(screen.getByTestId("check-unlock-form-submit"));
  expect(await screen.findByTestId("check-unlock-form-error")).toHaveTextContent(
    "The passphrase did not open this archive.",
  );
  await user.type(screen.getByTestId("check-unlock-form-input"), SECRET);
  await user.click(screen.getByTestId("check-unlock-form-submit"));
  expect(await screen.findByTestId("check-unlocked")).toBeInTheDocument();
  expect(screen.queryByTestId("check-unlock")).toBeNull();
  expect(attempts).toBe(2);
  expect(container.innerHTML).not.toContain(SECRET);
  expect(b.calls.filter((c) => JSON.stringify(c.args).includes(SECRET)).map((c) => c.cmd)).toEqual([
    "unlock_check_subject",
  ]);
});

test("a Core archive chosen for testing never asks for a passphrase", async () => {
  const user = userEvent.setup();
  backend({
    choose_check_subject: () => ({ id: 5, file_name: "plain.mochi", encrypted: false, unlocked: false }),
  });
  render(<Checker archive={null} />);
  await user.click(screen.getByTestId("check-choose"));
  await screen.findByText("plain.mochi");
  expect(screen.queryByTestId("check-unlock")).toBeNull();
});
