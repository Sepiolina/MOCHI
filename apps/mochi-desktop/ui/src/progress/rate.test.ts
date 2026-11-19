// The rate and time-left arithmetic, with injected timestamps (no timers).
import { describe, expect, test } from "vitest";

import { etaText, rateText, transferView } from "./messages";
import { INITIAL_RATE, estimate, percentOf, phaseKind, rateReducer, spanOf } from "./rate";
import type { RateAction, RateState } from "./rate";

const MIB = 1024 * 1024;

function run(actions: RateAction[]): RateState {
  return actions.reduce(rateReducer, INITIAL_RATE);
}
const sample = (phase: string, completed: number, total: number | null, now: number): RateAction => ({
  type: "sample",
  phase,
  completed,
  total,
  now,
});

describe("phaseKind", () => {
  test("names the bulk phases and treats every other as a finishing step", () => {
    expect(phaseKind("content")).toBe("bytes");
    expect(phaseKind("export")).toBe("bytes");
    expect(phaseKind("restore")).toBe("items");
    expect(phaseKind("export_scan")).toBe("scan");
    expect(phaseKind("catalog")).toBe("finishing");
    expect(phaseKind("publish")).toBe("finishing");
  });
});

describe("estimate", () => {
  test("is not ready before about 1.5 seconds of samples", () => {
    const s = run([sample("content", 0, 100 * MIB, 1000), sample("content", 10 * MIB, 100 * MIB, 2000)]);
    expect(estimate(s, 10 * MIB, 100 * MIB)).toBeNull();
  });

  test("a steady 10 MiB/s gives the right time left", () => {
    const actions: RateAction[] = [];
    for (let i = 0; i <= 30; i++) actions.push(sample("content", i * 0.5 * 10 * MIB, 200 * MIB, 1000 + i * 500));
    const s = run(actions);
    const done = 15 * 10 * MIB;
    const e = estimate(s, done, 200 * MIB);
    expect(e?.perSecond).toBeCloseTo(10 * MIB, -3);
    expect(e?.secondsLeft).toBeCloseTo(5, 1);
  });

  test("smoothing follows a change of speed without jumping to it", () => {
    let s = run([sample("content", 0, 1e9, 0), sample("content", 10 * MIB, 1e9, 1000), sample("content", 20 * MIB, 1e9, 2000)]);
    const before = s.rate ?? 0;
    s = rateReducer(s, sample("content", 20 * MIB + 40 * MIB, 1e9, 3000));
    expect(s.rate).toBeGreaterThan(before);
    expect(s.rate).toBeLessThan(40 * MIB / 1000);
  });

  test("a new phase starts over", () => {
    const s = run([
      sample("content", 0, 100, 0),
      sample("content", 50, 100, 2000),
      sample("catalog", 1, null, 2100),
    ]);
    expect(s.phase).toBe("catalog");
    expect(s.rate).toBeNull();
    expect(s.bulkSeen).toBe(true);
    expect(estimate(s, 1, null)).toBeNull();
  });

  test("no progress is not an estimate", () => {
    const s = run([sample("content", 5, 100, 0), sample("content", 5, 100, 3000)]);
    expect(estimate(s, 5, 100)).toBeNull();
  });

  test("time spent paused is left out of the clock", () => {
    // 10 per second for 2 s, a 60 s pause, then 10 per second for 1 s more.
    const s = run([
      sample("content", 0, 1000, 0),
      sample("content", 10, 1000, 1000),
      sample("content", 20, 1000, 2000),
      { type: "pause", paused: true, now: 2000 },
      { type: "pause", paused: false, now: 62000 },
      sample("content", 30, 1000, 63000),
    ]);
    expect(s.pausedTotal).toBe(60000);
    expect(s.lastAt - s.startAt).toBe(3000);
    expect((s.rate ?? 0) * 1000).toBeCloseTo(10, 5);
    expect(estimate(s, 30, 1000)?.secondsLeft).toBeCloseTo(97, 0);
  });

  test("pausing twice or resuming when not paused changes nothing", () => {
    const paused = rateReducer(INITIAL_RATE, { type: "pause", paused: true, now: 5 });
    expect(rateReducer(paused, { type: "pause", paused: true, now: 9 }).pausedAt).toBe(5);
    expect(rateReducer(INITIAL_RATE, { type: "pause", paused: false, now: 9 })).toBe(INITIAL_RATE);
  });

  test("reset forgets everything", () => {
    expect(run([sample("content", 1, 2, 3), { type: "reset" }])).toBe(INITIAL_RATE);
  });

  test("a count that goes backwards is no progress, never a negative rate", () => {
    const s = run([sample("content", 50, 100, 0), sample("content", 10, 100, 2000)]);
    expect(s.rate).toBe(0);
  });
});

