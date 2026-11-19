// Plan D4 on the real app over real IPC: "Test archive" on a good archive and
// on a corrupted one (FAIL shown, the overall status and exit code not
// hidden), the health view, "Export report JSON" (byte-equal to the report
// core produced), a repair of a damaged archive into a new file with the
// re-verification shown, and cancel. WebDriver cannot drive a native dialog,
// so the `e2e` build takes every chosen file from MOCHI_E2E_PICK_FILE, which
// this spec writes before each click.
//
// The independent oracle is the command-line tool: `mochi verify --json`
// over the same file must report the same thing (apart from the run's start
// and completion times). It is built by the CI job (`cargo build -p
// mochi-cli`); MOCHI_CLI_BINARY overrides its path.
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { $, expect } from "@wdio/globals";

const here = path.dirname(fileURLToPath(import.meta.url));
const fixtures = process.env.MOCHI_E2E_FIXTURES;
const pickFile = process.env.MOCHI_E2E_PICK_FILE;
const queuePicks = (...paths) => writeFileSync(pickFile, paths.join("\n"));

const windows = process.platform === "win32";
const cli =
  process.env.MOCHI_CLI_BINARY ??
  path.resolve(here, "../../../../target/debug", windows ? "mochi.exe" : "mochi");

const root = mkdtempSync(path.join(os.tmpdir(), "mochi-d4-"));
const good = path.join(fixtures, "browse.mochi");
const damaged = path.join(fixtures, "damaged.mochi");
const large = path.join(fixtures, "large.mochi");

const BANNED = ["backed up", "backup", "safe", "preserved", "secure", "repaired", "recovered completely"];
const assertNoBannedWording = async () => {
  const text = (await browser.execute(() => document.body.textContent)).toLowerCase();
  for (const word of BANNED) expect(text).not.toContain(word);
};

const sha = (file) => createHash("sha256").update(readFileSync(file)).digest("hex");

/** `mochi verify --json` over `file`: its report without the run's times, and its exit code. */
function cliVerify(file, ...args) {
  if (!existsSync(cli)) throw new Error(`missing ${cli}: cargo build -p mochi-cli`);
  const r = spawnSync(cli, ["verify", file, "--json", "--no-local-history", ...args], { encoding: "utf8" });
  return { report: withoutTimes(JSON.parse(r.stdout)), exit: r.status };
}

function withoutTimes(report) {
  const copy = { ...report };
  delete copy.started_at;
  delete copy.completed_at;
  return copy;
}

async function press(testId) {
  const el = await $(`[data-testid="${testId}"]`);
  // The page's own scrollIntoView: WebKitWebDriver's wheel-based scroll can
  // leave a control in the tools drawer under the drawer's sticky summary.
  await browser.execute((e) => e.scrollIntoView({ block: "center" }), el);
  await el.click();
}

const text = (selector) => browser.execute((s) => document.querySelector(s)?.textContent ?? null, selector);

async function choose(file) {
  queuePicks(file);
  await press("check-choose");
  await browser.waitUntil(async () => (await $('[data-testid="check-subject"]').getText()).includes(path.basename(file)), {
    timeoutMsg: `${file} was not chosen`,
  });
}

/** Run "Test archive" and wait for a new report. */
async function testArchive() {
  const before = await text('[data-testid="report-json"]');
  await press("check-test");
  await browser.waitUntil(
    async () => {
      const now = await text('[data-testid="report-json"]');
      return now !== null && now !== before && !(await $('[data-testid="check-running"]').isExisting());
    },
    { timeout: 120_000, timeoutMsg: "the test did not finish" },
  );
  return JSON.parse(await text('[data-testid="report-json"]'));
}

/** The technical part of the report is folded under Details; open it so its text is visible. */
async function openDetails() {
  const d = $('[data-testid="report"] [data-testid="report-details"]');
  if (!(await d.getAttribute("open"))) await d.$("summary").click();
}

const dimensionStatus = (name) =>
  browser.execute(
    (n) => document.querySelector(`[data-testid="report"] [data-dimension="${n}"]`)?.getAttribute("data-status"),
    name,
  );

/** Every badge on screen: success styling exactly when the status is PASS. */
async function assertOnlyPassLooksLikeSuccess() {
  const badges = await browser.execute(() =>
    Array.from(document.querySelectorAll('[data-testid="status-badge"]'), (b) => [
      b.getAttribute("data-status"),
      b.classList.contains("status-pass"),
      getComputedStyle(b).backgroundColor,
    ]),
  );
  expect(badges.length).toBeGreaterThan(0);
  const passColour = badges.find(([s]) => s === "PASS")?.[2];
  for (const [status, success, colour] of badges) {
    expect(success).toBe(status === "PASS");
    if (status !== "PASS" && passColour) expect(colour).not.toBe(passColour);
  }
}

