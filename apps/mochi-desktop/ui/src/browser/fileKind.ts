// Which icon, and which "Type" label, a listing row gets. Purely presentational.
//
// An entry's name comes from the archive and is untrusted. It never reaches the
// DOM through here except as text: the extension is looked up in a fixed table
// (a `Map`, so a name like `a.constructor` or `a.__proto__` finds nothing) that
// yields one of a fixed set of icon kinds, and the Type column shows the
// extension only when it is a short run of ASCII letters and digits.
import type { FileIconKind } from "../icons";
import { t } from "../i18n";

const GROUPS: [FileIconKind, string[]][] = [
  ["image", ["png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "webp", "svg", "ico", "heic", "raw"]],
  ["audio", ["mp3", "wav", "flac", "ogg", "oga", "opus", "m4a", "aac", "wma", "aif", "aiff", "alac", "ape", "mid", "midi", "amr"]],
  ["video", ["mp4", "m4v", "mkv", "mov", "avi", "wmv", "webm", "flv", "mpg", "mpeg", "3gp"]],
  ["archive", ["zip", "7z", "rar", "tar", "gz", "tgz", "zst", "tzst", "xz", "bz2", "mochi", "iso", "cab"]],
  ["code", ["rs", "ts", "tsx", "js", "jsx", "py", "c", "h", "cpp", "hpp", "cs", "go", "java", "kt", "rb", "php", "sh", "ps1", "bat", "html", "css", "sql", "lua", "swift"]],
  ["sheet", ["csv", "tsv", "xls", "xlsx", "xlsm", "ods"]],
  ["pdf", ["pdf"]],
  ["text", ["txt", "md", "rtf", "log", "doc", "docx", "odt", "json", "xml", "yaml", "yml", "toml", "ini"]],
];

const BY_EXTENSION: ReadonlyMap<string, FileIconKind> = new Map(
  GROUPS.flatMap(([kind, exts]) => exts.map((ext): [string, FileIconKind] => [ext, kind])),
);

/** The longest extension the Type column shows. */
const MAX_EXTENSION = 5;

/**
 * The extension of a name, lower-cased, or `null` when there is none worth
 * showing: no dot, a leading dot only (`.bashrc`), a trailing dot, or anything
 * other than 1 to 5 ASCII letters and digits.
 */
export function extensionOf(name: string): string | null {
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return null;
  const ext = name.slice(dot + 1);
  return ext.length >= 1 && ext.length <= MAX_EXTENSION && /^[A-Za-z0-9]+$/.test(ext)
    ? ext.toLowerCase()
    : null;
}

/** The icon for an entry: from its kind, and for a file from its extension. */
export function iconKindOf(kind: "directory" | "file" | "link" | "special", name: string): FileIconKind {
  if (kind === "directory") return "folder";
  if (kind === "link") return "link";
  if (kind === "special") return "generic";
  const ext = extensionOf(name);
  return (ext === null ? undefined : BY_EXTENSION.get(ext)) ?? "generic";
}

/** The Type column: "Folder", the extension in capitals (`TIF`), or a plain kind word. */
export function typeLabelOf(kind: "directory" | "file" | "link" | "special", name: string): string {
  switch (kind) {
    case "directory":
      return t("common.folder");
    case "link":
      return t("foreign.kindLink");
    case "special":
      return t("foreign.kindSpecial");
    case "file": {
      const ext = extensionOf(name);
      return ext === null ? t("common.file") : ext.toUpperCase();
    }
  }
}
