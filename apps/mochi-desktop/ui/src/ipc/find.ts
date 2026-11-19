// Typed wrappers over the MOCHI Find commands. Types come from Rust through
// ts-rs (./generated, never edited by hand). The webview names no path: the
// folders are chosen in Rust (the native dialog, or the shell's --find launch),
// and a hit is opened by its scan number and archive position.
import { Channel, invoke } from "@tauri-apps/api/core";

import { t } from "../i18n";
import type { OpenOutcome } from "./generated/OpenOutcome";
import type { FindCoverageView } from "./generated/FindCoverageView";
import type { FindEvent } from "./generated/FindEvent";
import type { FindHit } from "./generated/FindHit";
import type { FindResultView } from "./generated/FindResultView";
import type { FindScopeView } from "./generated/FindScopeView";
import type { FindSkippedView } from "./generated/FindSkippedView";
import type { FindEntryFilter } from "./generated/FindEntryFilter";
import type { FindQuery } from "./generated/FindQuery";
import type { FindTermView } from "./generated/FindTermView";
import type { RetrieveArchiveView } from "./generated/RetrieveArchiveView";
import type { RetrieveLayout } from "./generated/RetrieveLayout";
import type { RetrieveReportView } from "./generated/RetrieveReportView";

export type {
  FindCoverageView,
  FindEntryFilter,
  FindEvent,
  FindHit,
  FindQuery,
  FindResultView,
  FindScopeView,
  FindSkippedView,
  FindTermView,
  RetrieveArchiveView,
  RetrieveLayout,
  RetrieveReportView,
};

/** The folders to search. */
export function findScopes(): Promise<FindScopeView[]> {
  return invoke<FindScopeView[]>("find_scopes");
}

/** The native folder dialog (in Rust); the folders chosen are added. `null` if cancelled. */
export function findChooseFolders(): Promise<FindScopeView[] | null> {
  return invoke<FindScopeView[] | null>("find_choose_folders", { text: { title: t("find.dialogFolders") } });
}

export function findRemoveScope(index: number): Promise<FindScopeView[]> {
  return invoke<FindScopeView[]>("find_remove_scope", { index });
}

/**
 * Scan the folders (and their subfolders unless `subfolders` is `false`). Events (started,
 * progress, one finished) go to `onEvent`. Resolves to the scan's number.
 */
export function findStart(fresh: boolean, onEvent: (event: FindEvent) => void, subfolders = true): Promise<number> {
  const channel = new Channel<FindEvent>();
  channel.onmessage = onEvent;
  return invoke<number>("find_start", { fresh, subfolders, onEvent: channel });
}

/** Stop the scan. `false` if none is running. */
export function findCancel(): Promise<boolean> {
  return invoke<boolean>("find_cancel");
}

/** Names matching `text` in the latest scan, with the scan's coverage. */
export function findQuery(text: string): Promise<FindResultView> {
  return invoke<FindResultView>("find_query", { text });
}

/** Entries matching any of several names, with the options (spec Annex B.2.21), and what each name found. */
export function findQueryMany(query: FindQuery): Promise<FindResultView> {
  return invoke<FindResultView>("find_query_many", { query });
}

/** The native folder dialog (in Rust) for where found files go; Rust keeps the choice. `null` if cancelled. */
export function findChooseRetrieveFolder(): Promise<string | null> {
  return invoke<string | null>("find_choose_retrieve_folder", { text: { title: t("find.dialogRetrieve") } });
}

/** Open the archive of a hit read-only where archives open (here, the window that has it, or a new one) and show the entry `key` in it. */
export function findOpenHit(run: number, archive: number, key: string): Promise<OpenOutcome> {
  return invoke<OpenOutcome>("find_open_hit", { run, archive, key });
}

/** Show the folder that holds the archive of a hit. */
export function findShowFolder(run: number, archive: number): Promise<null> {
  return invoke<null>("find_show_folder", { run, archive });
}
