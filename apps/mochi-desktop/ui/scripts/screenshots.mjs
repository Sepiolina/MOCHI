// Screenshots of the window in Chromium with the Tauri bridge stubbed, to look at the
// layout without the desktop shell. Not a test: nothing here asserts behaviour.
//
//   pnpm exec vite --port 5179 --strictPort &
//   node scripts/screenshots.mjs <out-dir> [url]
//
// Needs playwright-core and a Chromium (PLAYWRIGHT_CORE, CHROMIUM below). The fake
// archive lists a hostile name; the page must still have no <img> and no sideways scroll.
import { mkdirSync } from "node:fs";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_CORE ?? "/opt/node-tools/node_modules/playwright-core");

const out = process.argv[2] ?? "screenshots";
const url = process.argv[3] ?? "http://localhost:5179/";
mkdirSync(out, { recursive: true });

/** The stub, as a string so it runs in the page before the app does. */
const stub = () => {
  const hex = (s) => Array.from(new TextEncoder().encode(s), (b) => b.toString(16).padStart(2, "0")).join("");
  const HOSTILE = "<img src=x onerror=alert(1)>";
  const names = ["Documents", "Photos", "Music", "notes", HOSTILE];
  const files = Array.from({ length: 60 }, (_, i) => `report-${String(i + 1).padStart(2, "0")}.${["pdf", "txt", "png", "xlsx", "zip"][i % 5]}`);
  const rows = [
    ...names.slice(0, 3).map((n) => ({ key: hex(n), name: { text: n, verbatim: true }, kind: "directory", size: "0", child_dirs: 2, child_files: 14 })),
    { key: hex(HOSTILE), name: { text: HOSTILE, verbatim: true }, kind: "file", size: "1536", child_dirs: 0, child_files: 0 },
    ...files.map((n, i) => ({ key: hex(n), name: { text: n, verbatim: true }, kind: "file", size: String(900 + i * 7919), child_dirs: 0, child_files: 0 })),
  ];
  const mochi = {
    handle: 3,
    file_name: "holiday-2026.mochi",
    archive_id: "ab".repeat(16),
    profile: { tar_compatible: false, encrypted: false, redundancy_percent: null },
    commits: [0, 1, 2].map((seq) => ({ seq, commit_id: String(seq).repeat(64), time: "2026-10-1" + (seq + 1) + "T10:00:00Z", checkpoint: seq !== 1, retained: true, expired: false, holds: [] })),
    retention_error: null,
  };
  const foreign = {
    handle: 5, file_name: "photos.zip", format: "ZIP", entries: 64, complete: true, stop_reason: null, notes: [],
    checks: "CRC32 per entry", opened_with_password: false, weak_encryption: false,
    dimensions: { recoverability: "UNSUPPORTED", freshness: "UNSUPPORTED", retention: "UNSUPPORTED" },
  };
  const frows = rows.map((r) => ({ ...r, encryption: null, note: null }));
  const callbacks = new Map();
  let nextId = 1;
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: "main" }, currentWebview: { windowLabel: "main", label: "main" } },
    transformCallback(cb) { const id = nextId++; callbacks.set(id, cb); return id; },
    unregisterCallback(id) { callbacks.delete(id); },
    runCallback(id, data) { callbacks.get(id)?.(data); },
    convertFileSrc: (p) => p,
    async invoke(cmd, args = {}) {
      switch (cmd) {
        case "staged_sources": return { items: [], destination: null, default_destination: null };
        case "get_crash_settings": return { enabled: false, include_details: false, entries: 0, bytes: 0 };
        case "recent_archives": return [{ index: 0, name: "holiday-2026.mochi", folder: "/home/me/Archives" }, { index: 1, name: "taxes.mochi", folder: "/home/me/Documents" }];
        case "get_update_settings": return { check_on_start: false, current_version: "0.1.0", install_mode: "manual", download_page: "https://github.com/immerh8/MOCHI/releases/latest", needs_admin_prompt: false };
        case "open_archive": return { status: "opened", archive: mochi };
        case "open_snapshot": return { seq: args.seq, commit_id: "c".repeat(64), entries: 437, root_dirs: 3, root_files: 61, catalog_source: { source: "image" } };
        case "list_children": {
          const pool = args.dir ? [] : args.directoriesOnly ? rows.filter((r) => r.kind === "directory") : rows;
          return { total: pool.length, offset: args.offset, entries: pool.slice(args.offset, args.offset + args.limit) };
        }
        case "entry_details": return { key: args.key, path: { text: HOSTILE, verbatim: true }, kind: "file", size: "1536", content_hash: "d".repeat(64), file_version_id: "e".repeat(64), attributes: { state: "recorded", posix: { mode: 0o644, uid: 1000, gid: 1000 }, windows: null, mtime: "2026-10-12T10:00:00Z" } };
        case "open_foreign_archive": return foreign;
        case "foreign_children": return { total: frows.length, offset: args.offset, entries: frows.slice(args.offset, args.offset + args.limit) };
        case "close_archive": case "close_foreign_archive": return true;
        default: return null;
      }
    },
  };
};

