// Typed wrappers over the test, health, and repair commands (plan D4). Types
// come from Rust through ts-rs (./generated, never edited by hand). The
// webview never names a path: Rust shows every native dialog (the archive to
// test or repair, a copy, the new archive, where a report is exported) and
// keeps the choice. The test and repair jobs start through start_job.
import { invoke } from "@tauri-apps/api/core";

import { checkSubjectDialog, reportDialog, repairCopyDialog, repairOutputDialog } from "./dialogs";

import type { CheckLevel } from "./generated/CheckLevel";
import type { CheckResult } from "./generated/CheckResult";
import type { CopyView } from "./generated/CopyView";
import type { DimensionView } from "./generated/DimensionView";
import type { FindingView } from "./generated/FindingView";
import type { HealthView } from "./generated/HealthView";
import type { LadderStepView } from "./generated/LadderStepView";
import type { LostSnapshotView } from "./generated/LostSnapshotView";
import type { OmittedView } from "./generated/OmittedView";
import type { RepairOutcomeView } from "./generated/RepairOutcomeView";
import type { RepairPlanView } from "./generated/RepairPlanView";
import type { RepairResultView } from "./generated/RepairResultView";
import type { RepairSetupAction } from "./generated/RepairSetupAction";
import type { RepairSetupView } from "./generated/RepairSetupView";
import type { ReportView } from "./generated/ReportView";
import type { RetentionView } from "./generated/RetentionView";
import type { StatusView } from "./generated/StatusView";
import type { SubjectSource } from "./generated/SubjectSource";
import type { SubjectView } from "./generated/SubjectView";

export type {
  CheckLevel,
  CheckResult,
  CopyView,
  DimensionView,
  FindingView,
  HealthView,
  LadderStepView,
  LostSnapshotView,
  OmittedView,
  RepairOutcomeView,
  RepairPlanView,
  RepairResultView,
  RepairSetupAction,
  RepairSetupView,
  ReportView,
  RetentionView,
  StatusView,
  SubjectSource,
  SubjectView,
};

/**
 * Choose the archive to test or repair: the native open dialog (`null` if
 * the user cancelled it), the archive open in the browser, or the new archive
 * the last repair wrote. Nothing is read yet.
 */
export function chooseCheckSubject(from: SubjectSource): Promise<SubjectView | null> {
  return invoke<SubjectView | null>("choose_check_subject", { from, text: checkSubjectDialog() });
}

/** The health summary of the chosen archive (runs no check). */
export function healthReport(subject: number): Promise<HealthView> {
  return invoke<HealthView>("health_report", { subject });
}

/**
 * Export a report: Rust shows the save dialog and writes the exact JSON core
 * produced to a new file. The file's name, or `null` if the user cancelled.
 */
export function exportReport(report: number): Promise<string | null> {
  return invoke<string | null>("export_report", { report, text: reportDialog() });
}

/** Change (or only read) the repair's copies and new file. */
export function repairSetup(action: RepairSetupAction): Promise<RepairSetupView> {
  return invoke<RepairSetupView>("repair_setup", {
    action,
    text: action.action === "choose_output" ? repairOutputDialog() : repairCopyDialog(),
  });
}
