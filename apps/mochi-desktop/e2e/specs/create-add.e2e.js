// Plan D2 on the real app over real IPC: create an archive, add to it, browse
// it, remove an entry, and cancel a create. WebDriver cannot drive a native
// dialog, so the `e2e` build takes the paths of its dialogs (choose files,
// choose folder, save as) from MOCHI_E2E_PICK_FILE, in order; this spec
// writes that file before each flow. Dropping files from a file manager is
// an operating-system gesture WebDriver cannot make: it is covered by the
// Rust drop-event tests and the UI tests, and is on the release checklist.
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { $, $$, expect } from "@wdio/globals";
import {
  clickAction,
  clickNewArchive,
  closeHelp,
  closeResultDetails,
  closeVersions,
  openHelp,
  openResultDetails,
  openVersions,
} from "./_ui.js";

const pickFile = process.env.MOCHI_E2E_PICK_FILE;
const queuePicks = (...paths) => writeFileSync(pickFile, paths.join("\n"));

const root = mkdtempSync(path.join(os.tmpdir(), "mochi-d2-"));
const src = path.join(root, "src");
const out = path.join(root, "out");
mkdirSync(path.join(src, "photos"), { recursive: true });
mkdirSync(path.join(src, "bulk"), { recursive: true });
mkdirSync(out, { recursive: true });
writeFileSync(path.join(src, "photos", "a.txt"), "alpha");
writeFileSync(path.join(src, "photos", "b.txt"), "bravo");
writeFileSync(path.join(src, "extra.txt"), "extra content");
// 60 entries at MOCHI_E2E_PACE_MS each: seconds to click Cancel in.
for (let i = 0; i < 60; i++) writeFileSync(path.join(src, "bulk", `f${String(i).padStart(3, "0")}`), "x");

const mainArchive = path.join(out, "main.mochi");
const tarArchive = path.join(out, "tar.mochi");
const cancelled = path.join(out, "cancelled.mochi");

const entryNames = () =>
  browser.execute(() =>
    Array.from(document.querySelectorAll('[data-testid="entry"] .name'), (e) => e.textContent),
  );

const textOf = (testid) =>
  browser.execute(
    (id) => document.querySelector(`[data-testid="${id}"]`)?.textContent ?? "",
    testid,
  );

const waitForStatus = (text) =>
  browser.waitUntil(
    async () => {
      const s = await $('[data-testid="write-status"]');
      return (await s.isExisting()) && (await s.getText()).includes(text);
    },
    { timeout: 60_000, timeoutMsg: `no status containing ${JSON.stringify(text)}` },
  );

const BANNED = ["backed up", "backup", "safe", "preserved"];
const assertNoBannedWording = async () => {
  const text = (await $("body").getText()).toLowerCase();
  for (const word of BANNED) expect(text).not.toContain(word);
};

