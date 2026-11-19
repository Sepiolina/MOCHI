// What the app says about a create, add, or delete (spec §23.3). After a
// commit it may say the commit completed; it must not say the data is backed
// up, safe, or preserved (#6). A deletion is removal from the current
// snapshot, with a clear statement that the content stays in history (#5).
// Nothing here is a verification, so none of it gets success styling.
import { formatNumber, phaseLabel, t, tc, tn } from "../i18n";
import type { CommitReport, ExportReportView } from "../ipc/write";
import type { JobOutcome } from "../ipc/jobs";
import { formatSize } from "../browser/format";
import { bytesOfText } from "../progress/messages";
import { countParts, joinParts } from "../result/details";
import { formatLabel } from "./format";
import type { DetailRow, ResultView } from "../result/details";

export type Operation = "create" | "add" | "delete" | "export";

/** Shown with every commit: the honest limit of a local commit (spec §5.3). */
export const localCommitNote = () => t("write.localCommitNote");

/** The deletion statement (spec §23.3 #5, §16.3). */
export const deleteNote = () => t("write.deleteNote");

/** The trade-offs of the TAR-compatible option (plan O4, spec D4, D19). */
export const tarNotes = () => [t("write.tar1"), t("write.tar2"), t("write.tar3"), t("write.tar4")];

/**
 * What a finished job shows: plain lines (a cancel or a failure), a compact commit result, or
 * (plan O31) what a ZIP or tar export wrote.
 */
export type WriteResult = { kind: "text"; lines: string[] } | ({ kind: "commit" | "export" } & ResultView);

/** The red line of a job that failed or stopped unexpectedly; `""` for any other outcome. */
export function failureText(outcome: JobOutcome): string {
  if (outcome.status === "failed") {
    return t("transfer.errorLineCode", { code: outcome.code, message: outcome.message });
  }
  return outcome.status === "aborted" ? t("transfer.errorLine", { message: outcome.message }) : "";
}

/** The result of a job. A commit is one short line plus details; the rest are plain lines. */
export function outcomeResult(operation: Operation, outcome: JobOutcome): WriteResult {
  switch (outcome.status) {
    case "completed":
      if (outcome.export) return { kind: "export", ...exportView(outcome.export) };
      return outcome.commit
        ? { kind: "commit", ...commitView(outcome.commit) }
        : { kind: "text", lines: [t("common.completed", { summary: outcome.summary })] };
    case "cancelled":
      return {
        kind: "text",
        lines: [t(operation === "create" || operation === "export" ? "write.cancelledCreate" : "write.cancelledOther")],
      };
    case "failed": {
      if (operation === "export") {
        // Nothing but a file was being written, under a temporary name until the end.
        return {
          kind: "text",
          lines: [t("common.failed", { code: outcome.code, message: outcome.message }), t("write.nothingWritten")],
        };
      }
      // A commit that could not be confirmed may or may not have been
      // published (spec §12.4): do not claim either.
      const unsure = outcome.code === "COMMIT_UNCONFIRMED" || outcome.code === "WRITER_POISONED";
      return {
        kind: "text",
        lines: [
          t("common.failed", { code: outcome.code, message: outcome.message }),
          t(unsure ? "write.unsureCommit" : "write.nothingCommitted"),
        ],
      };
    }
    case "aborted":
      return {
        kind: "text",
        lines: [t("common.stoppedUnexpectedly", { message: outcome.message }), t("write.openAgain")],
      };
  }
}

/**
 * A commit as one short line and a details panel. The line states what was
 * done; anything that is not fully good (items not stored, an archive that
 * could not be opened again) is in the line as well, so an overall line never
 * hides it. The directory-entry note is a degraded dimension: it is in the
 * details with a caution marker and counts toward the badge on the Details
 * button.
 */
