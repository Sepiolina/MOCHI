// Test data shaped like what the Rust side sends (generated types), for the
// D4 tests only.
import type { RepairPlanView, RepairResultView, ReportView } from "../ipc/check";

export function report(extra: Partial<ReportView> = {}): ReportView {
  return {
    report_id: 1,
    kind: "test",
    file_name: "a.mochi",
    deep: false,
    keyless: false,
    schema_version: 1,
    archive_id: "ab".repeat(32),
    checked_commit: "cd".repeat(32),
    expected_head: null,
    freshness_anchor: "none",
    started_at: "2026-10-13T00:00:00Z",
    completed_at: "2026-10-13T00:00:01Z",
    level: "restoration",
    scope: { text: "full retained history", verbatim: true },
    dimensions: [
      { dimension: "durability", status: "UNKNOWN", required: false },
      { dimension: "integrity", status: "PASS", required: true },
      { dimension: "recoverability", status: "PASS", required: true },
      { dimension: "freshness", status: "UNKNOWN", required: false },
    ],
    overall_status: "UNKNOWN",
    policy_result: "PASS",
    exit_code: 0,
    operational_error: false,
    coverage: {
      expected_objects: "4",
      checked_objects: "4",
      expected_bytes: "2048",
      checked_bytes: "2048",
      evidence_age_seconds: null,
    },
    findings: [],
    skipped: [],
    json: '{"schema_version":1}\n',
    ...extra,
  };
}

export function planView(extra: Partial<RepairPlanView> = {}): RepairPlanView {
  return {
    plan_id: 4,
    file_name: "bad.mochi",
    outcome: "partial",
    exit_code: 2,
    archive_id: "ab".repeat(32),
    head_found_by: "eof",
    head_seq: 2,
    tail_state: "clean",
    tail_len: "0",
    chain_break_at: null,
    chain_break_reason: null,
    ladder: [
      { step: 1, name: "expected head", status: "used", detail: { text: "head at end of file", verbatim: true } },
      { step: 7, name: "bounded salvage scan", status: "not_attempted", detail: { text: "not asked for", verbatim: true } },
    ],
    snapshots: [0, 1, 2].map((seq) => ({
      seq,
      commit_id: String(seq).repeat(64),
      entries: "4",
      omitted: seq === 2 ? "1" : "0",
      catalog: "image",
      trust: "chain",
    })),
    lost: [],
    omitted: [
      {
        path: { text: "big", verbatim: true },
        kind: "file",
        snapshots: [2],
        code: "STORED_INTEGRITY_FAILED",
        reason: { text: "stored bytes do not match", verbatim: true },
      },
    ],
    omitted_total: "1",
    manifest_failures: 0,
    retention: { state: "carried", holds: 0, holds_lost: 0, expired: 0 },
    parity: null,
    copies: [],
    donations: "0",
    salvage: null,
    salvage_used: false,
    damage_found: true,
    ...extra,
  };
}

export function resultView(extra: Partial<RepairResultView> = {}): RepairResultView {
  return {
    outcome: "partial",
    exit_code: 2,
    file_name: "new.mochi",
    new_archive_id: "ef".repeat(32),
    new_len: "4096",
    commits: [0, 1, 2].map((s) => ({ source_seq: s, new_seq: s, entries: "4", omitted: "0", trust: "chain" })),
    lost: [],
    omitted: planView().omitted,
    omitted_total: "1",
    profile_constraints_lost: [],
    retention_loss_accepted: false,
    reverification: {
      level: "restoration",
      overall_status: "UNKNOWN",
      policy_result: "PASS",
      exit_code: 0,
      findings: "0",
    },
    durability_note: null,
    source_kept: true,
    salvage_used: false,
    ...extra,
  };
}
