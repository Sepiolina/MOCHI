// Typed wrappers over the D5 commands. Types come from Rust through ts-rs
// (./generated, never edited by hand). A passphrase is passed to exactly one
// command call and is not returned, kept, or logged by anything here.
import { invoke } from "@tauri-apps/api/core";

import type { ArchiveSummary } from "./generated/ArchiveSummary";
import type { EnvelopeList } from "./generated/EnvelopeList";
import type { EnvelopeView } from "./generated/EnvelopeView";
import type { HitView } from "./generated/HitView";
import type { OpenOutcome } from "./generated/OpenOutcome";
import type { RekeyReport } from "./generated/RekeyReport";
import type { SearchCoverageView } from "./generated/SearchCoverageView";
import type { SearchResultView } from "./generated/SearchResultView";
import type { SearchScopeChoice } from "./generated/SearchScopeChoice";
import type { SubjectView } from "./generated/SubjectView";

export type {
  EnvelopeList,
  EnvelopeView,
  HitView,
  OpenOutcome,
  RekeyReport,
  SearchCoverageView,
  SearchResultView,
  SearchScopeChoice,
};

/** Open the Encrypted archive waiting as `pending` with its passphrase. */
export function unlockArchive(pending: number, passphrase: string): Promise<ArchiveSummary> {
  return invoke<ArchiveSummary>("unlock_archive", { pending, passphrase });
}

/** Stop waiting for a passphrase. */
export function cancelUnlock(pending: number): Promise<boolean> {
  return invoke<boolean>("cancel_unlock", { pending });
}

/** The head's key envelopes of an open Encrypted archive (needs no key). */
export function listEnvelopes(handle: number): Promise<EnvelopeList> {
  return invoke<EnvelopeList>("list_envelopes", { handle });
}

/** Give the archive chosen for testing or repair its passphrase. */
export function unlockCheckSubject(subject: number, passphrase: string): Promise<SubjectView> {
  return invoke<SubjectView>("unlock_check_subject", { subject, passphrase });
}
