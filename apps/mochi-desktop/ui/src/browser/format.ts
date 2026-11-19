// Presentation helpers. Sizes arrive as decimal strings (an archive may
// claim values above 2^53), so they are handled as BigInt.
import { formatNumber, t } from "../i18n";
import { isIpcError } from "../ipc/jobs";

const UNITS = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/** The exact byte count, and (from 1 KiB up) the same size in binary units. */
function sizeParts(decimal: string): { exact: string; human: string | null } | string {
  let n: bigint;
  try {
    n = BigInt(decimal);
  } catch {
    return decimal;
  }
  const exact = t(n === 1n ? "common.byteOne" : "common.bytesMany", { n: formatNumber(n) });
  if (n < 1024n) return { exact, human: null };
  let unit = 0;
  let base = 1024n;
  while (unit < UNITS.length - 1 && n >= base * 1024n) {
    base *= 1024n;
    unit += 1;
  }
  // Tenths of the unit, rounded down, in integers.
  const tenths = (n * 10n) / base;
  const human = t("common.sizeHuman", {
    value: `${(tenths / 10n).toString()}.${(tenths % 10n).toString()}`,
    unit: UNITS[unit] ?? "",
  });
  return { exact, human };
}

/** "1,234 bytes", or "1.5 MiB (1,572,864 bytes)" from 1 KiB up. */
export function formatSize(decimal: string): string {
  const p = sizeParts(decimal);
  if (typeof p === "string") return p;
  return p.human === null ? p.exact : t("common.sizeWithExact", { human: p.human, exact: p.exact });
}

/** Compact size for a list column. */
export function shortSize(decimal: string): string {
  const p = sizeParts(decimal);
  if (typeof p === "string") return p;
  return p.human ?? p.exact;
}

/** `-rw-r--r--`-style permission bits plus the octal mode. */
export function formatMode(mode: number): string {
  const bits = ["r", "w", "x"];
  let s = "";
  for (let i = 8; i >= 0; i--) {
    s += mode & (1 << i) ? bits[(8 - i) % 3] : "-";
  }
  return `${s} (${(mode & 0o7777).toString(8).padStart(4, "0")})`;
}

/** An IPC error as one sentence; anything else as a generic failure. */
export function errorText(what: string, e: unknown): string {
  return isIpcError(e)
    ? t("common.errorWithCode", { what, code: e.code, message: e.message })
    : t("common.errorPlain", { what });
}
