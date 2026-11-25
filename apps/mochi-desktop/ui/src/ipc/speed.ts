// Typed wrappers over the speed commands: compression cores (release 0.9.11)
// and fast extract. Rust saves the choices and applies them to jobs started
// afterwards.
import { invoke } from "@tauri-apps/api/core";

import type { SpeedSettingsView } from "./generated/SpeedSettingsView";

export type { SpeedSettingsView };

export function getSpeedSettings(): Promise<SpeedSettingsView> {
  return invoke<SpeedSettingsView>("get_speed_settings");
}

/**
 * `trustNtfsFlush` (Windows, spec Annex B.2.20), `localHistory` (remember
 * archives on this computer, Annex B.2.22) and `waitForWriter` (wait for
 * another MOCHI program, Annex B.2.23) are kept as saved when omitted.
 */
export function setSpeedSettings(
  allCores: boolean,
  fastExtract: boolean,
  trustNtfsFlush?: boolean,
  localHistory?: boolean,
  waitForWriter?: boolean,
): Promise<SpeedSettingsView> {
  const args: Record<string, boolean> = { allCores, fastExtract };
  if (trustNtfsFlush !== undefined) args.trustNtfsFlush = trustNtfsFlush;
  if (localHistory !== undefined) args.localHistory = localHistory;
  if (waitForWriter !== undefined) args.waitForWriter = waitForWriter;
  return invoke<SpeedSettingsView>("set_speed_settings", args);
}
