import { useLayoutEffect } from "react";
import type { RefObject } from "react";

/**
 * Words beside the toolbar's icons when they fit, icons alone when they do not (owner request
 * 2026-11-10: a toolbar people can read, as a Mac app's "Icon and Text" bar).
 *
 * Whether they fit is measured, not guessed from a breakpoint: the labels are tried on, and if
 * the bar's contents then overflow it they come off again. That holds for every language (a
 * Thai or Chinese label is not the width of the English one) and for every window width. The
 * check runs before paint (a layout effect, a ResizeObserver callback, a MutationObserver
 * callback), so the bar never flickers between the two.
 *
 * It runs again when the window's width changes and when the bar's own contents change: the
 * buttons other components draw into it arrive through portals, and a language change rewrites
 * every label. The bar's width comes from the space the title bar leaves it, not from its
 * contents, so toggling the labels cannot feed back into the observers.
 *
 * The bar carries `data-labels="on"` or `"off"`; the stylesheet shows or visually hides the
 * labels, which stay in the accessible name either way.
 */
export function useToolbarLabels(bar: RefObject<HTMLElement | null>): void {
  useLayoutEffect(() => {
    const el = bar.current;
    if (el === null) return;
    const fit = () => {
      el.setAttribute("data-labels", "on");
      // scrollWidth is the content's width: wider than the box means the labels push
      // something out (the search field has a floor, so it cannot absorb them all).
      if (el.scrollWidth > el.clientWidth + 1) el.setAttribute("data-labels", "off");
    };
    fit();
    // The title bar is the window's width; the toolbar inside it is whatever is left.
    const resize = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(fit);
    resize?.observe(el.parentElement ?? el);
    // Attributes are not watched, so the bar's own data-labels change does not come back.
    const mutation = typeof MutationObserver === "undefined" ? null : new MutationObserver(fit);
    mutation?.observe(el, { childList: true, subtree: true, characterData: true });
    return () => {
      resize?.disconnect();
      mutation?.disconnect();
    };
  }, [bar]);
}
