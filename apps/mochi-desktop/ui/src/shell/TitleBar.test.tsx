// The window's own title bar (release 0.9.10): the title is text only, each button calls
// its own command on the calling window, the maximize button follows the window's state,
// and a press on the empty part moves the window (a double click maximizes it).
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import { setLocale } from "../i18n";
import type { FrameState } from "../ipc/window";
import { TitleBar } from "./TitleBar";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () => (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

const HOSTILE = '<img src=x onerror="alert(1)"><b>x</b>.mochi';

function backend(start: FrameState = { maximized: false, focused: true }) {
  const cmds: string[] = [];
  let channel: Channel<FrameState> | undefined;
  let index = 0;
  let maximized = start.maximized;
  mockIPC((cmd, args) => {
    cmds.push(cmd);
    switch (cmd) {
      case "window_watch":
        channel = (args as { onChange: Channel<FrameState> }).onChange;
        return start;
      case "window_toggle_maximize":
        maximized = !maximized;
        return maximized;
      case "window_minimize":
      case "window_close":
      case "window_start_drag":
        return null;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  return {
    cmds,
    send(state: FrameState) {
      if (!channel) throw new Error("no frame channel yet");
      const id = channel.id;
      act(() => internals().runCallback(id, { index: index++, message: state }));
    },
  };
}

afterEach(() => {
  clearMocks();
  setLocale("en", false);
});

test("the archive's name is text only, cut short by CSS, and is the document's title too", () => {
  backend();
  render(<TitleBar name={HOSTILE} heading />);
  const title = screen.getByTestId("window-title");
  expect(title.tagName).toBe("H1");
  expect(title.textContent).toBe(HOSTILE);
  expect(title).toHaveAttribute("title", `${HOSTILE} - MOCHI`);
  const bar = screen.getByTestId("titlebar");
  expect(bar.querySelector("img, b, script")).toBeNull();
  expect(document.title).toBe(`${HOSTILE} - MOCHI`);
});

test("with no archive the title is the product name", () => {
  backend();
  render(<TitleBar name={null} />);
  expect(screen.getByTestId("window-title")).toHaveTextContent(/^MOCHI$/);
  expect(screen.getByTestId("window-title").tagName).toBe("SPAN");
  expect(document.title).toBe("MOCHI");
});

test("each window button calls its own command, and has a name and a tooltip", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<TitleBar name="a.mochi" />);
  const group = screen.getByRole("group", { name: "Window" });
  const buttons = within(group).getAllByRole("button");
  expect(buttons.map((x) => x.getAttribute("aria-label"))).toEqual(["Minimize", "Maximize", "Close"]);
  for (const x of buttons) expect(x).toHaveAttribute("title", x.getAttribute("aria-label"));
  await user.click(screen.getByRole("button", { name: "Minimize" }));
  await user.click(screen.getByRole("button", { name: "Maximize" }));
  await user.click(await screen.findByRole("button", { name: "Restore" }));
  await user.click(screen.getByRole("button", { name: "Close" }));
  expect(b.cmds.filter((c) => c !== "window_watch")).toEqual([
    "window_minimize",
    "window_toggle_maximize",
    "window_toggle_maximize",
    "window_close",
  ]);
  // A press on a button is the button's, never a drag of the window.
  expect(b.cmds).not.toContain("window_start_drag");
});

test("the buttons are reachable with Tab and work with Enter", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<TitleBar name={null} />);
  await user.tab();
  expect(document.activeElement).toBe(screen.getByTestId("window-minimize"));
  await user.tab();
  await user.tab();
  expect(document.activeElement).toBe(screen.getByTestId("window-close"));
  await user.keyboard("{Enter}");
  expect(b.cmds).toContain("window_close");
});

test("maximize and restore follow the window's state as Rust reports it", async () => {
  const b = backend({ maximized: true, focused: true });
  render(<TitleBar name={null} />);
  const button = screen.getByTestId("window-maximize");
  // The state the window starts in.
  expect(await screen.findByRole("button", { name: "Restore" })).toBe(button);
  expect(button).toHaveAttribute("data-state", "maximized");
  // Restored by the system (a drag off the top, Windows+Down): the button follows.
  b.send({ maximized: false, focused: true });
  expect(button).toHaveAccessibleName("Maximize");
  expect(button).toHaveAttribute("data-state", "normal");
  b.send({ maximized: true, focused: false });
  expect(button).toHaveAccessibleName("Restore");
  expect(screen.getByTestId("titlebar")).toHaveAttribute("data-focused", "false");
});

test("a press on the empty part moves the window; a double click maximizes or restores it", async () => {
  const b = backend();
  render(
    <TitleBar name="a.mochi">
      <div className="toolbar">
        <button type="button">{"Add"}</button>
        <input aria-label="Search" />
      </div>
    </TitleBar>,
  );
  const title = screen.getByTestId("window-title");
  fireEvent.mouseDown(title, { button: 0, detail: 1 });
  expect(b.cmds).toContain("window_start_drag");
  fireEvent.mouseDown(title, { button: 0, detail: 2 });
  expect(b.cmds).toContain("window_toggle_maximize");
  expect(await screen.findByRole("button", { name: "Restore" })).toBeInTheDocument();
  // The toolbar's gaps are part of the bar; its controls are not.
  const before = b.cmds.length;
  fireEvent.mouseDown(screen.getByRole("button", { name: "Add" }), { button: 0, detail: 1 });
  fireEvent.mouseDown(screen.getByRole("textbox", { name: "Search" }), { button: 0, detail: 1 });
  fireEvent.mouseDown(title, { button: 2, detail: 1 });
  expect(b.cmds.length).toBe(before);
  fireEvent.mouseDown(screen.getByTestId("titlebar").querySelector(".toolbar") as Element, { button: 0, detail: 1 });
  expect(b.cmds.filter((c) => c === "window_start_drag")).toHaveLength(2);
});

test("the compact window has no maximize, does not watch the window, and its double click does nothing", () => {
  const b = backend();
  render(<TitleBar name={null} maximizable={false} />);
  expect(screen.queryByTestId("window-maximize")).toBeNull();
  fireEvent.mouseDown(screen.getByTestId("window-title"), { button: 0, detail: 2 });
  expect(b.cmds).toEqual([]);
});

test("the buttons are named in Thai and Chinese too", () => {
  backend();
  for (const [locale, names] of [
    ["th", ["ย่อหน้าต่าง", "ขยายเต็มจอ", "ปิด"]],
    ["zh-CN", ["最小化", "最大化", "关闭"]],
  ] as const) {
    setLocale(locale, false);
    const view = render(<TitleBar name={null} />);
    const labels = within(view.container)
      .getAllByRole("button")
      .map((x) => x.getAttribute("aria-label"));
    expect(labels).toEqual(names);
    view.unmount();
  }
});
