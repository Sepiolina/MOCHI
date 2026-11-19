// Shared steps for the specs: the window's panes and menus are closed until a person opens them
// (Info and the other rarer actions in the title bar's More menu, the path bar's versions), so a
// spec that reads or clicks something inside one opens it the way a person would. Not a spec
// itself (wdio runs *.e2e.js).
import { $, browser } from "@wdio/globals";

async function expand(testId) {
  const button = await $(`[data-testid="${testId}"]`);
  if ((await button.getAttribute("aria-expanded")) !== "true") await button.click();
}

async function collapse(testId) {
  const button = await $(`[data-testid="${testId}"]`);
  if ((await button.getAttribute("aria-expanded")) === "true") await button.click();
}

/**
 * The Details panel of a finished create, add, remove, or extract (`write`, `extract`; `quick` in
 * the compact window). A side panel over the window's right edge: close it with its own Close
 * button before clicking in the window, since it covers the result's buttons while it is open.
 */
export async function openResultDetails(prefix) {
  // Right after a create the window is still opening the new archive, and a click can land before
  // the button takes it (WebKitGTK): click again until the button says the panel is open.
  await browser.waitUntil(
    async () => {
      const button = await $(`[data-testid="${prefix}-details-toggle"]`);
      if ((await button.getAttribute("aria-expanded")) === "true") return true;
      await button.click();
      return (await button.getAttribute("aria-expanded")) === "true";
    },
    { timeoutMsg: `${prefix} Details button did not open its panel` },
  );
  // The panel renders on the next frame: wait until it shows before a spec reads its text.
  await $(`[data-testid="${prefix}-details"]`).waitForDisplayed({
    timeoutMsg: `${prefix} Details panel is open (aria-expanded) but not displayed`,
  });
}
export async function closeResultDetails(prefix) {
  const button = await $(`[data-testid="${prefix}-details-toggle"]`);
  if ((await button.getAttribute("aria-expanded")) !== "true") return;
  const close = await $(`[data-testid="${prefix}-details-close"]`);
  if (await close.isExisting()) await close.click();
  else await button.click();
  await $(`[data-testid="${prefix}-details"]`).waitForDisplayed({ reverse: true, timeoutMsg: `${prefix} Details did not close` });
}

/**
 * Make an action that sits in the More menu (New archive, Extract, Remove, Info, Help; release
 * 0.9.10 moved them off the title bar) clickable: open the menu if the item is in it and the menu
 * is closed. Anything else is left as it is.
 */
export async function revealAction(testId) {
  const inMenu = await browser.execute(
    (id) => document.querySelector(`[data-testid="${id}"]`)?.closest("#more-menu") != null,
    testId,
  );
  if (inMenu && !(await $(`[data-testid="${testId}"]`).isDisplayed())) await openMore();
}

/** Click an action wherever it is: on the title bar, or in the More menu. */
export async function clickAction(testId) {
  await revealAction(testId);
  await $(`[data-testid="${testId}"]`).click();
}

/** The Info panel (entry details; for a foreign archive, what the app says about the format). */
async function setInfo(open) {
  const item = await $('[data-testid="toolbar-info"]');
  if ((await item.getAttribute("aria-pressed")) === String(open)) return;
  await clickAction("toolbar-info");
}
export const openInfo = () => setInfo(true);
export const closeInfo = () => setInfo(false);

/** The version list under the archive's name. Close it again before clicking in the list. */
export const openVersions = () => expand("toggle-versions");
export const closeVersions = () => collapse("toggle-versions");

/** The More menu; choosing an item closes it. */
export const openMore = () => expand("toolbar-more");

/**
 * The tool window (Help, Passphrases, Test and repair, Settings): one tool at a time
 * over the list. It covers the window while it is open, so close it before clicking elsewhere.
 */
export async function openTool(menuTestId) {
  await openMore();
  await $(`[data-testid="${menuTestId}"]`).click();
  await $('[data-testid="tool-sheet"]').waitForDisplayed({ timeoutMsg: `${menuTestId} did not open its tool` });
}
export async function closeTool() {
  const sheet = await $('[data-testid="tool-sheet"]');
  if (!(await sheet.isDisplayed())) return;
  await $('[data-testid="tool-sheet-close"]').click();
  await sheet.waitForDisplayed({ reverse: true, timeoutMsg: "the tool window did not close" });
}

/** The Help panel (keyboard keys, what an archive is not, the user guide): More > Help. */
export const openHelp = () => openTool("toolbar-help");
export const closeHelp = closeTool;

/** Start a new archive: the start screen's button, or the toolbar's New archive. */
export async function clickNewArchive() {
  if (await $('[data-testid="start-screen"]').isExisting()) {
    await $('[data-testid="start-create"]').click();
    return;
  }
  await clickAction("new-archive");
}

/**
 * Run `action` (a click that opens an archive while this window already shows one) and talk to the
 * window it opens: one window per archive, so the archive appears in a new window, not in this one.
 * Resolves with the new window's handle; the earlier window's handle is `await browser.getWindowHandle()`
 * taken before the call.
 */
export async function inNewWindow(action) {
  const before = await browser.getWindowHandles();
  await action();
  let made;
  await browser.waitUntil(
    async () => {
      made = (await browser.getWindowHandles()).find((h) => !before.includes(h));
      return made !== undefined;
    },
    { timeout: 60_000, timeoutMsg: "no new window opened for the archive" },
  );
  await browser.switchToWindow(made);
  return made;
}
