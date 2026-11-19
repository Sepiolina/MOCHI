// Plan D1 exit, on the real app over real IPC: browse a snapshot, its
// history, and a 100,000-entry folder without loading it whole into the
// webview; archive names stay text (spec §23.3 #9). The archives come from
// the d1_browse_fixtures example (wdio.conf.js): browse.mochi on the first
// "Open archive" click, large.mochi on the second.
import { $, $$, expect } from "@wdio/globals";
import { closeVersions, openInfo, openVersions } from "./_ui.js";

const countElements = (selector) =>
  browser.execute((s) => document.querySelectorAll(s).length, selector);

const entryNames = () =>
  browser.execute(() =>
    Array.from(document.querySelectorAll('[data-testid="entry"] .name'), (e) => e.textContent),
  );

async function openNext() {
  await $('[data-testid="open-archive"]').click();
  await $('[data-testid="archive-meta"]').waitForExist({ timeout: 60_000 });
}

async function enterFolder(name) {
  await browser.waitUntil(async () => (await entryNames()).includes(name), {
    timeoutMsg: `no entry ${name}`,
  });
  const rows = await $$('[data-testid="entry"]');
  for (const row of rows) {
    if ((await row.$(".name").getText()) === name) {
      await row.click();
      await browser.keys("Enter");
      return;
    }
  }
  throw new Error(`no entry ${name}`);
}

