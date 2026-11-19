// Typed wrappers over the speed commands: compression cores (release 0.9.11)
// and fast extract. Rust saves the choices and applies them to jobs started
// afterwards.
import { invoke } from "@tauri-apps/api/core";

import type { SpeedSettingsView } from "./generated/SpeedSettingsView";

export type { SpeedSettingsView };

export function getSpeedSettings(): Promise<SpeedSettingsView> {
  return invoke<SpeedSettingsView>("get_speed_settings");
}

/** `trustNtfsFlush` (Windows, spec Annex B.2.20) is kept as saved when omitted. */
export function setSpeedSettings(
  allCores: boolean,
  fastExtract: boolean,
  trustNtfsFlush?: boolean,
): Promise<SpeedSettingsView> {
  return trustNtfsFlush === undefined
    ? invoke<SpeedSettingsView>("set_speed_settings", { allCores, fastExtract })
    : invoke<SpeedSettingsView>("set_speed_settings", { allCores, fastExtract, trustNtfsFlush });
}
