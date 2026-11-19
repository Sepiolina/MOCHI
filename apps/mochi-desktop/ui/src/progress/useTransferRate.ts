import { useCallback, useReducer } from "react";

import { INITIAL_RATE, rateReducer } from "./rate";
import type { RateState } from "./rate";

export interface TransferRate {
  state: RateState;
  /** A progress event arrived (`now` is `Date.now()` taken by the caller). */
  sample: (phase: string, completed: number, total: number | null, now: number) => void;
  /** The job was paused or resumed. */
  pause: (paused: boolean, now: number) => void;
  /** A new job starts. */
  reset: () => void;
}

/**
 * Speed and time-left bookkeeping for one job at a time. The caller feeds it
 * from its event handler, with the time, so rendering stays pure; the math
 * is `rateReducer`.
 */
export function useTransferRate(): TransferRate {
  const [state, dispatch] = useReducer(rateReducer, INITIAL_RATE);
  const sample = useCallback(
    (phase: string, completed: number, total: number | null, now: number) =>
      dispatch({ type: "sample", phase, completed, total, now }),
    [],
  );
  const pause = useCallback((paused: boolean, now: number) => dispatch({ type: "pause", paused, now }), []);
  const reset = useCallback(() => dispatch({ type: "reset" }), []);
  return { state, sample, pause, reset };
}
