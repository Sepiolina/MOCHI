// Plan D6 exit, on the real app: a second launch with an archive path routes to
// the running instance instead of starting another one. The app wdio started is
// the running instance; this spec starts the binary again, the way a file
// manager or Explorer's "Open with MOCHI" / "Extract here" verbs do, and checks
// that the second process hands its arguments over and exits, and that the
// result appears where the one-window-per-archive rule puts it: in the empty main
// window, else in a new window of its own (an archive already open is not opened
// twice: its window is brought forward). The path travels process to process and into
// Rust only; the window is told "an archive was opened" and never a path.
//
// On Linux the single-instance plugin uses the session D-Bus: CI runs this
// under `dbus-run-session` (a desktop session has one already).
import { spawn } from "node:child_process";
import { chmodSync, copyFileSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { $, expect } from "@wdio/globals";
import { closeVersions, openVersions } from "./_ui.js";

// Set by wdio.conf.js to the binary it launches.
const application = process.env.MOCHI_DESKTOP_BINARY;
const foreignFixtures = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../../fixtures/foreign");
const fixture = path.join(process.env.MOCHI_E2E_FIXTURES, "browse.mochi");

/** Start the app again with `args`; resolves with how the second process ended. */
function secondLaunch(args) {
  return new Promise((resolve, reject) => {
    const child = spawn(application, args, { stdio: "ignore", cwd: os.tmpdir() });
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error(`the second launch did not hand over and exit within 20 s: ${args.join(" ")}`));
    }, 20_000);
    child.on("error", reject);
    child.on("exit", (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal });
    });
  });
}

/** Extracted folders keep the archive's modes (some are read-only): make them removable. */
function makeWritable(dir) {
  for (const name of readdirSync(dir)) {
    const p = path.join(dir, name);
    if (lstatSync(p).isDirectory()) {
      chmodSync(p, 0o755);
      makeWritable(p);
    }
  }
  chmodSync(dir, 0o755);
}

/** Make the window that shows `testid` the one WebDriver talks to (the shell's quick verbs open a compact second window). */
async function switchToWindowWith(testid, timeout = 60_000, text = "") {
  // `text`: the element must also contain it. Several windows can show the
  // same test id (an earlier test's window keeps its own note), and the order
  // of window handles differs between WebKitWebDriver and WebView2.
  await browser.waitUntil(
    async () => {
      for (const handle of await browser.getWindowHandles()) {
        await browser.switchToWindow(handle);
        const el = $(`[data-testid="${testid}"]`);
        if ((await el.isExisting()) && (await el.getText()).includes(text)) return true;
      }
      return false;
    },
    { timeout, timeoutMsg: `no window shows ${testid}` },
  );
}

/**
 * Close every compact window with its Close button (each quick job has a window of its own since
 * 0.9.19, owner report 2026-11-12), then talk to the main window again. The button makes Rust destroy
 * the window, which can happen before WebDriver answers the click; WebKitWebDriver then reports
 * "no such window" for a click that worked. Only that error is ignored: the window count below is
 * what the test checks.
 */
async function closeQuickWindow(mainWindow) {
  for (const handle of await browser.getWindowHandles()) {
    if (archiveWindows.has(handle)) continue;
    try {
      await browser.switchToWindow(handle);
      if (await $('[data-testid="quick-close"]').isExisting()) await $('[data-testid="quick-close"]').click();
    } catch (err) {
      if (!/no such window/i.test(String(err?.message ?? err))) throw err;
    }
  }
  await browser.waitUntil(async () => (await browser.getWindowHandles()).length === archiveWindows.size, {
    timeoutMsg: "the compact window did not close",
  });
  await browser.switchToWindow(mainWindow);
}

/**
 * The windows this spec has opened for archives (the main window and every `win-N` it caused). They stay open
 * for the rest of the spec: WebDriver's own "close window" closes the page but not the operating-system window,
 * so Rust never hears the window closed (a person's click on the title bar's X does), and a test that closed
 * one this way would leave Rust's state for that window behind.
 */
const archiveWindows = new Set();

