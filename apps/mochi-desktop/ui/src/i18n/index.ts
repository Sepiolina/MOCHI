// Message lookup (plan D7 i18n scaffolding). English, Thai and Simplified
// Chinese ship. The English catalogue defines the keys and placeholders; the
// others must match it (`satisfies Catalogue<...>`), so a key that does not
// exist, or a placeholder that is not supplied, fails `tsc` and the tests
// rather than rendering blank.
//
// Rules (docs: apps/mochi-desktop/README.md "Messages", ./README.md):
// - Only the catalogue's own templates are ever interpreted. Archive-supplied
//   strings (file names, labels, error messages from a foreign archive) are
//   *parameters*: they are inserted as plain text in a single pass and are
//   never scanned for `{placeholders}`, never used as a key, and never used
//   as a template (spec §23.3 #9).
// - Wording is the catalogue's; callers do not concatenate English around it.
import { useSyncExternalStore } from "react";

import { en } from "./en";
import { th } from "./th";
import { zhCN } from "./zh-CN";
import type { Catalogue } from "./types";

// ---- Locales -------------------------------------------------------------

export type Locale = "en" | "th" | "zh-CN";

/** Each language in its own name (never translated), for the selector. */
export const LOCALES: readonly { id: Locale; name: string }[] = [
  { id: "en", name: "English" },
  { id: "th", name: "ไทย" },
  { id: "zh-CN", name: "简体中文" },
];

/** The English catalogue defines the keys and placeholders; the others must match it. */
export const CATALOGUES: Record<Locale, Catalogue<typeof en>> = { en, th, "zh-CN": zhCN };

/** localStorage key of the saved choice (a per-computer convenience, like a remembered tab). */
export const LANGUAGE_KEY = "mochi.language";

export function isLocale(v: unknown): v is Locale {
  return v === "en" || v === "th" || v === "zh-CN";
}

/** th* -> Thai, zh* -> Simplified Chinese, anything else English. */
export function localeFromLanguageTags(tags: readonly string[]): Locale {
  for (const tag of tags) {
    const primary = tag.toLowerCase().split(/[-_]/)[0];
    if (primary === "th") return "th";
    if (primary === "zh") return "zh-CN";
    if (primary === "en") return "en";
  }
  return "en";
}

function savedLocale(): Locale | null {
  try {
    const v = window.localStorage.getItem(LANGUAGE_KEY);
    return isLocale(v) ? v : null;
  } catch {
    return null;
  }
}

/** The saved choice if there is one, else the OS/webview language. */
export function detectLocale(): Locale {
  const saved = savedLocale();
  if (saved) return saved;
  if (typeof navigator === "undefined") return "en";
  const tags = navigator.languages?.length ? navigator.languages : navigator.language ? [navigator.language] : [];
  return localeFromLanguageTags(tags);
}

let current: Locale = detectLocale();
const listeners = new Set<() => void>();

function applyDocumentLanguage() {
  if (typeof document !== "undefined") document.documentElement.lang = current;
}
applyDocumentLanguage();

export function getLocale(): Locale {
  return current;
}

/** Switch language. `persist` is false when following another window's change. */
export function setLocale(next: Locale, persist = true): void {
  if (persist) {
    try {
      window.localStorage.setItem(LANGUAGE_KEY, next);
    } catch {
      // Not remembered; the choice still applies until the app closes.
    }
  }
  if (next === current) return;
  current = next;
  applyDocumentLanguage();
  for (const listener of [...listeners]) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

// The quick window and the main window share storage: follow the other one.
if (typeof window !== "undefined") {
  window.addEventListener("storage", (e) => {
    if (e.key === LANGUAGE_KEY && isLocale(e.newValue)) setLocale(e.newValue, false);
  });
}

/**
 * Subscribe a component to language changes. The root of each window calls
 * this, so a change re-renders the whole tree in place (no reload, no lost
 * state). Module-level wording must therefore be a function, not a constant.
 */
export function useLocale(): Locale {
  return useSyncExternalStore(subscribe, getLocale, getLocale);
}

/** Format a number for display in the current language (grouping, digits). */
export function formatNumber(n: number | bigint): string {
  return n.toLocaleString(current);
}

/**
 * A recorded RFC 3339 time as a short local date and time in the current
 * language ("14 Nov 2026, 10:23"). A value the engine cannot read is shown as
 * recorded, never guessed at.
 */
export function formatTime(rfc3339: string): string {
  const d = new Date(rfc3339);
  if (Number.isNaN(d.getTime())) return rfc3339;
  return d.toLocaleString(current, { dateStyle: "medium", timeStyle: "short" });
}

// ---- Lookup --------------------------------------------------------------

export type MessageKey = keyof typeof en;

type Placeholders<S extends string> = S extends `${string}{${infer P}}${infer Rest}`
  ? P | Placeholders<Rest>
  : never;

type ParamsOf<K extends MessageKey> = Placeholders<(typeof en)[K]>;

/** `t(key)` for a template without placeholders, `t(key, { name })` for one with. */
export type MessageArgs<K extends MessageKey> = [ParamsOf<K>] extends [never]
  ? []
  : [params: { [P in ParamsOf<K>]: string | number }];

const PLACEHOLDER = /\{([A-Za-z0-9_]+)\}/g;

/**
 * Look up a message. The template comes from the catalogue; each placeholder
 * is replaced once, from `params`, and the replacement is not rescanned.
 */
export function t<K extends MessageKey>(key: K, ...args: MessageArgs<K>): string {
  const template: string = CATALOGUES[current][key];
  const params = (args[0] ?? {}) as Record<string, string | number>;
  return template.replace(PLACEHOLDER, (whole, name: string) => {
    const value = params[name];
    return value === undefined ? whole : String(value);
  });
}

/** The two keys of a count-dependent message: `one` when `n === 1`. */
export function tn<O extends MessageKey, M extends MessageKey>(
  n: number,
  one: O,
  many: M,
  ...args: MessageArgs<O> extends MessageArgs<M> ? MessageArgs<O> : never
): string {
  return (t as (key: MessageKey, ...a: unknown[]) => string)(n === 1 ? one : many, ...args);
}

/** Keys whose only placeholder is `{n}`: a count, formatted with grouping. */
export type CountKey = {
  [K in MessageKey]: [ParamsOf<K>] extends ["n"] ? K : never;
}[MessageKey];

/** "1 file" / "2 files": the count is formatted with grouping for the current language. */
export function tc(n: number, one: CountKey, many: CountKey): string {
  return (t as (key: MessageKey, ...a: unknown[]) => string)(n === 1 ? one : many, {
    n: formatNumber(n),
  });
}

/** Every template in the current catalogue, for tests over the whole wording. */
export function allMessages(): readonly string[] {
  return Object.values(CATALOGUES[current]);
}

/**
 * The wording of a closed vocabulary the backend reports as a code (`not_needed`,
 * `copy_confirmed`): `prefix` + `.` + the code is a catalogue key. A code the
 * catalogue does not know is shown as the backend spelled it, never guessed at.
 */
export function tCode(prefix: string, code: string): string {
  const catalogue: Record<string, string> = CATALOGUES[current];
  return catalogue[`${prefix}.${code}`] ?? code;
}

/** A job phase (core's phase identifier) in the current language; an unknown one reads "Working". */
export function phaseLabel(phase: string): string {
  const label = tCode("common.phase", phase);
  return label === phase ? t("common.phase.unknown") : label;
}