describe("spanOf and etaText", () => {
  test.each([
    [0, { kind: "less" }],
    [4.9, { kind: "less" }],
    [5, { kind: "seconds", n: 5 }],
    [56, { kind: "seconds", n: 56 }],
    [59.6, { kind: "minutes", n: 1 }],
    [180, { kind: "minutes", n: 3 }],
    [3599, { kind: "hours", hours: 1, minutes: 0 }],
    [3900, { kind: "hours", hours: 1, minutes: 5 }],
    [7380, { kind: "hours", hours: 2, minutes: 3 }],
  ])("%s seconds", (seconds, span) => {
    expect(spanOf(seconds)).toEqual(span);
  });

  test("reads humanely in English", () => {
    expect(etaText(56)).toBe("About 56 seconds left");
    expect(etaText(180)).toBe("About 3 minutes left");
    expect(etaText(60)).toBe("About 1 minute left");
    expect(etaText(3900)).toBe("About 1 hour 5 minutes left");
    expect(etaText(7200)).toBe("About 2 hours left");
    expect(etaText(2)).toBe("Less than 5 seconds left");
  });

  test("speed reads through the size formatter", () => {
    expect(rateText(23.1 * MIB)).toBe("23.1 MiB/s");
  });
});

describe("percentOf", () => {
  test("rounds down, so 100% means all of it", () => {
    expect(percentOf(38, 100)).toBe(38);
    expect(percentOf(999, 1000)).toBe(99);
    expect(percentOf(1000, 1000)).toBe(100);
    expect(percentOf(5, null)).toBeNull();
    expect(percentOf(0, 0)).toBeNull();
  });
});

describe("transferView", () => {
  const steady = (): RateState => {
    const actions: RateAction[] = [];
    for (let i = 0; i <= 8; i++) actions.push(sample("content", i * 5 * MIB, 100 * MIB, i * 500));
    return run(actions);
  };

  test("the bulk phase shows percent, time left and speed", () => {
    const v = transferView(steady(), "content", 40 * MIB, 100 * MIB);
    expect(v.heading).toBe("40% complete");
    expect(v.fraction).toBeCloseTo(0.4);
    expect(v.timeLine).toBe("About 6 seconds left · 10.0 MiB/s");
  });

  test("it says it is calculating until there are enough samples", () => {
    const v = transferView(run([sample("content", 0, 100, 0)]), "content", 0, 100);
    expect(v.timeLine).toBe("Calculating time left…");
  });

  test("items show time left without a speed", () => {
    const actions: RateAction[] = [];
    for (let i = 0; i <= 8; i++) actions.push(sample("restore", i * 10, 400, i * 500));
    const v = transferView(run(actions), "restore", 80, 400);
    expect(v.heading).toBe("20% complete");
    expect(v.timeLine).toBe("About 16 seconds left");
  });

  test("the steps after the bulk phase read Finishing with a full bar, not 0%", () => {
    const after = run([sample("content", 100, 100, 0), sample("catalog", 0, null, 10)]);
    const v = transferView(after, "catalog", 0, null);
    expect(v).toMatchObject({ heading: "Finishing…", fraction: 1, finishing: true, timeLine: "Updating the catalog" });
  });

  test("a finishing step with no bulk phase before it is an indeterminate bar", () => {
    const v = transferView(run([sample("publish", 0, null, 0)]), "publish", 0, null);
    expect(v.fraction).toBeNull();
  });

  test("a count with no total (the scan, an import) has no percentage", () => {
    expect(transferView(INITIAL_RATE, "export_scan", 12, null)).toMatchObject({ heading: null, fraction: null });
    expect(transferView(INITIAL_RATE, "import", 4096, null)).toMatchObject({ heading: null, fraction: null });
  });

  test("before the first event it says Starting", () => {
    expect(transferView(INITIAL_RATE, "", 0, null).heading).toBe("Starting…");
  });
});
