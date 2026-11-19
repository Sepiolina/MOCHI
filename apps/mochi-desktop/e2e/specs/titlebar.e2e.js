// Release 0.9.10, owner request: one title bar. The archive windows are built without the
// operating system's bar (src-tauri/src/frame.rs); the page draws the icon, the title, the
// toolbar and minimize, maximize/restore and close. On the real app over real IPC: the title is
// the archive's name as text, Maximize and Restore change the real window and the button
// follows, the rarer actions sit in More, and Close closes the window the way the system's
// close button does (Rust releases the window). WebDriver cannot press a mouse button for the
// operating system's window move, so dragging the bar is covered by the UI tests
// (shell/TitleBar.test.tsx) and the release checklist, as are the resize edges and Alt+F4.
//
// The archives come from the d1_browse_fixtures example (wdio.conf.js): browse.mochi on the first
// "Open archive" click, large.mochi on the second.
import { $, browser, expect } from "@wdio/globals";
import { inNewWindow, openMore } from "./_ui.js";

const rect = () => browser.getWindowRect();

describe("the window's own title bar", () => {
  it("is one bar: the product name, the primary actions, More, and the window buttons", async () => {
    await $('[data-testid="start-screen"]').waitForExist({ timeout: 60_000 });
    const bar = await $('[data-testid="titlebar"]');
    await expect(bar).toBeDisplayed();
    await expect($('[data-testid="window-title"]')).toHaveText("MOCHI");
    expect(await browser.getTitle()).toBe("MOCHI");
    for (const id of ["toolbar-open", "extract-to", "toolbar-test", "toolbar-find", "toolbar-more"]) {
      await expect(bar.$(`[data-testid="${id}"]`)).toBeDisplayed();
    }
    // The rarer actions are not on the bar; More holds them.
    for (const id of ["new-archive", "extract-all", "remove-entry", "toolbar-info", "toolbar-help"]) {
      await expect($(`[data-testid="${id}"]`)).not.toBeDisplayed();
    }
    await openMore();
    for (const id of ["new-archive", "extract-all", "remove-entry", "toolbar-info", "toolbar-help"]) {
      await expect($(`[data-testid="${id}"]`)).toBeDisplayed();
    }
    await browser.keys(["Escape"]);
    const names = await browser.execute(() =>
      Array.from(document.querySelectorAll(".window-controls button"), (b) => [
        b.getAttribute("aria-label"),
        b.getAttribute("title"),
        Math.round(b.getBoundingClientRect().width),
      ]),
    );
    expect(names).toEqual([
      ["Minimize", "Minimize", 46],
      ["Maximize", "Maximize", 46],
      ["Close", "Close", 46],
    ]);
  });

  it("shows the open archive's name as text, and the window's title says it too", async () => {
    await $('[data-testid="open-archive"]').click();
    await $('[data-testid="archive-meta"]').waitForExist({ timeout: 60_000 });
    await expect($('[data-testid="window-title"]')).toHaveText("browse.mochi");
    await browser.waitUntil(async () => (await browser.getTitle()) === "browse.mochi - MOCHI", {
      timeoutMsg: "the document title did not name the archive",
    });
    // Text only: nothing in the bar is markup that came from a name.
    expect(await browser.execute(() => document.querySelector('[data-testid="titlebar"] img, [data-testid="titlebar"] script'))).toBe(null);
  });

  it("Maximize fills the screen and the button becomes Restore; Restore gives the size back", async () => {
    const before = await rect();
    const button = await $('[data-testid="window-maximize"]');
    await button.click();
    await browser.waitUntil(async () => (await button.getAttribute("data-state")) === "maximized", {
      timeoutMsg: "the button did not hear that the window is maximized",
    });
    await expect(button).toHaveAttribute("aria-label", "Restore");
    // Under Xvfb (the Linux E2E job) there is no window manager: nothing gives a maximized
    // window the screen or confirms the state, so restore cannot be observed there. Where the
    // window really grew (Windows, a desktop), restore is checked too; W-09c covers Ubuntu.
    const big = await rect();
    expect(big.width).toBeGreaterThanOrEqual(before.width);
    if (big.width === before.width) return;
    await button.click();
    await browser.waitUntil(async () => (await button.getAttribute("data-state")) === "normal", {
      timeoutMsg: "the button did not hear that the window was restored",
    });
    await expect(button).toHaveAttribute("aria-label", "Maximize");
    const back = await rect();
    expect(Math.abs(back.width - before.width)).toBeLessThan(4);
  });

  it("Close closes only its own window, as the system's close button does", async () => {
    const main = await browser.getWindowHandle();
    // An archive is open here: Open puts the next one in a window of its own.
    const made = await inNewWindow(() => $('[data-testid="toolbar-open"]').click());
    await browser.waitUntil(
      async () => (await $('[data-testid="window-title"]').getText()) === "large.mochi",
      { timeout: 60_000, timeoutMsg: "the new window did not show large.mochi" },
    );
    // The window goes while the driver is still finishing the click: that is the point.
    await $('[data-testid="window-close"]')
      .click()
      .catch((e) => {
        if (!/no such window|target window already closed|window was closed/i.test(String(e))) throw e;
      });
    await browser.waitUntil(async () => !(await browser.getWindowHandles()).includes(made), {
      timeout: 30_000,
      timeoutMsg: "the window did not close",
    });
    await browser.switchToWindow(main);
    await expect($('[data-testid="window-title"]')).toHaveText("browse.mochi");
  });
});
