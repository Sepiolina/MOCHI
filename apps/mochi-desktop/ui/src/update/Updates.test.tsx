// Plan D7 / T13: the Settings control and the update dialog. Wording follows
// spec §23.3; release notes are remote strings and must stay text.
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, describe, expect, test } from "vitest";

import type { JobEvent } from "../ipc/jobs";
import type { UpdateCheckView, UpdateOffer, UpdateSettingsView } from "../ipc/update";
import { t } from "../i18n";
import { failureText, Updates } from "./Updates";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () =>
  (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

const settings = (over: Partial<UpdateSettingsView> = {}): UpdateSettingsView => ({
  check_on_start: false,
  current_version: "0.1.0",
  install_mode: "in_place",
  download_page: "https://github.com/immerh8/MOCHI/releases/latest",
  needs_admin_prompt: false,
  ...over,
});

const offer = (over: Partial<UpdateOffer> = {}): UpdateOffer => ({
  version: "0.2.0",
  current_version: "0.1.0",
  notes: "- fixes",
  notes_truncated: false,
  install_mode: "in_place",
  download_page: "https://github.com/immerh8/MOCHI/releases/latest",
  needs_admin_prompt: false,
  ...over,
});

function backend(opts: { settings?: UpdateSettingsView; check?: UpdateCheckView }) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let channel: Channel<JobEvent> | undefined;
  let index = 0;
  let current = opts.settings ?? settings();
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "get_update_settings":
        return current;
      case "set_update_settings":
        current = { ...current, check_on_start: a.checkOnStart as boolean };
        return current;
      case "check_for_update":
        return opts.check ?? { result: "up_to_date", current_version: "0.1.0" };
      case "start_job":
        channel = a.onEvent as Channel<JobEvent>;
        return 7;
      case "cancel_job":
        return true;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return {
    calls,
    emit: (message: JobEvent) => {
      if (!channel) throw new Error("no job was started");
      const id = channel.id;
      act(() => {
        internals().runCallback(id, { index: index++, message });
      });
    },
  };
}

afterEach(() => clearMocks());

describe("Settings: Check for updates", () => {
  test("does nothing on its own: the start-up check is off by default and no request is made", async () => {
    const b = backend({});
    render(<Updates />);
    const box = await screen.findByTestId("update-startup");
    expect(box).not.toBeChecked();
    await waitFor(() => expect(screen.getByTestId("update-version")).toBeInTheDocument());
    expect(b.calls.map((c) => c.cmd)).toEqual(["get_update_settings"]);
    expect(screen.queryByTestId("update-dialog")).toBeNull();
  });

  test("the explicit button checks, and an up-to-date answer is plain text", async () => {
    const b = backend({});
    render(<Updates />);
    await userEvent.click(await screen.findByTestId("update-check"));
    await waitFor(() =>
      expect(screen.getByTestId("update-status")).toHaveTextContent(
        "No newer version was found. You are running version 0.1.0.",
      ),
    );
    expect(b.calls.find((c) => c.cmd === "check_for_update")?.args).toEqual({ automatic: false });
  });

  test("turning the opt-in on is saved, and the check then runs at start-up", async () => {
    const b = backend({});
    const { unmount } = render(<Updates />);
    await userEvent.click(await screen.findByTestId("update-startup"));
    await waitFor(() => expect(screen.getByTestId("update-startup")).toBeChecked());
    expect(b.calls.find((c) => c.cmd === "set_update_settings")?.args).toEqual({ checkOnStart: true });
    unmount();
    // A new start with the setting on asks once, as an automatic check.
    const b2 = backend({ settings: settings({ check_on_start: true }) });
    render(<Updates />);
    await waitFor(() =>
      expect(b2.calls.find((c) => c.cmd === "check_for_update")?.args).toEqual({ automatic: true }),
    );
    expect(b2.calls.filter((c) => c.cmd === "check_for_update")).toHaveLength(1);
  });
});

