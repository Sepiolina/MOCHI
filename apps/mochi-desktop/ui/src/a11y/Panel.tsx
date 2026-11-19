import { useEffect, useId, useRef } from "react";
import type { ReactNode } from "react";

import { CloseIcon } from "../icons";
import { t } from "../i18n";

/**
 * Move focus into a panel the user just opened and give it back when the panel
 * goes away. The panels are inline (not modal), so there is no trap: Tab
 * continues through the page in reading order, and Escape is not needed to
 * leave. If the control that opened the panel is gone or disabled when the
 * panel closes, focus falls back to the element named by `fallbackId`.
 */
export function useFocusIn<T extends HTMLElement>(fallbackId?: string) {
  const ref = useRef<T>(null);
  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    ref.current?.focus();
    return () => {
      const usable =
        opener !== null &&
        opener.isConnected &&
        !(opener as HTMLButtonElement).disabled &&
        opener !== document.body;
      if (usable) {
        opener.focus();
      } else if (fallbackId) {
        document.getElementById(fallbackId)?.focus();
      }
    };
  }, [fallbackId]);
  return ref;
}

export interface PanelProps {
  /** The panel's heading content; focus lands on it when the panel opens. */
  heading: ReactNode;
  /** The group's accessible name, when it differs from the heading's text. */
  label?: string;
  testId?: string;
  /** Section heading to focus when the opener cannot take focus back. */
  fallbackId?: string;
  className?: string;
  /** Closes the panel: a close button in its title bar, and Escape. */
  onClose?: (() => void) | undefined;
  children: ReactNode;
}

/**
 * A panel the user opened: a labelled group whose heading takes focus. In the
 * main window it is drawn as a small dialog over the list (a desktop app's
 * way); inside a tool it stays in the tool's own flow.
 */
export function Panel({ heading, label, testId, fallbackId, className, onClose, children }: PanelProps) {
  const headingId = useId();
  const ref = useFocusIn<HTMLHeadingElement>(fallbackId);
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    onCloseRef.current = onClose;
  });
  const closable = onClose !== undefined;
  useEffect(() => {
    if (!closable) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !e.defaultPrevented) {
        e.preventDefault();
        onCloseRef.current?.();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [closable]);
  return (
    <div className="dialog-layer">
      <div
        className={className ? `panel dialog ${className}` : "panel dialog"}
        role="group"
        aria-label={label}
        aria-labelledby={label === undefined ? headingId : undefined}
        data-testid={testId}
      >
        <div className="dialog-head">
          <h3 id={headingId} ref={ref} tabIndex={-1}>
            {heading}
          </h3>
          {onClose && (
            <button
              type="button"
              className="icon-button"
              aria-label={t("common.close")}
              title={t("common.close")}
              onClick={onClose}
            >
              <CloseIcon />
            </button>
          )}
        </div>
        {children}
      </div>
    </div>
  );
}
