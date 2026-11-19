import type { KeyboardEvent, MouseEvent, PointerEvent as ReactPointerEvent } from "react";

import type { DragOut } from "./dragOut";
import type { Selection } from "./selection";

/** The handlers a row of a file list shares: select, open, drag out. */
export function rowHandlers<T extends { key: string | null }>(
  sel: Selection<T>,
  row: T,
  index: number,
  open: () => void,
  drag: DragOut<T>,
  /** Enter opens the row (a folder); otherwise Enter, like Space, selects it. */
  opensOnEnter: boolean,
) {
  return {
    onClick: (e: MouseEvent) => sel.click(row, index, e),
    // A right-click on a row outside the selection selects it first, as in a
    // file manager. The list around the rows opens the menu.
    onContextMenu: () => sel.ensure(row, index),
    onDoubleClick: open,
    onKeyDown: (e: KeyboardEvent) => {
      if (e.key === "Enter" && opensOnEnter) {
        e.preventDefault();
        open();
      }
    },
    onPointerDown: (e: ReactPointerEvent<HTMLElement>) => drag.onPointerDown(e, row),
    onPointerMove: drag.onPointerMove,
    onPointerUp: drag.onPointerUp,
  };
}

/**
 * Ctrl+A selects the folder, Escape selects nothing, Backspace goes up,
 * Delete removes (MOCHI only).
 */
export function listKeys(
  e: KeyboardEvent,
  handlers: { all: () => void; none: () => boolean; up: () => void; remove?: () => void },
) {
  if (e.target instanceof HTMLInputElement) return;
  if (e.key === "Escape") {
    // Only when there is a selection to drop: otherwise Escape is for whatever else listens.
    if (handlers.none()) e.preventDefault();
  } else if ((e.ctrlKey || e.metaKey) && (e.key === "a" || e.key === "A")) {
    e.preventDefault();
    handlers.all();
  } else if (e.key === "Backspace" && !e.ctrlKey && !e.metaKey) {
    e.preventDefault();
    handlers.up();
  } else if (e.key === "Delete" && handlers.remove) {
    e.preventDefault();
    handlers.remove();
  }
}
