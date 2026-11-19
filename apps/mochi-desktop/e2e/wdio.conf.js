// End-to-end tests of the built desktop app (plan D0 exit) through
// tauri-driver, which forwards WebDriver to the platform's native driver:
// WebKitWebDriver on Linux, msedgedriver (matching WebView2) on Windows.
//
// Build first, with the UI embedded:
//   pnpm --dir ../ui build
//   cargo build -p mochi-desktop --features custom-protocol,e2e
// Linux needs a display: run under `xvfb-run`.
//
// The D1 browse spec opens archives written by
//   cargo run --release -p mochi-testkit --example d1_browse_fixtures -- DIR
// (DIR is MOCHI_E2E_FIXTURES, default target/d1-fixtures). WebDriver cannot
// drive a native file dialog, so the `e2e` build takes the paths of its
// "Open archive" clicks from MOCHI_E2E_PICK, in order, once per app launch.
// The D2 spec (create-add.e2e.js) builds its own source trees and takes the
// paths of its dialogs from MOCHI_E2E_PICK_FILE. The D4 spec (check.e2e.js)
// also opens damaged.mochi and compares the app's reports with
// `mochi verify --json` (cargo build -p mochi-cli; MOCHI_CLI_BINARY overrides).
// The F8 help spec (help.e2e.js) needs the generated user guide and records, instead of
// launching a browser, the path the Help action opens:
//   python3 packaging/guide.py target/guide     (MOCHI_E2E_GUIDE_DIR overrides)
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const exe = process.platform === "win32" ? "mochi-desktop.exe" : "mochi-desktop";
const application =
  process.env.MOCHI_DESKTOP_BINARY ?? path.resolve(here, "../../../target/debug", exe);

// The D6 launch spec starts the same binary again (a second instance).
process.env.MOCHI_DESKTOP_BINARY = application;

const driverArgs = [];
if (process.env.MOCHI_NATIVE_DRIVER) {
  driverArgs.push("--native-driver", process.env.MOCHI_NATIVE_DRIVER);
}

const fixtures =
  process.env.MOCHI_E2E_FIXTURES ?? path.resolve(here, "../../../target/d1-fixtures");
const picks = ["browse.mochi", "large.mochi"].map((f) => path.join(fixtures, f));
for (const f of [...picks, path.join(fixtures, "damaged.mochi")]) {
  if (!existsSync(f)) {
    console.error(
      `missing E2E fixture ${f}: run cargo run --release -p mochi-testkit --example d1_browse_fixtures -- ${fixtures}`,
    );
    process.exit(1);
  }
}
// The D3 spec opens the same archives through MOCHI_E2E_PICK_FILE.
process.env.MOCHI_E2E_FIXTURES = fixtures;
// Inherited by tauri-driver, the native driver, and the app it starts.
process.env.MOCHI_E2E_PICK = picks.join(path.delimiter);
// The D2 spec rewrites this file before its clicks (one path per line; its
// lines are used before MOCHI_E2E_PICK), and the walk of a create or add is
// slowed per entry so that its cancel case is deterministic.
process.env.MOCHI_E2E_PICK_FILE = path.join(os.tmpdir(), "mochi-e2e-picks.txt");
process.env.MOCHI_E2E_PACE_MS = "100";

// The `e2e` build resolves the guide from this directory instead of the installer's
// resource directory, and appends the path it opens to this file instead of starting a
// browser (src/help.rs).
const guideDir = process.env.MOCHI_E2E_GUIDE_DIR ?? path.resolve(here, "../../../target/guide");
if (!existsSync(path.join(guideDir, "index.html"))) {
  console.error(
    `missing user guide ${guideDir}: run python3 packaging/guide.py ${guideDir} from the repository root`,
  );
  process.exit(1);
}
process.env.MOCHI_E2E_GUIDE_DIR = guideDir;
process.env.MOCHI_E2E_OPEN_LOG = path.join(os.tmpdir(), "mochi-e2e-opened.txt");
// "Show in folder" records the folder it was asked to show here (src/help.rs `open_folder`).
process.env.MOCHI_E2E_FOLDER_LOG = path.join(os.tmpdir(), "mochi-e2e-folders.txt");
// "Replace existing files" (owner decision 2026-11-11) moves replaced files here in an e2e
// build, never into the runner's real Recycle Bin or Trash (src-tauri/src/trashbin.rs).
process.env.MOCHI_E2E_TRASH_DIR = path.join(os.tmpdir(), "mochi-e2e-bin");

