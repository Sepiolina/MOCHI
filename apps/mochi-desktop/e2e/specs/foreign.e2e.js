// Plan D8 on the real app over real IPC: open a ZIP and a 7z read-only, browse
// them, read and check their entries, extract them, and meet the cases that
// matter: an archive whose names are encrypted asks for the password before
// listing, and a hostile archive writes nothing outside the chosen folder.
// WebDriver cannot drive a native dialog, so the `e2e` build takes the archive
// and the folder from MOCHI_E2E_PICK_FILE, in order (as the D3 spec does). The
// archives are the committed fixtures/foreign set; the helper binary
// (mochi-foreign-helper) sits next to the app binary because CI builds both.
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { $, $$, expect } from "@wdio/globals";
import { closeInfo, closeResultDetails, openInfo, openMore, openResultDetails, revealAction } from "./_ui.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const foreign = path.resolve(here, "../../../../fixtures/foreign");
const pickFile = process.env.MOCHI_E2E_PICK_FILE;
const queuePicks = (...paths) => writeFileSync(pickFile, paths.join("\n"));

const root = mkdtempSync(path.join(os.tmpdir(), "mochi-d8-"));
const dir = (name) => {
  const d = path.join(root, name);
  mkdirSync(d, { recursive: true });
  return d;
};

const BANNED = ["backed up", "backup", "safe", "preserved", "secure", "verified", "healthy"];
const assertNoBannedWording = async () => {
  // The open archive is drawn in the window's own area (foreign-stage), not in the tools drawer.
  const text = (await $('[data-testid="foreign-stage"]').getText()).toLowerCase();
  for (const word of BANNED) expect(text).not.toContain(word);
};

// The panels open below the fold of the 640 px test window.
async function press(testId) {
  // Extract sits in the title bar's More menu (release 0.9.10).
  await revealAction(testId);
  const el = await $(`[data-testid="${testId}"]`);
  await el.scrollIntoView({ block: "center" });
  await el.click();
}

const waitForStatus = (state) =>
  browser.waitUntil(
    async () => {
      const s = await $('[data-testid="foreign-status"]');
      return (await s.isExisting()) && (await s.getAttribute("data-state")) === state;
    },
    { timeout: 120_000, timeoutMsg: `the job did not reach ${state}` },
  );

const entryNames = () =>
  browser.execute(() =>
    Array.from(
      document.querySelectorAll('[data-testid="foreign-entry"] .name'),
      (e) => e.textContent,
    ),
  );

async function openForeign(file) {
  queuePicks(file);
  // As a person does: More > Other formats (the drawer's own Open button stays closed away).
  await openMore();
  await $('[data-testid="menu-foreign"]').click();
}

