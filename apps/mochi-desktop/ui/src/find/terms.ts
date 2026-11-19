// Turning what the user typed or pasted into the list of names MOCHI Find looks for
// (spec Annex B.2.21). Pure: the component shows what this returns, and Rust checks the
// list again (at most MAX_TERMS names of at most MAX_TERM_BYTES bytes each).

/** How the pasted text is cut into names. */
export type Separator = "auto" | "lines" | "commas" | "semicolons" | "tabs" | "spaces";

export const SEPARATORS: readonly Separator[] = ["auto", "lines", "commas", "semicolons", "tabs", "spaces"];

/** Most names one search takes (Rust's `MAX_TERMS`). */
export const MAX_TERMS = 1_000;
/** Most bytes in one name (Rust's `MAX_QUERY_BYTES`). */
export const MAX_TERM_BYTES = 1_024;

const PATTERNS: Record<Separator, RegExp> = {
  // Line breaks, commas, semicolons and tabs: what a spreadsheet column, a CSV
  // row or a chat message holds. Not spaces: file names have them.
  auto: /\r\n|[\n\r,;\t]/,
  lines: /\r\n|[\n\r]/,
  commas: /,/,
  semicolons: /;/,
  tabs: /\t/,
  spaces: /\s+/,
};

export interface ParsedTerms {
  /** The names to look for, in the order given, each once. */
  terms: string[];
  /** Names left out because they repeat an earlier one. */
  duplicates: number;
  /** Names left out because they are longer than MAX_TERM_BYTES. */
  tooLong: number;
  /** Names left out because the list is longer than MAX_TERMS. */
  overLimit: number;
}

const encoder = new TextEncoder();

/** `"a name"` or `'a name'` without its quotes (Explorer's "Copy as path" adds them). */
function unquote(s: string): string {
  if (s.length >= 2) {
    const first = s[0];
    if ((first === '"' || first === "'") && s.endsWith(first)) return s.slice(1, -1).trim();
  }
  return s;
}

/** The last part of a path, by either separator (for "File names only"). */
export function lastComponent(s: string): string {
  const parts = s.split(/[\\/]/).filter((p) => p !== "");
  return parts.at(-1) ?? s;
}

export interface ParseOptions {
  separator: Separator;
  /** Names that differ only in case are the same name (when Match case is off). */
  ignoreCase: boolean;
  /** Keep only each line's last path component: `C:\docs\a.pdf` becomes `a.pdf`. */
  namesOnly: boolean;
}

/**
 * Cut `text` into names: split by the separator, trim, drop quotes and empty
 * pieces, optionally keep only the file name, and drop repeats. The rest is
 * kept exactly as written: names are compared by Rust, never normalized here.
 */
export function parseTerms(text: string, options: ParseOptions): ParsedTerms {
  const seen = new Set<string>();
  const out: ParsedTerms = { terms: [], duplicates: 0, tooLong: 0, overLimit: 0 };
  for (const piece of text.split(PATTERNS[options.separator])) {
    let name = unquote(piece.trim());
    if (options.namesOnly) name = lastComponent(name).trim();
    if (name === "") continue;
    if (encoder.encode(name).length > MAX_TERM_BYTES) {
      out.tooLong += 1;
      continue;
    }
    const key = options.ignoreCase ? name.toLowerCase() : name;
    if (seen.has(key)) {
      out.duplicates += 1;
      continue;
    }
    seen.add(key);
    if (out.terms.length >= MAX_TERMS) {
      out.overLimit += 1;
      continue;
    }
    out.terms.push(name);
  }
  return out;
}

/** Whether pasted text holds more than one name, so the list is the better place for it. */
export function looksLikeList(text: string): boolean {
  return parseTerms(text, { separator: "auto", ignoreCase: false, namesOnly: false }).terms.length > 1 && /[\n\r,;\t]/.test(text);
}
