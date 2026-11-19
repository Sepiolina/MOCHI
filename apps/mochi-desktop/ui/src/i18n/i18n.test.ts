// The message catalogue as a whole (plan D7 i18n scaffolding; spec §23.3).
/// <reference types="vite/client" />
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { afterEach, expect, test, vi } from "vitest";

import {
  allMessages,
  CATALOGUES,
  detectLocale,
  formatNumber,
  getLocale,
  LANGUAGE_KEY,
  localeFromLanguageTags,
  LOCALES,
  setLocale,
  t,
  tc,
  tn,
} from "./index";
import type { Locale } from "./index";
import { en } from "./en";

/**
 * Words the app never uses about an archive, a repair, or a passphrase
 * (§23.3 #4, #6; AGENTS.md "Encrypted profile"). Applies to every template in
 * every catalogue, so a new screen cannot add one by the back door.
 *
 * Translators must follow the same §23.3 wording rules in their language:
 * never say an archive is backed up, safe, secure or preserved, that a
 * passphrase was revoked, or that something was repaired or recovered
 * completely. These lists are the mechanical backstop for the most direct
 * words; they are not the whole rule (see ./README.md).
 */
const BANNED: Record<Locale, string[]> = {
  en: [
    "backed up",
    "backup",
    "safe",
    "preserved",
    "secure",
    "revoked",
    "repaired",
    "recovered completely",
    "fully recovered",
  ],
  th: [
    "สำรองข้อมูล", // backup
    "แบ็กอัป",
    "แบ็คอัพ",
    "แบคอัพ",
    "ปลอดภัย", // safe / secure
    "เก็บรักษาแล้ว", // preserved
    "เพิกถอน", // revoke
    "ซ่อมแซมแล้ว", // repaired
    "กู้คืนทั้งหมดแล้ว", // fully recovered
    "กู้คืนสมบูรณ์",
  ],
  "zh-CN": [
    "已备份", // backed up
    "备份", // backup
    "安全", // safe / secure
    "已保存完好",
    "已保全", // preserved
    "已撤销", // revoked
    "撤销", // revoke
    "已修复", // repaired
    "完全恢复", // fully recovered
  ],
};

const LOCALE_IDS = LOCALES.map((l) => l.id);

test("no catalogue entry uses a banned word, in any language (§23.3)", () => {
  for (const locale of LOCALE_IDS) {
    for (const [key, text] of Object.entries(CATALOGUES[locale])) {
      for (const word of BANNED[locale]) {
        expect(text.toLowerCase(), `${locale}: "${word}" in ${key}: ${text}`).not.toContain(word);
      }
    }
  }
});

const placeholders = (text: string) => [...text.matchAll(/\{([A-Za-z0-9_]+)\}/g)].map((m) => m[1] ?? "").sort();

test("every translation has exactly the English keys and the same placeholders", () => {
  for (const locale of LOCALE_IDS) {
    const cat = CATALOGUES[locale] as Record<string, string>;
    expect(Object.keys(cat).sort(), locale).toEqual(Object.keys(en).sort());
    for (const [key, english] of Object.entries(en)) {
      const text = cat[key] ?? "";
      expect(text.length, `${locale}: ${key}`).toBeGreaterThan(0);
      expect(placeholders(text), `${locale}: ${key}: ${text}`).toEqual(placeholders(english));
      expect(text.replace(/\{[A-Za-z0-9_]+\}/g, ""), `${locale}: ${key}`).not.toMatch(/[{}]/);
    }
  }
});

test("the catalogue has English text and well-formed placeholders", () => {
  expect(allMessages().length).toBeGreaterThan(300);
  for (const [key, text] of Object.entries(en)) {
    expect(text.length, key).toBeGreaterThan(0);
    // Every brace belongs to a `{name}` placeholder.
    expect(text.replace(/\{[A-Za-z0-9_]+\}/g, ""), key).not.toMatch(/[{}]/);
  }
});

test("a placeholder is replaced once, and a value is never read as a template", () => {
  // File names are archive-supplied (§23.3 #9): braces in one stay braces.
  expect(t("browser.isEncrypted", { name: "{name} {x}" })).toBe("{name} {x} is encrypted");
  expect(t("browser.retentionError", { code: "{message}", message: "{code}" })).toBe(
    "Retention could not be determined ({message}): {code}",
  );
  // A lookup is by the catalogue's own key only: the translator takes no
  // archive string as a key or as a template.
  expect(t("common.errorPlain", { what: "common.cancel" })).toBe("common.cancel.");
});

test("counts pick the one or the many form", () => {
  expect(tn(1, "browser.metaCommitOne", "browser.metaCommitMany", { n: 1 })).toBe("1 version");
  expect(tn(2, "browser.metaCommitOne", "browser.metaCommitMany", { n: 2 })).toBe("2 versions");
});

test("a key that is not in the catalogue is a type error", () => {
  // These calls are never run: they exist so that `tsc` (pnpm typecheck)
  // fails if the lookup ever stops rejecting a missing key or a missing value.
  const neverCalled = () => {
    // @ts-expect-error  no such key
    t("no.such.key");
    // @ts-expect-error  the placeholder {seq} is not supplied
    t("browser.commit");
    // @ts-expect-error  and a misspelled one is not accepted either
    t("browser.commit", { sequence: 1 });
  };
  expect(typeof neverCalled).toBe("function");
});

