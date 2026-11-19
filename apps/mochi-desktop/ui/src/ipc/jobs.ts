// Typed wrappers over the job commands. The types come from Rust through
// ts-rs (./generated, never edited by hand); this file only names the
// commands and their arguments once.
import { Channel, invoke } from "@tauri-apps/api/core";

import type { IpcError } from "./generated/IpcError";
import type { JobEvent } from "./generated/JobEvent";
import type { JobOutcome } from "./generated/JobOutcome";
import type { JobRequest } from "./generated/JobRequest";

export type { IpcError, JobEvent, JobOutcome, JobRequest };

/** Start a job. Events (started, progress, one finished) go to `onEvent`. */
export function startJob(
  request: JobRequest,
  onEvent: (event: JobEvent) => void,
): Promise<number> {
  const channel = new Channel<JobEvent>();
  channel.onmessage = onEvent;
  return invoke<number>("start_job", { request, onEvent: channel });
}

/** Request cancellation. `false` if the job is no longer running. */
export function cancelJob(jobId: number): Promise<boolean> {
  return invoke<boolean>("cancel_job", { jobId });
}

/**
 * Pause (`true`) or resume (`false`) a job. A paused job waits at its next safe point; cancelling
 * it still works. `false` if the job is no longer running.
 */
export function pauseJob(jobId: number, paused: boolean): Promise<boolean> {
  return invoke<boolean>("pause_job", { jobId, paused });
}

export function isIpcError(value: unknown): value is IpcError {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as { code?: unknown }).code === "string" &&
    typeof (value as { message?: unknown }).message === "string"
  );
}
