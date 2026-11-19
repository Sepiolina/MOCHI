// The rubber band on a real VirtualList: where a press starts it, what the list is told while it
// is dragged, the drawn rectangle, auto-scroll at the edges, and that the row handlers (click,
// drag out) are left alone when the press is theirs. jsdom has no layout, so the list's size and
// scroll position are given by hand.
import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

import { VirtualList } from "../browser/VirtualList";
import type { MarqueeEvent } from "./selection";

const H = 26;

interface Harness {
  events: MarqueeEvent[];
  rowDowns: number[];
  rowClicks: number[];
  list: HTMLElement;
  scroll: { top: number };
}

/** Layout for the scrolling element: 200 px wide (a 15 px scrollbar), 300 px tall. */
function layout(list: HTMLElement, content: number): { top: number } {
  const scroll = { top: 0 };
  Object.defineProperty(list, "scrollTop", {
    configurable: true,
    get: () => scroll.top,
    set: (v: number) => {
      scroll.top = Math.max(0, Math.min(v, content - 300));
    },
  });
  Object.defineProperty(list, "clientWidth", { configurable: true, value: 185 });
  Object.defineProperty(list, "clientHeight", { configurable: true, value: 300 });
  Object.defineProperty(list, "scrollHeight", { configurable: true, value: Math.max(300, content) });
  list.getBoundingClientRect = () =>
    ({ left: 0, top: 0, right: 200, bottom: 300, width: 200, height: 300, x: 0, y: 0, toJSON: () => ({}) }) as DOMRect;
  return scroll;
}

async function mount(total = 5, selected: number[] = []): Promise<Harness> {
  const events: MarqueeEvent[] = [];
  const rowDowns: number[] = [];
  const rowClicks: number[] = [];
  render(
    <VirtualList
      total={total}
      rowHeight={H}
      label="Rows"
      testId="vl"
      loadPage={(offset, limit) => Promise.resolve(Array.from({ length: limit }, (_, i) => offset + i))}
      onError={() => undefined}
      onMarquee={(e) => events.push(e)}
      renderRow={(n: number, index, tabIndex) => (
        <button
          type="button"
          className="entry"
          data-testid="row"
          aria-pressed={selected.includes(n)}
          tabIndex={tabIndex}
          onPointerDown={() => rowDowns.push(index)}
          onClick={() => rowClicks.push(index)}
        >
          <span className="kind" aria-label="File" />
          <span className="entry-name">
            <span className="name">{`row ${n}`}</span>
          </span>
          <span className="size">1 KB</span>
        </button>
      )}
    />,
  );
  await screen.findAllByTestId("row");
  const list = screen.getByTestId("vl");
  const scroll = layout(list, total * H);
  return { events, rowDowns, rowClicks, list, scroll };
}

let nextId = 1;
function pointer(target: Element, type: string, x: number, y: number, init: PointerEventInit = {}) {
  act(() => {
    target.dispatchEvent(
      new PointerEvent(type, {
        bubbles: true,
        cancelable: true,
        button: 0,
        buttons: type === "pointerup" ? 0 : 1,
        clientX: x,
        clientY: y,
        pointerId: nextId,
        pointerType: "mouse",
        ...init,
      }),
    );
  });
}

beforeEach(() => {
  nextId += 1;
});
afterEach(() => {
  vi.useRealTimers();
});

const updates = (h: Harness) => h.events.filter((e) => e.phase === "update");

test("pressing on empty space and dragging reports the rows the band touches and draws it", async () => {
  const h = await mount(5);
  // Below the five rows (130 px): empty space in the list.
  pointer(h.list, "pointerdown", 50, 200);
  expect(h.events).toEqual([]);
  pointer(h.list, "pointermove", 50, 40);
  expect(h.events[0]).toEqual({ phase: "start", additive: false });
  expect(updates(h).at(-1)).toEqual({ phase: "update", lo: 1, hi: 4, additive: false });
  const band = screen.getByTestId("marquee-band");
  expect(band).toHaveStyle({ left: "50px", top: "40px", width: "0px", height: "160px" });
  // Moving on draws it wider and reports only a change of rows.
  pointer(h.list, "pointermove", 120, 30);
  expect(band).toHaveStyle({ left: "50px", width: "70px", top: "30px", height: "170px" });
  const before = h.events.length;
  pointer(h.list, "pointermove", 125, 31);
  expect(h.events).toHaveLength(before);
  pointer(h.list, "pointermove", 120, 10);
  expect(updates(h).at(-1)).toEqual({ phase: "update", lo: 0, hi: 4, additive: false });
  pointer(h.list, "pointerup", 120, 10);
  expect(h.events.at(-1)).toEqual({ phase: "end", moved: true, additive: false });
  expect(screen.queryByTestId("marquee-band")).toBeNull();
});

test("the click that ends a band does not select the row under the pointer", async () => {
  const h = await mount(5);
  pointer(h.list, "pointerdown", 50, 200);
  pointer(h.list, "pointermove", 50, 60);
  pointer(h.list, "pointerup", 50, 60);
  const row = screen.getAllByTestId("row")[2] as HTMLElement;
  act(() => row.click());
  expect(h.rowClicks).toEqual([]);
});

test("Ctrl (or Command) makes the band additive", async () => {
  const h = await mount(5);
  pointer(h.list, "pointerdown", 50, 200, { ctrlKey: true });
  pointer(h.list, "pointermove", 50, 60);
  expect(h.events[0]).toEqual({ phase: "start", additive: true });
  pointer(h.list, "pointerup", 50, 60);
  expect(h.events.at(-1)).toEqual({ phase: "end", moved: true, additive: true });
});

