// The compact window of the shell's quick verbs (plan D6, owner decision
// 2026-10-16). Rust runs the job and tells this window what happened; the
// window sends back a passphrase, a cancel, "show in folder", and "close". It
// never sees or sends a path.
//
// One channel is registered for the life of the window and every event since
// the job began is kept, so a listener that mounts late (React strict mode
// mounts twice in development) still hears the whole job.
import { Channel, invoke } from "@tauri-apps/api/core";

import { destinationDialog, moveFolderDialog } from "./dialogs";

import type { ArchiveFormat } from "./generated/ArchiveFormat";
import type { QuickAction } from "./generated/QuickAction";
import type { QuickAddRequest } from "./generated/QuickAddRequest";
import type { QuickEvent } from "./generated/QuickEvent";
import type { QuickFolder } from "./generated/QuickFolder";
import type { QuickLocation } from "./generated/QuickLocation";

export type { ArchiveFormat, QuickAction, QuickAddRequest, QuickEvent, QuickFolder, QuickLocation };

const listeners = new Set<(event: QuickEvent) => void>();
let log: QuickEvent[] = [];
let registered = false;

function remember(event: QuickEvent): void {
  if (event.event === "started") log = [];
  // A form sent again (another item joined it) replaces the one before.
  if (event.event === "add_prompt") log = log.filter((e) => e.event !== "add_prompt");
  const last = log[log.length - 1];
  if (event.event === "progress" && last?.event === "progress") log.pop();
  log.push(event);
}

/** Listen for the job's events; returns the function that stops listening. */
export function subscribeQuick(onEvent: (event: QuickEvent) => void): () => void {
  listeners.add(onEvent);
  for (const past of [...log]) onEvent(past);
  if (!registered) {
    registered = true;
    const channel = new Channel<QuickEvent>();
    channel.onmessage = (event) => {
      remember(event);
      for (const l of [...listeners]) l(event);
    };
    invoke<null>("watch_quick", { onEvent: channel }).catch(() => {
      registered = false;
    });
  }
  return () => {
    listeners.delete(onEvent);
  };
}

/** The passphrase the job asked for. It crosses IPC once and is not kept. */
export function sendQuickPassphrase(passphrase: string): Promise<null> {
  return invoke<null>("quick_passphrase", { passphrase });
}

export function cancelQuick(): Promise<null> {
  return invoke<null>("quick_cancel");
}

/** Pause (`true`) or resume (`false`) the running job; `false` if none is running. */
export function quickPause(paused: boolean): Promise<boolean> {
  return invoke<boolean>("quick_pause", { paused });
}

/** Open the folder the job wrote into; Rust remembers which. */
export function showQuickFolder(): Promise<null> {
  return invoke<null>("quick_show_folder");
}

/** "Open the folder when done": on or off, saved by Rust for the next extraction too. */
export function setQuickOpenFolder(open: boolean): Promise<boolean> {
  return invoke<boolean>("quick_set_open_folder", { open });
}

/**
 * Rename the folder "Extract to new folder" made. Only a folder name crosses; Rust renames it in
 * place and refuses a name that exists (an `IpcError`).
 */
export function renameQuickFolder(name: string): Promise<QuickFolder> {
  return invoke<QuickFolder>("quick_rename_folder", { name });
}

/**
 * Move that folder: Rust shows the folder dialog and moves it there, keeping its name. `null`
 * when the dialog was cancelled; a name that exists there, or another drive, is refused.
 */
export function moveQuickFolder(): Promise<QuickFolder | null> {
  return invoke<QuickFolder | null>("quick_move_folder", { text: moveFolderDialog() });
}

export function closeQuick(): Promise<null> {
  return invoke<null>("quick_close");
}

/**
 * The Add form's Browse…: Rust shows the save dialog and keeps the folder.
 * Resolves with the folder's display text and the file name, or `null` when
 * the dialog was cancelled.
 */
export function chooseQuickLocation(name: string, format: ArchiveFormat): Promise<QuickLocation | null> {
  return invoke<QuickLocation | null>("quick_choose_location", {
    name,
    format,
    text: destinationDialog(format),
  });
}

/**
 * OK on the Add form. Rust checks the request; a refusal (a name that exists,
 * a bad name) rejects with an `IpcError` and the form stays open. A passphrase
 * crosses here, once.
 */
export function submitQuickAdd(request: QuickAddRequest): Promise<null> {
  return invoke<null>("quick_add_submit", { request });
}

/** Forget what was heard (tests). */
export function resetQuickForTests(): void {
  log = [];
  listeners.clear();
  registered = false;
}
