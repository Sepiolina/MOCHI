import { useCallback, useRef, useState } from "react";

import type { Crumb } from "./Tree";

/** Most folders the Back list remembers. */
const MAX_HISTORY = 100;

type State = {
  /** Folders from the root down to the one on screen. */
  path: Crumb[];
  back: Crumb[][];
  forward: Crumb[][];
  /** Set when the folder shown is `reveal`'s parent: the child just left. */
  reveal: string | null;
  /** The new listing takes keyboard focus (a folder opened from the list). */
  focusList: boolean;
};

const START: State = { path: [], back: [], forward: [], reveal: null, focusList: false };

/** The key of the child of `to` that `from` runs through, when `to` is a strict ancestor of `from`. */
export function cameOutOf(from: readonly Crumb[], to: readonly Crumb[]): string | null {
  if (to.length >= from.length) return null;
  for (let i = 0; i < to.length; i++) if (from[i]?.key !== to[i]?.key) return null;
  return from[to.length]?.key ?? null;
}

/**
 * The folder on screen as a stack of (key, name) crumbs, with Back and
 * Forward history. Every way of moving (the path bar, the folder tree, a
 * double click, Backspace, Alt+arrows) goes through `go`, so that going to a
 * parent always means the parent the stack holds, never one computed from a
 * display string. Leaving a folder for an ancestor records `reveal`: the
 * child just left, which the list selects as a file manager does.
 *
 * `onMove(fn)` registers what runs on every move: the caller clears its
 * selection there (the selection is built from the listing, which is built
 * from `path`, so it cannot be passed in to the hook).
 */
export function useFolderPath() {
  const [s, setS] = useState<State>(START);
  const changedRef = useRef<() => void>(() => undefined);
  const onMove = useCallback((fn: () => void) => {
    changedRef.current = fn;
  }, []);

  const go = useCallback(
    (to: Crumb[], fromList = false) => {
      changedRef.current();
      setS((p) => {
        const same = p.path.length === to.length && p.path.every((c, i) => c.key === to[i]?.key);
        if (same) return { ...p, focusList: fromList, reveal: null };
        return {
          path: to,
          back: [...p.back, p.path].slice(-MAX_HISTORY),
          forward: [],
          reveal: cameOutOf(p.path, to),
          focusList: fromList,
        };
      });
    },
    [],
  );

  // Up, Back and Forward do nothing (not even clear the selection) when they
  // cannot act: at the root, or with no history that way. Each takes its
  // target from the state it updates, so two presses in one render cannot
  // both act on the same (stale) entry.
  const up = useCallback(
    (fromList = true) => {
      if (s.path.length === 0) return;
      go(s.path.slice(0, -1), fromList);
    },
    [go, s.path],
  );

  const back = useCallback(() => {
    if (s.back.length === 0) return;
    changedRef.current();
    setS((p) => {
      const to = p.back.at(-1);
      if (to === undefined) return p;
      return {
        path: to,
        back: p.back.slice(0, -1),
        forward: [...p.forward, p.path],
        reveal: cameOutOf(p.path, to),
        focusList: true,
      };
    });
  }, [s.back.length]);

  const forward = useCallback(() => {
    if (s.forward.length === 0) return;
    changedRef.current();
    setS((p) => {
      const to = p.forward.at(-1);
      if (to === undefined) return p;
      return {
        path: to,
        back: [...p.back, p.path].slice(-MAX_HISTORY),
        forward: p.forward.slice(0, -1),
        reveal: cameOutOf(p.path, to),
        focusList: true,
      };
    });
  }, [s.forward.length]);

  /** A different archive or snapshot: back to the root with no history. */
  const reset = useCallback(() => {
    changedRef.current();
    setS(START);
  }, []);

  return {
    onMove,
    path: s.path,
    reveal: s.reveal,
    focusList: s.focusList,
    canBack: s.back.length > 0,
    canForward: s.forward.length > 0,
    go,
    up,
    back,
    forward,
    reset,
  };
}

/**
 * Alt+Left and Alt+Right (and the mouse's back and forward buttons) walk the
 * history, Alt+Up goes to the parent folder. Returns true if it handled `e`.
 */
export function folderKeys(
  e: { key: string; altKey: boolean; ctrlKey: boolean; metaKey: boolean; shiftKey: boolean; preventDefault: () => void },
  nav: { up: () => void; back: () => void; forward: () => void },
): boolean {
  if (!e.altKey || e.ctrlKey || e.metaKey || e.shiftKey) return false;
  if (e.key === "ArrowLeft") nav.back();
  else if (e.key === "ArrowRight") nav.forward();
  else if (e.key === "ArrowUp") nav.up();
  else return false;
  e.preventDefault();
  return true;
}