test("a press on a row's size column starts a band and the row's own handlers do not see it", async () => {
  const h = await mount(5);
  const size = screen.getAllByTestId("row")[1]?.querySelector(".size") as HTMLElement;
  pointer(size, "pointerdown", 150, 40);
  expect(h.rowDowns).toEqual([]);
  pointer(size, "pointermove", 150, 100);
  expect(h.events[0]).toEqual({ phase: "start", additive: false });
  expect(updates(h).at(-1)).toMatchObject({ lo: 1, hi: 3 });
  pointer(size, "pointerup", 150, 100);
});

test("a press on the name text (or the row's icon) of an unselected row is left to the row: drag out", async () => {
  const h = await mount(5);
  const rows = screen.getAllByTestId("row");
  pointer(rows[1]?.querySelector(".name") as HTMLElement, "pointerdown", 30, 40);
  pointer(rows[1]?.querySelector(".name") as HTMLElement, "pointermove", 30, 90);
  pointer(rows[2]?.querySelector(".kind") as HTMLElement, "pointerdown", 10, 60);
  expect(h.rowDowns).toEqual([1, 2]);
  expect(h.events).toEqual([]);
  expect(screen.queryByTestId("marquee-band")).toBeNull();
});

test("a press anywhere on a selected row is left to the row, so the selection can be dragged out", async () => {
  const h = await mount(5, [3]);
  const row = screen.getAllByTestId("row")[3] as HTMLElement;
  pointer(row.querySelector(".size") as HTMLElement, "pointerdown", 150, 3 * H + 5);
  pointer(row, "pointerdown", 150, 3 * H + 5);
  expect(h.rowDowns).toEqual([3, 3]);
  pointer(row, "pointermove", 150, 3 * H + 80);
  expect(h.events).toEqual([]);
});

test("a click on empty space reports an end without movement; a click on a row's other columns reports nothing", async () => {
  const h = await mount(5);
  pointer(h.list, "pointerdown", 50, 200);
  pointer(h.list, "pointerup", 50, 200);
  expect(h.events).toEqual([{ phase: "end", moved: false, additive: false }]);
  h.events.length = 0;
  const size = screen.getAllByTestId("row")[0]?.querySelector(".size") as HTMLElement;
  pointer(size, "pointerdown", 150, 10);
  pointer(size, "pointermove", 152, 11);
  pointer(size, "pointerup", 152, 11);
  expect(h.events).toEqual([]);
  // The click goes to the row as usual.
  act(() => (screen.getAllByTestId("row")[0] as HTMLElement).click());
  expect(h.rowClicks).toEqual([0]);
});

test("a few pixels of jitter do not start a band", async () => {
  const h = await mount(5);
  pointer(h.list, "pointerdown", 50, 200);
  pointer(h.list, "pointermove", 52, 202);
  expect(h.events).toEqual([]);
  expect(screen.queryByTestId("marquee-band")).toBeNull();
  pointer(h.list, "pointerup", 52, 202);
});

test("the scrollbar, the right button, and touch never start a band", async () => {
  const h = await mount(5);
  pointer(h.list, "pointerdown", 190, 200); // x beyond clientWidth: the scrollbar
  pointer(h.list, "pointermove", 190, 40);
  pointer(h.list, "pointerup", 190, 40);
  pointer(h.list, "pointerdown", 50, 200, { button: 2, buttons: 2 });
  pointer(h.list, "pointermove", 50, 40, { buttons: 2 });
  pointer(h.list, "pointerup", 50, 40);
  pointer(h.list, "pointerdown", 50, 200, { pointerType: "touch" });
  pointer(h.list, "pointermove", 50, 40, { pointerType: "touch" });
  expect(h.events).toEqual([]);
});

test("losing the button (released outside) ends the band", async () => {
  const h = await mount(5);
  pointer(h.list, "pointerdown", 50, 200);
  pointer(h.list, "pointermove", 50, 60);
  pointer(h.list, "pointermove", 50, 70, { buttons: 0 });
  expect(h.events.at(-1)).toEqual({ phase: "end", moved: true, additive: false });
  expect(screen.queryByTestId("marquee-band")).toBeNull();
});

test("holding the pointer at the bottom edge scrolls the list and the band keeps growing", async () => {
  const h = await mount(100); // 2,600 px of rows in a 300 px window
  vi.useFakeTimers();
  pointer(h.list, "pointerdown", 50, 100);
  pointer(h.list, "pointermove", 50, 295); // inside the bottom 28 px
  const first = updates(h).at(-1);
  expect(first).toMatchObject({ lo: 3, hi: 11 });
  act(() => {
    vi.advanceTimersByTime(300);
  });
  expect(h.scroll.top).toBeGreaterThan(100);
  const later = updates(h).at(-1);
  expect(later && later.phase === "update" && later.hi).toBeGreaterThan(11);
  // Moving back to the middle stops the scrolling.
  pointer(h.list, "pointermove", 50, 150);
  const stopped = h.scroll.top;
  act(() => {
    vi.advanceTimersByTime(300);
  });
  expect(h.scroll.top).toBe(stopped);
  pointer(h.list, "pointerup", 50, 150);
  // And no timer is left running (the click guard clears itself at once).
  act(() => {
    vi.advanceTimersByTime(10);
  });
  expect(vi.getTimerCount()).toBe(0);
});

test("holding the pointer above the top edge scrolls up, and the band reaches the first row", async () => {
  const h = await mount(100);
  vi.useFakeTimers();
  h.scroll.top = 1000;
  pointer(h.list, "pointerdown", 50, 150); // content y 1150
  pointer(h.list, "pointermove", 50, -20);
  act(() => {
    vi.advanceTimersByTime(3000);
  });
  expect(h.scroll.top).toBe(0);
  expect(updates(h).at(-1)).toMatchObject({ lo: 0 });
  pointer(h.list, "pointerup", 50, -20);
});