describe("test, health, and repair", () => {
  it("tests a good archive read-only and shows core's report per dimension", async () => {
    const before = sha(good);
    await choose(good);
    const report = await testArchive();
    expect(await dimensionStatus("integrity")).toBe("PASS");
    expect(await dimensionStatus("recoverability")).toBe("PASS");
    expect(await dimensionStatus("freshness")).toBe("UNKNOWN");
    await openDetails();
    await expect($('[data-testid="report-freshness-note"]')).toHaveText(expect.stringContaining("not a failure"));
    expect(await $('[data-testid="report"]').getAttribute("data-overall")).not.toBe("PASS");
    await assertOnlyPassLooksLikeSuccess();
    // The same report as the command-line tool's.
    const oracle = cliVerify(good);
    expect(withoutTimes(report)).toEqual(oracle.report);
    expect(report.exit_code).toBe(oracle.exit);
    expect(sha(good)).toBe(before);
    await assertNoBannedWording();
  });

  it("exports the exact report JSON core produced", async () => {
    const out = path.join(root, "good-report.json");
    queuePicks(out);
    await press("report-export");
    await $('[data-testid="report-exported"]').waitForExist({ timeout: 30_000 });
    await expect($('[data-testid="report-exported"]')).toHaveText(expect.stringContaining("good-report.json"));
    const shown = await text('[data-testid="report-json"]');
    expect(readFileSync(out, "utf8")).toBe(shown);
    // Never replaced: exporting to the same file again is refused.
    queuePicks(out);
    await press("report-export");
    await $('[data-testid="check-error"]').waitForExist({ timeout: 30_000 });
    await expect($('[data-testid="check-error"]')).toHaveText(expect.stringContaining("DESTINATION_EXISTS"));
    expect(readFileSync(out, "utf8")).toBe(shown);
  });

  it("shows FAIL for a corrupted archive without hiding it in the overall status", async () => {
    const before = sha(damaged);
    await choose(damaged);
    const report = await testArchive();
    expect(await dimensionStatus("integrity")).toBe("FAIL");
    await expect($('[data-testid="report"]')).toHaveAttribute("data-overall", "FAIL");
    await expect($('[data-testid="report"]')).toHaveAttribute("data-exit", "1");
    await expect($('[data-testid="report-headline"]')).toHaveText("Problems found");
    await openDetails();
    await expect($('[data-testid="report-overall"]')).toHaveText(expect.stringContaining("Failing: Integrity"));
    await expect($('[data-testid="report-overall"]')).toHaveText(expect.stringContaining("exit code 1"));
    await assertOnlyPassLooksLikeSuccess();
    const oracle = cliVerify(damaged);
    expect(oracle.exit).toBe(1);
    expect(withoutTimes(report)).toEqual(oracle.report);
    expect(sha(damaged)).toBe(before);

    // Health from this session's evidence: the failure stands.
    await press("check-health");
    await $('[data-testid="health"]').waitForExist({ timeout: 30_000 });
    const health = await browser.execute(() =>
      document.querySelector('[data-testid="health"] [data-dimension="integrity"]')?.getAttribute("data-status"),
    );
    expect(health).toBe("FAIL");
    await assertOnlyPassLooksLikeSuccess();
  });

  it("repairs the damaged archive into a new file through a reviewed plan, and re-verifies", async () => {
    const before = sha(damaged);
    const out = path.join(root, "new.mochi");
    await press("repair-plan");
    await $('[data-testid="repair-plan-view"]').waitForExist({ timeout: 120_000 });
    await expect($('[data-testid="repair-plan-view"]')).toHaveAttribute("data-outcome", "partial");
    await expect($('[data-testid="repair-plan-line"]')).toHaveText(expect.stringContaining("PARTIAL"));
    await expect($('[data-testid="repair-omitted"]')).toHaveText(expect.stringContaining("big"));
    expect(existsSync(out)).toBe(false);
    // Nothing can be written before a new file is chosen and the plan approved.
    expect(await $('[data-testid="repair-apply"]').isEnabled()).toBe(false);
    queuePicks(out);
    await press("repair-choose-output");
    await expect($('[data-testid="repair-output"]')).toHaveText(expect.stringContaining("new.mochi"));
    expect(await $('[data-testid="repair-apply"]').isEnabled()).toBe(false);
    await press("repair-approve");
    await press("repair-apply");
    await $('[data-testid="repair-result"]').waitForExist({ timeout: 120_000 });
    await expect($('[data-testid="repair-result"]')).toHaveAttribute("data-outcome", "partial");
    await expect($('[data-testid="repair-result"]')).toHaveText(expect.stringContaining("Repair result: PARTIAL"));
    await expect($('[data-testid="repair-reverification"]')).toHaveText(expect.stringContaining("Re-verified before it was published"));
    expect(existsSync(out)).toBe(true);
    expect(sha(damaged)).toBe(before);
    await assertOnlyPassLooksLikeSuccess();
    await assertNoBannedWording();

    // Test the new archive in the app; the command-line tool agrees.
    await press("repair-test-new");
    await browser.waitUntil(async () => (await $('[data-testid="check-subject"]').getText()).includes("new.mochi"));
    await browser.waitUntil(async () => (await dimensionStatus("integrity")) === "PASS", {
      timeout: 120_000,
      timeoutMsg: "the new archive was not tested",
    });
    const oracle = cliVerify(out);
    expect(oracle.exit).toBe(0);
    expect(oracle.report.dimensions.integrity).toBe("PASS");
  });

  it("cancels a repair plan and a test, and says nothing was written", async () => {
    await choose(large);
    // Paced per progress event (MOCHI_E2E_PACE_MS), so a cancel lands mid-run.
    await press("repair-plan");
    await $('[data-testid="check-progress"]').waitForExist({ timeout: 60_000 });
    await press("check-cancel");
    await browser.waitUntil(
      async () => ((await text('[data-testid="check-status"]')) ?? "").includes("cancelled"),
      { timeout: 60_000, timeoutMsg: "the plan was not cancelled" },
    );
    await expect($('[data-testid="check-status"]')).toHaveText(expect.stringContaining("Nothing was written"));
    expect(await $('[data-testid="repair-plan-view"]').isExisting()).toBe(false);

    await press("check-test");
    await $('[data-testid="check-progress"]').waitForExist({ timeout: 60_000 });
    await press("check-cancel");
    await browser.waitUntil(
      async () => ((await text('[data-testid="check-status"]')) ?? "").includes("The test was cancelled"),
      { timeout: 60_000, timeoutMsg: "the test was not cancelled" },
    );
  });
});
