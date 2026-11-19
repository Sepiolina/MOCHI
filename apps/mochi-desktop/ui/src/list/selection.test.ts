// The pure parts of rubber-band selection (list/selection.ts): which rows a band touches, how
// a band combines with the selection, where a press may start one, and the auto-scroll speed.
import { renderHook, act, waitFor } from "@testing-library/react";
import { expect, test } from "vitest";

import {
  AUTOSCROLL_EDGE,
  AUTOSCROLL_MAX,
  autoScrollDelta,
  bandRows,
  combineMarquee,
  MAX_SELECTED,
  pressIntent,
  useSelection,
} from "./selection";

const H = 26;

test("a band touches the rows it overlaps, whichever way it was dragged", () => {
  expect(bandRows(0, 1, H, 10)).toEqual({ lo: 0, hi: 0 });
  expect(bandRows(10, 60, H, 10)).toEqual({ lo: 0, hi: 2 });
  expect(bandRows(60, 10, H, 10)).toEqual({ lo: 0, hi: 2 });
  // Starting exactly at a row's end does not touch the next row; one pixel into it does.
  expect(bandRows(0, 26, H, 10)).toEqual({ lo: 0, hi: 0 });
  expect(bandRows(0, 27, H, 10)).toEqual({ lo: 0, hi: 1 });
  expect(bandRows(26, 40, H, 10)).toEqual({ lo: 1, hi: 1 });
});

test("a band beyond the rows is clamped, and one entirely outside touches none", () => {
  expect(bandRows(-50, 30, H, 10)).toEqual({ lo: 0, hi: 1 });
  expect(bandRows(200, 5000, H, 10)).toEqual({ lo: 7, hi: 9 });
  expect(bandRows(10 * H, 10 * H + 100, H, 10)).toBeNull();
  expect(bandRows(-100, -1, H, 10)).toBeNull();
  expect(bandRows(0, 100, H, 0)).toBeNull();
});

test("a band in a long scrolled list still names the right rows (content coordinates)", () => {
  // Row 5000 starts at 130,000 px.
  expect(bandRows(5000 * H + 3, 5003 * H + 3, H, 100_000)).toEqual({ lo: 5000, hi: 5003 });
});

type Row = { key: string | null };
const r = (k: string | null): Row => ({ key: k });

test("a plain band replaces the selection with the rows it touches; rows without a key are skipped", () => {
  expect(combineMarquee([r("a")], [r("b"), r(null), r("c")], false).map((x) => x.key)).toEqual(["b", "c"]);
  expect(combineMarquee([r("a")], [], false)).toEqual([]);
});

test("a Ctrl band toggles against the selection the drag began with", () => {
  const base = [r("a"), r("b")];
  // Over b and c: b was selected, so it goes; c was not, so it joins; a stays.
  expect(combineMarquee(base, [r("b"), r("c")], true).map((x) => x.key)).toEqual(["a", "c"]);
  // The band leaves b again (recomputed from the same base): everything is as it began.
  expect(combineMarquee(base, [], true).map((x) => x.key)).toEqual(["a", "b"]);
});

test("a band never holds more rows than a selection may", () => {
  const many = Array.from({ length: MAX_SELECTED + 5 }, (_, i) => r(`k${i}`));
  expect(combineMarquee([], many, false)).toHaveLength(MAX_SELECTED);
  expect(combineMarquee([], many, true)).toHaveLength(MAX_SELECTED);
});

test("auto-scroll: still in the middle, faster the nearer the edge, capped beyond it", () => {
  const top = 100;
  const bottom = 400;
  expect(autoScrollDelta(250, top, bottom)).toBe(0);
  expect(autoScrollDelta(top + AUTOSCROLL_EDGE, top, bottom)).toBe(0);
  const near = autoScrollDelta(top + AUTOSCROLL_EDGE - 4, top, bottom);
  const nearer = autoScrollDelta(top + 2, top, bottom);
  expect(near).toBeLessThan(0);
  expect(nearer).toBeLessThan(near);
  expect(autoScrollDelta(top - 500, top, bottom)).toBe(-AUTOSCROLL_MAX);
  expect(autoScrollDelta(bottom - AUTOSCROLL_EDGE + 1, bottom - 300, bottom)).toBeGreaterThan(0);
  expect(autoScrollDelta(bottom + 500, top, bottom)).toBe(AUTOSCROLL_MAX);
  // Never a standstill once inside the zone.
  expect(Math.abs(autoScrollDelta(top + AUTOSCROLL_EDGE - 1, top, bottom))).toBeGreaterThanOrEqual(2);
});

const left = (over: Partial<Parameters<typeof pressIntent>[0]> = {}) =>
  pressIntent({ button: 0, pointerType: "mouse", onScrollbar: false, row: null, ...over });

