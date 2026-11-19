// Typed wrapper over the extract plan command (plan D3). Types come from Rust
// through ts-rs (./generated, never edited by hand). Here Rust shows the
// native dialog and keeps the choice, and the job is started with start_job
// (ExtractArchive), which carries only an archive handle, a snapshot, and
// entry keys. The one place the webview sends folder text is "Extract to..."
// (ipc/start.ts planExtractTo), which Rust validates.
import { invoke } from "@tauri-apps/api/core";

import { extractFolderDialog } from "./dialogs";

import type { AttributeNote } from "./generated/AttributeNote";
import type { ConflictChoice } from "./generated/ConflictChoice";
import type { ConflictView } from "./generated/ConflictView";
import type { ExtractPlan } from "./generated/ExtractPlan";
import type { ExtractReport } from "./generated/ExtractReport";
import type { NotExtractedView } from "./generated/NotExtractedView";

export type {
  AttributeNote,
  ConflictChoice,
  ConflictView,
  ExtractPlan,
  ExtractReport,
  NotExtractedView,
};

/**
 * Plan an extraction of `keys` (empty: the whole snapshot) of snapshot `seq`.
 * With `pick`, Rust first shows the native folder dialog; `null` means the
 * user cancelled it. Without, the folder chosen earlier is checked again.
 * Nothing is written.
 */
export function planExtract(
  handle: number,
  seq: number,
  keys: string[],
  pick: boolean,
): Promise<ExtractPlan | null> {
  return invoke<ExtractPlan | null>("plan_extract", {
    handle,
    seq,
    keys,
    pick,
    text: extractFolderDialog(),
  });
}