describe("the update dialog", () => {
  test("shows the version and the notes as text, never as HTML, and installs nothing until confirmed", async () => {
    const hostile = '<img src=x onerror="window.pwned=1"><b>bold</b><script>window.pwned=2</script>';
    const b = backend({
      check: { result: "available", offer: offer({ notes: hostile }) },
    });
    const { container } = render(<Updates />);
    await userEvent.click(await screen.findByTestId("update-check"));
    expect(await screen.findByText(/Version 0\.2\.0 is available/)).toBeInTheDocument();
    const notes = screen.getByTestId("update-notes");
    expect(notes.textContent).toBe(hostile);
    expect(notes.querySelector("img, b, script")).toBeNull();
    expect(container.querySelector("img, script")).toBeNull();
    expect((window as unknown as { pwned?: number }).pwned).toBeUndefined();
    expect(b.calls.some((c) => c.cmd === "start_job")).toBe(false);
    // "Not now" closes it; still nothing was started.
    await userEvent.click(screen.getByTestId("update-later"));
    expect(screen.queryByTestId("update-dialog")).toBeNull();
    expect(b.calls.some((c) => c.cmd === "start_job")).toBe(false);
  });

  test("confirming starts the install job for that version, with progress, and a finished job says restart", async () => {
    const b = backend({ check: { result: "available", offer: offer() } });
    render(<Updates />);
    await userEvent.click(await screen.findByTestId("update-check"));
    await userEvent.click(await screen.findByTestId("update-install"));
    expect(b.calls.find((c) => c.cmd === "start_job")?.args.request).toEqual({
      kind: "install_update",
      version: "0.2.0",
    });
    b.emit({ event: "started", job_id: 7 });
    b.emit({ event: "progress", job_id: 7, phase: "update_download", completed: 1024, total: 4096 });
    expect(screen.getByTestId("update-progress-text")).toHaveTextContent(/1\.0 KiB.*of.*4\.0 KiB/);
    b.emit({
      event: "finished",
      job_id: 7,
      outcome: { status: "completed", summary: "update to version 0.2.0 installed" },
    });
    expect(screen.getByTestId("update-status")).toHaveTextContent(
      "Version 0.2.0 was installed. Close MOCHI and open it again to use it.",
    );
  });

  test("a download can be cancelled", async () => {
    const b = backend({ check: { result: "available", offer: offer() } });
    render(<Updates />);
    await userEvent.click(await screen.findByTestId("update-check"));
    await userEvent.click(await screen.findByTestId("update-install"));
    b.emit({ event: "started", job_id: 7 });
    await userEvent.click(await screen.findByTestId("update-cancel"));
    expect(b.calls.find((c) => c.cmd === "cancel_job")?.args).toEqual({ jobId: 7 });
    b.emit({ event: "finished", job_id: 7, outcome: { status: "cancelled" } });
    expect(screen.getByTestId("update-status")).toHaveTextContent("The update was not installed.");
  });

  test("a refused signature is an alert that says nothing was installed", async () => {
    const b = backend({ check: { result: "available", offer: offer() } });
    render(<Updates />);
    await userEvent.click(await screen.findByTestId("update-check"));
    await userEvent.click(await screen.findByTestId("update-install"));
    b.emit({ event: "started", job_id: 7 });
    b.emit({
      event: "finished",
      job_id: 7,
      outcome: {
        status: "failed",
        code: "IO_ERROR",
        message: "The update's signature did not verify, so it was refused and nothing was installed.",
      },
    });
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(/refused/);
    expect(screen.getByTestId("update-status")).not.toHaveTextContent(/installed\./);
  });

  test("a build that cannot update itself says where to download, and offers no install", async () => {
    backend({
      check: {
        result: "available",
        offer: offer({ install_mode: "manual" }),
      },
    });
    render(<Updates />);
    await userEvent.click(await screen.findByTestId("update-check"));
    expect(await screen.findByTestId("update-page")).toHaveTextContent(
      "https://github.com/immerh8/MOCHI/releases/latest",
    );
    expect(screen.queryByTestId("update-install")).toBeNull();
  });

  test("the Ubuntu install says it will ask for a password", async () => {
    backend({ check: { result: "available", offer: offer({ needs_admin_prompt: true }) } });
    render(<Updates />);
    await userEvent.click(await screen.findByTestId("update-check"));
    expect(await screen.findByText(t("update.adminPrompt"))).toBeInTheDocument();
  });
});

describe("failures", () => {
  test("a failed check is an alert, not a status, and never claims the app is current", async () => {
    backend({ check: { result: "failed", kind: "offline", detail: "no route" } });
    render(<Updates />);
    await userEvent.click(await screen.findByTestId("update-check"));
    const alert = await screen.findByTestId("update-failed");
    expect(alert).toHaveAttribute("role", "alert");
    expect(alert).toHaveTextContent("Could not reach the update server");
    expect(screen.getByTestId("update-status")).toHaveTextContent("");
  });

  test("a signature failure at check time is refused in words", () => {
    expect(failureText("signature", "")).toMatch(/refused/);
  });

  test("no wording calls anything verified, trusted, safe, or secure", () => {
    const all = [
      failureText("offline", "x"),
      failureText("server", "x"),
      failureText("signature", "x"),
      failureText("install", "x"),
      failureText("unavailable", "x"),
      failureText("other", "x"),
    ].join("\n");
    expect(all).not.toMatch(/\b(verified|trusted|safe|secure|backed up|preserved)\b/i);
  });
});
