import { useCallback, useEffect, useRef, useState } from "react";
import type { MouseEvent } from "react";

/** A row that can be selected: anything with a key the backend understands. */
export interface Keyed {
  key: string | null;
}

/** Most rows one selection holds (an extract or a remove names at most this many). */
export const MAX_SELECTED = 10_000;

/** Most rows one `load` call is asked for (the backend refuses a page above 1,000). */
export const LOAD_CHUNK = 1000;

/** Rows `offset .. offset + count`, fetched in pages the backend accepts. */
async function loadAll<T>(
  load: (offset: number, limit: number) => Promise<T[]>,
  offset: number,
  count: number,
): Promise<T[]> {
  const out: T[] = [];
  while (out.length < count) {
    const got = await load(offset + out.length, Math.min(LOAD_CHUNK, count - out.length));
    if (got.length === 0) break;
    out.push(...got);
  }
  return out;
}

// ---- Rubber-band (marquee) selection: the pure parts --------------------------

/** What the list tells its selection while the band is dragged (`useMarquee`). */
export type MarqueeEvent =
  /** The pointer moved far enough: the band exists now. `additive` is Ctrl/⌘. */
  | { phase: "start"; additive: boolean }
  /** Rows `lo ..= hi` touch the band; `hi < lo` means none do. */
  | { phase: "update"; lo: number; hi: number; additive: boolean }
  /** The button was released; `moved` is false for a plain click on empty space. */
  | { phase: "end"; moved: boolean; additive: boolean };

/**
 * The rows a band touches. `top` and `bottom` are in the list's content
 * coordinates (pixels from the top of the first row, scrolling included), in
 * either order; row `i` covers `i * rowHeight` up to `(i + 1) * rowHeight`.
 * Rows are as wide as the list, so only the vertical extent matters. `null`
 * when no row is touched (the band is entirely above or below the rows).
 */
export function bandRows(
  top: number,
  bottom: number,
  rowHeight: number,
  total: number,
): { lo: number; hi: number } | null {
  if (total <= 0 || rowHeight <= 0) return null;
  const a = Math.min(top, bottom);
  const b = Math.max(top, bottom);
  if (b < 0 || a >= total * rowHeight) return null;
  const lo = Math.max(0, Math.floor(a / rowHeight));
  const hi = Math.min(total - 1, Math.max(lo, Math.ceil(b / rowHeight) - 1));
  return { lo, hi };
}

/**
 * The selection after a band update. A plain drag replaces the selection with
 * the rows in the band. With Ctrl (or ⌘) the band toggles: rows in the band
 * that were selected when the drag began are deselected, the others are
 * added, so the selection follows the band as it grows and shrinks. Rows
 * without a key cannot be selected.
 */
export function combineMarquee<T extends Keyed>(
  base: readonly T[],
  inBand: readonly T[],
  additive: boolean,
): T[] {
  const hit = inBand.filter((r) => r.key !== null);
  if (!additive) return hit.slice(0, MAX_SELECTED);
  const hitKeys = new Set(hit.map((r) => r.key));
  const baseKeys = new Set(base.map((r) => r.key));
  const kept = base.filter((r) => !hitKeys.has(r.key));
  const added = hit.filter((r) => !baseKeys.has(r.key));
  return [...kept, ...added].slice(0, MAX_SELECTED);
}

/** Pixels from the viewport edge where dragging a band starts to scroll the list. */
export const AUTOSCROLL_EDGE = 28;
/** Most pixels the list scrolls per tick. */
export const AUTOSCROLL_MAX = 28;

/**
 * How far to scroll (negative: up) while a band is dragged with the pointer at
 * `y`, in the same coordinates as the viewport's `top` and `bottom`. Zero in
 * the middle; it grows as the pointer nears the edge and is fastest at, and
 * beyond, `AUTOSCROLL_EDGE` outside it.
 */
export function autoScrollDelta(y: number, top: number, bottom: number): number {
  const into = (depth: number) =>
    Math.max(2, Math.ceil(Math.min(depth, 2 * AUTOSCROLL_EDGE) / (2 * AUTOSCROLL_EDGE) * AUTOSCROLL_MAX));
  if (y < top + AUTOSCROLL_EDGE) return -into(top + AUTOSCROLL_EDGE - y);
  if (y > bottom - AUTOSCROLL_EDGE) return into(y - (bottom - AUTOSCROLL_EDGE));
  return 0;
}

/** What a press of the primary button in the list means. */
export type PressIntent = "marquee" | "dragOut" | "ignore";

/**
 * Whether a press starts a rubber band or leaves the press to the row (a click
 * to select, and a drag out to the file manager).
 *
 * The rule, as in WinRAR and Windows Explorer's details view:
 * - Empty space (below the last row, or between rows' edges) starts a band.
 * - On a row, the icon and the name text are the item: pressing there and
 *   dragging carries the entry out, as before.
 * - On a row, anywhere else (the Size and Type columns, the gap to the right
 *   of a short name) starts a band, so a band can start inside the rows'
 *   area without having to find empty space.
 * - A selected row always keeps drag out wherever it is pressed, so a
 *   selection can be dragged by any part of its rows.
 * - Other buttons, touch (which scrolls), and the scrollbar are not ours.
 */
