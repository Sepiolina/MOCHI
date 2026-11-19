// Plan D7: the crash-report policy is opt-in, local, and never sent. These
// tests pin what the screen offers and what it asks Rust for.
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import type { CrashLogView, CrashSettingsView } from "../ipc/crash";
import { CrashReports } from "./CrashReports";

type Call = { cmd: string; args: Record<string, unknown> };

function backend(initial: Partial<CrashSettingsView> = {}, log: Partial<CrashLogView> = {}) {
  const calls: Call[] = [];
  let state: CrashSettingsView = { enabled: false, include_details: false, entries: 0, bytes: 0, ...initial };
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "get_crash_settings":
        return state;
      case "set_crash_settings": {
        const enabled = a.enabled as boolean;
        state = { ...state, enabled, include_details: enabled && (a.includeDetails as boolean) };
        return state;
      }
      case "read_crash_log":
        return { text: "=== MOCHI crash record ===\nlocation: x.rs:1:1\n", truncated: false, entries: 1, ...log };
      case "export_crash_log":
        return "mochi-crash-log.txt";
      case "clear_crash_log":
        state = { ...state, entries: 0, bytes: 0 };
        return state;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return calls;
}

afterEach(() => clearMocks());

test("both switches are off when nothing was ever chosen, and the policy says nothing is sent", async () => {
  const calls = backend();
  render(<CrashReports />);
  // Before Rust has answered, the switch is off and cannot be flipped.
  expect(screen.getByTestId("crash-enabled")).not.toBeChecked();
  expect(screen.getByTestId("crash-enabled")).toBeDisabled();
  await waitFor(() => expect(screen.getByTestId("crash-enabled")).toBeEnabled());
  expect(screen.getByTestId("crash-enabled")).not.toBeChecked();
  expect(screen.getByTestId("crash-details")).not.toBeChecked();
  expect(screen.getByTestId("crash-details")).toBeDisabled();
  expect(screen.getByTestId("crash-policy")).toHaveTextContent(/never sends crash reports/);
  expect(screen.getByTestId("crash-policy")).toHaveTextContent(/no network request/);
  // Looking at the screen changes nothing.
  expect(calls.map((c) => c.cmd)).toEqual(["get_crash_settings"]);
  // With no records there is nothing to show, save, or delete.
  for (const id of ["crash-show", "crash-export", "crash-clear"]) expect(screen.getByTestId(id)).toBeDisabled();
});

test("turning the log on asks Rust, and the message option warns about file names", async () => {
  const user = userEvent.setup();
  const calls = backend();
  render(<CrashReports />);
  await waitFor(() => expect(screen.getByTestId("crash-enabled")).toBeEnabled());
  await user.click(screen.getByTestId("crash-enabled"));
  expect(calls.at(-1)).toEqual({ cmd: "set_crash_settings", args: { enabled: true, includeDetails: false } });
  await waitFor(() => expect(screen.getByTestId("crash-details")).toBeEnabled());
  expect(screen.getByTestId("crash-status")).toHaveTextContent("The local crash log is on.");
  expect(screen.getByTestId("crash-details-warning")).toHaveTextContent(/name of a file or folder/);
  // The option is described by its warning, so a screen reader reads both.
  expect(screen.getByTestId("crash-details")).toHaveAccessibleDescription(/name of a file or folder/);

  await user.click(screen.getByTestId("crash-details"));
  expect(calls.at(-1)).toEqual({ cmd: "set_crash_settings", args: { enabled: true, includeDetails: true } });
  await waitFor(() => expect(screen.getByTestId("crash-details")).toBeChecked());

  // Turning the log off also turns the message option off.
  await user.click(screen.getByTestId("crash-enabled"));
  expect(calls.at(-1)).toEqual({ cmd: "set_crash_settings", args: { enabled: false, includeDetails: false } });
  await waitFor(() => expect(screen.getByTestId("crash-details")).not.toBeChecked());
  expect(screen.getByTestId("crash-status")).toHaveTextContent("The local crash log is off.");
});

test("the log can be shown as text, saved through a dialog Rust shows, and deleted", async () => {
  const user = userEvent.setup();
  const calls = backend({ enabled: true, entries: 1, bytes: 2048 });
  render(<CrashReports />);
  await waitFor(() => expect(screen.getByTestId("crash-state")).toHaveTextContent("1 record in the log, 2.0 KiB"));

  await user.click(screen.getByTestId("crash-show"));
  const text = await screen.findByTestId("crash-log-text");
  expect(text).toHaveTextContent("location: x.rs:1:1");
  // Reachable and scrollable by keyboard, and named.
  expect(text).toHaveAttribute("tabindex", "0");
  expect(text).toHaveAccessibleName("Crash log text");
  await user.click(screen.getByTestId("crash-show")); // now "Hide the log"
  expect(screen.queryByTestId("crash-log")).toBeNull();

  await user.click(screen.getByTestId("crash-export"));
  await waitFor(() =>
    expect(screen.getByTestId("crash-status")).toHaveTextContent("Saved a copy of the crash log to mochi-crash-log.txt."),
  );
  // The webview sends no path: the export command takes no arguments.
  // Rust shows the dialog; the window passes only its words, never a path.
  expect(calls.find((c) => c.cmd === "export_crash_log")?.args).toEqual({
    text: { title: expect.any(String), filter: expect.any(String) },
  });

  await user.click(screen.getByTestId("crash-clear"));
  await waitFor(() => expect(screen.getByTestId("crash-status")).toHaveTextContent("The crash log was deleted."));
  expect(screen.getByTestId("crash-enabled")).toBeChecked(); // deleting the log keeps the choice
  expect(screen.getByTestId("crash-clear")).toBeDisabled();
});

test("log text that looks like markup stays text", async () => {
  const user = userEvent.setup();
  backend({ enabled: true, include_details: true, entries: 1, bytes: 40 }, { text: "message: <img src=x onerror=alert(1)>" });
  render(<CrashReports />);
  await waitFor(() => expect(screen.getByTestId("crash-show")).toBeEnabled());
  await user.click(screen.getByTestId("crash-show"));
  const text = await screen.findByTestId("crash-log-text");
  expect(text).toHaveTextContent("<img src=x onerror=alert(1)>");
  expect(text.querySelector("img")).toBeNull();
});

test("a failure to save the choice is an alert and does not look like success", async () => {
  const user = userEvent.setup();
  mockIPC((cmd) => {
    if (cmd === "get_crash_settings") return { enabled: false, include_details: false, entries: 0, bytes: 0 };
    throw { code: "IO_ERROR", message: "disk full" };
  });
  render(<CrashReports />);
  await waitFor(() => expect(screen.getByTestId("crash-enabled")).toBeEnabled());
  await user.click(screen.getByTestId("crash-enabled"));
  expect(await screen.findByRole("alert")).toHaveTextContent("Could not change the crash-report settings (IO_ERROR): disk full");
  expect(screen.getByTestId("crash-enabled")).not.toBeChecked();
});