describe("create and add", () => {
  it("starts from the start screen with two plain actions; the note is in Help", async () => {
    await expect($('[data-testid="start-screen"]')).toBeExisting();
    for (const id of ["start-create", "open-archive"]) {
      await expect($(`[data-testid="${id}"]`)).toBeEnabled();
    }
    await openHelp();
    await expect($('[data-testid="start-note"]')).toHaveText(
      expect.stringContaining("not a second copy of them"),
    );
    await closeHelp();
    await assertNoBannedWording();
  });

  it("creates an archive next to the first item by default, never over an existing file", async () => {
    // A file is already called photos.mochi: the default name steps aside.
    writeFileSync(path.join(src, "photos.mochi"), "mine");
    queuePicks(path.join(src, "photos"));
    await $('[data-testid="start-create"]').click();
    await $('[data-testid="stage-folder"]').click();
    await browser.waitUntil(async () => (await $$('[data-testid="staged-item"]')).length === 1);
    await expect($('[data-testid="save-location"]')).toHaveText(path.join(src, "photos (2).mochi"));
    await expect($('[data-testid="save-location"]')).toHaveAttribute(
      "title",
      expect.stringContaining("An existing file is never replaced"),
    );
    // Cancel: nothing was created, and the start screen is back.
    await $('[data-testid="stage-clear"]').click();
    await $('[data-testid="write-close"]').click();
    await expect($('[data-testid="start-screen"]')).toBeExisting();
    expect(existsSync(path.join(src, "photos (2).mochi"))).toBe(false);
    expect(readFileSync(path.join(src, "photos.mochi"), "utf8")).toBe("mine");
  });

  it("creates an archive from a folder and says it was created, nothing more", async () => {
    queuePicks(path.join(src, "photos"), mainArchive);
    await $('[data-testid="start-create"]').click();
    await $('[data-testid="stage-folder"]').click();
    await browser.waitUntil(async () => (await $$('[data-testid="staged-item"]')).length === 1);
    await expect($('[data-testid="staged-item"]')).toHaveText(expect.stringContaining("photos"));
    // Choosing another place uses the save dialog, which starts at the default.
    await $('[data-testid="change-destination"]').click();
    // The panel re-renders when the place changes: poll the text, hold no element.
    await browser.waitUntil(async () => (await textOf("save-location")).includes("main.mochi"), {
      timeoutMsg: "the chosen place was not shown",
    });

    // The options are collapsed; they hold neither encryption nor a recovery record unless asked (D5).
    await expect($('[data-testid="create-options"]')).not.toHaveAttribute("open");
    await $('[data-testid="create-options"] summary').click();
    await expect($('[data-testid="preset-balanced"]')).toBeSelected();
    await expect($('[data-testid="tar-compatible"]')).not.toBeSelected();
    await expect($('[data-testid="encrypt"]')).not.toBeSelected();
    await expect($('[data-testid="recovery-none"]')).toBeSelected();

    await $('[data-testid="create-confirm"]').click();
    await waitForStatus("Created main.mochi");
    // One short line; the technical record is in the Details panel (owner request, 0.9.3).
    const status = await $('[data-testid="write-status"]').getText();
    expect(status).toContain("Created main.mochi · ");
    expect(status).not.toContain("LOCAL_COMMITTED");
    await openResultDetails("write");
    const details = await $('[data-testid="write-details"]').getText();
    expect(details).toContain("Committed on this computer");
    expect(details).toContain("commit 0 · LOCAL_COMMITTED");
    expect(details).toContain("one file on this computer");
    await assertNoBannedWording();
    await closeResultDetails("write");
    expect(existsSync(mainArchive)).toBe(true);

    // The new archive opened in the browser, at its head.
    await expect($('[data-testid="window-title"]')).toHaveText(expect.stringContaining("main.mochi"));
    await expect($('[data-testid="toggle-versions"]')).toHaveAttribute("title", "1 version");
    await browser.waitUntil(async () => (await entryNames()).includes("photos"));
  });

  it("adds a file as a new commit", async () => {
    queuePicks(path.join(src, "extra.txt"));
    await $('[data-testid="add-to-archive"]').click();
    await $('[data-testid="stage-files"]').click();
    await browser.waitUntil(async () => (await $$('[data-testid="staged-item"]')).length === 1);
    await $('[data-testid="add-confirm"]').click();
    await waitForStatus("Added to main.mochi");
    await expect($('[data-testid="toggle-versions"]')).toHaveAttribute("title", "2 versions");
    await openVersions();
    await expect($('[data-testid="commit-1"]')).toHaveText(expect.stringContaining("(latest)"));
    await closeVersions();
    await browser.waitUntil(async () => {
      const names = await entryNames();
      return names.includes("extra.txt") && names.includes("photos");
    });
    await assertNoBannedWording();
  });

  it("browses what was stored", async () => {
    const rows = await $$('[data-testid="entry"]');
    let photos;
    for (const row of rows) if ((await row.$(".name").getText()) === "photos") photos = row;
    await photos.click();
    await browser.keys("Enter");
    await browser.waitUntil(async () => {
      const names = await entryNames();
      return names.includes("a.txt") && names.includes("b.txt");
    });
    await expect($('[data-testid="toggle-versions"]')).toHaveText("v1");
    await expect($('[data-testid="crumbs"]')).toHaveText(expect.stringContaining("photos"));
    // Back to the root for the next step.
    await $('[data-testid="crumbs"] button').click();
    await browser.waitUntil(async () => (await entryNames()).includes("extra.txt"));
  });

  it("removes an entry from the current snapshot, which history keeps", async () => {
    const rows = await $$('[data-testid="entry"]');
    for (const row of rows) {
      if ((await row.$(".name").getText()) === "extra.txt") await row.click();
    }
    await expect($('[data-testid="details"]')).toBeExisting();
    await clickAction("remove-entry");
    await expect($('[data-testid="delete-note"]')).toHaveText(
      expect.stringContaining("stays in the archive's history"),
    );

    // Keep it: nothing happens.
    await $('[data-testid="delete-keep"]').click();
    await expect($('[data-testid="write-panel"]')).not.toBeExisting();
    await expect($('[data-testid="toggle-versions"]')).toHaveAttribute("title", "2 versions");

    await clickAction("remove-entry");
    await $('[data-testid="delete-confirm"]').click();
    await waitForStatus("Removed 1 item from this version");
    const status = await $('[data-testid="write-status"]').getText();
    expect(status).toContain("Earlier versions still have it");
    await openResultDetails("write");
    expect(await $('[data-testid="write-details"]').getText()).toContain("stays in the archive's history");
    await closeResultDetails("write");
    await expect($('[data-testid="toggle-versions"]')).toHaveAttribute("title", "3 versions");
    await browser.waitUntil(async () => {
      const names = await entryNames();
      return names.includes("photos") && !names.includes("extra.txt");
    });

    // The earlier snapshot still has it; and only the latest can be changed.
    await openVersions();
    await $('[data-testid="commit-1"]').click();
    await browser.waitUntil(async () => (await entryNames()).includes("extra.txt"));
    await assertNoBannedWording();
  });

  it("creates a TAR-compatible archive when asked, and shows it", async () => {
    queuePicks(path.join(src, "photos"), tarArchive);
    await clickNewArchive();
    await $('[data-testid="stage-folder"]').click();
    await browser.waitUntil(async () => (await $$('[data-testid="staged-item"]')).length === 1);
    await $('[data-testid="change-destination"]').click();
    await $('[data-testid="create-options"] summary').click();
    await expect($('[data-testid="tar-notes"]')).toHaveElementProperty(
      "textContent",
      expect.stringContaining("not the latest snapshot"),
    );
    await $('[data-testid="preset-fast"]').click();
    await $('[data-testid="tar-compatible"]').click();
    await $('[data-testid="create-confirm"]').click();
    await waitForStatus("Created tar.mochi");
    await expect($('[data-testid="window-title"]')).toHaveText(expect.stringContaining("tar.mochi"));
    await expect($('[data-testid="archive-technical"]')).toHaveElementProperty(
      "textContent",
      expect.stringContaining("TAR-compatible"),
    );
    // A standard zstd decoder reads the file's first frame: the TAR stream.
    expect(readFileSync(tarArchive).length).toBeGreaterThan(0);
  });

  it("cancels a create midway: no archive appears and the archive on screen is unchanged", async () => {
    queuePicks(path.join(src, "bulk"), cancelled);
    await clickNewArchive();
    await $('[data-testid="stage-folder"]').click();
    await browser.waitUntil(async () => (await $$('[data-testid="staged-item"]')).length === 1);
    await $('[data-testid="change-destination"]').click();
    await $('[data-testid="create-confirm"]').click();

    // Progress streams over the channel; cancel once some content was read.
    const progress = await $('[data-testid="write-progress"]');
    await progress.waitForExist({ timeout: 30_000 });
    await browser.waitUntil(async () => (await progress.getText()).includes("Reading files"));
    await $('[data-testid="write-cancel"]').click();
    await waitForStatus("Cancelled. No archive was created.");
    expect(existsSync(cancelled)).toBe(false);
    await expect($('[data-testid="window-title"]')).toHaveText(expect.stringContaining("tar.mochi"));
    await expect($('[data-testid="write-status"]')).toHaveAttribute("data-committed", "false");
  });

  // The earlier flows can leave the create panel open (a cancelled create keeps it for another
  // try): empty and close it before the next one, as a person would.
  const startFresh = async () => {
    if (await $('[data-testid="write-panel"]').isExisting()) {
      await $('[data-testid="stage-clear"]').click();
      await $('[data-testid="write-close"]').click();
      await $('[data-testid="write-panel"]').waitForExist({ reverse: true });
    }
    await clickNewArchive();
  };

  // Owner decision 2026-10-20 (plan O31): ZIP, tar, tar.gz and tar.zst can be created too.
  const zipPath = path.join(out, "photos.zip");
  const header = (file, ...magic) => readFileSync(file).subarray(0, magic.length).equals(Buffer.from(magic));

  it("creates a ZIP when asked: a plain file, no MOCHI options, and the result says what was written", async () => {
    // The save dialog's answer has no extension: the format's is added.
    queuePicks(path.join(src, "photos"), path.join(out, "photos"));
    await startFresh();
    await $('[data-testid="stage-folder"]').click();
    await browser.waitUntil(async () => (await $$('[data-testid="staged-item"]')).length === 1);
    await expect($('[data-testid="create-format"]')).toHaveValue("mochi");
    await $('[data-testid="create-format"]').selectByAttribute("value", "zip");
    await expect($('[data-testid="plain-note"]')).toExist();
    await $('[data-testid="create-options"] summary').click();
    for (const id of ["encrypt", "tar-compatible", "recovery-none"]) {
      await expect($(`[data-testid="${id}"]`)).not.toExist();
    }
    await expect($('[data-testid="preset-balanced"]')).toBeSelected();
    await $('[data-testid="change-destination"]').click();
    await browser.waitUntil(async () => (await textOf("save-location")).endsWith("photos.zip"), {
      timeoutMsg: "the name did not take the ZIP extension",
    });
    await $('[data-testid="create-confirm"]').click();
    await waitForStatus("Wrote 2 files");
    await expect($('[data-testid="write-status"]')).toHaveAttribute("data-committed", "false");
    expect(existsSync(zipPath)).toBe(true);
    expect(header(zipPath, 0x50, 0x4b)).toBe(true);
    expect(existsSync(path.join(out, "photos.mochi"))).toBe(false);
    // No temporary file is left beside it.
    expect(readdirSync(out).filter((n) => n.endsWith(".part"))).toEqual([]);
    const status = (await $('[data-testid="write-status"]').getText()).toLowerCase();
    for (const word of ["committed", "backed up", "safe", "preserved"]) expect(status).not.toContain(word);
    await openResultDetails("write");
    await expect($('[data-testid="write-details"]')).toHaveText(expect.stringContaining("ZIP (.zip)"));
    await expect($('[data-testid="write-details"]')).not.toHaveText(expect.stringContaining("LOCAL_COMMITTED"));
    await closeResultDetails("write");
    // "Show in folder" asks for the folder the file went to.
    await $('[data-testid="write-show-folder"]').click();
    await browser.waitUntil(() => readFileSync(process.env.MOCHI_E2E_FOLDER_LOG, "utf8").split("\n").includes(out), {
      timeoutMsg: "the folder was not shown",
    });
    // The archive on screen is the one from before: a ZIP is not opened by creating it.
    await expect($('[data-testid="window-title"]')).toHaveText(expect.stringContaining("tar.mochi"));
    await assertNoBannedWording();
  });

  it("never replaces an existing file, in any plain format", async () => {
    const before = readFileSync(zipPath);
    queuePicks(path.join(src, "photos"), path.join(out, "photos"));
    await startFresh();
    await $('[data-testid="stage-folder"]').click();
    await browser.waitUntil(async () => (await $$('[data-testid="staged-item"]')).length === 1);
    await $('[data-testid="create-format"]').selectByAttribute("value", "zip");
    await $('[data-testid="change-destination"]').click();
    await browser.waitUntil(async () => (await textOf("save-location")).endsWith("photos.zip"));
    await $('[data-testid="create-confirm"]').click();
    await waitForStatus("Failed (DESTINATION_EXISTS)");
    // The second line is for screen readers and the tooltip, not the one-line bar.
    await expect($('[data-testid="write-status"]')).toHaveElementProperty(
      "textContent",
      expect.stringContaining("Nothing was written under that name."),
    );
    expect(readFileSync(zipPath).equals(before)).toBe(true);
    expect(readdirSync(out).filter((n) => n.endsWith(".part"))).toEqual([]);
    // Back to a clean state for the next flow.
    await $('[data-testid="stage-clear"]').click();
    await $('[data-testid="write-close"]').click();
  });

  it("writes tar, tar.gz and tar.zst files with their own extensions", async () => {
    for (const [format, ext, magic] of [
      ["tar", "tar", null],
      ["tar_gz", "tar.gz", [0x1f, 0x8b]],
      ["tar_zst", "tar.zst", [0x28, 0xb5, 0x2f, 0xfd]],
    ]) {
      queuePicks(path.join(src, "photos"), path.join(out, `plain-${format}`));
      await startFresh();
      await $('[data-testid="stage-folder"]').click();
      await browser.waitUntil(async () => (await $$('[data-testid="staged-item"]')).length === 1);
      await $('[data-testid="create-format"]').selectByAttribute("value", format);
      await $('[data-testid="create-options"] summary').click();
      // Plain tar is not compressed, so it has no level to choose.
      if (format === "tar") await expect($('[data-testid="no-level"]')).toExist();
      else await expect($('[data-testid="preset-fast"]')).toExist();
      await $('[data-testid="change-destination"]').click();
      await browser.waitUntil(async () => (await textOf("save-location")).endsWith(`plain-${format}.${ext}`));
      await $('[data-testid="create-confirm"]').click();
      await waitForStatus(`Wrote 2 files · 1 folder`);
      const file = path.join(out, `plain-${format}.${ext}`);
      expect(existsSync(file)).toBe(true);
      if (magic) expect(header(file, ...magic)).toBe(true);
      else expect(readFileSync(file).subarray(257, 262).toString()).toBe("ustar");
    }
  });

  it("cancels a ZIP midway: no file and no temporary file appear", async () => {
    const cancelledZip = path.join(out, "cancelled-zip");
    queuePicks(path.join(src, "bulk"), cancelledZip);
    await startFresh();
    await $('[data-testid="stage-folder"]').click();
    await browser.waitUntil(async () => (await $$('[data-testid="staged-item"]')).length === 1);
    await $('[data-testid="create-format"]').selectByAttribute("value", "zip");
    await $('[data-testid="change-destination"]').click();
    await $('[data-testid="create-confirm"]').click();
    const progress = await $('[data-testid="write-progress"]');
    await progress.waitForExist({ timeout: 30_000 });
    await browser.waitUntil(async () => (await progress.getText()).includes("Looking through the files"));
    await $('[data-testid="write-cancel"]').click();
    await waitForStatus("Cancelled. No archive was created.");
    expect(existsSync(`${cancelledZip}.zip`)).toBe(false);
    expect(readdirSync(out).filter((n) => n.startsWith("cancelled-zip") || n.endsWith(".part"))).toEqual([]);
    await expect($('[data-testid="window-title"]')).toHaveText(expect.stringContaining("tar.mochi"));
  });
});
