// Animations (owner request 2026-11-14): on, off, or as the operating system says (its
// "reduce motion" / "show animations" setting). A per-computer convenience kept in
// localStorage, like the language; every window follows a change made in another.
//
// The choice is put on <html data-motion="...">; styles.css turns every transition and
// animation off for "off", and for "system" when the system asks for reduced motion.
import { useSyncExternalStore } from "react";

export type MotionChoice = "system" | "on" | "off";

export const MOTION_KEY = "mochi.motion";

export function isMotionChoice(v: unknown): v is MotionChoice {
  return v === "system" || v === "on" || v === "off";
}

function saved(): MotionChoice {
  try {
    const v = window.localStorage.getItem(MOTION_KEY);
    return isMotionChoice(v) ? v : "system";
  } catch {
    return "system";
  }
}

let current: MotionChoice = typeof window === "undefined" ? "system" : saved();
const listeners = new Set<() => void>();

function apply() {
  if (typeof document !== "undefined") document.documentElement.dataset.motion = current;
}
apply();

export function getMotion(): MotionChoice {
  return current;
}

/** Change the choice. `persist` is false when following another window's change. */
export function setMotion(next: MotionChoice, persist = true): void {
  if (persist) {
    try {
      window.localStorage.setItem(MOTION_KEY, next);
    } catch {
      // Not remembered; the choice still applies until the app closes.
    }
  }
  if (next === current) return;
  current = next;
  apply();
  for (const l of [...listeners]) l();
}

if (typeof window !== "undefined") {
  window.addEventListener("storage", (e) => {
    if (e.key === MOTION_KEY && isMotionChoice(e.newValue)) setMotion(e.newValue, false);
  });
}

function subscribe(l: () => void): () => void {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
}

export function useMotion(): MotionChoice {
  return useSyncExternalStore(subscribe, getMotion, getMotion);
}
