import { useCallback, useEffect, useRef, useState } from "react";
import type { MouseEvent, PointerEvent, RefObject } from "react";

import { autoScrollDelta, bandRows, pressIntent } from "./selection";
import type { MarqueeEvent } from "./selection";

/** How far, in pixels, the pointer moves with the button down before a band starts. */
const THRESHOLD = 4;
/** How often, in milliseconds, a list scrolls while a band is held at its edge. */
const TICK_MS = 30;

/** The band as drawn: pixels inside the scrolling content of the list. */
export interface Band {
  left: number;
  top: number;
  width: number;
  height: number;
}

/**
 * What a press landed on, read from the DOM of a list row. A row is a
 * `[data-row-index]` element; it is selected when its button says so
 * (`aria-pressed`); the item is the icon and the name text (`.kind`, `.name`,
 * `.escaped`), as opposed to the row's other columns and its empty gap.
 */
function rowAt(target: EventTarget | null): { selected: boolean; onItem: boolean } | null {
  if (!(target instanceof Element)) return null;
  const row = target.closest("[data-row-index]");
  if (row === null) return null;
  return {
    selected: row.querySelector('[aria-pressed="true"]') !== null,
    onItem: target.closest(".kind, .name, .escaped") !== null,
  };
}

interface Gesture {
  id: number;
  /** Where the press began, in content coordinates (so scrolling moves the band). */
  ax: number;
  ay: number;
  /** The pointer now, in the window's coordinates. */
  cx: number;
  cy: number;
  startX: number;
  startY: number;
  active: boolean;
  additive: boolean;
  onRow: boolean;
  last: string;
  timer: ReturnType<typeof setInterval> | null;
}

/**
 * The rubber band of a virtualized list (`VirtualList`): press on empty space
 * (rule in `pressIntent`), drag, and every row the translucent rectangle
 * touches is selected, as in a file manager. The list is told through
 * `onMarquee` (the selection hook's `marquee`). Dragging past the top or bottom
 * edge scrolls the list, and the band keeps growing with it. The band is
 * anchored in content coordinates, so it also follows a wheel scroll.
 *
 * Spread `handlers` on the scrolling element and draw `band` inside it.
 */
