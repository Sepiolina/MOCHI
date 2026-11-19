// The window's own title bar (release 0.9.10): the windows are built without the operating
// system's bar, and these commands are its buttons. Each acts on the window that calls it
// (Rust takes the window from the call; the page names none), so the webview needs no
// `core:window` permission (src-tauri/src/frame.rs, capabilities/*.json).
import { Channel, invoke } from "@tauri-apps/api/core";

import type { FrameState } from "./generated/FrameState";

export type { FrameState };

export function minimizeWindow(): Promise<null> {
  return invoke<null>("window_minimize");
}

/** Maximize, or restore a maximized window. Resolves to whether it is maximized now. */
export function toggleMaximizeWindow(): Promise<boolean> {
  return invoke<boolean>("window_toggle_maximize");
}

/** The same close request as the operating system's close button and Alt+F4. */
export function closeWindow(): Promise<null> {
  return invoke<null>("window_close");
}

/** Start moving the window (a press on the title bar's empty part). */
export function startWindowDrag(): Promise<null> {
  return invoke<null>("window_start_drag");
}

/**
 * Hear whether the window is maximized and focused: now, and on each change. Returns the
 * function that stops listening (Rust keeps one listener per window; the newest wins).
 */
export function watchWindow(onChange: (state: FrameState) => void): () => void {
  let live = true;
  const channel = new Channel<FrameState>();
  channel.onmessage = (state) => {
    if (live) onChange(state);
  };
  invoke<FrameState | null>("window_watch", { onChange: channel }).then(
    (state) => {
      if (live && state) onChange(state);
    },
    () => {
      // Outside the app (tests, the Vite page in a browser) there is no window to watch.
    },
  );
  return () => {
    live = false;
  };
}
