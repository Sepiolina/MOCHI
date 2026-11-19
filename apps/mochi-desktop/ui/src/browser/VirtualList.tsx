import { useCallback, useEffect, useRef, useState } from "react";
import type { CSSProperties, KeyboardEvent, ReactNode, UIEvent } from "react";

import { t } from "../i18n";
import { useMarquee } from "../list/marquee";
import type { MarqueeEvent } from "../list/selection";

/** Rows fetched per request. */
export const PAGE = 200;
/** Rows rendered beyond the viewport on each side. */
const OVERSCAN = 10;
/** Rows assumed visible before the viewport has been measured (tests, first paint). */
const FALLBACK_VISIBLE = 30;

type Props<T> = {
  /** How many rows the listing has (the backend's `total`). */
  total: number;
  /** Fixed row height in pixels; the list is a spacer of `total` rows. */
  rowHeight: number;
  /** Fetch rows `offset .. offset + limit`. */
  loadPage: (offset: number, limit: number) => Promise<T[]>;
  /**
   * Render one row. `tabIndex` is the row's place in the tab order (roving:
   * only one row is a tab stop, the arrow keys move it); put it on the row's
   * main control.
   */
  renderRow: (row: T, index: number, tabIndex: 0 | -1) => ReactNode;
  label: string;
  /**
   * A column-header row drawn above the rows, outside the scrolling area so
   * it stays put. It is presentational: the list itself is the control. The
   * scrollbar's width is passed on as `--sbw` so the header's columns line up
   * with the rows'.
   */
  head?: ReactNode;
  testId?: string;
  /** Id of an element that explains the keys; it describes the list. */
  hintId?: string;
  /** Put keyboard focus on the first row as soon as it is loaded (after a folder opened by key). */
  focusFirst?: boolean;
  /**
   * The key of a row to scroll to and report through `onReveal` once it is
   * loaded (the folder just left, after going up). Pages are fetched in order
   * until it is found; if it is not in this listing, nothing happens.
   */
  revealKey?: string | null | undefined;
  /** The row's key, for `revealKey`. */
  keyOf?: ((row: T) => string | null) | undefined;
  onReveal?: ((row: T, index: number) => void) | undefined;
  /**
   * Turns on the rubber band: pressing on empty space and dragging draws a
   * rectangle, and the list reports the rows it touches here (the selection's
   * `marquee`). See `pressIntent` in `list/selection.ts` for where a press may start it.
   */
  onMarquee?: ((e: MarqueeEvent) => void) | undefined;
  onError: (e: unknown) => void;
  /** Scroll so that this row is in the middle of the view (a hit opened from Find). */
  scrollToIndex?: number | undefined;
};

/**
 * A virtualized list (plan D1 exit): only the rows in view, plus a margin,
 * are in the DOM, and rows are fetched from the backend a page at a time as
 * they scroll into view, so a directory of 100,000 entries never crosses IPC
 * or enters the DOM whole. Remount it (React `key`) when the listing changes.
 */
