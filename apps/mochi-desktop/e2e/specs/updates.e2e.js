// T13: the Settings control exists and does nothing on its own. The start-up
// check is opt-in and off by default, so a fresh start makes no request and
// shows no update dialog or message; only the button asks.
import { $, expect } from "@wdio/globals";

describe("updates (Settings)", () => {
  it("has a Check for updates button and an unchecked start-up option", async () => {
    const check = await $('[data-testid="update-check"]');
    // The first wait of a fresh session covers the app's cold start, which on
    // a Windows runner has taken longer than the 15 s default.
    await check.waitForExist({ timeout: 60_000 });
    await expect(check).toBeEnabled();
    await expect(check).toHaveText("Check for updates");
    await expect($('[data-testid="update-startup"]')).not.toBeSelected();
  });

  it("does nothing until it is asked", async () => {
    const status = await $('[data-testid="update-status"]');
    await browser.pause(1500);
    await expect(status).toHaveAttribute("data-state", "idle");
    await expect(status).toHaveText("");
    await expect($('[data-testid="update-dialog"]')).not.toBeExisting();
    await expect($('[data-testid="update-failed"]')).not.toBeExisting();
  });
});
