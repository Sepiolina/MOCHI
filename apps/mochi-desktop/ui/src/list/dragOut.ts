import { useEffect, useRef } from "react";
import type { PointerEvent } from "react";

/** How far, in pixels, the pointer moves with the button down before a drag starts. */
const THRESHOLD = 6;

export interface DragOut<T> {
  onPointerDown: (e: PointerEvent<HTMLElement>, row: T) => void;
  onPointerMove: (e: PointerEvent<HTMLElement>) => void;
  onPointerUp: () => void;
}

/**
 * Drag entries out of the list onto the desktop or a file-manager window.
 * The webview names no path: `begin` asks Rust to extract the entries into a
 * folder of its own and to start the system's drag with those files. It is
 * given the row the drag started on (the caller decides whether the drag
 * carries the selection or that row alone), and returns `null` to do nothing.
 */
export function useDragOut<T>(begin: (row: T) => Promise<unknown> | null): DragOut<T> {
  const startRef = useRef<{ x: number; y: number; id: number; row: T } | null>(null);
  const busyRef = useRef(false);
  const beginRef = useRef(begin);
  useEffect(() => {
    beginRef.current = begin;
  });
  return {
    onPointerDown: (e, row) => {
      if (e.button !== 0 || e.pointerType === "touch") return;
      startRef.current = { x: e.clientX, y: e.clientY, id: e.pointerId, row };
    },
    onPointerUp: () => {
      startRef.current = null;
    },
    onPointerMove: (e) => {
      const s = startRef.current;
      if (s === null || busyRef.current || (e.buttons & 1) === 0 || s.id !== e.pointerId) return;
      if (Math.abs(e.clientX - s.x) < THRESHOLD && Math.abs(e.clientY - s.y) < THRESHOLD) return;
      startRef.current = null;
      const running = beginRef.current(s.row);
      if (running === null) return;
      busyRef.current = true;
      document.body.classList.add("drag-preparing");
      void running.finally(() => {
        busyRef.current = false;
        document.body.classList.remove("drag-preparing");
      });
    },
  };
}
