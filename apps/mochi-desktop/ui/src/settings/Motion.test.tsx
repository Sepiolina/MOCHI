// Animations (owner request 2026-11-14): on, off, or as the system; applied to the page at
// once, remembered on this computer, and followed by the other windows.
import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test } from "vitest";

import { audit } from "../test/audit";
import { Motion } from "./Motion";
import { MOTION_KEY, getMotion, setMotion } from "./motionChoice";

afterEach(() => {
  setMotion("system");
  window.localStorage.clear();
});

test("starts as the system, and a choice applies to the page at once and is remembered", async () => {
  const user = userEvent.setup();
  render(<Motion />);
  const select = screen.getByTestId("motion-select");
  expect(select).toHaveValue("system");
  expect(document.documentElement.dataset.motion).toBe("system");
  expect(audit()).toEqual([]);
  await user.selectOptions(select, "off");
  expect(document.documentElement.dataset.motion).toBe("off");
  expect(window.localStorage.getItem(MOTION_KEY)).toBe("off");
  await user.selectOptions(select, "on");
  expect(document.documentElement.dataset.motion).toBe("on");
});

test("another window's change is followed, and nonsense is ignored", () => {
  render(<Motion />);
  act(() => {
    window.dispatchEvent(new StorageEvent("storage", { key: MOTION_KEY, newValue: "off" }));
  });
  expect(getMotion()).toBe("off");
  expect(screen.getByTestId("motion-select")).toHaveValue("off");
  act(() => {
    window.dispatchEvent(new StorageEvent("storage", { key: MOTION_KEY, newValue: "sideways" }));
  });
  expect(getMotion()).toBe("off");
});

test("the stylesheet turns motion off for Off, and for the system's reduce-motion unless On", async () => {
  const { readFileSync } = await import("node:fs");
  const { join } = await import("node:path");
  const css = readFileSync(join(process.cwd(), "src/styles.css"), "utf8");
  expect(css).toMatch(/:root\[data-motion="off"\] \*[^{]*\{\s*animation: none !important;\s*transition: none !important;/);
  expect(css).toMatch(/@media \(prefers-reduced-motion: reduce\) \{\s*:root:not\(\[data-motion="on"\]\) \*/);
});
