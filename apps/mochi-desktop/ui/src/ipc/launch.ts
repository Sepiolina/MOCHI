// The shell's open, extract, and add verbs (plan D6). Rust receives the
// command line and opens or stages for itself; the webview hears only that an
// archive was opened, or why a launch failed. It never sees or sends a path.
//
// Rust has one listener for launches, and two sections of the window care:
// the MOCHI browser (`opened`) and the foreign browser (`foreign_opened`,
// `foreign_password`, plan D8). `subscribeLaunches` registers with Rust once
// while anyone listens and fans each event out; every subscriber ignores the
// events that are not its own.
import { Channel, invoke } from "@tauri-apps/api/core";

import type { LaunchEvent } from "./generated/LaunchEvent";

export type { LaunchEvent };

const listeners = new Set<(event: LaunchEvent) => void>();
let registered = false;

/**
 * Listen for launches; returns the function that stops listening. Anything
 * that arrived before the first subscription is delivered now.
 */
export function subscribeLaunches(onEvent: (event: LaunchEvent) => void): () => void {
  listeners.add(onEvent);
  if (!registered) {
    registered = true;
    const channel = new Channel<LaunchEvent>();
    channel.onmessage = (event) => {
      for (const l of [...listeners]) l(event);
    };
    invoke<null>("watch_launches", { onLaunch: channel }).catch(() => {
      registered = false;
    });
  }
  return () => {
    listeners.delete(onEvent);
    // The next subscriber registers again (Rust keeps only the newest sink).
    if (listeners.size === 0) registered = false;
  };
}
