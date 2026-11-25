// Owner request 2026-11-12: Settings read as a wall of text. One section at a time, each
// setting a row; and Extraction can say where "Extract to..." starts (the Desktop, the folder
// used last, or a folder of the user's), since the folder used last may be a slow network drive.
import { render, screen, waitFor } from "@testing-library/react";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test } from "vitest";

import type { ExtractSettingsView } from "../ipc/start";
import { Settings } from "./Settings";

function backend(initial: Partial<ExtractSettingsView> = {}, pick: string | null = "/mnt/nas/inbox") {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let extract: ExtractSettingsView = { start: "desktop", custom: "", open_folder: true, ...initial };
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "get_crash_settings":
        return { enabled: false, include_details: false, entries: 0, bytes: 0 };
      case "get_speed_settings":
        return { all_cores: true, fast_extract: false, cores: 8, trust_ntfs_flush: false, local_history: true, wait_for_writer: false, windows: false };
      case "get_update_settings":
        return {
          check_on_start: false,
          current_version: "0.9.19",
          install_mode: "manual",
          download_page: "https://example.invalid/releases",
          needs_admin_prompt: false,
        };
      case "get_extract_settings":
        return extract;
      case "set_extract_settings": {
        const start = a.start as ExtractSettingsView["start"];
        const custom = (a.custom as string | null) ?? extract.custom;
        extract = { ...extract, start, custom };
        return extract;
      }
      case "browse_extract_folder":
        return pick;
      case "set_open_folder_when_done":
        extract = { ...extract, open_folder: a.open as boolean };
        return a.open;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return calls;
}

afterEach(() => clearMocks());

test("one section at a time; the arrow keys, Home and End move between them", async () => {
  const user = userEvent.setup();
  backend();
  render(<Settings />);
  const tabs = screen.getAllByRole("tab").map((t) => t.textContent);
  expect(tabs).toEqual(["General", "Extraction", "Performance", "Updates", "Crash reports"]);
  expect(screen.getByTestId("settings-panel-general")).toBeVisible();
  expect(screen.getByTestId("settings-panel-privacy")).not.toBeVisible();
  // Every section is mounted, hidden or not, so each keeps its state.
  expect(screen.getByTestId("crash-enabled")).toBeInTheDocument();

  await user.click(screen.getByTestId("settings-tab-performance"));
  expect(screen.getByTestId("settings-tab-performance")).toHaveAttribute("aria-selected", "true");
  expect(screen.getByTestId("settings-panel-performance")).toBeVisible();
  expect(screen.getByTestId("settings-panel-general")).not.toBeVisible();

  await user.keyboard("{ArrowDown}");
  expect(screen.getByTestId("settings-tab-updates")).toHaveFocus();
  expect(screen.getByTestId("settings-panel-updates")).toBeVisible();
  await user.keyboard("{End}");
  expect(screen.getByTestId("settings-tab-privacy")).toHaveFocus();
  await user.keyboard("{ArrowDown}");
  expect(screen.getByTestId("settings-tab-general")).toHaveFocus();
  await user.keyboard("{ArrowUp}");
  expect(screen.getByTestId("settings-tab-privacy")).toHaveFocus();
  await user.keyboard("{Home}");
  expect(screen.getByTestId("settings-tab-general")).toHaveFocus();
  // Only the selected tab is a Tab stop.
  for (const tab of screen.getAllByRole("tab")) {
    expect(tab).toHaveAttribute("tabindex", tab.getAttribute("aria-selected") === "true" ? "0" : "-1");
  }
});

test("each setting is a row with its name, a short line, and the longer words behind an i", async () => {
  backend();
  render(<Settings />);
  const box = screen.getByTestId("speed-fast-extract");
  await waitFor(() => expect(box).toBeEnabled());
  expect(box).toHaveAccessibleName("Fast extract");
  expect(box).toHaveAccessibleDescription(/if the power fails during or just after an extraction/);
  expect(screen.getByTestId("speed-all-cores")).toHaveAccessibleName("Use every processor core (8)");
  // The longer explanation is a note, not a paragraph on the page.
  const row = box.closest(".setting-row");
  expect(row?.querySelector(".hint")).toHaveAttribute("title", expect.stringContaining("Applies from the next job"));
});

test("Extract to... starts on the Desktop by default, and the folder used last is one click", async () => {
  const user = userEvent.setup();
  const calls = backend();
  render(<Settings />);
  await user.click(screen.getByTestId("settings-tab-extraction"));
  const desktop = screen.getByTestId("extract-start-desktop");
  await waitFor(() => expect(desktop).toBeEnabled());
  expect(desktop).toBeChecked();
  expect(screen.getByTestId("extract-custom")).toHaveTextContent("No folder chosen yet");
  await user.click(screen.getByTestId("extract-start-last_used"));
  await waitFor(() => expect(screen.getByTestId("extract-start-last_used")).toBeChecked());
  expect(calls.filter((c) => c.cmd === "set_extract_settings").map((c) => c.args)).toEqual([
    { start: "last_used", custom: null },
  ]);
  // The group is named and described for a screen reader.
  expect(screen.getByRole("group", { name: /Extract to… starts in/ })).toHaveAccessibleDescription(/suggests first/);
});

test("a folder of my choice asks for the folder first; the path is shown as text", async () => {
  const user = userEvent.setup();
  const calls = backend({}, "/mnt/nas/<i>inbox</i>");
  render(<Settings />);
  await user.click(screen.getByTestId("settings-tab-extraction"));
  await waitFor(() => expect(screen.getByTestId("extract-start-custom")).toBeEnabled());
  await user.click(screen.getByTestId("extract-start-custom"));
  await waitFor(() => expect(screen.getByTestId("extract-start-custom")).toBeChecked());
  const cmds = calls.map((c) => c.cmd);
  expect(cmds.indexOf("browse_extract_folder")).toBeLessThan(cmds.indexOf("set_extract_settings"));
  expect(calls.find((c) => c.cmd === "set_extract_settings")?.args).toEqual({
    start: "custom",
    custom: "/mnt/nas/<i>inbox</i>",
  });
  const shown = screen.getByTestId("extract-custom");
  expect(shown).toHaveTextContent("/mnt/nas/<i>inbox</i>");
  expect(shown.querySelector("i")).toBeNull();
});

test("cancelling the folder dialog changes nothing", async () => {
  const user = userEvent.setup();
  const calls = backend({}, null);
  render(<Settings />);
  await user.click(screen.getByTestId("settings-tab-extraction"));
  await waitFor(() => expect(screen.getByTestId("extract-start-custom")).toBeEnabled());
  await user.click(screen.getByTestId("extract-start-custom"));
  await waitFor(() => expect(calls.some((c) => c.cmd === "browse_extract_folder")).toBe(true));
  expect(calls.some((c) => c.cmd === "set_extract_settings")).toBe(false);
  await waitFor(() => expect(screen.getByTestId("extract-start-desktop")).toBeChecked());
});

test("Open the folder when done is the same saved choice as the compact window's", async () => {
  const user = userEvent.setup();
  const calls = backend({ open_folder: true });
  render(<Settings />);
  await user.click(screen.getByTestId("settings-tab-extraction"));
  const box = screen.getByTestId("extract-open-folder");
  await waitFor(() => expect(box).toBeEnabled());
  expect(box).toBeChecked();
  await user.click(box);
  await waitFor(() => expect(box).not.toBeChecked());
  expect(calls.filter((c) => c.cmd === "set_open_folder_when_done").map((c) => c.args)).toEqual([{ open: false }]);
});