// The cold-start spec (specs/add-cold.e2e.js) needs the app started WITH arguments, as Explorer
// starts it for "Add to archive...": a second session whose capabilities carry them. Each
// capability runs its own specs, so the other specs never see these arguments and this one never
// shares an app with them. (No "--" in the args: msedgedriver reads a bare "--" as an empty
// switch name and refuses the session; these paths do not start with "-".) Three files in one folder, in one launch.
const coldDir = path.join(os.tmpdir(), "mochi-e2e-cold", "trip");
process.env.MOCHI_E2E_COLD_DIR = coldDir;
const coldSpec = path.resolve(here, "specs/add-cold.e2e.js");
const coldArgs = ["--add", ...["a.txt", "b.txt", "c.txt"].map((n) => path.join(coldDir, n))];
// On Windows, tauri-driver hands `tauri:options.args` to msedgedriver as WebView2 *browser*
// arguments, never to the app (CI: the app started with no verb and showed the main window).
// There the test build (feature e2e) reads its arguments, once, from this file instead, which
// beforeSession fills for the cold spec only (`automation::launch_args`).
process.env.MOCHI_E2E_ARGS_FILE = path.join(os.tmpdir(), "mochi-e2e-args.txt");
const isWindows = process.platform === "win32";

let tauriDriver;

/**
 * The spec files that are about one tool, the More menu item that opens it, and for
 * Settings the section the spec is about (Settings shows one section at a time).
 */
const TOOL_OF_SPEC = [
  [/check\.e2e\.js$/, "menu-repair"],
  [/crash\.e2e\.js$/, "start-settings", "settings-tab-privacy"],
  [/updates\.e2e\.js$/, "start-settings", "settings-tab-updates"],
];

export const config = {
  runner: "local",
  hostname: "127.0.0.1",
  port: 4444,
  specs: ["./specs/**/*.e2e.js"],
  maxInstances: 1,
  capabilities: [
    {
      maxInstances: 1,
      "wdio:exclude": [coldSpec],
      "tauri:options": { application },
    },
    {
      maxInstances: 1,
      "wdio:specs": [coldSpec],
      "tauri:options": isWindows ? { application } : { application, args: coldArgs },
    },
  ],
  logLevel: "warn",
  reporters: ["spec"],
  framework: "mocha",
  mochaOpts: { ui: "bdd", timeout: 60_000 },
  waitforTimeout: 15_000,

  beforeSession: (_config, _caps, specs) => {
    // One line per argument; the app empties the file when it reads it.
    const cold = specs.some((s) => /add-cold\.e2e\.js$/.test(s));
    writeFileSync(process.env.MOCHI_E2E_ARGS_FILE, cold && isWindows ? coldArgs.join("\n") : "");
  },

  onPrepare: () => {
    // The files the cold start adds, made fresh.
    rmSync(path.dirname(coldDir), { recursive: true, force: true });
    mkdirSync(coldDir, { recursive: true });
    for (const n of ["a.txt", "b.txt", "c.txt"]) writeFileSync(path.join(coldDir, n), n);
    // No pick left over from an earlier run.
    writeFileSync(process.env.MOCHI_E2E_PICK_FILE, "");
    writeFileSync(process.env.MOCHI_E2E_OPEN_LOG, "");
    writeFileSync(process.env.MOCHI_E2E_FOLDER_LOG, "");
    writeFileSync(process.env.MOCHI_E2E_ARGS_FILE, "");
    // On PATH after `cargo install tauri-driver` (~/.cargo/bin).
    const bin = process.env.TAURI_DRIVER ?? "tauri-driver";
    tauriDriver = spawn(bin, driverArgs, { stdio: [null, process.stdout, process.stderr] });
    tauriDriver.on("error", (e) => {
      console.error(`tauri-driver failed to start (${bin}): ${e.message}`);
      process.exit(1);
    });
    // Give the driver a moment to listen before the first session.
    return new Promise((resolve) => setTimeout(resolve, 1500));
  },
  // The tools (Test and repair, Settings) open one at a time in a tool window over
  // the list, from the toolbar's More menu. Specs that are only about one tool just need it open;
  // the other specs open and close tools themselves, as a person would.
  beforeTest: async (test) => {
    const [, tool, tab] = TOOL_OF_SPEC.find(([re]) => re.test(test.file ?? "")) ?? [];
    if (tool === undefined) return;
    // The first test of a file can start before the page has rendered.
    await $('[data-testid="toolbar-more"]').waitForExist({ timeout: 60_000 });
    if (!(await $('[data-testid="tool-sheet"]').isDisplayed())) {
      await $('[data-testid="toolbar-more"]').click();
      await $(`[data-testid="${tool}"]`).click();
      await $('[data-testid="tool-sheet"]').waitForDisplayed();
    }
    if (tab !== undefined) await $(`[data-testid="${tab}"]`).click();
  },
  onComplete: () => {
    tauriDriver?.kill();
  },
};