/**
 * Start the app again with `args` and talk to the window that was made for it: the main window shows an
 * archive, so an archive opened by the shell gets a window of its own (one window per archive).
 */
async function launchIntoNewWindow(args, mainWindow) {
  const before = [...archiveWindows];
  const ended = await secondLaunch(args);
  expect(ended.code).toBe(0);
  let made;
  await browser.waitUntil(
    async () => {
      // A compact window may be open too: only the one that shows an archive counts.
      for (const handle of await browser.getWindowHandles()) {
        if (before.includes(handle)) continue;
        await browser.switchToWindow(handle);
        if (await $('[data-testid="archive-meta"], [data-testid="foreign-meta"], [data-testid="foreign-password"], [data-testid="unlock-panel"]').isExisting()) {
          made = handle;
          return true;
        }
      }
      return false;
    },
    { timeoutMsg: `no new window opened for ${args.join(" ")}` },
  );
  expect(made).not.toBe(mainWindow);
  archiveWindows.add(made);
  await browser.switchToWindow(made);
  return made;
}

describe("a second launch", () => {
  let work;
  let mainWindow;
  before(async () => {
    work = mkdtempSync(path.join(os.tmpdir(), "mochi-e2e-launch-"));
    mainWindow = await browser.getWindowHandle();
    archiveWindows.add(mainWindow);
  });
  // Each test starts by talking to the main window.
  afterEach(async () => {
    await browser.switchToWindow(mainWindow);
  });
  after(() => {
    makeWritable(work);
    rmSync(work, { recursive: true, force: true });
  });

  it("opens the archive in the running window instead of starting a second app", async () => {
    await $('[data-testid="start-screen"]').waitForExist();
    // No dialog click: the path comes from the second process's command line.
    const ended = await secondLaunch(["--open", "--", fixture]);
    expect(ended.code).toBe(0);
    await $('[data-testid="archive-meta"]').waitForExist({ timeout: 30_000 });
    await expect($('[data-testid="window-title"]')).toHaveText(expect.stringContaining("browse.mochi"));
    await openVersions();
    await expect($('[data-testid="commit-3"]')).toHaveText(expect.stringContaining("(latest)"));
    await closeVersions();
  });

  it("takes a bare path, which is what a file manager passes, and opens it in a window of its own", async () => {
    const other = path.join(work, "again.mochi");
    copyFileSync(fixture, other);
    // The main window shows browse.mochi: one window per archive.
    const second = await launchIntoNewWindow([other], mainWindow);
    await $('[data-testid="archive-meta"]').waitForExist({ timeout: 30_000 });
    await expect($('[data-testid="window-title"]')).toHaveText(expect.stringContaining("again.mochi"));
    // The first window still shows the first archive.
    await browser.switchToWindow(mainWindow);
    await expect($('[data-testid="window-title"]')).toHaveText(expect.stringContaining("browse.mochi"));
    expect(await browser.getWindowHandles()).toHaveLength(archiveWindows.size);

    // The same archive again, by the same path or another one (the fixture is open in the main window):
    // nothing new opens, the window that has it is brought forward.
    expect((await secondLaunch([other])).code).toBe(0);
    expect((await secondLaunch(["--open", "--", fixture])).code).toBe(0);
    await browser.pause(1500);
    expect(await browser.getWindowHandles()).toHaveLength(archiveWindows.size);
    await browser.switchToWindow(second);
    await expect($('[data-testid="window-title"]')).toHaveText(expect.stringContaining("again.mochi"));
    await browser.switchToWindow(mainWindow);
    await expect($('[data-testid="window-title"]')).toHaveText(expect.stringContaining("browse.mochi"));
  });

  // Owner decision 2026-10-16: the quick verbs work at once in a small second window.
  it("'Extract to a new folder' extracts at once in a compact window and leaves the main window alone", async () => {
    const archive = path.join(work, "photos.mochi");
    copyFileSync(fixture, archive);
    const ended = await secondLaunch(["--extract-to-folder", "--", archive]);
    expect(ended.code).toBe(0);
    await switchToWindowWith("quick-result");
    await expect($('[data-testid="quick-title"]')).toHaveText(expect.stringContaining("photos.mochi"));
    await expect($('[data-testid="quick-result"]')).toHaveText(expect.stringContaining("Extracted"));
    // Written at once, into a new folder named for the archive.
    expect(readdirSync(path.join(work, "photos")).length).toBeGreaterThan(0);
    // Not a verification, and no success claim beyond what happened.
    const text = (await $('[data-testid="quick-result"]').getText()).toLowerCase();
    for (const word of ["backed up", "safe", "preserved"]) expect(text).not.toContain(word);
    // The main window did not open the extract panel.
    await browser.switchToWindow(mainWindow);
    await expect($('[data-testid="extract-panel"]')).not.toExist();
    // A second run never reuses the folder.
    const again = await secondLaunch(["--extract-to-folder", "--", archive]);
    expect(again.code).toBe(0);
    await browser.waitUntil(() => readdirSync(work).includes("photos (2)"), {
      timeoutMsg: "no second, new folder was made",
    });
    // The second run has a compact window of its own, beside the first one's.
    await switchToWindowWith("quick-result", 60_000, "photos (2)");
    expect((await browser.getWindowHandles()).length).toBe(archiveWindows.size + 2);
    await closeQuickWindow(mainWindow);
  });

  // Owner request 2026-11-09: the folder opens in the file manager when done (unless turned
  // off), and the folder "Extract to new folder" made is renamed or moved from the result.
  it("a quick extraction opens its folder, and the new folder is renamed and moved without replacing anything", async () => {
    const dir = path.join(work, "after");
    mkdirSync(dir);
    const elsewhere = path.join(work, "elsewhere");
    mkdirSync(elsewhere);
    mkdirSync(path.join(dir, "taken"));
    const archive = path.join(dir, "docs.mochi");
    copyFileSync(fixture, archive);
    writeFileSync(process.env.MOCHI_E2E_OPEN_LOG, "");
    const ended = await secondLaunch(["--extract-to-folder", "--", archive]);
    expect(ended.code).toBe(0);
    await switchToWindowWith("quick-result");
    await browser.waitUntil(
      () => readFileSync(process.env.MOCHI_E2E_OPEN_LOG, "utf8").split("\n").includes(path.join(dir, "docs")),
      { timeoutMsg: "the extracted folder was not opened" },
    );
    await expect($('[data-testid="quick-opened"]')).toExist();
    await expect($('[data-testid="quick-open-when-done"]')).toBeSelected();
    // A name in the way is refused; nothing is replaced.
    await $('[data-testid="quick-folder-name"]').setValue("taken");
    await $('[data-testid="quick-rename-submit"]').click();
    await expect($('[data-testid="quick-problem"]')).toHaveText(expect.stringContaining("DESTINATION_EXISTS"));
    expect(readdirSync(path.join(dir, "taken"))).toEqual([]);
    await $('[data-testid="quick-folder-name"]').setValue("Report");
    await $('[data-testid="quick-rename-submit"]').click();
    await expect($('[data-testid="quick-moved"]')).toHaveText(expect.stringContaining(path.join(dir, "Report")));
    expect(readdirSync(dir).sort()).toEqual(["Report", "docs.mochi", "taken"]);
    writeFileSync(process.env.MOCHI_E2E_PICK_FILE, elsewhere);
    await $('[data-testid="quick-move"]').click();
    await expect($('[data-testid="quick-moved"]')).toHaveText(expect.stringContaining(path.join(elsewhere, "Report")));
    expect(readdirSync(elsewhere)).toEqual(["Report"]);
    expect(readdirSync(path.join(elsewhere, "Report")).length).toBeGreaterThan(0);
    // Turned off, the next extraction opens nothing.
    await $('[data-testid="quick-open-when-done"]').click();
    await expect($('[data-testid="quick-open-when-done"]')).not.toBeSelected();
    await closeQuickWindow(mainWindow);
    writeFileSync(process.env.MOCHI_E2E_OPEN_LOG, "");
    const again = await secondLaunch(["--extract-to-folder", "--", archive]);
    expect(again.code).toBe(0);
    await switchToWindowWith("quick-result");
    await browser.waitUntil(() => readdirSync(dir).includes("docs"), { timeoutMsg: "the second extraction did not run" });
    await expect($('[data-testid="quick-opened"]')).not.toExist();
    expect(readFileSync(process.env.MOCHI_E2E_OPEN_LOG, "utf8")).toBe("");
    // Back on for the specs that follow.
    await $('[data-testid="quick-open-when-done"]').click();
    await closeQuickWindow(mainWindow);
  });

  it("'Extract here' replaces nothing: a name in the way writes nothing and opens the plan in a new window", async () => {
    const dir = path.join(work, "collide");
    mkdirSync(dir);
    const archive = path.join(dir, "again.mochi");
    copyFileSync(fixture, archive);
    // A file where the archive has the folder "docs".
    writeFileSync(path.join(dir, "docs"), "mine");
    const ended = await secondLaunch(["--extract-here", "--", archive]);
    expect(ended.code).toBe(0);
    await switchToWindowWith("quick-result");
    await expect($('[data-testid="quick-result"]')).toHaveText(expect.stringContaining("Nothing was written"));
    await expect($('[data-testid="quick-show-folder"]')).not.toExist();
    // The plan needs a window that shows the archive: the main window has another one open, so a new one is made.
    await switchToWindowWith("extract-panel");
    expect(await browser.getWindowHandle()).not.toBe(mainWindow);
    archiveWindows.add(await browser.getWindowHandle());
    // The panel asks (a file is where the archive has a folder): the folder is named in the
    // question, and its whole path is the question's tooltip.
    await expect($('[data-testid="extract-choices-title"]')).toHaveAttribute("title", dir);
    expect(readdirSync(dir).sort()).toEqual(["again.mochi", "docs"]);
    expect(readFileSync(path.join(dir, "docs"), "utf8")).toBe("mine");
    await $('[data-testid="extract-close"]').click();
    await switchToWindowWith("quick-result");
    await closeQuickWindow(mainWindow);
  });

  it("'Add to archive' collects one selection's launches into one archive and says it is a local commit", async () => {
    const dir = path.join(work, "trip");
    mkdirSync(dir);
    const files = ["a.txt", "b.txt", "c.txt"].map((n) => {
      const f = path.join(dir, n);
      writeFileSync(f, n);
      return f;
    });
    // One process per item, started together, as Explorer does.
    const ended = await Promise.all(files.map((f) => secondLaunch(["--add-now", "--", f])));
    expect(ended.map((e) => e.code)).toEqual([0, 0, 0]);
    await switchToWindowWith("quick-result");
    await expect($('[data-testid="quick-result"]')).toHaveText(expect.stringContaining("Archive created: trip.mochi"));
    // The technical status is behind the small Details disclosure.
    await expect($('[data-testid="quick-result"]')).not.toHaveText(expect.stringContaining("LOCAL_COMMITTED"));
    await $('[data-testid="quick-details-toggle"]').click();
    await expect($('[data-testid="quick-details"]')).toHaveText(expect.stringContaining("LOCAL_COMMITTED"));
    const text = (await $('[data-testid="quick-result"]').getText()).toLowerCase();
    for (const word of ["backed up", "safe", "preserved"]) expect(text).not.toContain(word);
    expect(readdirSync(dir).filter((n) => n.endsWith(".mochi"))).toEqual(["trip.mochi"]);
    await closeQuickWindow(mainWindow);
    // Again: a new name, the first archive stays.
    const lengthBefore = readFileSync(path.join(dir, "trip.mochi")).length;
    const second = await secondLaunch(["--add-now", "--", files[0]]);
    expect(second.code).toBe(0);
    await browser.waitUntil(() => readdirSync(dir).includes("a.mochi"), { timeoutMsg: "no archive named for the file" });
    expect(readFileSync(path.join(dir, "trip.mochi")).length).toBe(lengthBefore);
    await switchToWindowWith("quick-close");
    await closeQuickWindow(mainWindow);
  });

  // Owner decision 2026-10-20: "Add to archive..." is a small WinRAR-style form in the compact
  // window, not the main window's add panel. Nothing is written until OK.
  const bytesOf = (file) => readFileSync(file);
  const startsWith = (file, ...magic) => bytesOf(file).subarray(0, magic.length).equals(Buffer.from(magic));

  it("'Add to archive...' opens a small form, writes nothing until OK, and OK makes the chosen format", async () => {
    const dir = path.join(work, "panel");
    mkdirSync(dir);
    const file = path.join(dir, "note.txt");
    writeFileSync(file, "x");
    const ended = await secondLaunch(["--add", "--", file]);
    expect(ended.code).toBe(0);
    await switchToWindowWith("quick-form");
    await expect($('[data-testid="quick-title"]')).toHaveText(expect.stringContaining("Add note.txt to an archive"));
    // The suggested name follows the Add now rule; the folder is shown as text.
    await expect($('[data-testid="quick-name"]')).toHaveValue("note.mochi");
    await expect($('[data-testid="quick-folder"]')).toHaveText(expect.stringContaining(dir));
    await expect($('[data-testid="quick-format"]')).toHaveValue("mochi");
    await expect($('[data-testid="quick-mochi-options"]')).toExist();
    expect(readdirSync(dir)).toEqual(["note.txt"]);
    // The main window did not open an add panel: the form is the whole flow.
    await browser.switchToWindow(mainWindow);
    await expect($('[data-testid="staged-item"]')).not.toExist();
    await expect($('[data-testid="write-panel"]')).not.toExist();
    await switchToWindowWith("quick-form");
    // Another format: the extension follows, and the .mochi-only options are gone.
    await $('[data-testid="quick-format"]').selectByAttribute("value", "zip");
    await expect($('[data-testid="quick-name"]')).toHaveValue("note.zip");
    await expect($('[data-testid="quick-mochi-options"]')).not.toExist();
    expect(readdirSync(dir)).toEqual(["note.txt"]);
    await $('[data-testid="quick-ok"]').click();
    await switchToWindowWith("quick-result");
    await expect($('[data-testid="quick-result"]')).toHaveText(expect.stringContaining("Wrote 1 file"));
    expect(readdirSync(dir).sort()).toEqual(["note.txt", "note.zip"]);
    expect(startsWith(path.join(dir, "note.zip"), 0x50, 0x4b)).toBe(true);
    const text = (await $('[data-testid="quick-result"]').getText()).toLowerCase();
    for (const word of ["backed up", "safe", "preserved", "committed"]) expect(text).not.toContain(word);
    // "Show in folder" opens the folder the archive went to.
    writeFileSync(process.env.MOCHI_E2E_OPEN_LOG, "");
    await $('[data-testid="quick-show-folder"]').click();
    await browser.waitUntil(() => readFileSync(process.env.MOCHI_E2E_OPEN_LOG, "utf8").split("\n").includes(dir), {
      timeoutMsg: "the folder was not shown",
    });
    await closeQuickWindow(mainWindow);
  });

  it("'Add to archive...' on one selection (one process per item) makes one form and one .tar.gz", async () => {
    const dir = path.join(work, "form-trip");
    mkdirSync(dir);
    const files = ["a.txt", "b.txt", "c.txt"].map((n) => {
      const f = path.join(dir, n);
      writeFileSync(f, n);
      return f;
    });
    const ended = await Promise.all(files.map((f) => secondLaunch(["--add", "--", f])));
    expect(ended.map((e) => e.code)).toEqual([0, 0, 0]);
    await switchToWindowWith("quick-form");
    await expect($('[data-testid="quick-title"]')).toHaveText(expect.stringContaining("and 2 more"));
    expect(await browser.execute(() => document.querySelectorAll('[data-testid="quick-form"]').length)).toBe(1);
    await expect($('[data-testid="quick-name"]')).toHaveValue("form-trip.mochi");
    await $('[data-testid="quick-format"]').selectByAttribute("value", "tar_gz");
    await expect($('[data-testid="quick-name"]')).toHaveValue("form-trip.tar.gz");
    await $('[data-testid="quick-level"]').selectByAttribute("value", "fast");
    await $('[data-testid="quick-ok"]').click();
    await switchToWindowWith("quick-result");
    await expect($('[data-testid="quick-result"]')).toHaveText(expect.stringContaining("Wrote 3 files"));
    expect(readdirSync(dir).sort()).toEqual(["a.txt", "b.txt", "c.txt", "form-trip.tar.gz"]);
    expect(startsWith(path.join(dir, "form-trip.tar.gz"), 0x1f, 0x8b)).toBe(true);
    await closeQuickWindow(mainWindow);
  });

  it("a later 'Add to archive...' launch joins the form while it is open", async () => {
    const dir = path.join(work, "join");
    mkdirSync(dir);
    const first = path.join(dir, "one.txt");
    const second = path.join(dir, "two.txt");
    writeFileSync(first, "1");
    writeFileSync(second, "22");
    expect((await secondLaunch(["--add", "--", first])).code).toBe(0);
    await switchToWindowWith("quick-form");
    await expect($('[data-testid="quick-title"]')).toHaveText(expect.stringContaining("Add one.txt to an archive"));
    // Typed by the user before the second launch arrives: it must survive.
    await $('[data-testid="quick-name"]').setValue("both");
    // Well after the collection window.
    await browser.pause(1500);
    expect((await secondLaunch(["--add", "--", second])).code).toBe(0);
    await browser.waitUntil(async () => (await $('[data-testid="quick-title"]').getText()).includes("and 1 more"), {
      timeoutMsg: "the second item did not join the open form",
    });
    await expect($('[data-testid="quick-name"]')).toHaveValue("both");
    await $('[data-testid="quick-format"]').selectByAttribute("value", "zip");
    await $('[data-testid="quick-ok"]').click();
    await switchToWindowWith("quick-result");
    await expect($('[data-testid="quick-result"]')).toHaveText(expect.stringContaining("Wrote 2 files"));
    expect(readdirSync(dir).sort()).toEqual(["both.zip", "one.txt", "two.txt"]);
    await closeQuickWindow(mainWindow);
  });

  it("the form never replaces a file: a name in the way is refused and the form stays open", async () => {
    const dir = path.join(work, "taken");
    mkdirSync(dir);
    const file = path.join(dir, "x.txt");
    writeFileSync(file, "x");
    writeFileSync(path.join(dir, "taken.zip"), "mine");
    expect((await secondLaunch(["--add", "--", file])).code).toBe(0);
    await switchToWindowWith("quick-form");
    await $('[data-testid="quick-format"]').selectByAttribute("value", "zip");
    await $('[data-testid="quick-name"]').setValue("taken");
    await $('[data-testid="quick-ok"]').click();
    await expect($('[data-testid="quick-form-error"]')).toHaveText(expect.stringContaining("does not replace files"));
    await expect($('[data-testid="quick-form"]')).toExist();
    expect(readFileSync(path.join(dir, "taken.zip"), "utf8")).toBe("mine");
    await $('[data-testid="quick-name"]').setValue("free");
    await $('[data-testid="quick-ok"]').click();
    await switchToWindowWith("quick-result");
    await expect($('[data-testid="quick-result"]')).toHaveText(expect.stringContaining("Wrote 1 file"));
    expect(readFileSync(path.join(dir, "taken.zip"), "utf8")).toBe("mine");
    expect(readdirSync(dir).sort()).toEqual(["free.zip", "taken.zip", "x.txt"]);
    await closeQuickWindow(mainWindow);
  });

  it("Browse... picks the place in Rust; the form shows only its folder and the file name", async () => {
    const dir = path.join(work, "browse");
    const elsewhere = path.join(work, "browse-elsewhere");
    mkdirSync(dir);
    mkdirSync(elsewhere);
    const file = path.join(dir, "y.txt");
    writeFileSync(file, "y");
    expect((await secondLaunch(["--add", "--", file])).code).toBe(0);
    await switchToWindowWith("quick-form");
    // The native dialog is replaced in the e2e build by this queue (README, "Sources and destinations").
    writeFileSync(process.env.MOCHI_E2E_PICK_FILE, path.join(elsewhere, "Chosen"));
    await $('[data-testid="quick-browse"]').click();
    await browser.waitUntil(async () => (await $('[data-testid="quick-name"]').getValue()) === "Chosen.mochi", {
      timeoutMsg: "the chosen name was not shown",
    });
    await expect($('[data-testid="quick-folder"]')).toHaveText(expect.stringContaining(elsewhere));
    await $('[data-testid="quick-ok"]').click();
    await switchToWindowWith("quick-result");
    await expect($('[data-testid="quick-result"]')).toHaveText(expect.stringContaining("Archive created: Chosen.mochi"));
    expect(readdirSync(elsewhere)).toEqual(["Chosen.mochi"]);
    expect(readdirSync(dir)).toEqual(["y.txt"]);
    await closeQuickWindow(mainWindow);
  });

  it("Cancel on the form closes the window and writes nothing", async () => {
    const dir = path.join(work, "cancel-form");
    mkdirSync(dir);
    const file = path.join(dir, "z.txt");
    writeFileSync(file, "z");
    expect((await secondLaunch(["--add", "--", file])).code).toBe(0);
    await switchToWindowWith("quick-form");
    try {
      await $('[data-testid="quick-form-cancel"]').click();
    } catch (err) {
      if (!/no such window/i.test(String(err?.message ?? err))) throw err;
    }
    await browser.waitUntil(async () => (await browser.getWindowHandles()).length === archiveWindows.size, {
      timeoutMsg: "the compact window did not close",
    });
    await browser.switchToWindow(mainWindow);
    expect(readdirSync(dir)).toEqual(["z.txt"]);
  });

  it("refuses a passphrase option without echoing its value", async () => {
    const ended = await secondLaunch(["--passphrase=hunter2", "--", fixture]);
    expect(ended.code).toBe(0);
    // A notice goes to the window used last, which is not necessarily the main window.
    await switchToWindowWith("browse-error", 30_000, "passphrase cannot be given on the command line");
    const text = await $('[data-testid="browse-error"]').getText();
    expect(text).toContain("passphrase cannot be given on the command line");
    expect(text).not.toContain("hunter2");
  });

  const entryNames = () =>
    browser.execute(() =>
      Array.from(document.querySelectorAll('[data-testid="foreign-entry"] .name'), (e) => e.textContent),
    );

  it("opens a ZIP from the command line in the foreign browser (plan D8)", async () => {
    const zip = path.join(work, "photos.zip");
    copyFileSync(path.join(foreignFixtures, "basic_infozip.zip"), zip);
    await launchIntoNewWindow(["--open", "--", zip], mainWindow);
    await $('[data-testid="foreign-meta"]').waitForExist({ timeout: 30_000 });
    await expect($('[data-testid="foreign-meta"]')).toHaveText(expect.stringContaining("photos.zip"));
    await browser.waitUntil(async () => (await entryNames()).includes("a.txt"), {
      timeoutMsg: "the ZIP's entries were not listed",
    });
    expect(await entryNames()).toEqual(["empty", "sub", "a.txt"]);
    // Foreign wording: read-only, no MOCHI health dimension claimed.
    const text = (await $('[data-testid="foreign"]').getText()).toLowerCase();
    for (const word of ["backed up", "safe", "preserved", "verified", "healthy"]) {
      expect(text).not.toContain(word);
    }
  });

  // Users reported "Add to archive..." did nothing while a ZIP, 7z or RAR file was open: the panel it
  // used sat in a part of the window that is hidden then. The form is its own window now.
  it("'Add to archive...' shows its form while a ZIP is open in another window", async () => {
    const dir = path.join(work, "while-foreign");
    mkdirSync(dir);
    const file = path.join(dir, "n.txt");
    writeFileSync(file, "n");
    const zip = path.join(work, "while-open.zip");
    copyFileSync(path.join(foreignFixtures, "basic_infozip.zip"), zip);
    const zipWindow = await launchIntoNewWindow(["--open", "--", zip], mainWindow);
    await expect($('[data-testid="foreign-meta"]')).toExist();
    expect((await secondLaunch(["--add", "--", file])).code).toBe(0);
    await switchToWindowWith("quick-form");
    await expect($('[data-testid="quick-name"]')).toHaveValue("n.mochi");
    await expect($('[data-testid="quick-form"]')).toBeDisplayed();
    // The foreign browser is still what its window shows.
    await browser.switchToWindow(zipWindow);
    await expect($('[data-testid="foreign-meta"]')).toExist();
    await switchToWindowWith("quick-form");
    await $('[data-testid="quick-form-cancel"]').click().catch(() => undefined);
    await browser.waitUntil(async () => (await browser.getWindowHandles()).length === archiveWindows.size, {
      timeoutMsg: "the compact window did not close",
    });
    await browser.switchToWindow(mainWindow);
    expect(readdirSync(dir)).toEqual(["n.txt"]);
  });

  it("shows a hostile name in a ZIP as text and marks it not extractable", async () => {
    const zip = path.join(work, "hostile.zip");
    copyFileSync(path.join(foreignFixtures, "traversal.zip"), zip);
    await launchIntoNewWindow(["--", zip], mainWindow);
    await browser.waitUntil(
      async () => (await $('[data-testid="foreign-meta"]').getText()).includes("hostile.zip"),
      { timeoutMsg: "the hostile ZIP was not shown" },
    );
    await browser.waitUntil(async () => (await entryNames()).some((n) => n.includes("evil.txt")), {
      timeoutMsg: "the hostile entry was not listed",
    });
    // Text, never markup.
    expect(await browser.execute(() => document.querySelectorAll('[data-testid="foreign"] img').length)).toBe(0);
  });

  it("opens a 7z with encrypted names by asking for the password in the window", async () => {
    const archive = path.join(work, "secret.7z");
    copyFileSync(path.join(foreignFixtures, "7z", "enc_names.7z"), archive);
    await launchIntoNewWindow(["--open", "--", archive], mainWindow);
    await $('[data-testid="foreign-password"]').waitForExist({ timeout: 30_000 });
    await $('[data-testid="foreign-password-input"]').setValue("SECRET");
    await $('[data-testid="foreign-password-submit"]').click();
    await browser.waitUntil(
      async () => (await $('[data-testid="foreign-meta"]').getText()).includes("secret.7z"),
      { timeoutMsg: "the 7z did not open after the password" },
    );
  });

  it("an extract verb on a ZIP opens it for browsing and extracts nothing", async () => {
    const zip = path.join(work, "verb.zip");
    copyFileSync(path.join(foreignFixtures, "basic_infozip.zip"), zip);
    await launchIntoNewWindow(["--extract-to-folder", "--", zip], mainWindow);
    await browser.waitUntil(
      async () => (await $('[data-testid="foreign-meta"]').getText()).includes("verb.zip"),
      { timeoutMsg: "the ZIP was not opened for browsing" },
    );
    await browser.waitUntil(
      async () => (await $('[data-testid="browse-error"]').getText()).includes("extracting from the shell is for .mochi"),
      { timeoutMsg: "no plain note that shell extraction is for .mochi" },
    );
    expect(readdirSync(work)).not.toContain("verb");
  });

  it("still refuses what D8 does not support, with the typed message", async () => {
    // The last part of a split ZIP: an archive MOCHI cannot read on its own.
    const zip = path.join(work, "split.zip");
    copyFileSync(path.join(foreignFixtures, "split_last.zip"), zip);
    const ended = await secondLaunch(["--open", "--", zip]);
    expect(ended.code).toBe(0);
    await switchToWindowWith("browse-error", 30_000, "could not be opened (UNSUPPORTED");
    await browser.waitUntil(
      async () => (await $('[data-testid="browse-error"]').getText()).includes("could not be opened (UNSUPPORTED"),
      { timeoutMsg: "no typed refusal for the split ZIP" },
    );
    expect(await $('[data-testid="browse-error"]').getText()).not.toContain("cannot be opened yet");
  });
});
