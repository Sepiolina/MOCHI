// Owner request 2026-11-10: words beside the toolbar's icons whenever they all fit, icons alone
// when they do not. jsdom lays nothing out, so the widths are given here.
import { act, render, screen } from "@testing-library/react";
import { useRef, useState } from "react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

import { useToolbarLabels } from "./useToolbarLabels";

let observed: (() => void)[] = [];
class FakeResizeObserver {
  constructor(private readonly cb: () => void) {}
  observe() {
    observed.push(this.cb);
  }
  disconnect() {
    observed = observed.filter((c) => c !== this.cb);
  }
}

beforeEach(() => {
  observed = [];
  vi.stubGlobal("ResizeObserver", FakeResizeObserver);
});
afterEach(() => vi.unstubAllGlobals());

/** A bar whose content is `content` px wide (more with labels on) in a box of `box` px. */
const size = { box: 600, content: 400, labels: 300 };

function Bar() {
  const ref = useRef<HTMLDivElement>(null);
  const [extra, setExtra] = useState(false);
  useToolbarLabels(ref);
  return (
    <div
      ref={(el) => {
        ref.current = el;
        if (el === null) return;
        Object.defineProperty(el, "clientWidth", { configurable: true, get: () => size.box });
        Object.defineProperty(el, "scrollWidth", {
          configurable: true,
          get: () => size.content + (el.dataset.labels === "on" ? size.labels : 0) + (extra ? 200 : 0),
        });
      }}
      data-testid="bar"
    >
      <button type="button" onClick={() => setExtra(true)}>
        more
      </button>
      {extra && <span>another button</span>}
    </div>
  );
}

test("the words stay on while everything fits, and come off when they would overflow", () => {
  size.box = 800;
  render(<Bar />);
  const bar = screen.getByTestId("bar");
  expect(bar).toHaveAttribute("data-labels", "on");
  // The window narrows: 400 + 300 no longer fits in 650.
  size.box = 650;
  act(() => observed.forEach((cb) => cb()));
  expect(bar).toHaveAttribute("data-labels", "off");
  // Wide again: the words come back.
  size.box = 1000;
  act(() => observed.forEach((cb) => cb()));
  expect(bar).toHaveAttribute("data-labels", "on");
});

test("new contents (a button drawn into the bar, a language change) measure again", async () => {
  size.box = 800;
  render(<Bar />);
  const bar = screen.getByTestId("bar");
  expect(bar).toHaveAttribute("data-labels", "on");
  await act(async () => {
    screen.getByRole("button").click();
    // The MutationObserver reports after the change, in a microtask.
    await Promise.resolve();
  });
  expect(bar).toHaveAttribute("data-labels", "off");
});

test("the observers are let go when the bar goes away", () => {
  const { unmount } = render(<Bar />);
  expect(observed).toHaveLength(1);
  unmount();
  expect(observed).toHaveLength(0);
});
