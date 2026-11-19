import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import { setLocale } from "../i18n";
import { en } from "../i18n/en";
import { th } from "../i18n/th";
import { zhCN } from "../i18n/zh-CN";
import { openArchive } from "./browse";
import { exportCrashLog } from "./crash";
import { chooseDestination, stageSources } from "./write";
import { chooseCheckSubject, exportReport, repairSetup } from "./check";
import { planExtract } from "./extract";
import { chooseQuickLocation } from "./quick";
import { openForeignArchive, planExtractForeign } from "./foreign";

// The native dialogs are Rust's. The window passes the title (and filter
// labels) in its own language; Rust keeps the English default for anything it
// refuses (picker::dialog_text_tests).

afterEach(() => {
  clearMocks();
  setLocale("en");
  window.localStorage.clear();
});

function record() {
  const calls: { cmd: string; text: unknown }[] = [];
  mockIPC((cmd, args) => {
    calls.push({ cmd, text: (args as { text?: unknown } | undefined)?.text });
    return null;
  });
  return calls;
}

test("the open dialog gets its title and file-type labels in Thai", async () => {
  setLocale("th");
  const calls = record();
  await openArchive();
  expect(calls).toEqual([
    {
      cmd: "open_archive",
      text: {
        title: th["dialog.openArchive"],
        filter: th["dialog.filterArchives"],
        mochi_archive: th["dialog.filterMochi"],
        all_files: th["dialog.filterAll"],
      },
    },
  ]);
  expect(th["dialog.openArchive"]).not.toBe("Open an archive");
});

test("every dialog command carries a localized title, and the language can change between calls", async () => {
  const calls = record();
  setLocale("th");
  await stageSources("files");
  await stageSources("folder");
  await chooseDestination("zip");
  await chooseCheckSubject({ from: "dialog" });
  await exportReport(1);
  await repairSetup({ action: "add_copy" });
  await repairSetup({ action: "choose_output" });
  await planExtract(1, 1, [], true);
  await planExtractForeign(1, [], true);
  await openForeignArchive(false, null);
  await chooseQuickLocation("x", "tar_gz");
  await exportCrashLog();
  const titles = calls.map((c) => (c.text as { title?: string } | undefined)?.title);
  expect(titles).toEqual([
    th["dialog.chooseFiles"],
    th["dialog.chooseFolder"],
    th["dialog.saveArchiveAs"],
    th["dialog.chooseSubject"],
    th["dialog.exportReport"],
    th["dialog.chooseCopy"],
    th["dialog.saveArchiveAs"],
    th["dialog.extractTo"],
    th["dialog.extractTo"],
    th["dialog.openForeign"],
    th["dialog.saveArchiveAs"],
    th["dialog.exportCrashLog"],
  ]);
  expect(calls[2]?.text).toMatchObject({ filter: th["dialog.filterZip"] });
  expect(calls[10]?.text).toMatchObject({ filter: th["dialog.filterTarGz"] });

  calls.length = 0;
  setLocale("zh-CN");
  await chooseDestination();
  expect(calls[0]?.text).toEqual({ title: zhCN["dialog.saveArchiveAs"], filter: zhCN["dialog.filterMochi"] });
});

test("English is what the dialogs say in English, and every label is short plain text", async () => {
  const calls = record();
  await openArchive();
  expect(calls[0]?.text).toMatchObject({ title: "Open an archive", filter: "Archives", all_files: "All files" });
  for (const locale of ["en", "th", "zh-CN"] as const) {
    setLocale(locale);
    for (const [key, value] of Object.entries({ en, th, "zh-CN": zhCN }[locale])) {
      if (!key.startsWith("dialog.")) continue;
      expect(value.length, key).toBeLessThanOrEqual(120);
      expect(value, key).not.toMatch(/\p{Cc}/u);
    }
  }
});
