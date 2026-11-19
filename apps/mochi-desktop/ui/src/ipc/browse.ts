// Typed wrappers over the browsing commands (plan D1). Types come from Rust
// through ts-rs (./generated, never edited by hand). The webview never names
// a file: `openArchive` asks Rust to show the native dialog.
import { invoke } from "@tauri-apps/api/core";

import { openArchiveDialog } from "./dialogs";

import type { ArchiveInfoView } from "./generated/ArchiveInfoView";
import type { ArchiveSummary } from "./generated/ArchiveSummary";
import type { AttributesView } from "./generated/AttributesView";
import type { ChildrenPage } from "./generated/ChildrenPage";
import type { CommitView } from "./generated/CommitView";
import type { EntryDetails } from "./generated/EntryDetails";
import type { EntryRow } from "./generated/EntryRow";
import type { OpenOutcome } from "./generated/OpenOutcome";
import type { NameText } from "./generated/NameText";
import type { SnapshotView } from "./generated/SnapshotView";

export type {
  ArchiveInfoView,
  ArchiveSummary,
  AttributesView,
  ChildrenPage,
  CommitView,
  EntryDetails,
  EntryRow,
  NameText,
  OpenOutcome,
  SnapshotView,
};

/** Most entries one page may hold (`mochi_core::browse::MAX_PAGE`). */
export const MAX_PAGE = 1000;

/**
 * Show the open dialog and open the chosen archive; `null` if cancelled. An
 * Encrypted archive comes back as `needs_passphrase` and waits in Rust.
 */
export function openArchive(): Promise<OpenOutcome | null> {
  return invoke<OpenOutcome | null>("open_archive", { text: openArchiveDialog() });
}

export function closeArchive(handle: number): Promise<boolean> {
  return invoke<boolean>("close_archive", { handle });
}

export function openSnapshot(handle: number, seq: number): Promise<SnapshotView> {
  return invoke<SnapshotView>("open_snapshot", { handle, seq });
}

/** One page of `dir`'s children (`null` for the root). */
export function listChildren(
  handle: number,
  seq: number,
  dir: string | null,
  directoriesOnly: boolean,
  offset: number,
  limit: number,
): Promise<ChildrenPage> {
  return invoke<ChildrenPage>("list_children", {
    handle,
    seq,
    dir,
    directoriesOnly,
    offset,
    limit,
  });
}

export function entryDetails(handle: number, seq: number, key: string): Promise<EntryDetails> {
  return invoke<EntryDetails>("entry_details", { handle, seq, key });
}

/** The archive Info window's facts for snapshot `seq` (totals and recorded settings). */
export function archiveInfo(handle: number, seq: number): Promise<ArchiveInfoView> {
  return invoke<ArchiveInfoView>("archive_info", { handle, seq });
}
