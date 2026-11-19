// The in-app updater (plan D7, T13). Rust makes every request, verifies the
// signature, and installs; the webview only asks, shows what came back as text,
// and confirms. It supplies no URL and is granted none of the updater plugin's
// own permissions. The install runs as the `install_update` job (startJob).
import { invoke } from "@tauri-apps/api/core";

import type { UpdateCheckView } from "./generated/UpdateCheckView";
import type { UpdateFailureKind } from "./generated/UpdateFailureKind";
import type { UpdateInstallMode } from "./generated/UpdateInstallMode";
import type { UpdateOffer } from "./generated/UpdateOffer";
import type { UpdateSettingsView } from "./generated/UpdateSettingsView";

export type { UpdateCheckView, UpdateFailureKind, UpdateInstallMode, UpdateOffer, UpdateSettingsView };

export function getUpdateSettings(): Promise<UpdateSettingsView> {
  return invoke<UpdateSettingsView>("get_update_settings");
}

export function setUpdateSettings(checkOnStart: boolean): Promise<UpdateSettingsView> {
  return invoke<UpdateSettingsView>("set_update_settings", { checkOnStart });
}

/** `automatic`: the check made when the app starts (Rust refuses it while the setting is off). */
export function checkForUpdate(automatic: boolean): Promise<UpdateCheckView> {
  return invoke<UpdateCheckView>("check_for_update", { automatic });
}
