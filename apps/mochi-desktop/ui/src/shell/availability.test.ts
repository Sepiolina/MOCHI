// The one function that decides which actions are on (shell/availability.ts): every archive
// kind and state the window knows, against no selection and a selection.
import { expect, test } from "vitest";

import { availableActions, reasonText, tipFor } from "./availability";
import type { ActionId, ArchiveContext } from "./availability";

const NONE = { selected: 0, total: 0 };
const SOME = { selected: 2, total: 5 };

const enabledOf = (a: ArchiveContext, s = NONE): ActionId[] =>
  Object.entries(availableActions(a, s))
    .filter(([, v]) => v.enabled)
    .map(([k]) => k as ActionId);

const MOCHI_HEAD: ArchiveContext = { kind: "mochi", viewingHead: true };

test("with nothing open, only New archive is on, and every other action says to open one", () => {
  expect(enabledOf({ kind: "none" })).toEqual(["newArchive"]);
  const a = availableActions({ kind: "none" }, NONE);
  for (const id of ["add", "remove", "extract", "extractTo", "test", "info", "search"] as const) {
    expect(a[id]).toEqual({ enabled: false, reason: "noArchive" });
  }
});

test("an archive waiting for its passphrase is not open: its actions say so", () => {
  const a = availableActions({ kind: "none", locked: true }, NONE);
  expect(a.extract).toEqual({ enabled: false, reason: "locked" });
  expect(a.add.reason).toBe("locked");
  expect(reasonText("locked")).toBe("Enter the archive's passphrase first");
});

test("the latest version of a MOCHI archive: everything but the selection actions is on", () => {
  expect(enabledOf(MOCHI_HEAD).sort()).toEqual(
    ["add", "extract", "extractTo", "info", "newArchive", "search", "test"].sort(),
  );
  // Plus the selection actions once there is a selection and a list to select from.
  expect(enabledOf(MOCHI_HEAD, SOME).sort()).toEqual(
    [
      "add",
      "copyNames",
      "extract",
      "extractTo",
      "info",
      "newArchive",
      "remove",
      "search",
      "selectAll",
      "test",
    ].sort(),
  );
});

test("Extract and Extract to... take the whole archive when nothing is selected", () => {
  const a = availableActions(MOCHI_HEAD, NONE);
  expect(a.extract.enabled).toBe(true);
  expect(a.extractTo.enabled).toBe(true);
  // Remove needs a selection.
  expect(a.remove).toEqual({ enabled: false, reason: "noSelection" });
  expect(a.copyNames.reason).toBe("noSelection");
});

test("an older version of a MOCHI archive is read-only: Add and Remove are off, extracting is on", () => {
  const old: ArchiveContext = { kind: "mochi", viewingHead: false };
  const a = availableActions(old, SOME);
  expect(a.add).toEqual({ enabled: false, reason: "oldVersion" });
  expect(a.remove).toEqual({ enabled: false, reason: "oldVersion" });
  expect(a.extract.enabled && a.extractTo.enabled && a.test.enabled).toBe(true);
});

test("a ZIP, 7z, RAR, or tar archive is read-only: Add and Remove are off, reading is on", () => {
  const a = availableActions({ kind: "foreign" }, SOME);
  expect(a.add).toEqual({ enabled: false, reason: "readOnly" });
  expect(a.remove).toEqual({ enabled: false, reason: "readOnly" });
  for (const id of ["extract", "extractTo", "test", "copyNames", "selectAll", "info", "search"] as const) {
    expect(a[id].enabled, id).toBe(true);
  }
  // The reason shown for read-only is the existing wording.
  expect(tipFor(a.add, "Add files")).toBe("Archives in other formats are read-only");
  expect(tipFor(a.extract, "Extract")).toBe("Extract");
});

test("the read-only reason wins over a missing selection (it will not change by selecting)", () => {
  expect(availableActions({ kind: "foreign" }, NONE).remove.reason).toBe("readOnly");
});

test("while a job runs, every action that starts one waits; selecting and copying do not", () => {
  const a = availableActions({ ...MOCHI_HEAD, busy: true }, SOME);
  for (const id of ["newArchive", "add", "remove", "extract", "extractTo", "test"] as const) {
    expect(a[id], id).toEqual({ enabled: false, reason: "busy" });
  }
  for (const id of ["copyNames", "selectAll", "info", "search"] as const) expect(a[id].enabled, id).toBe(true);
});

test("Select all needs something to select", () => {
  expect(availableActions(MOCHI_HEAD, { selected: 0, total: 0 }).selectAll).toEqual({
    enabled: false,
    reason: "emptyList",
  });
  expect(availableActions(MOCHI_HEAD, { selected: 0, total: 3 }).selectAll.enabled).toBe(true);
});

test("passphrases are for an open encrypted MOCHI archive only", () => {
  expect(availableActions({ kind: "none" }, NONE).passphrases.reason).toBe("noArchive");
  expect(availableActions({ kind: "foreign" }, NONE).passphrases.reason).toBe("noArchive");
  expect(availableActions(MOCHI_HEAD, NONE).passphrases.reason).toBe("notEncrypted");
  expect(availableActions({ ...MOCHI_HEAD, encrypted: true }, NONE).passphrases.enabled).toBe(true);
});

test("every reason has wording, and none of it claims success or safety", () => {
  for (const r of ["noArchive", "locked", "readOnly", "oldVersion", "noSelection", "emptyList", "busy", "notEncrypted"] as const) {
    const text = reasonText(r);
    expect(text.length).toBeGreaterThan(5);
    expect(text).not.toMatch(/safe|backed up|preserved|secure/i);
  }
});
