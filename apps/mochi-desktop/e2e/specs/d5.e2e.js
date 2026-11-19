// Plan D5 on the real app over real IPC: create an encrypted archive and open
// it again with a wrong and then the right passphrase, change its passphrases
// (add one, remove one), create an archive with a recovery record, and search
// with the coverage in view. WebDriver cannot drive a native dialog, so the
// `e2e` build takes the paths of its dialogs from MOCHI_E2E_PICK_FILE, in
// order; this spec writes that file before each flow (see create-add.e2e.js).
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { $, $$, expect } from "@wdio/globals";
import { clickNewArchive, closeResultDetails, closeTool, openResultDetails, openTool } from "./_ui.js";

const pickFile = process.env.MOCHI_E2E_PICK_FILE;
const queuePicks = (...paths) => writeFileSync(pickFile, paths.join("\n"));

const PASS = "e2e correct horse 8842";
const OTHER = "e2e second passphrase 1207";
const WRONG = "e2e not the passphrase 3391";

const root = mkdtempSync(path.join(os.tmpdir(), "mochi-d5-"));
const src = path.join(root, "src");
const out = path.join(root, "out");
mkdirSync(path.join(src, "vault-docs"), { recursive: true });
mkdirSync(out, { recursive: true });
writeFileSync(path.join(src, "vault-docs", "payroll-2026.txt"), "salaries: confidential marker QXZV");
writeFileSync(path.join(src, "vault-docs", "notes.txt"), "plain notes");

const vault = path.join(out, "vault.mochi");
const recovered = path.join(out, "recovered.mochi");

const entryNames = () =>
  browser.execute(() =>
    Array.from(document.querySelectorAll('[data-testid="entry"] .name'), (e) => e.textContent),
  );

const waitForStatus = (text) =>
  browser.waitUntil(
    async () => {
      const s = await $('[data-testid="write-status"]');
      return (await s.isExisting()) && (await s.getText()).includes(text);
    },
    { timeout: 60_000, timeoutMsg: `no status containing ${JSON.stringify(text)}` },
  );

const waitForRekey = (text) =>
  browser.waitUntil(
    async () => {
      const s = await $('[data-testid="rekey-status"]');
      return (await s.isExisting()) && (await s.getText()).includes(text);
    },
    { timeout: 60_000, timeoutMsg: `no rekey status containing ${JSON.stringify(text)}` },
  );

const BANNED = ["backed up", "backup", "safe", "preserved", "secure", "revoked"];
const assertNoBannedWording = async () => {
  const text = (await $("body").getText()).toLowerCase();
  for (const word of BANNED) expect(text).not.toContain(word);
};
const assertNoPassphraseOnPage = async () => {
  const html = await browser.execute(() => document.documentElement.outerHTML);
  const text = await $("body").getText();
  for (const p of [PASS, OTHER, WRONG]) {
    expect(html).not.toContain(p);
    expect(text).not.toContain(p);
  }
};

// From the start screen when nothing is open, from the toolbar otherwise. The
// save-as dialog is the "Change…" one (the default place would be next to the
// folder), and the options are opened as a person would.
const stageFolderAndOpenCreate = async () => {
  await clickNewArchive();
  await $('[data-testid="stage-folder"]').click();
  await browser.waitUntil(async () => (await $$('[data-testid="staged-item"]')).length === 1);
  await $('[data-testid="change-destination"]').click();
  await browser.waitUntil(async () =>
    ((await $('[data-testid="save-location"]').getAttribute("title")) ?? "").startsWith("Saved as"),
  );
  await $('[data-testid="create-options"] summary').click();
};