export function useMarquee({
  viewportRef,
  rowHeight,
  total,
  onMarquee,
}: {
  viewportRef: RefObject<HTMLDivElement | null>;
  rowHeight: number;
  total: number;
  onMarquee?: ((e: MarqueeEvent) => void) | undefined;
}) {
  const [band, setBand] = useState<Band | null>(null);
  const gestureRef = useRef<Gesture | null>(null);
  const swallowClickRef = useRef(false);
  const latestRef = useRef({ rowHeight, total, onMarquee });
  useEffect(() => {
    latestRef.current = { rowHeight, total, onMarquee };
  });

  const stop = useCallback(() => {
    const g = gestureRef.current;
    if (g?.timer) clearInterval(g.timer);
    gestureRef.current = null;
    setBand(null);
  }, []);
  useEffect(() => stop, [stop]);

  /** Recompute the band from the pointer and the scroll position, and tell the list. */
  const update = useCallback(() => {
    const g = gestureRef.current;
    const el = viewportRef.current;
    if (g === null || el === null || !g.active) return;
    const r = el.getBoundingClientRect();
    const maxX = el.clientWidth > 0 ? el.clientWidth : Number.POSITIVE_INFINITY;
    const maxY = el.scrollHeight > 0 ? el.scrollHeight : Number.POSITIVE_INFINITY;
    const clamp = (v: number, max: number) => Math.min(Math.max(0, v), max);
    const x = clamp(g.cx - r.left, maxX);
    const y = clamp(g.cy - r.top + el.scrollTop, maxY);
    const left = Math.min(g.ax, x);
    const top = Math.min(g.ay, y);
    setBand({ left, top, width: Math.abs(x - g.ax), height: Math.abs(y - g.ay) });
    const { rowHeight: h, total: n, onMarquee: tell } = latestRef.current;
    const hit = bandRows(Math.min(g.ay, y), Math.max(g.ay, y), h, n);
    const range = hit ?? { lo: 0, hi: -1 };
    const key = `${range.lo}:${range.hi}`;
    if (key === g.last) return;
    g.last = key;
    tell?.({ phase: "update", lo: range.lo, hi: range.hi, additive: g.additive });
  }, [viewportRef]);

  const finish = useCallback(
    (e: PointerEvent<HTMLElement>) => {
      const g = gestureRef.current;
      if (g === null || g.id !== e.pointerId) return;
      const tell = latestRef.current.onMarquee;
      const moved = g.active;
      const fromEmpty = !g.onRow;
      const additive = g.additive;
      try {
        e.currentTarget.releasePointerCapture?.(e.pointerId);
      } catch {
        // The pointer was already released.
      }
      stop();
      // The click that follows a band must not select the row it ends on.
      if (moved) {
        swallowClickRef.current = true;
        setTimeout(() => {
          swallowClickRef.current = false;
        }, 0);
      }
      // A plain click on empty space ends with the selection cleared; a click on
      // a row's other columns is left to the row.
      if (moved || fromEmpty) tell?.({ phase: "end", moved, additive });
    },
    [stop],
  );

  const handlers = {
    onPointerDownCapture: (e: PointerEvent<HTMLDivElement>) => {
      if (latestRef.current.onMarquee === undefined) return;
      const el = e.currentTarget;
      const r = el.getBoundingClientRect();
      const intent = pressIntent({
        button: e.button,
        pointerType: e.pointerType,
        onScrollbar:
          (el.clientWidth > 0 && e.clientX >= r.left + el.clientWidth) ||
          (el.clientHeight > 0 && e.clientY >= r.top + el.clientHeight),
        row: rowAt(e.target),
      });
      if (intent !== "marquee") return;
      // The row's own handlers (a click, a drag out) are not for this press.
      e.stopPropagation();
      const x = e.clientX - r.left;
      const y = e.clientY - r.top + el.scrollTop;
      gestureRef.current = {
        id: e.pointerId,
        ax: x,
        ay: y,
        cx: e.clientX,
        cy: e.clientY,
        startX: e.clientX,
        startY: e.clientY,
        active: false,
        additive: e.ctrlKey || e.metaKey,
        onRow: rowAt(e.target) !== null,
        last: "",
        timer: null,
      };
    },
    onPointerMove: (e: PointerEvent<HTMLDivElement>) => {
      const g = gestureRef.current;
      if (g === null || g.id !== e.pointerId) return;
      if ((e.buttons & 1) === 0) {
        finish(e);
        return;
      }
      g.cx = e.clientX;
      g.cy = e.clientY;
      if (!g.active) {
        if (Math.abs(g.cx - g.startX) < THRESHOLD && Math.abs(g.cy - g.startY) < THRESHOLD) return;
        g.active = true;
        // From here moves outside the list still arrive (a click on a row, before
        // the band exists, must keep its own target, so capture waits for this).
        try {
          e.currentTarget.setPointerCapture?.(e.pointerId);
        } catch {
          // No capture: the band still works inside the list.
        }
        latestRef.current.onMarquee?.({ phase: "start", additive: g.additive });
        // Hold the pointer at an edge and the list keeps scrolling.
        g.timer = setInterval(() => {
          const now = gestureRef.current;
          const el = viewportRef.current;
          if (now === null || el === null) return;
          const r = el.getBoundingClientRect();
          const d = autoScrollDelta(now.cy, r.top, r.bottom);
          if (d !== 0) {
            el.scrollTop += d;
            update();
          }
        }, TICK_MS);
      }
      update();
    },
    onPointerUp: finish,
    onPointerCancel: finish,
    onClickCapture: (e: MouseEvent<HTMLDivElement>) => {
      if (!swallowClickRef.current) return;
      swallowClickRef.current = false;
      e.stopPropagation();
      e.preventDefault();
    },
  };

  return { band, handlers };
}