test("where a press may start a band: empty space, and a row's parts that are not the item", () => {
  // Empty space (below the rows).
  expect(left()).toBe("marquee");
  // An unselected row: the icon and name text are the item (drag out); the rest starts a band.
  expect(left({ row: { selected: false, onItem: true } })).toBe("dragOut");
  expect(left({ row: { selected: false, onItem: false } })).toBe("marquee");
  // A selected row keeps drag out wherever it is pressed.
  expect(left({ row: { selected: true, onItem: false } })).toBe("dragOut");
  expect(left({ row: { selected: true, onItem: true } })).toBe("dragOut");
});

test("other buttons, touch, and the scrollbar never start a band", () => {
  expect(left({ button: 2 })).toBe("ignore");
  expect(left({ button: 1 })).toBe("ignore");
  expect(left({ pointerType: "touch" })).toBe("ignore");
  expect(left({ onScrollbar: true })).toBe("ignore");
  expect(left({ onScrollbar: true, row: { selected: false, onItem: false } })).toBe("ignore");
});

// ---- The selection hook follows a band ----------------------------------------------------------

type Item = { key: string | null; n: number };
const ROWS: Item[] = Array.from({ length: 50 }, (_, n) => ({ key: `k${n}`, n }));

function setup() {
  const calls: [number, number][] = [];
  const load = (offset: number, limit: number) => {
    calls.push([offset, limit]);
    return Promise.resolve(ROWS.slice(offset, offset + limit));
  };
  const view = renderHook(() => useSelection<Item>(load, () => undefined));
  return { view, calls };
}

const keys = (rows: readonly Item[]) => rows.map((x) => x.key);

test("a plain band selects the rows it touches, follows the band as it changes, and focuses the last", async () => {
  const { view, calls } = setup();
  act(() => view.result.current.click(ROWS[9] as Item, 9, { ctrlKey: false, metaKey: false, shiftKey: false }));
  expect(keys(view.result.current.rows)).toEqual(["k9"]);
  act(() => view.result.current.marquee({ phase: "start", additive: false }));
  act(() => view.result.current.marquee({ phase: "update", lo: 2, hi: 4, additive: false }));
  await waitFor(() => expect(keys(view.result.current.rows)).toEqual(["k2", "k3", "k4"]));
  expect(view.result.current.focus?.key).toBe("k4");
  // Growing the band fetches only the rows it had not seen.
  act(() => view.result.current.marquee({ phase: "update", lo: 2, hi: 6, additive: false }));
  await waitFor(() => expect(keys(view.result.current.rows)).toEqual(["k2", "k3", "k4", "k5", "k6"]));
  expect(calls).toEqual([
    [2, 3],
    [5, 2],
  ]);
  // Shrinking it needs no fetch at all.
  act(() => view.result.current.marquee({ phase: "update", lo: 3, hi: 3, additive: false }));
  await waitFor(() => expect(keys(view.result.current.rows)).toEqual(["k3"]));
  expect(calls).toHaveLength(2);
  // A band over no rows selects nothing.
  act(() => view.result.current.marquee({ phase: "update", lo: 0, hi: -1, additive: false }));
  await waitFor(() => expect(view.result.current.rows).toEqual([]));
  act(() => view.result.current.marquee({ phase: "end", moved: true, additive: false }));
});

test("a Ctrl band adds to and toggles the selection that was there", async () => {
  const { view } = setup();
  act(() => view.result.current.click(ROWS[1] as Item, 1, { ctrlKey: false, metaKey: false, shiftKey: false }));
  act(() => view.result.current.click(ROWS[2] as Item, 2, { ctrlKey: true, metaKey: false, shiftKey: false }));
  expect(keys(view.result.current.rows)).toEqual(["k1", "k2"]);
  act(() => view.result.current.marquee({ phase: "start", additive: true }));
  act(() => view.result.current.marquee({ phase: "update", lo: 2, hi: 3, additive: true }));
  await waitFor(() => expect(keys(view.result.current.rows)).toEqual(["k1", "k3"]));
});

test("a plain click on empty space ends with nothing selected; a Ctrl click or a drag keeps what it made", async () => {
  const { view } = setup();
  act(() => view.result.current.click(ROWS[1] as Item, 1, { ctrlKey: false, metaKey: false, shiftKey: false }));
  act(() => view.result.current.marquee({ phase: "end", moved: false, additive: true }));
  expect(keys(view.result.current.rows)).toEqual(["k1"]);
  act(() => view.result.current.marquee({ phase: "end", moved: false, additive: false }));
  expect(view.result.current.rows).toEqual([]);
  expect(view.result.current.focus).toBeNull();
});