describe("passphrases, rekey, recovery record, and search", () => {
  it("creates an encrypted archive: warnings first, a mismatch refused, then a local commit", async () => {
    queuePicks(path.join(src, "vault-docs"), vault);
    await stageFolderAndOpenCreate();
    await $('[data-testid="encrypt"]').click();
    await expect($('[data-testid="lost-passphrase"]')).toHaveText(
      expect.stringContaining("no recovery"),
    );
    await expect($('[data-testid="dedup-leak"]')).toHaveText(expect.stringContaining("stored once"));
    // What stays visible without the key is the "i" note's: read its text, not what is drawn.
    await expect($('[data-testid="visible-without-key"]')).toHaveElementProperty(
      "textContent",
      expect.stringContaining("key envelopes"),
    );
    // TAR-compatible and encrypted exclude each other.
    await expect($('[data-testid="tar-compatible"]')).toBeDisabled();

    await $('[data-testid="create-passphrase"]').setValue(PASS);
    await $('[data-testid="create-passphrase-again"]').setValue(WRONG);
    await $('[data-testid="create-confirm"]').click();
    await expect($('[data-testid="write-error"]')).toHaveText(
      expect.stringContaining("The two passphrases differ"),
    );
    expect(existsSync(vault)).toBe(false);

    await $('[data-testid="create-passphrase-again"]').setValue(PASS);
    await $('[data-testid="create-confirm"]').click();
    await waitForStatus("Created vault.mochi");
    await openResultDetails("write");
    expect(await $('[data-testid="write-details"]').getText()).toContain("LOCAL_COMMITTED");
    await closeResultDetails("write");
    await expect($('[data-testid="encrypted-note"]')).toHaveAttribute("title", expect.stringContaining("Encrypted"));
    await browser.waitUntil(async () => (await entryNames()).includes("vault-docs"));
    await assertNoBannedWording();
    await assertNoPassphraseOnPage();

    // On disk the names and content are sealed.
    const bytes = readFileSync(vault);
    expect(bytes.includes(Buffer.from("payroll-2026"))).toBe(false);
    expect(bytes.includes(Buffer.from("QXZV"))).toBe(false);
    expect(bytes.includes(Buffer.from(PASS))).toBe(false);
  });

  it("reopens it: a wrong passphrase is 'did not open', not damage; the right one opens", async () => {
    await $('[data-testid="close-archive"]').click();
    queuePicks(vault);
    await $('[data-testid="open-archive"]').click();
    await $('[data-testid="unlock-panel"]').waitForExist();
    await expect($('[data-testid="unlock-panel"]')).toHaveText(expect.stringContaining("is encrypted"));
    await expect($('[data-testid="unlock-panel"]')).toHaveText(expect.stringContaining("no recovery"));

    await $('[data-testid="unlock-input"]').setValue(WRONG);
    await $('[data-testid="unlock-submit"]').click();
    const err = await $('[data-testid="unlock-error"]');
    await err.waitForExist({ timeout: 30_000 });
    await expect(err).toHaveText(expect.stringContaining("The passphrase did not open this archive."));
    const shown = (await err.getText()).toLowerCase();
    for (const w of ["damage", "corrupt"]) expect(shown).not.toContain(w);
    await expect($('[data-testid="unlock-input"]')).toHaveValue("");
    await expect($('[data-testid="archive-meta"]')).not.toBeExisting();
    await assertNoPassphraseOnPage();

    await $('[data-testid="unlock-input"]').setValue(PASS);
    await $('[data-testid="unlock-submit"]').click();
    await $('[data-testid="archive-meta"]').waitForExist({ timeout: 30_000 });
    await expect($('[data-testid="crumb-root"]')).toHaveText(expect.stringContaining("vault.mochi"));
    await expect($('[data-testid="encrypted-note"]')).toBeExisting();
    await browser.waitUntil(async () => (await entryNames()).includes("vault-docs"));
    await assertNoPassphraseOnPage();
  });

  it("adds a passphrase, then removes one and is told removal is not revocation", async () => {
    await openTool("menu-passphrases");
    await browser.waitUntil(async () => (await $$('[data-testid="envelope"]')).length === 1);
    await $('[data-testid="rekey-add"]').click();
    await $('[data-testid="rekey-add-form-input"]').setValue(OTHER);
    await $('[data-testid="rekey-add-form-confirm"]').setValue(OTHER);
    await $('[data-testid="rekey-add-form-submit"]').click();
    await waitForRekey("a passphrase was added");
    expect(await $('[data-testid="rekey-status"]').getText()).toContain("LOCAL_COMMITTED");
    await browser.waitUntil(async () => (await $$('[data-testid="envelope"]')).length === 2);
    await expect($('[data-testid="toggle-versions"]')).toHaveAttribute("title", "2 versions");

    await $('[data-testid="rekey-remove"]').click();
    await expect($('[data-testid="not-revocation"]')).toHaveText(
      expect.stringContaining("Removing a passphrase is not revocation"),
    );
    // Remove the envelope that did not open this session.
    const items = await $$('[data-testid="rekey-panel"] li');
    for (const li of items) {
      if (!(await li.getText()).includes("opened this archive")) {
        await li.$('[data-testid="remove-envelope"]').click();
        break;
      }
    }
    await waitForRekey("not revocation");
    await browser.waitUntil(async () => (await $$('[data-testid="envelope"]')).length === 1);
    // The last passphrase cannot be removed.
    await expect($('[data-testid="rekey-remove"]')).toBeDisabled();
    await assertNoBannedWording();
    await assertNoPassphraseOnPage();
    await closeTool();
  });

  it("creates an archive with a recovery record and states what it does and does not do", async () => {
    queuePicks(path.join(src, "vault-docs"), recovered);
    await stageFolderAndOpenCreate();
    // The previous create's choices were not carried over.
    await expect($('[data-testid="encrypt"]')).not.toBeSelected();
    await expect($('[data-testid="recovery-notes"]')).toHaveElementProperty(
      "textContent",
      expect.stringContaining("not a second copy"),
    );
    await expect($('[data-testid="recovery-notes"]')).toHaveElementProperty(
      "textContent",
      expect.stringContaining("Damage beyond that is reported and left as it is"),
    );
    await $('[data-testid="recovery-10"]').click();
    await $('[data-testid="create-confirm"]').click();
    await waitForStatus("Created recovered.mochi");
    await expect($('[data-testid="crumb-root"]')).toHaveText(expect.stringContaining("recovered.mochi"));
    // The profile is technical detail, in a closed disclosure: read its text, not what is shown.
    await expect($('[data-testid="archive-technical"]')).toHaveElementProperty(
      "textContent",
      expect.stringContaining("recovery record (10% parity)"),
    );
    await assertNoBannedWording();
    expect(existsSync(recovered)).toBe(true);
  });

  it("searches names from the toolbar and says what the search covered", async () => {
    await browser.waitUntil(async () => (await entryNames()).includes("vault-docs"));
    const field = await $('[data-testid="search-query"]');
    await field.setValue("payroll");
    await browser.keys(["Enter"]);
    await browser.waitUntil(async () => (await $$('[data-testid="search-hit"]')).length === 1, {
      timeout: 30_000,
      timeoutMsg: "the search found no hit",
    });
    const hits = await $$('[data-testid="search-hit"]');
    await expect(hits[0]).toHaveText(expect.stringContaining("payroll-2026.txt"));
    // The count is in the status bar; what was searched (and that contents were not) is its tooltip.
    const count = await $('[data-testid="entry-count"]');
    await expect(count).toHaveText("1 match");
    await expect(count).toHaveAttribute("title", expect.stringContaining("File contents were not searched"));
    // File content is not searched: a word that is only inside a file finds nothing.
    await field.setValue("QXZV");
    await browser.keys(["Enter"]);
    await $('[data-testid="search-none"]').waitForExist({ timeout: 30_000 });
    await expect(count).toHaveText("0 matches");
    await expect(count).toHaveAttribute("title", expect.stringContaining("File contents were not searched"));
    // Clearing the field shows the folder again.
    await $('[data-testid="search-clear"]').click();
    await browser.waitUntil(async () => (await entryNames()).includes("vault-docs"));
    await assertNoBannedWording();
  });
});
