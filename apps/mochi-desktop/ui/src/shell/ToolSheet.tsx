import { useEffect, useRef } from "react";
import type { ReactNode, RefObject } from "react";

import { CloseIcon } from "../icons";
import { t } from "../i18n";

/** The tools the More menu and the Help button open. */
export type ToolId = "help" | "passphrases" | "check" | "settings" | "find";

const TITLES: Record<ToolId, () => string> = {
  help: () => t("help.heading"),
  passphrases: () => t("app.sectionPassphrases"),
  check: () => t("app.sectionCheck"),
  settings: () => t("app.sectionSettings"),
  find: () => t("find.heading"),
};

/**
 * A window over the list that shows one tool at a time, as a desktop app's
 * dialog does: a title bar with a close button, and its own scrolling. Every
 * tool stays mounted while another is shown, so a running test or an open
 * report is still there when the user comes back. Escape closes it.
 */
export function ToolSheet({
  tool,
  onClose,
  returnFocus,
  children,
}: {
  tool: ToolId | null;
  onClose: () => void;
  /** Where focus goes when the sheet closes and its opener cannot take it. */
  returnFocus?: RefObject<HTMLElement | null>;
  children: Partial<Record<ToolId, ReactNode>>;
}) {
  const bodyRef = useRef<HTMLDivElement>(null);
  const openerRef = useRef<HTMLElement | null>(null);
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    onCloseRef.current = onClose;
  });

  useEffect(() => {
    if (tool === null) return;
    if (openerRef.current === null) {
      const active = document.activeElement;
      openerRef.current = active instanceof HTMLElement && active !== document.body ? active : null;
    }
    // Each tool's own heading is the sheet's title; it takes focus.
    bodyRef.current?.querySelector<HTMLElement>(`[data-tool="${tool}"] h2`)?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !e.defaultPrevented) {
        e.preventDefault();
        onCloseRef.current();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [tool]);

  // Give focus back to what opened the sheet (or the More button).
  const wasOpenRef = useRef(false);
  useEffect(() => {
    if (wasOpenRef.current && tool === null) {
      const opener = openerRef.current;
      openerRef.current = null;
      const usable = opener !== null && opener.isConnected && !(opener as HTMLButtonElement).disabled;
      // The opener may sit in the closed More menu: fall back to the More button.
      if (usable && opener.offsetParent !== null) opener.focus();
      else returnFocus?.current?.focus();
    }
    wasOpenRef.current = tool !== null;
  }, [tool, returnFocus]);

  return (
    <div className="dialog-layer" hidden={tool === null} data-testid="tool-sheet">
      <div className="dialog tool-sheet" role="group" aria-label={tool === null ? undefined : TITLES[tool]()}>
        <div className="dialog-close">
          <button
            type="button"
            className="icon-button"
            data-testid="tool-sheet-close"
            aria-label={t("common.close")}
            title={t("common.close")}
            onClick={onClose}
          >
            <CloseIcon />
          </button>
        </div>
        <div className="dialog-body" ref={bodyRef}>
          {(Object.keys(TITLES) as ToolId[]).map((id) =>
            children[id] === undefined ? null : (
              <div key={id} className="tool-pane" data-tool={id} hidden={tool !== id}>
                {children[id]}
              </div>
            ),
          )}
        </div>
      </div>
    </div>
  );
}
