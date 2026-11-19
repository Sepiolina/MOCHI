// Plan D7 crash-report policy, on the real app over real IPC: the crash log is
// off until the user turns it on, the switches are saved by Rust and survive a
// reload, "include the panic message" needs the log on, and turning the log off
// puts everything back. This also proves that the capability file grants the five
// crash-log commands (a command that is not granted is refused at run time).
// The log itself is written by the panic hook, which the Rust tests cover; the
// app has no way to panic on request, and none is added for a test.
import { $, expect } from "@wdio/globals";
import { openTool } from "./_ui.js";

const enabled = () => $('[data-testid="crash-enabled"]');
const details = () => $('[data-testid="crash-details"]');

describe("crash reports (opt-in, local)", () => {
  it("starts off, says nothing is sent, and offers nothing to show", async () => {
    await $('[data-testid="crash-state"]').waitForExist();
    expect(await enabled().isSelected()).toBe(false);
    expect(await details().isSelected()).toBe(false);
    await expect(details()).toBeDisabled();
    await expect($('[data-testid="crash-policy"]')).toHaveText(expect.stringContaining("never sends crash reports"));
    await expect($('[data-testid="crash-state"]')).toHaveText(expect.stringContaining("0 records in the log"));
    for (const id of ["crash-show", "crash-export", "crash-clear"]) {
      await expect($(`[data-testid="${id}"]`)).toBeDisabled();
    }
  });

  it("turning it on is saved by Rust and survives a reload; the message option needs it", async () => {
    await enabled().click();
    await browser.waitUntil(async () => (await $('[data-testid="crash-status"]').getText()).includes("is on"), {
      timeoutMsg: "no confirmation that the log is on",
    });
    await expect(details()).toBeEnabled();
    expect(await details().isSelected()).toBe(false);
    await details().click();
    await browser.waitUntil(async () => details().isSelected());

    await browser.refresh();
    // A reload starts with the tool window closed: open Settings again.
    await openTool("start-settings");
    await $('[data-testid="settings-tab-privacy"]').click();
    await $('[data-testid="crash-state"]').waitForDisplayed();
    await browser.waitUntil(async () => enabled().isSelected(), { timeoutMsg: "the choice was not kept" });
    expect(await details().isSelected()).toBe(true);
  });

  it("turning it off also turns the message option off", async () => {
    await enabled().click();
    await browser.waitUntil(async () => (await $('[data-testid="crash-status"]').getText()).includes("is off"));
    expect(await enabled().isSelected()).toBe(false);
    expect(await details().isSelected()).toBe(false);
    await expect(details()).toBeDisabled();
  });
});
