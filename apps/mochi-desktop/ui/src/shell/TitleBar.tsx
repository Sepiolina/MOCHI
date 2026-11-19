import { useEffect, useState } from "react";
import type { MouseEvent, ReactNode } from "react";

import { CloseGlyph, MaximizeGlyph, MinimizeGlyph, RestoreGlyph } from "../icons";
import { t } from "../i18n";
import { closeWindow, minimizeWindow, startWindowDrag, toggleMaximizeWindow, watchWindow } from "../ipc/window";
import type { FrameState } from "../ipc/window";

/**
 * The window's one title bar (release 0.9.10, owner request). The windows are built without
 * the operating system's bar (src-tauri/src/frame.rs); this draws the app icon, the window's
 * title, whatever the window puts beside it (the archive window's toolbar), and minimize,
 * maximize/restore and close at the right, as on Windows 11 (the same on Ubuntu).
 *
 * - A press on the bar's empty part moves the window; a double click maximizes or restores
 *   it. A press on anything the user can operate (a button, the search field, a menu) is
 *   that control's own.
 * - The title is the archive's file name, untrusted text: it is rendered as text only and
 *   cut short with an ellipsis (its tooltip holds the whole name). `document.title` carries
 *   the same words for assistive technology; Rust sets the native title for the taskbar.
 * - Close is the same request as the operating system's close button and Alt+F4.
 */
export function TitleBar({
  name,
  heading = false,
  maximizable = true,
  children,
}: {
  /** The file name of the archive on screen (untrusted text), or `null` for none. */
  name: string | null;
  /** Whether the title is the page's level-one heading (the archive window) or plain text. */
  heading?: boolean;
  /** The archive windows can be maximized; the compact window cannot. */
  maximizable?: boolean;
  children?: ReactNode;
}) {
  const [frame, setFrame] = useState<FrameState>({ maximized: false, focused: true });
  useEffect(() => (maximizable ? watchWindow(setFrame) : undefined), [maximizable]);

  const shown = name === null || name === "" ? t("app.title") : name;
  const full = windowTitle(name);
  useEffect(() => {
    document.title = full;
  }, [full]);

  const toggle = () => {
    if (!maximizable) return;
    toggleMaximizeWindow().then(
      (maximized) => {
        if (typeof maximized === "boolean") setFrame((f) => ({ ...f, maximized }));
      },
      () => {},
    );
  };

  // Mirrors Tauri's own drag region: the first press of a click moves the window, the
  // second press of a double click maximizes or restores it.
  const onMouseDown = (e: MouseEvent<HTMLElement>) => {
    if (e.button !== 0) return;
    const target = e.target instanceof Element ? e.target : null;
    if (target === null || target.closest(INTERACTIVE) !== null) return;
    if (e.detail === 2) toggle();
    else startWindowDrag().catch(() => {});
  };

  const Title = heading ? "h1" : "span";
  return (
    <header
      className="titlebar"
      data-testid="titlebar"
      data-focused={frame.focused}
      data-maximized={frame.maximized}
      onMouseDown={onMouseDown}
    >
      <span className="titlebar-icon mascot-face" aria-hidden="true" />
      <Title className="titlebar-title" data-testid="window-title" title={full}>
        {shown}
      </Title>
      {children}
      <div className="window-controls" role="group" aria-label={t("window.controls")}>
        <button
          type="button"
          className="caption"
          data-testid="window-minimize"
          aria-label={t("window.minimize")}
          title={t("window.minimize")}
          onClick={() => void minimizeWindow().catch(() => {})}
        >
          <MinimizeGlyph />
        </button>
        {maximizable && (
          <button
            type="button"
            className="caption"
            data-testid="window-maximize"
            data-state={frame.maximized ? "maximized" : "normal"}
            aria-label={frame.maximized ? t("window.restore") : t("window.maximize")}
            title={frame.maximized ? t("window.restore") : t("window.maximize")}
            onClick={toggle}
          >
            {frame.maximized ? <RestoreGlyph /> : <MaximizeGlyph />}
          </button>
        )}
        <button
          type="button"
          className="caption caption-close"
          data-testid="window-close"
          aria-label={t("window.close")}
          title={t("window.close")}
          onClick={() => void closeWindow().catch(() => {})}
        >
          <CloseGlyph />
        </button>
      </div>
    </header>
  );
}

/** What on the bar is a control of its own, not a place to drag the window from. */
const INTERACTIVE = "button, input, select, textarea, a, label, [role='menu'], .menu, .searchbox";

/** The window's title as the taskbar shows it (the same words as Rust's `WindowStates::title`). */
export function windowTitle(name: string | null): string {
  return name === null || name === "" ? t("app.title") : `${name} - ${t("app.title")}`;
}
