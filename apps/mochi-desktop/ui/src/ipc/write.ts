// Typed wrappers over the staging commands (plan D2). Types come from Rust
// through ts-rs (./generated, never edited by hand). The webview never names a
// file: Rust shows the dialogs and receives dropped paths, and the webview
// sees only display names.
import { Channel, invoke } from "@tauri-apps/api/core";

import { destinationDialog, sourcesDialog } from "./dialogs";

import type { ArchiveFormat } from "./generated/ArchiveFormat";
import type { CommitReport } from "./generated/CommitReport";
import type { CompressionPreset } from "./generated/CompressionPreset";
import type { DropEvent } from "./generated/DropEvent";
import type { ExportReportView } from "./generated/ExportReportView";
import type { SourcePick } from "./generated/SourcePick";
import type { StageOutcome } from "./generated/StageOutcome";
import type { StagedItem } from "./generated/StagedItem";
import type { StagedView } from "./generated/StagedView";

export type {
  ArchiveFormat,
  CommitReport,
  CompressionPreset,
  DropEvent,
  ExportReportView,
  SourcePick,
  StageOutcome,
  StagedItem,
  StagedView,
};

/** Listen for items dropped on the window. Rust stages them; only names arrive. */
export function watchDrops(onEvent: (event: DropEvent) => void): Promise<null> {
  const channel = new Channel<DropEvent>();
  channel.onmessage = onEvent;
  return invoke<null>("watch_drops", { onDrop: channel });
}

export function stagedSources(): Promise<StagedView> {
  return invoke<StagedView>("staged_sources");
}

/** Show the native files or folder dialog and stage the choice. */
export function stageSources(pick: SourcePick): Promise<StageOutcome> {
  return invoke<StageOutcome>("stage_sources", { pick, text: sourcesDialog(pick === "folder") });
}

export function unstageSource(index: number): Promise<StagedView> {
  return invoke<StagedView>("unstage_source", { index });
}

export function clearStaged(): Promise<StagedView> {
  return invoke<StagedView>("clear_staged");
}

/** Show the save dialog; `null` if the user cancelled it. */
export function chooseDestination(format: ArchiveFormat = "mochi"): Promise<StagedView | null> {
  return invoke<StagedView | null>("choose_destination", { text: destinationDialog(format) });
}

/**
 * Choose the format of the next new archive (plan O31): `.mochi`, ZIP, tar,
 * tar.gz or tar.zst. Rust returns the staged view with names that follow the
 * format's extension.
 */
export function setArchiveFormat(format: ArchiveFormat): Promise<StagedView> {
  return invoke<StagedView>("set_archive_format", { format });
}
