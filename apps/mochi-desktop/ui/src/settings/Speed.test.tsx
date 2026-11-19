// Release 0.9.11: the compression-speed switch. On (every core) unless the
// user turned it off. Fast extract: off unless the user turned it on. Both
// only ask Rust to change the settings, and each keeps the other as it is.
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import type { SpeedSettingsView } from "../ipc/speed";
import { Speed } from "./Speed";

function backend(initial: SpeedSettingsView = { all_cores: true, fast_extract: false, cores: 6, trust_ntfs_flush: false, windows: false }, failSet = false) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let state = initial;
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    if (cmd === "get_speed_settings") return state;
    if (cmd === "set_speed_settings") {
      if (failSet) throw { code: "IO_ERROR", message: "disk full" };
      state = {
        ...state,
        all_cores: a.allCores as boolean,
        fast_extract: a.fastExtract as boolean,
        trust_ntfs_flush: (a.trustNtfsFlush as boolean | undefined) ?? state.trust_ntfs_flush,
      };
      return state;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return calls;
}

afterEach(() => clearMocks());

test("every core is on by default and the label names how many", async () => {
  backend();
  render(<Speed />);
  const box = screen.getByTestId("speed-all-cores");
  await waitFor(() => expect(box).toBeEnabled());
  expect(box).toBeChecked();
  expect(screen.getByText(/every processor core \(6\)/)).toBeInTheDocument();
  expect(screen.getByText(/The archive is the same either way/)).toBeInTheDocument();
});

test("turning it off asks Rust for one core, and back on for every core", async () => {
  const user = userEvent.setup();
  const calls = backend();
  render(<Speed />);
  const box = screen.getByTestId("speed-all-cores");
  await waitFor(() => expect(box).toBeEnabled());
  await user.click(box);
  await waitFor(() => expect(box).not.toBeChecked());
  await user.click(box);
  await waitFor(() => expect(box).toBeChecked());
  expect(calls.filter((c) => c.cmd === "set_speed_settings").map((c) => c.args)).toEqual([
    { allCores: false, fastExtract: false },
    { allCores: true, fastExtract: false },
  ]);
});

test("a failed change is shown and the switch keeps the saved state", async () => {
  const user = userEvent.setup();
  backend({ all_cores: true, fast_extract: false, cores: 4, trust_ntfs_flush: false, windows: false }, true);
  render(<Speed />);
  const box = screen.getByTestId("speed-all-cores");
  await waitFor(() => expect(box).toBeEnabled());
  await user.click(box);
  expect(await screen.findByRole("alert")).toHaveTextContent("Could not change the speed setting");
  expect(box).toBeChecked();
});

test("fast extract is off by default and its note names the cost", async () => {
  backend();
  render(<Speed />);
  const box = screen.getByTestId("speed-fast-extract");
  await waitFor(() => expect(box).toBeEnabled());
  expect(box).not.toBeChecked();
  expect(box).toHaveAccessibleDescription(/if the power fails during or just after an extraction/);
});

test("turning fast extract on keeps the core setting as it is", async () => {
  const user = userEvent.setup();
  const calls = backend({ all_cores: false, fast_extract: false, cores: 6, trust_ntfs_flush: false, windows: false });
  render(<Speed />);
  const box = screen.getByTestId("speed-fast-extract");
  await waitFor(() => expect(box).toBeEnabled());
  await user.click(box);
  await waitFor(() => expect(box).toBeChecked());
  expect(screen.getByTestId("speed-all-cores")).not.toBeChecked();
  expect(calls.filter((c) => c.cmd === "set_speed_settings").map((c) => c.args)).toEqual([
    { allCores: false, fastExtract: true },
  ]);
});

// Spec Annex B.2.20 (D30): the NTFS folder-flush setting exists on Windows only, is off
// by default (policy B), and turning it on keeps the other two switches as they are.
test("the NTFS setting is hidden off Windows", async () => {
  backend();
  render(<Speed />);
  await waitFor(() => expect(screen.getByTestId("speed-all-cores")).toBeEnabled());
  expect(screen.queryByTestId("speed-ntfs-flush")).toBeNull();
});

test("on Windows the NTFS setting is off by default and turning it on is saved", async () => {
  const user = userEvent.setup();
  const calls = backend({ all_cores: false, fast_extract: true, cores: 4, trust_ntfs_flush: false, windows: true });
  render(<Speed />);
  const box = await screen.findByTestId("speed-ntfs-flush");
  await waitFor(() => expect(box).toBeEnabled());
  expect(box).not.toBeChecked();
  expect(screen.getByText(/Count a folder flush as saved/)).toBeInTheDocument();
  await user.click(box);
  await waitFor(() => expect(box).toBeChecked());
  expect(calls.filter((c) => c.cmd === "set_speed_settings").map((c) => c.args)).toEqual([
    { allCores: false, fastExtract: true, trustNtfsFlush: true },
  ]);
  // The other switches leave it as saved (no third argument).
  await user.click(screen.getByTestId("speed-fast-extract"));
  await waitFor(() => expect(screen.getByTestId("speed-fast-extract")).not.toBeChecked());
  expect(box).toBeChecked();
});
