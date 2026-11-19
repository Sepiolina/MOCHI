// The folders above a search hit, for the path bar. A listing key is the
// stored path's bytes in hexadecimal (`parse_key` in src-tauri/src/browse.rs
// and foreign.rs), so the folders above an entry are the prefixes of its key
// that end before a `/` (0x2f). The names come from the hit's escaped path,
// split at the same separators: an escape never produces a `/`, so both
// splits agree. Display only; nothing here builds a key the backend did not
// send apart from these prefixes.
import type { NameText } from "../ipc/browse";

export type Crumb = { key: string; name: NameText };

/** The folders that contain the entry `key` (whose path is `path`), root first. */
export function ancestorsOf(key: string, path: NameText): Crumb[] {
  const names = path.text.split("/");
  const out: Crumb[] = [];
  let part = 0;
  for (let i = 0; i + 1 < key.length; i += 2) {
    if (key.slice(i, i + 2).toLowerCase() !== "2f") continue;
    if (i === 0) return [];
    const name = names[part];
    if (name === undefined) return [];
    out.push({ key: key.slice(0, i), name: { text: name, verbatim: path.verbatim } });
    part++;
  }
  return out.length === names.length - 1 ? out : [];
}

/** The last component of an escaped path, and the folder it is in. */
export function splitPath(path: NameText): { name: NameText; folder: string } {
  const at = path.text.lastIndexOf("/");
  return at < 0
    ? { name: path, folder: "" }
    : { name: { text: path.text.slice(at + 1), verbatim: path.verbatim }, folder: path.text.slice(0, at) };
}