export function commitView(c: CommitReport): ResultView {
  const file = c.archive?.file_name ?? "";
  const isDelete = c.operation === "delete";
  const warn = c.skipped.length > 0 || c.reopen_error !== null;

  const head = isDelete
    ? tn(c.removed, "write.committedDeleteOne", "write.committedDeleteMany", {
        items: tc(c.removed, "common.itemOne", "common.itemMany"),
      })
    : t(c.operation === "create" ? "write.committedCreate" : "write.committedAdd", {
        file,
        parts: joinParts(countParts(c.files, c.directories, c.bytes)),
      });
  const tail: string[] = [];
  if (c.skipped.length > 0) tail.push(tc(c.skipped.length, "write.lineNotStoredOne", "write.lineNotStoredMany"));
  if (c.reopen_error) tail.push(t("write.lineNotReopened"));
  const line = tail.length === 0 ? head : `${head}${t("common.statusSeparator")}${joinParts(tail)}`;

  const rows: DetailRow[] = [];
  if (isDelete) {
    rows.push({ label: t("write.rowRemoved"), value: tc(c.removed, "common.itemOne", "common.itemMany") });
  } else {
    rows.push(
      { label: t("write.rowFiles"), value: formatNumber(c.files) },
      { label: t("write.rowFolders"), value: formatNumber(c.directories) },
      { label: t("write.rowSize"), value: formatSize(String(c.bytes)) },
    );
  }
  rows.push({
    label: t("write.rowStatus"),
    value: t("write.statusCommitted"),
    sub: t("write.statusTechnical", { seq: c.seq, status: c.status }),
  });
  if (c.durability_note) {
    rows.push({
      label: t("write.rowFolderEntry"),
      value: t("write.folderEntryNotConfirmed"),
      sub: t("write.degradedMark"),
      note: c.durability_note,
      caution: true,
      testId: "write-durability",
    });
  }
  if (c.skipped.length > 0) {
    rows.push({
      label: t("write.rowNotStored"),
      value: tc(c.skipped.length, "common.itemOne", "common.itemMany"),
      items: c.skipped.map((s) => ({ name: s.path, text: s.reason })),
      caution: true,
      testId: "write-not-stored",
      itemTestId: "write-not-stored-item",
    });
  }
  if (c.reopen_error) {
    rows.push({
      label: t("write.rowReopen"),
      value: t("write.reopenFailed"),
      note: t("write.reopenDetail", { code: c.reopen_error.code, message: c.reopen_error.message }),
      caution: true,
    });
  }
  const notes = isDelete ? [deleteNote(), localCommitNote()] : [localCommitNote()];

  const record = [
    `${c.operation} commit ${c.seq} ${c.status}`,
    `files ${c.files}, folders ${c.directories}, bytes ${c.bytes}, removed ${c.removed}`,
    `not stored ${c.skipped.length}`,
    c.durability_note ? `directory entry not confirmed durable (${c.durability_note})` : "directory entry confirmed",
    ...(c.reopen_error ? [`reopen failed ${c.reopen_error.code}`] : []),
  ].join("; ");

  return { line, warn, details: { rows, notes }, record };
}

/**
 * What an export wrote, as one short line and a Details panel (plan O31). It says what ran
 * and no more: no commit status, no health dimension, nothing about the file being checked.
 */
export function exportView(e: ExportReportView): ResultView {
  const warn = e.skipped.length > 0;
  const head = t("write.exportLine", {
    parts: joinParts(countParts(e.files, e.directories, e.bytes)),
    name: e.name.text,
  });
  const line =
    e.skipped.length === 0
      ? head
      : `${head}${t("common.statusSeparator")}${tc(e.skipped.length, "write.lineNotStoredOne", "write.lineNotStoredMany")}`;
  const rows: DetailRow[] = [
    { label: t("write.rowFormat"), value: formatLabel(e.format) },
    { label: t("write.rowFiles"), value: formatNumber(e.files) },
    { label: t("write.rowFolders"), value: formatNumber(e.directories) },
    { label: t("write.rowSize"), value: formatSize(String(e.bytes)) },
    { label: t("write.rowFileSize"), value: formatSize(String(e.output_bytes)) },
  ];
  if (e.durability_note) {
    rows.push({
      label: t("write.rowFolderEntry"),
      value: t("write.folderEntryNotConfirmed"),
      sub: t("write.degradedMark"),
      note: e.durability_note,
      caution: true,
      testId: "write-durability",
    });
  }
  if (e.skipped.length > 0) {
    rows.push({
      label: t("write.rowNotStored"),
      value: tc(e.skipped.length, "common.itemOne", "common.itemMany"),
      items: e.skipped.map((s) => ({ name: s.path, text: s.reason })),
      caution: true,
      testId: "write-not-stored",
      itemTestId: "write-not-stored-item",
    });
  }
  const record = [
    `export ${e.format}`,
    `files ${e.files}, folders ${e.directories}, bytes ${e.bytes}, file size ${e.output_bytes}`,
    `not stored ${e.skipped.length}`,
    e.durability_note ? `directory entry not confirmed durable (${e.durability_note})` : "directory entry confirmed",
  ].join("; ");
  return { line, warn, details: { rows, notes: [t("write.exportNote")] }, record };
}

/** How a running job's progress reads. */
export function progressText(phase: string, completed: number, total: number | null): string {
  if (phase === "export_scan") return t("write.progressScan", { n: formatNumber(completed) });
  if (phase === "export") {
    return total === null
      ? t("write.progressImport", { size: formatSize(String(completed)) })
      : bytesOfText(completed, total);
  }
  // The bulk phase of a create or add: bytes read and stored, out of the bytes to store.
  if (phase === "content" && total !== null) return bytesOfText(completed, total);
  if (phase === "import") return t("write.progressImport", { size: formatSize(String(completed)) });
  if (phase === "delete") {
    return t("write.progressDelete", { completed, total: total ?? t("write.progressTotalUnknown") });
  }
  return total === null
    ? t("write.progressPhase", { phase: phaseLabel(phase), completed })
    : t("write.progressPhaseOf", { phase: phaseLabel(phase), completed, total });
}
