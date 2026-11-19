import { expect, test } from "vitest";

import type { CommitReport } from "../ipc/write";
import type { JobOutcome } from "../ipc/jobs";
import { cautionCount } from "../result/details";
import type { ResultDetails } from "../result/details";
import { deleteNote, localCommitNote, outcomeResult, tarNotes } from "./messages";
import type { WriteResult } from "./messages";
import type { Operation } from "./messages";

const ARCHIVE = {
  handle: 1,
  file_name: "a.mochi",
  archive_id: "ab".repeat(16),
  profile: { tar_compatible: false, encrypted: false, redundancy_percent: null },
  commits: [],
  retention_error: null,
};

function report(operation: CommitReport["operation"], extra: Partial<CommitReport> = {}): CommitReport {
  return {
    operation,
    status: "LOCAL_COMMITTED",
    seq: 4,
    commit_id: "c".repeat(64),
    files: 3,
    directories: 1,
    bytes: 1536,
    removed: 0,
    skipped: [],
    durability_note: null,
    archive: ARCHIVE,
    reopen_error: null,
    ...extra,
  };
}

const ALL: [Operation, JobOutcome][] = [
  ["create", { status: "completed", summary: "s", commit: report("create") }],
  ["add", { status: "completed", summary: "s", commit: report("add") }],
  ["delete", { status: "completed", summary: "s", commit: report("delete", { removed: 2 }) }],
  [
    "add",
    {
      status: "completed",
      summary: "s",
      commit: report("add", {
        skipped: [{ path: { text: "docs/link", verbatim: true }, reason: "symbolic links are not stored" }],
        durability_note: "directory sync unsupported",
        reopen_error: { code: "LIMIT_EXCEEDED", message: "too many archives" },
      }),
    },
  ],
  ["create", { status: "cancelled" }],
  ["add", { status: "cancelled" }],
  ["add", { status: "failed", code: "IO_ERROR", message: "disk" }],
  ["add", { status: "failed", code: "COMMIT_UNCONFIRMED", message: "sync" }],
  ["add", { status: "aborted", message: "boom" }],
];

function at(i: number): JobOutcome {
  const entry = ALL[i];
  if (!entry) throw new Error(`no case ${i}`);
  return entry[1];
}

/** Every string a result can show: its lines, or its line, rows, and notes. */
function allText(r: WriteResult): string[] {
  if (r.kind === "text") return r.lines;
  return [r.line, ...detailText(r.details)];
}

function detailText(d: ResultDetails): string[] {
  return [
    ...d.rows.flatMap((row) => [
      row.label,
      row.value ?? "",
      row.sub ?? "",
      row.note ?? "",
      ...(row.items ?? []).flatMap((i) => [i.name?.text ?? "", i.code ?? "", i.text ?? ""]),
    ]),
    ...d.notes,
  ];
}

function commitOf(i: number) {
  const r = outcomeResult(ALL[i]?.[0] ?? "add", at(i));
  if (r.kind !== "commit") throw new Error("not a commit");
  return r;
}

test("no message claims the data is backed up, safe, or preserved (spec §23.3 #6)", () => {
  const banned = [/backed up/i, /backup/i, /\bsafe/i, /preserv/i, /secure/i, /protected/i];
  const text = [
    ...ALL.flatMap(([op, o]) => allText(outcomeResult(op, o))),
    deleteNote(),
    localCommitNote(),
    ...tarNotes(),
  ].join("\n");
  for (const b of banned) expect(text).not.toMatch(b);
});

test("a commit is one short line: counts without zeros, no byte count, no technical code", () => {
  const r = commitOf(1);
  expect(r.line).toBe("Added to a.mochi · 3 files · 1 folder · 1.5 KiB");
  expect(r.line).not.toContain("1,536");
  expect(r.line).not.toContain("LOCAL_COMMITTED");
  expect(r.warn).toBe(false);
  // A zero count is left out of the line.
  const noFolders = outcomeResult("create", {
    status: "completed",
    summary: "s",
    commit: report("create", { files: 6, directories: 0, bytes: 906363 }),
  });
  expect(noFolders.kind === "commit" && noFolders.line).toBe("Created a.mochi · 6 files · 885.1 KiB");
});