describe("archive browser", () => {
  it("opens an archive and shows its history and hostile names as text", async () => {
    await openNext();
    await expect($('[data-testid="window-title"]')).toHaveText(expect.stringContaining("browse.mochi"));
    await expect($('[data-testid="toggle-versions"]')).toHaveAttribute("title", "4 versions");
    await openVersions();
    await expect($('[data-testid="commit-3"]')).toHaveText(expect.stringContaining("(latest)"));
    await closeVersions();

    // Info (owner request 2026-11-14): the version's totals from Rust, over real IPC.
    await $('[data-testid="toolbar-archive-info"]').click();
    await $('[data-testid="info-files"]').waitForDisplayed({ timeout: 30_000 });
    expect(Number((await $('[data-testid="info-files"]').getText()).replace(/\D/g, ""))).toBeGreaterThan(0);
    await expect($('[data-testid="info-ratio"]')).toHaveText(/^(\d+%|—)$/);
    // The fixture records its writer's chunk size (small, so a test archive has many chunks).
    await expect($('[data-testid="info-dictionary"]')).toHaveText(/^Up to \d/);
    // No comment on the fixture; the latest version offers to edit one (B.2.19).
    await expect($('[data-testid="info-comment-text"]')).toHaveText("No comment");
    await expect($('[data-testid="info-comment-edit"]')).toBeDisplayed();
    await $('[data-testid="archive-info-ok"]').click();
    await $('[data-testid="archive-info"]').waitForExist({ reverse: true });

    // Scan for viruses: the window opens; on Ubuntu a real job runs `/usr/bin/true` on the
    // extracted folder, so the extract, the run, the report and the clean-up all happen for real.
    await $('[data-testid="toolbar-scan"]').click();
    await $('[data-testid="scan-program"]').waitForDisplayed({ timeout: 30_000 });
    if (process.platform === "linux") {
      await $('[data-testid="scan-program"]').setValue("/usr/bin/true");
      await $('[data-testid="scan-arguments"]').setValue('"%f"');
      await $('[data-testid="scan-start"]').click();
      await $('[data-testid="scan-verdict"]').waitForDisplayed({ timeout: 120_000 });
      await expect($('[data-testid="scan-verdict"]')).toHaveText("true finished with exit code 0. Read its output to see what it found.");
      await expect($('[data-testid="scan-scope"]')).toHaveText(expect.stringContaining("of version 3"));
      await $('[data-testid="scan-close"]').click();
    } else {
      await browser.keys("Escape");
    }
    await $('[data-testid="scan-archive"]').waitForExist({ reverse: true });

    // Root of the head: the scripted history's hostile name, inert.
    await browser.waitUntil(async () => (await entryNames()).includes("<img src=x onerror=alert(1)>"), {
      timeoutMsg: "the hostile name is not listed as text",
    });
    await enterFolder("hostile");
    await expect($('[data-testid="toggle-versions"]')).toHaveText("v3");
    await expect($('[data-testid="crumbs"]')).toHaveText(expect.stringContaining("hostile"));
    await browser.waitUntil(async () => (await entryNames()).length === 6);
    const names = await entryNames();
    for (const n of ["<script>alert(1)", '"><svg onload=alert(1)>', "javascript:alert(1)", "&lt;b&gt;entity"]) {
      expect(names).toContain(n);
    }
    // Control and bidi characters arrive escaped, and are marked so.
    expect(names).toContain("esc\\u{1b}[31mred");
    expect(names).toContain("rtl\\u{202e}gpj.exe");
    expect(await countElements('[data-testid="entry"] .escaped')).toBe(2);

    expect(await countElements("img")).toBe(0);
    // The app's own icons are decorative inline svg; anything else, or any
    // inline event handler, came from archive text.
    expect(await countElements('svg:not([aria-hidden="true"][focusable="false"])')).toBe(0);
    expect(await countElements("[onload], [onerror], [onmouseover], [onclick]")).toBe(0);
    // No script element came from a name. (Counting all scripts does not
    // work: WebView2 under msedgedriver adds one of its own on Windows.)
    expect(
      await browser.execute(
        () =>
          Array.from(document.scripts).filter((s) => `${s.src} ${s.textContent}`.includes("alert(1)"))
            .length,
      ),
    ).toBe(0);
    expect(await countElements("a[href]")).toBe(0);

    // Details of one entry.
    const rows = await $$('[data-testid="entry"]');
    await rows[0].click();
    // The details are in the Info panel, which the toolbar's Info opens.
    await openInfo();
    await expect($('[data-testid="details"]')).toHaveText(expect.stringContaining("rw-r--r-- (0644)"));
  });

  it("switches snapshots from the timeline", async () => {
    await openVersions();
    await $('[data-testid="commit-0"]').click();
    await expect($('[data-testid="toggle-versions"]')).toHaveText("v0");
    await browser.waitUntil(async () => (await entryNames()).includes("docs"));
    // `hostile/` arrived in commit 3; commit 0 does not have it.
    expect(await entryNames()).not.toContain("hostile");
    await openVersions();
    await expect($('[data-testid="commit-0"]')).toHaveAttribute("aria-current", "true");
    await closeVersions();
  });

  it("pages a 100,000-entry folder and keeps only a window of rows in the DOM", async () => {
    await $('[data-testid="close-archive"]').click();
    await expect($('[data-testid="start-screen"]')).toBeExisting();
    await openNext();
    await expect($('[data-testid="window-title"]')).toHaveText(expect.stringContaining("large.mochi"));
    await enterFolder("many");
    await expect($('[data-testid="entry-count"]')).toHaveText(
      expect.stringContaining("100,000 items"),
    );
    await browser.waitUntil(async () => (await entryNames()).includes("f000000"));
    expect(await countElements('[data-testid="entries"] [role="listitem"]')).toBeLessThan(150);

    // Jump to the end: the last rows load; the first are gone from the DOM.
    await browser.execute(() => {
      const list = document.querySelector('[data-testid="entries"]');
      list.scrollTop = list.scrollHeight;
      list.dispatchEvent(new Event("scroll"));
    });
    await browser.waitUntil(async () => (await entryNames()).includes("f099999"), {
      timeoutMsg: "the last entry did not load",
    });
    expect(await entryNames()).not.toContain("f000000");
    expect(await countElements('[data-testid="entries"] [role="listitem"]')).toBeLessThan(150);
  });
});