export function VirtualList<T>({
  total,
  rowHeight,
  loadPage,
  renderRow,
  label,
  head,
  testId,
  hintId,
  focusFirst = false,
  revealKey = null,
  keyOf,
  onReveal,
  onMarquee,
  onError,
  scrollToIndex,
}: Props<T>) {
  const viewportRef = useRef<HTMLDivElement>(null);
  // Loaded pages by page number; copied per page, never per row.
  const [pages, setPages] = useState<ReadonlyMap<number, T[]>>(() => new Map());
  const requestedRef = useRef(new Set<number>());
  const [scrollTop, setScrollTop] = useState(0);
  const [height, setHeight] = useState(0);
  const [scrollbar, setScrollbar] = useState(0);
  // The row that is the tab stop, and the row that should take focus once it
  // is on screen (set by the keyboard handler and by `focusFirst`).
  const [active, setActive] = useState(0);
  const pendingFocusRef = useRef<number | null>(focusFirst ? 0 : null);
  // Rows that arrive after a scroll still re-render; only unmounting stops it.
  const mountedRef = useRef(false);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    const el = viewportRef.current;
    if (!el) return;
    const measure = () => {
      setHeight(el.clientHeight);
      setScrollbar(Math.max(0, el.offsetWidth - el.clientWidth));
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // A row asked for from outside (Find): put it in the middle of the view.
  useEffect(() => {
    const el = viewportRef.current;
    if (scrollToIndex === undefined || !el) return;
    const shown = el.clientHeight > 0 ? el.clientHeight : FALLBACK_VISIBLE * rowHeight;
    const top = Math.max(0, scrollToIndex * rowHeight - Math.floor((shown - rowHeight) / 2));
    el.scrollTop = top;
    setScrollTop(top);
  }, [scrollToIndex, rowHeight]);

  const { band, handlers: bandHandlers } = useMarquee({ viewportRef, rowHeight, total, onMarquee });

  const visible = height > 0 ? Math.ceil(height / rowHeight) : FALLBACK_VISIBLE;
  const first = Math.max(0, Math.floor(scrollTop / rowHeight) - OVERSCAN);
  const last = Math.min(total, Math.floor(scrollTop / rowHeight) + visible + OVERSCAN);

  // Pages loaded so far, for the reveal below (the state copy is for drawing).
  const loadedRef = useRef(new Map<number, T[]>());
  // Looks for the row to reveal after each page arrives (set below).
  const revealStepRef = useRef<() => void>(() => undefined);
  const fetchPage = useCallback(
    (page: number) => {
      if (requestedRef.current.has(page)) return;
      requestedRef.current.add(page);
      const offset = page * PAGE;
      loadPage(offset, Math.min(PAGE, total - offset)).then(
        (got) => {
          if (!mountedRef.current) return;
          loadedRef.current.set(page, got);
          setPages((prev) => new Map(prev).set(page, got));
          revealStepRef.current();
        },
        (e: unknown) => {
          requestedRef.current.delete(page);
          if (mountedRef.current) onError(e);
        },
      );
    },
    [loadPage, total, onError],
  );

  useEffect(() => {
    for (let page = Math.floor(first / PAGE); page * PAGE < last; page++) fetchPage(page);
  }, [first, last, fetchPage]);

  // Reveal: after each page arrives, look for the row among the pages loaded
  // so far; if it is not there, fetch the next page in order. A key that is
  // not in the listing ends the search at the last page.
  const revealedRef = useRef(false);
  useEffect(() => {
    revealStepRef.current = () => {
      if (revealKey === null || revealedRef.current || keyOf === undefined) return;
      for (const [page, rows] of loadedRef.current) {
        const at = rows.findIndex((r) => keyOf(r) === revealKey);
        const row = rows[at];
        if (row === undefined) continue;
        revealedRef.current = true;
        const index = page * PAGE + at;
        const shownHeight = height > 0 ? height : FALLBACK_VISIBLE * rowHeight;
        const top = Math.max(0, index * rowHeight - Math.floor(shownHeight / 2));
        if (viewportRef.current) viewportRef.current.scrollTop = top;
        setScrollTop(top);
        setActive(index);
        if (pendingFocusRef.current !== null) pendingFocusRef.current = index;
        onReveal?.(row, index);
        return;
      }
      for (let page = 0; page * PAGE < total; page++) {
        if (!loadedRef.current.has(page)) {
          fetchPage(page);
          return;
        }
      }
    };
  }, [revealKey, keyOf, onReveal, fetchPage, total, rowHeight, height]);

  // Keyboard (plan D7): arrows, Page Up/Down, Home, End move between rows.
  // Rows outside the window are not in the DOM, so the viewport is scrolled
  // first and the row is focused once it exists.
  const shown = height > 0 ? height : FALLBACK_VISIBLE * rowHeight;
  const firstVisible = Math.min(Math.max(0, Math.floor(scrollTop / rowHeight)), Math.max(0, total - 1));
  const lastVisible = Math.max(firstVisible, Math.min(total, Math.ceil((scrollTop + shown) / rowHeight)) - 1);
  const tabStop = Math.min(Math.max(active, firstVisible), lastVisible);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.altKey || e.ctrlKey || e.metaKey || e.shiftKey || total === 0) return;
    const target = e.target instanceof Element ? e.target.closest("[data-row-index]") : null;
    if (target === null) return;
    const from = Number(target.getAttribute("data-row-index"));
    const page = Math.max(1, Math.floor(shown / rowHeight) - 1);
    let to: number;
    switch (e.key) {
      case "ArrowDown":
        to = from + 1;
        break;
      case "ArrowUp": {
        // The ".." row above the list (when the folder has a parent) is outside the rows.
        const up = from === 0 ? viewportRef.current?.closest(".vtable")?.querySelector<HTMLElement>(".up-row") : null;
        if (up) {
          e.preventDefault();
          up.focus();
          return;
        }
        to = from - 1;
        break;
      }
      case "PageDown":
        to = from + page;
        break;
      case "PageUp":
        to = from - page;
        break;
      case "Home":
        to = 0;
        break;
      case "End":
        to = total - 1;
        break;
      default:
        return;
    }
    e.preventDefault();
    to = Math.min(Math.max(0, to), total - 1);
    const el = viewportRef.current;
    let top = scrollTop;
    if (to * rowHeight < top) top = to * rowHeight;
    else if ((to + 1) * rowHeight > top + shown) top = (to + 1) * rowHeight - shown;
    if (el && top !== scrollTop) el.scrollTop = top;
    setScrollTop(top);
    setActive(to);
    pendingFocusRef.current = to;
  };

  useEffect(() => {
    const want = pendingFocusRef.current;
    if (want === null) return;
    const control = viewportRef.current?.querySelector<HTMLElement>(
      `[data-row-index="${want}"] button, [data-row-index="${want}"] [tabindex]`,
    );
    if (control) {
      control.focus();
      pendingFocusRef.current = null;
    }
  });

  const items: ReactNode[] = [];
  for (let i = first; i < last; i++) {
    const row = pages.get(Math.floor(i / PAGE))?.[i % PAGE];
    items.push(
      <div
        key={i}
        role="listitem"
        className="vrow"
        style={{ top: i * rowHeight, height: rowHeight }}
        aria-posinset={i + 1}
        aria-setsize={total}
        data-row-index={i}
      >
        {row === undefined ? (
          <span className="loading">{t("common.loading")}</span>
        ) : (
          renderRow(row, i, i === tabStop ? 0 : -1)
        )}
      </div>,
    );
  }

  return (
    <div className="vtable" style={{ "--sbw": `${scrollbar}px` } as CSSProperties}>
      {head}
      <div
        ref={viewportRef}
        className="vlist"
        data-testid={testId}
        data-total={total}
        onScroll={(e: UIEvent<HTMLDivElement>) => setScrollTop(e.currentTarget.scrollTop)}
        {...bandHandlers}
      >
        <div
          role="list"
          aria-label={label}
          aria-describedby={hintId}
          className="vspacer"
          style={{ height: total * rowHeight }}
          onKeyDown={onKeyDown}
          onFocus={(e) => {
            const row = e.target.closest("[data-row-index]");
            if (row) setActive(Number(row.getAttribute("data-row-index")));
          }}
        >
          {items}
        </div>
        {band && (
          <div
            className="marquee-band"
            data-testid="marquee-band"
            aria-hidden="true"
            style={{ left: band.left, top: band.top, width: band.width, height: band.height }}
          />
        )}
      </div>
    </div>
  );
}