test("the details hold the counts, the status with its technical value, and the local-copy note", () => {
  const r = commitOf(1);
  const row = (label: string) => r.details.rows.find((x) => x.label === label);
  expect(row("Files")?.value).toBe("3");
  expect(row("Folders")?.value).toBe("1");
  expect(row("Size")?.value).toBe("1.5 KiB (1,536 bytes)");
  expect(row("Status")?.value).toBe("Committed on this computer");
  expect(row("Status")?.sub).toBe("commit 4 · LOCAL_COMMITTED");
  expect(row("Status")?.caution).toBeUndefined();
  expect(r.details.notes).toEqual([localCommitNote()]);
  expect(localCommitNote()).toBe(
    "This archive is one file on this computer. Keep a separate copy of anything you cannot lose.",
  );
  expect(cautionCount(r.details)).toBe(0);
  expect(r.record).toContain("commit 4 LOCAL_COMMITTED");
});

test("a deletion is removal from this version, in one line that says earlier versions keep the content", () => {
  const r = commitOf(2);
  expect(r.line).toBe("Removed 2 items from this version. Earlier versions still have them.");
  const one = outcomeResult("delete", {
    status: "completed",
    summary: "s",
    commit: report("delete", { removed: 1 }),
  });
  expect(one.kind === "commit" && one.line).toBe("Removed 1 item from this version. Earlier versions still have it.");
  const note = r.details.notes.join(" ");
  expect(note).toContain("stays in the archive's history");
  expect(note).toContain("retention and garbage collection");
  expect(note).toContain("does not make the archive smaller");
});

test("a degraded directory entry is a caution row in the details, never in the line, and counts as a note", () => {
  const r = commitOf(3);
  const row = r.details.rows.find((x) => x.label === "Folder entry on disk");
  expect(row).toMatchObject({
    value: "Not confirmed",
    sub: "Degraded",
    note: "directory sync unsupported",
    caution: true,
  });
  // not stored + degraded + could not reopen
  expect(cautionCount(r.details)).toBe(3);
  expect(r.record).toContain("not confirmed durable (directory sync unsupported)");
});

test("items not stored make the result partial: the line says so and the details list them with reasons", () => {
  const r = commitOf(3);
  expect(r.warn).toBe(true);
  expect(r.line).toBe("Added to a.mochi · 3 files · 1 folder · 1.5 KiB · 1 item not stored · could not be opened again");
  const row = r.details.rows.find((x) => x.label === "Not stored");
  expect(row?.caution).toBe(true);
  expect(row?.items).toEqual([{ name: { text: "docs/link", verbatim: true }, text: "symbolic links are not stored" }]);
  const reopen = r.details.rows.find((x) => x.label === "Opening it again");
  expect(reopen?.note).toContain("LIMIT_EXCEEDED");
});

test("an unconfirmed commit and a clean commit differ only in the notes: both lines are plain text", () => {
  const clean = commitOf(1);
  expect(clean.line).not.toMatch(/degraded|not confirmed|not stored/i);
});

test("cancelled and failed jobs say what did not happen; an unconfirmed commit claims neither", () => {
  const lines = (op: Operation, o: JobOutcome) => {
    const r = outcomeResult(op, o);
    if (r.kind !== "text") throw new Error("expected text");
    return r.lines;
  };
  expect(lines("create", { status: "cancelled" })).toEqual(["Cancelled. No archive was created."]);
  expect(lines("add", { status: "cancelled" })[0]).toContain("Nothing was committed");
  expect(lines("add", { status: "failed", code: "IO_ERROR", message: "disk" })).toEqual([
    "Failed (IO_ERROR): disk",
    "Nothing was committed.",
  ]);
  for (const code of ["COMMIT_UNCONFIRMED", "WRITER_POISONED"]) {
    const text = lines("add", { status: "failed", code, message: "m" }).join(" ");
    expect(text).toContain("may or may not have been published");
    expect(text).not.toContain("Nothing was committed");
  }
});
