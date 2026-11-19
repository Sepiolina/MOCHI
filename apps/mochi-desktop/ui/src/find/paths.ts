import { getLocale, t } from "../i18n";

/** The last component of a path as the file manager shows it (either separator). */
export function baseName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;
}

/** The archive's modified time in the current language, or "unknown". */
export function formatWhen(ms: number | null): string {
  if (ms === null) return t("find.noTime");
  try {
    return new Intl.DateTimeFormat(getLocale(), { dateStyle: "medium", timeStyle: "short" }).format(ms);
  } catch {
    return t("find.noTime");
  }
}
