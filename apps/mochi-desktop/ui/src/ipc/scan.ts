// Typed wrappers over the virus scan commands. The scan itself is the job
// kind `scan_archive` (`startJob`). Types come from Rust through ts-rs.
import { invoke } from "@tauri-apps/api/core";

import { t } from "../i18n";
import type { DialogText } from "./generated/DialogText";
import type { ScanReportView } from "./generated/ScanReportView";
import type { ScanSetup } from "./generated/ScanSetup";
import type { ScannerView } from "./generated/ScannerView";
import type { ScanVerdict } from "./generated/ScanVerdict";

export type { ScanReportView, ScanSetup, ScannerView, ScanVerdict };

/** The scanners found on this computer and the choice used last. */
export function scanSetup(): Promise<ScanSetup> {
  return invoke<ScanSetup>("scan_setup");
}

/** The native open dialog for the scanner's program; `null` if cancelled. */
export function pickScannerProgram(): Promise<string | null> {
  const text: DialogText = { title: t("scan.pickTitle"), filter: t("scan.pickFilter") };
  return invoke<string | null>("pick_scanner_program", { text });
}
