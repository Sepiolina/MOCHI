// Audit F8: Help, User guide. The webview names no path; Rust resolves the bundled guide and
// calls the OS open. The `e2e` build records the path it was about to open instead of
// starting a browser, so this spec shows which file the Help action opens and reads that
// file: the page the installer bundles, with its banner saying it is for the command line.
import { readFileSync } from "node:fs";
import path from "node:path";
import { $, expect } from "@wdio/globals";
import { openHelp } from "./_ui.js";

describe("help (User guide)", () => {
  it("says on the button that the guide is written for the command line", async () => {
    const button = await $('[data-testid="help-user-guide"]');
    await button.waitForExist({ timeout: 60_000 });
    // The Help panel is closed until the toolbar's ? opens it.
    await openHelp();
    await expect(button).toHaveText("User guide (command line)");
  });

  it("opens the bundled guide's first page, which says it is for the command line", async () => {
    await $('[data-testid="help-user-guide"]').click();
    const status = await $('[data-testid="help-status"]');
    await browser.waitUntil(async () => (await status.getAttribute("data-state")) === "requested", {
      timeout: 15_000,
      timeoutMsg: "the Help action did not report that it opened the guide",
    });
    const opened = readFileSync(process.env.MOCHI_E2E_OPEN_LOG, "utf8")
      .split(/\r?\n/)
      .filter(Boolean);
    expect(opened).toHaveLength(1);
    expect(path.resolve(opened[0])).toBe(path.resolve(process.env.MOCHI_E2E_GUIDE_DIR, "index.html"));
    const page = readFileSync(opened[0], "utf8");
    expect(page).toContain("This guide is written for the mochi command line.");
    expect(page).not.toMatch(/<script|https?:\/\//);
  });
});