const browser = await chromium.launch({ executablePath: process.env.CHROMIUM ?? "/opt/pw-browsers/chromium", args: ["--no-sandbox"] });
const sizes = [[1100, 720], [760, 600]];
const report = [];
for (const [w, h] of sizes) {
  const page = await browser.newPage({ viewport: { width: w, height: h } });
  page.on("pageerror", (e) => report.push(`pageerror ${w}x${h}: ${e.message}`));
  await page.addInitScript(stub);
  const shot = async (name) => {
    await page.screenshot({ path: `${out}/${name}-${w}x${h}.png` });
    const facts = await page.evaluate(() => ({
      imgs: document.querySelectorAll("img").length,
      hrefs: document.querySelectorAll("[href]").length,
      hscroll: document.documentElement.scrollWidth > document.documentElement.clientWidth,
      bodyScroll: document.documentElement.scrollHeight > document.documentElement.clientHeight,
    }));
    report.push(`${name} ${w}x${h}: ${JSON.stringify(facts)}`);
  };
  await page.goto(url);
  await page.waitForSelector('[data-testid="start-screen"]');
  await shot("1-empty");
  await page.click('[data-testid="toolbar-more"]');
  await shot("2-more-menu");
  await page.keyboard.press("Escape");
  await page.click('[data-testid="open-archive"]');
  await page.waitForSelector('[data-testid="entry"]');
  await shot("3-open");
  await page.click('[data-testid="entry"] >> nth=3');
  await page.waitForSelector('[data-testid="details"]', { state: "attached" });
  await shot("4-selected");
  await page.click('[data-testid="toolbar-info"]');
  await page.click('[data-testid="toggle-folders"]');
  await shot("5-info-folders");
  await page.click('[data-testid="toolbar-info"]');
  await page.click('[data-testid="toggle-folders"]');
  await page.click('[data-testid="toggle-versions"]');
  await shot("6-versions");
  await page.keyboard.press("Escape");
  await page.click('[data-testid="add-to-archive"]');
  await page.waitForSelector('[data-testid="write-panel"]');
  await shot("7-add-panel");
  await page.click('[data-testid="write-close"]');
  await page.click('[data-testid="toolbar-help"]');
  await shot("8-help");
  await page.click('[data-testid="toolbar-help"]');
  await page.click('[data-testid="search-archive"]');
  await shot("8b-advanced");
  await page.evaluate(() => { document.querySelector('[data-testid="advanced"]').open = false; });
  await page.click('[data-testid="close-archive"]');
  await page.click('[data-testid="toolbar-more"]');
  await page.click('[data-testid="menu-foreign"]');
  await page.waitForSelector('[data-testid="foreign-entry"]');
  await shot("9-foreign");
  await page.click('[data-testid="foreign-entry"] >> nth=4');
  await shot("10-foreign-selected");
  await page.click('[data-testid="toolbar-info"]');
  await shot("11-foreign-info");
  await page.close();
}
await browser.close();
console.log(report.join("\n"));
