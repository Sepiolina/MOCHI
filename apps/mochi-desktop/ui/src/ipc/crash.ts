// Typed wrappers over the crash-log commands (plan D7). The log is local and
// opt-in; Rust owns the file, shows the save dialog, and keeps the paths.
import { invoke } from "@tauri-apps/api/core";

import { crashLogDialog } from "./dialogs";

import type { CrashLogView } from "./generated/CrashLogView";
import type { CrashSettingsView } from "./generated/CrashSettingsView";

export type { CrashLogView, CrashSettingsView };

export function getCrashSettings(): Promise<CrashSettingsView> {
  return invoke<CrashSettingsView>("get_crash_settings");
}

export function setCrashSettings(enabled: boolean, includeDetails: boolean): Promise<CrashSettingsView> {
  return invoke<CrashSettingsView>("set_crash_settings", { enabled, includeDetails });
}

export function readCrashLog(): Promise<CrashLogView> {
  return invoke<CrashLogView>("read_crash_log");
}

/** Save a copy through a native dialog; the file's name, or `null` if cancelled. */
export function exportCrashLog(): Promise<string | null> {
  return invoke<string | null>("export_crash_log", { text: crashLogDialog() });
}

export function clearCrashLog(): Promise<CrashSettingsView> {
  return invoke<CrashSettingsView>("clear_crash_log");
}
