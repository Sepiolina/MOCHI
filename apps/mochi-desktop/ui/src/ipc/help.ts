// Typed wrapper over the Help command (audit F8). It takes no argument: Rust resolves the
// guide the installer bundled and opens it with the operating system, so the webview never
// names a path or a URL.
import { invoke } from "@tauri-apps/api/core";

/** Open the bundled user guide in the default program for web pages. */
export function openUserGuide(): Promise<null> {
  return invoke<null>("open_user_guide");
}
