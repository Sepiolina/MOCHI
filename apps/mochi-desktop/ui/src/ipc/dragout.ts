// Typed wrappers over the drag-out commands: dragging selected entries out of
// an open archive into the operating system's file manager. Rust extracts them
// into a temporary folder it makes itself and starts the native drag; the
// webview names no path and is never told one. Neither command has a result
// type to generate: they resolve with `null` once the drag has started, or
// reject with an `IpcError` (`INVALID_ARGUMENT` for a bad or empty selection,
// `LIMIT_EXCEEDED` above 8 GiB declared or while another drag is running;
// suggest Extract then).
import { invoke } from "@tauri-apps/api/core";

/**
 * Drag entries of snapshot `seq` of an open MOCHI archive (`keys` as
 * `list_children` returned them; a folder brings what is under it). Call it as
 * the drag gesture begins: the files are extracted first, and the system
 * starts a drag only while the mouse button is still held.
 */
export function dragOut(handle: number, seq: number, keys: string[]): Promise<null> {
  return invoke<null>("drag_out", { handle, seq, keys });
}

/** The same for an open foreign archive (ZIP, 7z, RAR, tar). */
export function dragOutForeign(handle: number, keys: string[]): Promise<null> {
  return invoke<null>("drag_out_foreign", { handle, keys });
}
