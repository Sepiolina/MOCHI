// The data behind a compact result (owner request, release 0.9.3): the one
// line a screen shows, and the rows of its Details panel. Wording is built in
// each screen's `messages.ts`; this file holds the shape and the helpers they
// share. Everything here is text: archive-supplied strings stay `NameText` or
// plain strings and are rendered as text nodes (spec §23.3 #9).
import { shortSize } from "../browser/format";
import { t, tc } from "../i18n";
import type { NameText } from "../ipc/browse";

/** One entry of a list row: an archive-supplied name and/or a reason. */
export interface DetailItem {
  name?: NameText;
  /** A stable error code, shown muted after the name. */
  code?: string;
  text?: string;
}

/** One label/value row of the Details panel. */
export interface DetailRow {
  label: string;
  value?: string;
  /** Small muted technical text after the value. */
  sub?: string;
  /** A short reason shown under the value. */
  note?: string;
  /**
   * The row is a dimension or an exception that is not fully good (degraded,
   * not stored, not applied). It gets a caution marker and counts toward the
   * badge on the Details button, so an overall line cannot hide it.
   */
  caution?: boolean;
  items?: DetailItem[];
  /** How many more items there were than are listed. */
  more?: number;
  testId?: string;
  itemTestId?: string;
}

export interface ResultDetails {
  rows: DetailRow[];
  /** Short paragraphs under the rows. */
  notes: string[];
}

/** What a finished job shows: one line, whether it is partial, and its details. */
export interface ResultView {
  line: string;
  /** Some part of the job did not happen (items not stored, not opened again). */
  warn: boolean;
  details: ResultDetails;
  /** The technical record, for the console. */
  record: string;
}

/** How many notes need the user's eye: the badge on the Details button. */
export function cautionCount(details: ResultDetails): number {
  return details.rows.filter((r) => r.caution === true).length;
}

/** "1 item needs attention" / "3 items need attention": the badge's accessible name. */
export function attentionText(n: number): string {
  return tc(n, "result.attentionOne", "result.attentionMany");
}

/**
 * The same, with a pointer to the panel. It is visually hidden text in the
 * live region of a result, so a screen reader announces that there is
 * something in Details without any durability wording on screen.
 */
export function attentionHint(n: number): string {
  return t("result.attentionHint", { items: attentionText(n) });
}

/**
 * The counts of a job as short parts for one line: zero counts and a zero
 * size are left out ("6 files", not "6 files, 0 folders"); a job that stored
 * nothing says "0 files" rather than nothing.
 */
export function countParts(files: number, directories: number, bytes: number): string[] {
  const parts: string[] = [];
  if (files > 0) parts.push(tc(files, "count.fileOne", "count.fileMany"));
  if (directories > 0) parts.push(tc(directories, "count.folderOne", "count.folderMany"));
  if (bytes > 0) parts.push(shortSize(String(bytes)));
  if (parts.length === 0) parts.push(tc(0, "count.fileOne", "count.fileMany"));
  return parts;
}

/** Join the parts of a line. */
export function joinParts(parts: string[]): string {
  return parts.join(t("common.statusSeparator"));
}

/**
 * Write the technical record of a result to the console, once per result.
 * It holds statuses, counts, and the durability note; never a secret.
 */
export function logRecord(record: string): void {
  console.info(`[mochi] ${record}`);
}
