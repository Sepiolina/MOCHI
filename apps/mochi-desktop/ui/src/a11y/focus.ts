import { useCallback, useEffect, useRef } from "react";

/**
 * Remember where keyboard focus is *now* (call it inside the click or key
 * handler that starts a job) and get back a function that puts it back when
 * the job ends. Starting a job disables the controls, and a browser drops focus
 * from a control that becomes disabled, so without this a keyboard user would
 * land at the top of the page after every test, extract, or search.
 *
 * Focus is only restored if it was lost (on the page body, or on something now
 * disabled): if the user already moved it somewhere, it stays there.
 */
export function rememberFocus(): () => void {
  const el = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  return () => {
    const now = document.activeElement;
    const lost = now === null || now === document.body || (now instanceof HTMLElement && now.matches(":disabled"));
    if (lost && el !== null && el.isConnected && !el.matches(":disabled")) el.focus();
  };
}

/**
 * `remember = useJobFocus(busy)`: call `remember()` in the handler that starts
 * a job; when `busy` goes from true to false, focus is given back (see
 * `rememberFocus`). The effect runs after the commit that re-enabled the
 * controls, so the control can take focus again.
 */
export function useJobFocus(busy: boolean): () => void {
  const restoreRef = useRef<(() => void) | null>(null);
  const wasBusyRef = useRef(false);
  useEffect(() => {
    if (wasBusyRef.current && !busy) {
      restoreRef.current?.();
      restoreRef.current = null;
    }
    wasBusyRef.current = busy;
  }, [busy]);
  return useCallback(() => {
    // An earlier call in the same action (before a dialog) already saw the
    // button; a later one that finds focus on the page body must not erase it.
    if (document.activeElement !== null && document.activeElement !== document.body) {
      restoreRef.current = rememberFocus();
    }
  }, []);
}