describe("foreign archives (ZIP and 7z, read-only)", () => {
  it("opens a ZIP labelled as a foreign format, with no MOCHI health vocabulary", async () => {
    await openForeign(path.join(foreign, "basic_infozip.zip"));
    await $('[data-testid="foreign-meta"]').waitForExist({ timeout: 60_000 });
    // The format is a badge in the path bar; what it means is its tooltip, not text on screen.
    await expect($('[data-testid="foreign-meta"]')).toHaveText(expect.stringContaining("ZIP"));
    await expect($('[data-testid="foreign-meta"] .format-badge')).toHaveAttribute(
      "title",
      "ZIP archive (foreign format)",
    );
    await openInfo();
    expect((await $("body").getText()).includes("UNSUPPORTED")).toBe(false);
    await closeInfo();
    await browser.waitUntil(async () => (await entryNames()).includes("a.txt"));
    expect(await entryNames()).toEqual(["empty", "sub", "a.txt"]);
    await assertNoBannedWording();
  });

  it("browses into a folder", async () => {
    for (const row of await $$('[data-testid="foreign-entry"]')) {
      if ((await row.$(".name").getText()) === "sub") {
        // WebDriver's pointer actions cannot reach a row below the fold of
        // the small test window: send the double click the list listens for.
        await browser.execute(
          (el) => el.dispatchEvent(new MouseEvent("dblclick", { bubbles: true })),
          row,
        );
        break;
      }
    }
    await browser.waitUntil(async () => (await entryNames()).includes("b.txt"));
    await expect($('[data-testid="foreign-crumbs"]')).toHaveText(expect.stringContaining("sub"));
  });

  it("finds names anywhere in the archive from the toolbar's search field", async () => {
    const field = await $('[data-testid="search-query"]');
    await field.setValue("b.txt");
    await browser.keys(["Enter"]);
    await browser.waitUntil(async () => (await $$('[data-testid="search-hit"]')).length === 1, {
      timeout: 30_000,
      timeoutMsg: "the foreign search found no hit",
    });
    await expect($('[data-testid="search-hit"]')).toHaveText(expect.stringContaining("b.txt"));
    await expect($('[data-testid="foreign-count"]')).toHaveText("1 match");
    await $('[data-testid="search-clear"]').click();
    await browser.waitUntil(async () => (await entryNames()).includes("b.txt"));
  });

  it("reads and checks every entry and reports the check that ran, not a verification", async () => {
    await press("foreign-test");
    await waitForStatus("completed");
    const status = await $('[data-testid="foreign-status"]').getText();
    expect(status).toContain("Read 3 entries of the ZIP archive: 0 problems.");
    expect(status).not.toMatch(/\bPASS\b/);
    // The checks that ran are in the Details panel.
    await openResultDetails("foreign");
    const details = await $('[data-testid="foreign-details"]').getText();
    expect(details).toContain("2 × CRC32 matched");
    expect(details).toContain("does not verify the archive the way MOCHI verifies its own");
    await closeResultDetails("foreign");
    await assertNoBannedWording();
  });

  it("extracts through the restore engine into the folder chosen in the native dialog", async () => {
    const out = dir("zip-all");
    queuePicks(out);
    await press("foreign-extract-all");
    await $('[data-testid="foreign-plan"]').waitForExist({ timeout: 60_000 });
    await expect($('[data-testid="foreign-destination"]')).toHaveText(out);
    expect(readdirSync(out)).toEqual([]); // nothing is written until the user confirms
    await press("foreign-extract-confirm");
    await waitForStatus("completed");
    expect(readFileSync(path.join(out, "a.txt"), "utf8")).toBe("hello\n");
    expect(readFileSync(path.join(out, "sub", "b.txt"), "utf8")).toBe("second file\n".repeat(50));
    expect(existsSync(path.join(out, "empty"))).toBe(true);
    await openResultDetails("foreign");
    const details = await $('[data-testid="foreign-details"]').getText();
    expect(details).toContain("CRC32 matched");
    expect(details).toContain("not a verification");
    await closeResultDetails("foreign");
    expect(readdirSync(out).some((n) => n.includes(".mochi-restore."))).toBe(false);
    await assertNoBannedWording();
  });

  it("opens a 7z, tests it, and extracts it byte for byte", async () => {
    await openForeign(path.join(foreign, "7z", "basic.7z"));
    await browser.waitUntil(
      async () => (await $('[data-testid="foreign-meta"]').getText()).includes("basic.7z"),
      { timeout: 60_000 },
    );
    await press("foreign-test");
    await waitForStatus("completed");
    expect(await $('[data-testid="foreign-status"]').getText()).toContain("Read 5 entries of the 7z archive: 0 problems.");
    const out = dir("sz-all");
    queuePicks(out);
    await press("foreign-extract-all");
    await $('[data-testid="foreign-plan"]').waitForExist({ timeout: 60_000 });
    await press("foreign-extract-confirm");
    await waitForStatus("completed");
    expect(readFileSync(path.join(out, "a.txt"), "utf8")).toBe("hello\n");
    expect(readFileSync(path.join(out, "sub", "b.txt"), "utf8")).toBe("second file\n".repeat(50));
    expect(readFileSync(path.join(out, "zero.txt"))).toHaveLength(0);
  });

  it("asks for the password before listing a 7z whose names are encrypted", async () => {
    await openForeign(path.join(foreign, "7z", "enc_names.7z"));
    await $('[data-testid="foreign-password"]').waitForExist({ timeout: 60_000 });
    await expect($('[data-testid="foreign-password-note"]')).toHaveText(
      expect.stringContaining("names are encrypted"),
    );
    // Nothing of this archive is listed yet: the one opened before is still shown.
    expect(await $('[data-testid="foreign-meta"]').getText()).not.toContain("enc_names.7z");
    await $('[data-testid="foreign-password-input"]').setValue("wrong");
    await press("foreign-password-submit");
    await browser.waitUntil(
      async () =>
        (await $('[data-testid="foreign-password-note"]').getText()).includes("did not open"),
      { timeout: 60_000 },
    );
    await $('[data-testid="foreign-password-input"]').setValue("SECRET");
    await press("foreign-password-submit");
    await $('[data-testid="foreign-password-badge"]').waitForExist({ timeout: 60_000 });
    await expect($('[data-testid="foreign-password-badge"]')).toHaveAttribute(
      "title",
      expect.stringContaining("opened with a password"),
    );
    await browser.waitUntil(async () => (await entryNames()).includes("a.txt"));
  });

  it("a hostile 7z writes nothing outside the folder and lists what it skipped", async () => {
    await openForeign(path.join(foreign, "7z", "traversal.7z"));
    await browser.waitUntil(
      async () => (await $('[data-testid="foreign-meta"]').getText()).includes("traversal.7z"),
      { timeout: 60_000 },
    );
    const parent = dir("hostile-parent");
    const out = path.join(parent, "out");
    mkdirSync(out);
    const before = readdirSync(parent);
    queuePicks(out);
    await press("foreign-extract-all");
    await $('[data-testid="foreign-plan"]').waitForExist({ timeout: 60_000 });
    await expect($('[data-testid="foreign-conflicts"]')).toBeExisting();
    await press("foreign-extract-confirm"); // "Extract the rest": what cannot be extracted is skipped
    await waitForStatus("completed");
    expect(readdirSync(parent)).toEqual(before);
    await openResultDetails("foreign");
    const skipped = await $('[data-testid="foreign-not-extracted"]').getText();
    expect(skipped).toContain("../evil.txt");
    expect(skipped).toContain("/abs/path.txt");
    // The harmless entries are there.
    expect(readFileSync(path.join(out, "ok", "fine.txt"), "utf8")).toBe("data of ok/fine.txt");
  });
});
