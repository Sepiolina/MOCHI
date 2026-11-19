// How the transfer card words its time line. Nothing here is a verification or a result.
import { shortSize } from "../browser/format";
import { formatNumber, phaseLabel, t, tc } from "../i18n";
import { estimate, percentOf, phaseKind, spanOf } from "./rate";
import type { RateState, Span } from "./rate";

/** "56 seconds", "3 minutes", "1 hour 5 minutes". */
export function spanText(span: Exclude<Span, { kind: "less" }>): string {
  switch (span.kind) {
    case "seconds":
      return tc(span.n, "transfer.secondOne", "transfer.secondMany");
    case "minutes":
      return tc(span.n, "transfer.minuteOne", "transfer.minuteMany");
    case "hours": {
      const hours = tc(span.hours, "transfer.hourOne", "transfer.hourMany");
      return span.minutes === 0
        ? hours
        : t("transfer.spanJoin", { hours, minutes: tc(span.minutes, "transfer.minuteOne", "transfer.minuteMany") });
    }
  }
}

/** "About 56 seconds left", "Less than 5 seconds left". */
export function etaText(seconds: number): string {
  const span = spanOf(seconds);
  return span.kind === "less" ? t("transfer.etaLess") : t("transfer.etaAbout", { span: spanText(span) });
}

/** "23.1 MiB/s". */
export function rateText(bytesPerSecond: number): string {
  return t("transfer.rate", { size: shortSize(String(Math.round(bytesPerSecond))) });
}

/** "296.7 MiB of 528.9 MiB". */
export function bytesOfText(done: number, total: number): string {
  return t("transfer.bytesOf", { done: shortSize(String(done)), total: shortSize(String(total)) });
}

export interface TransferView {
  /** The big heading: "38% complete", "Finishing…", or nothing. */
  heading: string | null;
  /** 0 to 1 for the bar; `null` is an indeterminate bar. */
  fraction: number | null;
  /** The time line while running: time left (and speed for bytes), or "Calculating…". */
  timeLine: string | null;
  finishing: boolean;
}

/**
 * What the card shows for one progress sample. The bulk phase (bytes or items with a total) gets
 * a percentage, a time estimate and, for bytes, a speed; the short steps after it read
 * "Finishing…" with a full bar instead of dropping back to 0%.
 */
export function transferView(
  rate: RateState,
  phase: string,
  completed: number,
  total: number | null,
): TransferView {
  const kind = phaseKind(phase);
  if (phase === "") return { heading: t("common.starting"), fraction: null, timeLine: null, finishing: false };
  if (kind === "finishing") {
    return {
      heading: t("transfer.finishing"),
      fraction: rate.bulkSeen ? 1 : null,
      timeLine: phaseLabel(phase),
      finishing: true,
    };
  }
  const percent = kind === "scan" ? null : percentOf(completed, total);
  if (percent === null || total === null) {
    return { heading: null, fraction: null, timeLine: null, finishing: false };
  }
  const est = estimate(rate, completed, total);
  let timeLine: string;
  if (est === null || est.secondsLeft === null) {
    timeLine = t("transfer.calculating");
  } else {
    timeLine = etaText(est.secondsLeft);
    if (kind === "bytes") timeLine += t("common.statusSeparator") + rateText(est.perSecond);
  }
  return {
    heading: t("transfer.percent", { n: formatNumber(percent) }),
    fraction: Math.min(1, Math.max(0, completed / total)),
    timeLine,
    finishing: false,
  };
}