export function pressIntent(press: {
  button: number;
  pointerType: string;
  onScrollbar: boolean;
  /** `null` off every row. */
  row: { selected: boolean; onItem: boolean } | null;
}): PressIntent {
  if (press.button !== 0 || press.pointerType === "touch" || press.onScrollbar) return "ignore";
  if (press.row === null) return "marquee";
  return press.row.selected || press.row.onItem ? "dragOut" : "marquee";
}

export interface Selection<T extends Keyed> {
  /** The selected rows, in the order they were selected. */
  rows: readonly T[];
  /** The row clicked last (the Info panel shows it). */
  focus: T | null;
  has: (row: T) => boolean;
  /**
   * A click on row `index`: alone it selects the row, with Ctrl (or ⌘) it
   * toggles it, with Shift it selects the range from the last plain click.
   * The range is fetched with `load`, since most of it may not be on screen.
   */
  click: (row: T, index: number, e: Pick<MouseEvent, "ctrlKey" | "metaKey" | "shiftKey">) => void;
  /** Select only this row (keeps it if it is already part of the selection). */
  ensure: (row: T, index: number) => void;
  /** Select every row of the listing, up to `MAX_SELECTED`. */
  all: (total: number) => void;
  clear: () => void;
  /** Follow a rubber band dragged over the list (see `MarqueeEvent`). */
  marquee: (e: MarqueeEvent) => void;
}

/**
 * Multiple selection for a file list, as in a desktop file manager. Rows
 * without a key (an entry whose name was refused) cannot be selected.
 */
export function useSelection<T extends Keyed>(
  load: (offset: number, limit: number) => Promise<T[]>,
  onError: (e: unknown) => void,
): Selection<T> {
  const [rows, setRows] = useState<readonly T[]>([]);
  const [focus, setFocus] = useState<T | null>(null);
  const [anchor, setAnchor] = useState<number | null>(null);

  const has = useCallback((row: T) => row.key !== null && rows.some((r) => r.key === row.key), [rows]);

  const range = useCallback(
    (from: number, to: number) => {
      const lo = Math.min(from, to);
      const hi = Math.min(Math.max(from, to), lo + MAX_SELECTED - 1);
      loadAll(load, lo, hi - lo + 1).then((got) => setRows(got.filter((r) => r.key !== null)), onError);
    },
    [load, onError],
  );

  const click = useCallback(
    (row: T, index: number, e: Pick<MouseEvent, "ctrlKey" | "metaKey" | "shiftKey">) => {
      if (row.key === null) return;
      setFocus(row);
      if (e.shiftKey && anchor !== null) {
        range(anchor, index);
        return;
      }
      setAnchor(index);
      if (e.ctrlKey || e.metaKey) {
        setRows((prev) =>
          prev.some((r) => r.key === row.key)
            ? prev.filter((r) => r.key !== row.key)
            : prev.length < MAX_SELECTED
              ? [...prev, row]
              : prev,
        );
        return;
      }
      setRows([row]);
    },
    [anchor, range],
  );

  const ensure = useCallback(
    (row: T, index: number) => {
      if (row.key === null || rows.some((r) => r.key === row.key)) return;
      setFocus(row);
      setAnchor(index);
      setRows([row]);
    },
    [rows],
  );

  const all = useCallback(
    (total: number) => {
      if (total > 0) range(0, total - 1);
    },
    [range],
  );

  const clear = useCallback(() => {
    setRows((prev) => (prev.length === 0 ? prev : []));
    setFocus(null);
    setAnchor(null);
  }, []);

  // The band: the selection when it began, the rows fetched so far, and a
  // counter so that a slow fetch never overwrites a newer one.
  const rowsRef = useRef<readonly T[]>(rows);
  useEffect(() => {
    rowsRef.current = rows;
  });
  const baseRef = useRef<readonly T[]>([]);
  const cacheRef = useRef(new Map<number, T>());
  const tokenRef = useRef(0);

  const marquee = useCallback(
    (e: MarqueeEvent) => {
      if (e.phase === "start") {
        tokenRef.current++;
        baseRef.current = e.additive ? rowsRef.current : [];
        cacheRef.current = new Map();
        setAnchor(null);
        return;
      }
      if (e.phase === "end") {
        // A plain click on empty space selects nothing, as in a file manager.
        if (!e.moved && !e.additive) clear();
        return;
      }
      const token = ++tokenRef.current;
      const cache = cacheRef.current;
      const hi = Math.min(e.hi, e.lo + MAX_SELECTED - 1);
      let from = -1;
      let to = -1;
      for (let i = e.lo; i <= hi; i++) {
        if (cache.has(i)) continue;
        if (from < 0) from = i;
        to = i;
      }
      const apply = () => {
        if (token !== tokenRef.current) return;
        const inBand: T[] = [];
        for (let i = e.lo; i <= hi; i++) {
          const r = cache.get(i);
          if (r !== undefined) inBand.push(r);
        }
        const next = combineMarquee(baseRef.current, inBand, e.additive);
        setRows(next);
        setFocus(next.at(-1) ?? null);
      };
      if (from < 0) {
        apply();
        return;
      }
      const start = from;
      loadAll(load, start, to - start + 1).then((got) => {
        got.forEach((r, k) => cache.set(start + k, r));
        apply();
      }, onError);
    },
    [clear, load, onError],
  );

  return { rows, focus, has, click, ensure, all, clear, marquee };
}
