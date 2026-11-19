// The arithmetic behind "About 56 seconds left · 23.1 MiB/s". Pure: every
// function takes the time it needs, so the tests need no timers.
//
// A job reports `completed` (bytes or items) for one phase at a time. The
// rate is smoothed with a time-weighted exponential average, so how often
// events arrive does not change it; time spent paused is taken out of the
// clock; a new phase starts over.

/** How the bar for a phase counts. */
export type PhaseKind =
  /** Bulk work counted in bytes (`content`, `export`, `import`). */
  | "bytes"
  /** Bulk work counted in items (`restore`, `delete`). */
  | "items"
  /** A count of things found, with no total (`export_scan`). */
  | "scan"
  /** A short closing step with no total: the bulk work is done. */
  | "finishing";

export function phaseKind(phase: string): PhaseKind {
  switch (phase) {
    case "content":
    case "export":
    case "import":
      return "bytes";
    case "restore":
    case "delete":
      return "items";
    case "export_scan":
      return "scan";
    default:
      return "finishing";
  }
}

/** Time constant of the smoothing, in milliseconds. */
export const SMOOTHING_MS = 3000;
/** Samples spanning less than this are not enough for an estimate. */
export const MIN_WINDOW_MS = 1500;

export interface RateState {
  phase: string;
  /** Active time (wall clock minus paused time) of the phase's first sample. */
  startAt: number;
  /** Active time and count of the latest sample. */
  lastAt: number;
  lastCompleted: number;
  /** Smoothed units per millisecond; `null` until a second sample. */
  rate: number | null;
  /** Wall-clock time the pause began, while paused. */
  pausedAt: number | null;
  /** Total wall-clock time spent paused so far. */
  pausedTotal: number;
  /** A bulk phase with a known total has been seen: later short phases are "finishing". */
  bulkSeen: boolean;
}

export const INITIAL_RATE: RateState = {
  phase: "",
  startAt: 0,
  lastAt: 0,
  lastCompleted: 0,
  rate: null,
  pausedAt: null,
  pausedTotal: 0,
  bulkSeen: false,
};

export type RateAction =
  | { type: "reset" }
  | { type: "sample"; phase: string; completed: number; total: number | null; now: number }
  | { type: "pause"; paused: boolean; now: number };

export function rateReducer(state: RateState, action: RateAction): RateState {
  switch (action.type) {
    case "reset":
      return INITIAL_RATE;
    case "pause": {
      if (action.paused) return state.pausedAt === null ? { ...state, pausedAt: action.now } : state;
      if (state.pausedAt === null) return state;
      return { ...state, pausedAt: null, pausedTotal: state.pausedTotal + Math.max(0, action.now - state.pausedAt) };
    }
    case "sample": {
      const active = action.now - state.pausedTotal;
      const kind = phaseKind(action.phase);
      const bulkSeen = state.bulkSeen || ((kind === "bytes" || kind === "items") && action.total !== null);
      if (action.phase !== state.phase) {
        return { ...state, phase: action.phase, startAt: active, lastAt: active, lastCompleted: action.completed, rate: null, bulkSeen };
      }
      const dt = active - state.lastAt;
      if (dt <= 0) return { ...state, bulkSeen };
      // Counts never go backwards; a smaller one is treated as no progress.
      const delta = Math.max(0, action.completed - state.lastCompleted);
      const instant = delta / dt;
      const weight = 1 - Math.exp(-dt / SMOOTHING_MS);
      const rate = state.rate === null ? instant : state.rate + weight * (instant - state.rate);
      return { ...state, lastAt: active, lastCompleted: action.completed, rate, bulkSeen };
    }
  }
}

export interface Estimate {
  /** Smoothed units per second. */
  perSecond: number;
  /** Seconds left, if a total is known. */
  secondsLeft: number | null;
}

/** The estimate, or `null` while there are too few samples ("Calculating…"). */
export function estimate(state: RateState, completed: number, total: number | null): Estimate | null {
  if (state.rate === null || state.rate <= 0) return null;
  if (state.lastAt - state.startAt < MIN_WINDOW_MS) return null;
  const perSecond = state.rate * 1000;
  const secondsLeft = total === null ? null : Math.max(0, total - completed) / perSecond;
  return { perSecond, secondsLeft };
}

/** The coarse span "about 56 seconds", "about 3 minutes", "about 1 hour 5 minutes". */
export type Span =
  | { kind: "less" }
  | { kind: "seconds"; n: number }
  | { kind: "minutes"; n: number }
  | { kind: "hours"; hours: number; minutes: number };

export function spanOf(seconds: number): Span {
  if (seconds < 5) return { kind: "less" };
  if (seconds < 59.5) return { kind: "seconds", n: Math.round(seconds) };
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return { kind: "minutes", n: Math.max(1, minutes) };
  const total = Math.round(seconds / 60);
  return { kind: "hours", hours: Math.floor(total / 60), minutes: total % 60 };
}

/** Whole percent 0 to 100 (never 100 before the work is all counted), or `null` with no total. */
export function percentOf(completed: number, total: number | null): number | null {
  if (total === null || total <= 0) return null;
  return Math.min(100, Math.max(0, Math.floor((completed / total) * 100)));
}
