import { useCallback, useEffect, useState } from "react";

import type { EntryRow } from "../ipc/browse";

/** Most rows looked through for the entry (the folder listing is paged; a huge folder is not scanned whole). */
export const REVEAL_LIMIT = 20_000;
const PAGE = 200;

/** What to bring into view: an entry of a folder of an open archive. */
export interface RevealTarget {
  handle: number;
  /** The folder's key (`null`: the archive root). */
  dir: string | null;
  /** The entry's key. */
  key: string;
}

/**
 * Select an entry in the folder listing, and say which row it is so that the
 * list can scroll to it (a hit opened from MOCHI Find). The target waits until
 * the listing on screen is the target's folder of the target's archive; if the
 * entry is not found in the first `REVEAL_LIMIT` rows (or the file changed), nothing is selected.
 */
export function useReveal({
  handle,
  dir,
  listingId,
  total,
  loadPage,
  select,
}: {
  handle: number | null;
  dir: string | null;
  listingId: string;
  /** Rows in the listing on screen, once counted. */
  total: number | null;
  loadPage: (offset: number, limit: number) => Promise<EntryRow[]>;
  select: (row: EntryRow, index: number) => void;
}) {
  const [target, setTarget] = useState<RevealTarget | null>(null);
  const [found, setFound] = useState<{ id: string; index: number } | null>(null);

  const reveal = useCallback((t: RevealTarget) => setTarget(t), []);

  useEffect(() => {
    if (target === null || handle !== target.handle || dir !== target.dir || total === null) return;
    let live = true;
    const wanted = target;
    void (async () => {
      try {
        for (let offset = 0; live && offset < Math.min(total, REVEAL_LIMIT); offset += PAGE) {
          const rows = await loadPage(offset, Math.min(PAGE, total - offset));
          const at = rows.findIndex((r) => r.key === wanted.key);
          const row = rows[at];
          if (row !== undefined) {
            if (live) {
              select(row, offset + at);
              setFound({ id: listingId, index: offset + at });
            }
            break;
          }
          if (rows.length === 0) break;
        }
      } catch {
        // The listing reports its own errors; a failed look-up just selects nothing.
      }
      if (live) setTarget(null);
    })();
    return () => {
      live = false;
    };
  }, [target, handle, dir, total, listingId, loadPage, select]);

  return { reveal, index: found?.id === listingId ? found.index : undefined };
}
