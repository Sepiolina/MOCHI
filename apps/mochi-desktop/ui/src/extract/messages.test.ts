import { expect, test } from "vitest";

import type { ExtractReport } from "../ipc/extract";
import type { JobOutcome } from "../ipc/jobs";
import { attributeLine, checkNote, describe, progressText } from "./messages";

const text = (s: string, verbatim = true) => ({ text: s, verbatim });

function report(extra: Partial<ExtractReport> = {}): ExtractReport {
  return {
    destination: "/home/me/out",
    seq: 3,
    files: 4,
    directories: 2,
    bytes: 2048,
    not_extracted: [],
    not_extracted_total: 0,
    attribute_notes: [],
    attributes_unavailable: null,
    durability_note: null,
    files_not_flushed: false,
    case_insensitive: false,
    opened_folder: false,
    replaced: 0,
    merged_folders: 0,
    ...extra,
  };
}

const done = (r: ExtractReport): JobOutcome => ({ status: "completed", summary: "s", extract: r });

const BANNED = ["backed up", "backup", "safe", "preserved", "secure", "verified", "pass"];

function everything(outcome: JobOutcome): string {
  const d = describe(outcome);
  const rows = (d.details?.rows ?? []).flatMap((r) => [
    r.label,
    r.value ?? "",
    r.sub ?? "",
    r.note ?? "",
    ...(r.items ?? []).map((i) => i.text ?? ""),
  ]);
  return [d.headline, ...d.notes, ...rows, ...(d.details?.notes ?? [])].join("\n").toLowerCase();
}

const row = (outcome: JobOutcome, label: string) => describe(outcome).details?.rows.find((r) => r.label === label);

test("a completed extraction is one short line; the details say what it checked and that it is not a verification", () => {
  const d = describe(done(report()));
  expect(d.headline).toBe("Extracted 4 files · 2 folders · 2.0 KiB");
  expect(d.headline).not.toContain("2,048");
  expect(d.completed).toBe(true);
  expect(d.warn).toBe(false);
  expect(d.notes).toEqual([]);
  expect(d.details?.notes).toContain(checkNote());
  expect(checkNote()).toContain("not a verification of the archive");
  expect(row(done(report()), "Folder")?.value).toBe("/home/me/out");
  expect(row(done(report()), "From version")?.value).toBe("3");
  expect(row(done(report()), "Size")?.value).toBe("2.0 KiB (2,048 bytes)");
  expect(d.record).toContain("extract version 3");
});

test("zero counts are left out of the line", () => {
  expect(describe(done(report({ files: 5, directories: 0, bytes: 10 }))).headline).toBe("Extracted 5 files · 10 bytes");
});

test("exceptions are in the line, which turns to a warning: the extraction is not described as complete", () => {
  const d = describe(done(report({ not_extracted_total: 1, files: 1, directories: 0, bytes: 1 })));
  expect(d.headline).toBe("Extracted 1 file · 1 byte · 1 entry not extracted");
  expect(d.warn).toBe(true);
  const many = describe(done(report({ not_extracted_total: 12 })));
  expect(many.headline).toContain("12 entries not extracted");
});

test("unavailable attributes, unconfirmed durability, and case folding are all in the details, as cautions", () => {
  const o = done(
    report({
      attributes_unavailable: text("snapshot manifest damaged"),
      durability_note: "no directory sync",
      case_insensitive: true,
    }),
  );
  const d = describe(o);
  expect(d.headline).not.toMatch(/degraded|durable|attribute/i);
  expect(row(o, "Times and permissions")).toMatchObject({ value: "Not applied", caution: true });
  expect(row(o, "Times and permissions")?.note).toContain("No times, permissions, or other attributes were applied");
  expect(row(o, "Times and permissions")?.note).toContain("snapshot manifest damaged");
  expect(row(o, "Folder entries on disk")).toMatchObject({
    value: "Not confirmed",
    sub: "Degraded",
    note: "no directory sync",
    caution: true,
  });
  expect(d.details?.notes.join(" ")).toContain("differ only in case");
});

test("fast extract is a caution in the details and the record, never in the headline", () => {
  const o = done(report({ files_not_flushed: true }));
  const d = describe(o);
  expect(d.headline).not.toMatch(/flush|fast/i);
  expect(row(o, "Files on disk")).toMatchObject({
    value: "Not flushed one by one",
    sub: "Fast extract",
    caution: true,
  });
  expect(row(o, "Files on disk")?.note).toContain("power fails");
  expect(d.record).toContain("files not flushed one by one (fast extract)");
  expect(row(done(report()), "Files on disk")).toBeUndefined();
});

test("an attribute note names the kind, the count, the first entry and the reason", () => {
  const line = attributeLine({
    attribute: "owner and group",
    count: 3,
    first_path: text("a\\u{a}b", false),
    reason: text("needs privilege"),
  });
  expect(line).toBe(
    "Not applied: owner and group, on 3 entries. First: a\\u{a}b (escaped). needs privilege",
  );
  expect(
    attributeLine({ attribute: "permissions", count: 1, first_path: text("x"), reason: text("r") }),
  ).toContain("on 1 entry.");
});

test("a cancelled extraction says what stays and that attributes may be missing", () => {
  const d = describe({ status: "cancelled" });
  expect(d.completed).toBe(false);
  expect(d.headline).toContain("Cancelled");
  expect(d.notes.join(" ")).toContain("stay in the folder, each one complete");
  expect(d.notes.join(" ")).toContain("file being written was discarded");
  expect(d.notes.join(" ")).toContain("recorded times and permissions");
});

test("a failure before writing says nothing was written; any other says files may remain", () => {
  for (const code of ["NAME_COLLISION", "NAME_UNSUPPORTED", "UNSUPPORTED_FEATURE", "INVALID_ARGUMENT"]) {
    const d = describe({ status: "failed", code, message: "m" });
    expect(d.headline).toBe(`Failed (${code}): m`);
    expect(d.notes).toEqual(["Nothing was written."]);
  }
  const d = describe({ status: "failed", code: "IO_ERROR", message: "disk full" });
  expect(d.notes[0]).toContain("Some files may already have been extracted");
  const a = describe({ status: "aborted", message: "boom" });
  expect(a.headline).toBe("Stopped unexpectedly: boom");
  expect(a.notes[0]).toContain("may already have been extracted");
});

test("no outcome is described as backed up, safe, preserved, or verified", () => {
  const outcomes: JobOutcome[] = [
    done(report()),
    done(report({ not_extracted_total: 2, attributes_unavailable: text("x") })),
    { status: "cancelled" },
    { status: "failed", code: "IO_ERROR", message: "m" },
    { status: "aborted", message: "m" },
  ];
  for (const o of outcomes) {
    const s = everything(o);
    for (const word of BANNED.filter((w) => w !== "verified")) expect(s).not.toContain(word);
    // "verification" appears only in the sentence that denies it.
    const without = s.replace(checkNote().toLowerCase(), "");
    expect(without).not.toContain("verified");
    expect(without).not.toContain("verification");
  }
});

test("progress reads as entries", () => {
  expect(progressText("restore", 3, 8)).toBe("Extracting: 3 of 8 entries");
  expect(progressText("restore", 3, null)).toBe("Extracting: 3 entries");
});