// Every screen and helper (not tests, generated IPC types, or the catalogue).
const SOURCES = import.meta.glob(
  ["../**/*.{ts,tsx}", "!../**/*.test.{ts,tsx}", "!../ipc/generated/**", "!../i18n/**", "!../test/**"],
  { query: "?raw", import: "default", eager: true },
) as Record<string, string>;

test("every catalogue key is used, so the catalogue does not collect dead wording", () => {
  const text = Object.values(SOURCES).join("\n");
  expect(Object.keys(SOURCES).length).toBeGreaterThan(20);
  // Keys built from a value (`status.${s}`) are named by their prefix.
  const dynamic = [
    "status.",
    "status.meaning.",
    "status.mark.",
    "dimension.",
    "check.outcome.",
    "check.enum.",
    "common.phase.",
    "find.split.",
  ];
  const unused = Object.keys(en).filter(
    (k) => !text.includes(`"${k}"`) && !dynamic.some((p) => k.startsWith(p)),
  );
  expect(unused).toEqual([]);
});

test("no message parameter is an English word or phrase written in the code", () => {
  // `t("key", { them: n === 1 ? "it" : "them" })` would leak English into Thai
  // and Chinese. Words belong in the catalogue, as separate full sentences;
  // a parameter is archive data, a number, or the result of another lookup.
  const call = /\bt[nc]?\(\s*(?:[^,()]+,\s*)*?(?:"[A-Za-z._-]+"|`[^`]*`)\s*,\s*(?:[^{}]*,\s*)?\{([^{}]*)\}/g;
  const offenders: string[] = [];
  for (const [file, text] of Object.entries(SOURCES)) {
    for (const m of text.matchAll(call)) {
      const params = m[1] ?? "";
      if (/(?:[:?]\s*)"[^"]*[A-Za-z][^"]*"/.test(params)) offenders.push(`${file}: ${params.trim()}`);
    }
  }
  expect(offenders).toEqual([]);
});

test("the page declares a default language and has a title", () => {
  const html = readFileSync(join(process.cwd(), "index.html"), "utf8");
  expect(html).toContain('<html lang="en">');
  expect(html).toMatch(/<title>MOCHI<\/title>/);
});

// ---- Runtime language ------------------------------------------------------

function setNavigatorLanguages(languages: string[]) {
  Object.defineProperty(window.navigator, "languages", { value: languages, configurable: true });
}

afterEach(() => {
  window.localStorage.clear();
  setNavigatorLanguages(["en-US"]);
  setLocale("en");
  window.localStorage.clear();
});

test("th* maps to Thai, zh* to Simplified Chinese, anything else to English", () => {
  expect(localeFromLanguageTags(["th-TH"])).toBe("th");
  expect(localeFromLanguageTags(["th"])).toBe("th");
  expect(localeFromLanguageTags(["zh-CN"])).toBe("zh-CN");
  expect(localeFromLanguageTags(["zh-TW"])).toBe("zh-CN");
  expect(localeFromLanguageTags(["zh-Hans-CN"])).toBe("zh-CN");
  expect(localeFromLanguageTags(["de-DE", "th"])).toBe("th");
  expect(localeFromLanguageTags(["fr-FR"])).toBe("en");
  expect(localeFromLanguageTags([])).toBe("en");
});

test("detection prefers a saved choice, then the webview language", () => {
  setNavigatorLanguages(["th-TH", "en"]);
  expect(detectLocale()).toBe("th");
  window.localStorage.setItem(LANGUAGE_KEY, "zh-CN");
  expect(detectLocale()).toBe("zh-CN");
  window.localStorage.setItem(LANGUAGE_KEY, "klingon");
  expect(detectLocale()).toBe("th");
});

test("switching language changes lookups and the document language, and is remembered", () => {
  expect(t("common.cancel")).toBe("Cancel");
  setLocale("th");
  expect(getLocale()).toBe("th");
  expect(document.documentElement.lang).toBe("th");
  expect(window.localStorage.getItem(LANGUAGE_KEY)).toBe("th");
  expect(detectLocale()).toBe("th");
  // Until a translator replaces the text the Thai catalogue holds the English
  // wording; the lookup must read the current catalogue all the same.
  const cat = CATALOGUES.th as Record<string, string>;
  const original = cat["common.cancel"];
  cat["common.cancel"] = "TH-CANCEL";
  try {
    expect(t("common.cancel")).toBe("TH-CANCEL");
    setLocale("en");
    expect(t("common.cancel")).toBe("Cancel");
    expect(document.documentElement.lang).toBe("en");
  } finally {
    if (original !== undefined) cat["common.cancel"] = original;
  }
});

test("numbers follow the current language", () => {
  expect(formatNumber(1234567)).toBe("1,234,567");
  expect(tc(1234, "common.byteOne", "common.bytesMany")).toBe("1,234 bytes");
  setLocale("zh-CN");
  expect(formatNumber(1234567)).toBe((1234567).toLocaleString("zh-CN"));
});

test("a storage that throws still lets the language switch", () => {
  const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new Error("blocked");
  });
  try {
    expect(() => setLocale("zh-CN")).not.toThrow();
    expect(getLocale()).toBe("zh-CN");
  } finally {
    spy.mockRestore();
  }
});
