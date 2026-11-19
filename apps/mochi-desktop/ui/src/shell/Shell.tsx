import { createContext, use, useCallback } from "react";
import type { ReactNode } from "react";
import { createPortal } from "react-dom";

import type { ForeignArchiveSummary } from "../ipc/foreign";

/** Which archive the window is showing: a MOCHI one, or a ZIP/7z/RAR/tar one. */
export type Owner = "mochi" | "foreign";

/**
 * The places the toolbar and the window offer to the components that own an
 * action. A component keeps its own state, test ids and focus handling and
 * only draws its button into the toolbar through `ToolSlot`; the toolbar
 * itself (in `Browser`) has no idea what the buttons do.
 */
export type SlotName =
  | "add"
  | "remove"
  | "extract"
  | "extractTo"
  | "test"
  | "new"
  | "stage"
  | "count"
  | "status";

export interface Shell {
  /** Whose archive is on screen; a slot only shows what its owner draws. */
  active: Owner;
  register: (name: SlotName, el: HTMLElement | null) => void;
  slots: Partial<Record<SlotName, HTMLElement>>;
  /**
   * The foreign browser tells the window when it has an archive open, and its
   * file name for the title bar (untrusted text, shown as text only).
   */
  setForeignOpen: (open: boolean, name?: string | null) => void;
  /** Changes when a MOCHI archive is shown, so an open foreign archive closes. */
  closeForeignToken: number;
  /** Changes when the toolbar asks the foreign browser to pick an archive. */
  openForeignToken: number;
  /** Whether the toolbar's Info panel is open. */
  infoOpen: boolean;
  /**
   * What the toolbar's search field holds, trimmed; empty when there is no
   * search. The browser on screen lists what matches instead of a folder.
   */
  query: string;
  /** Empty the search field (a folder was opened from the results). */
  clearQuery: () => void;
  /**
   * A foreign archive the window's own Open (or a drop) opened, or one that
   * needs its password: the foreign browser shows it when `token` changes.
   */
  foreignHandoff: ForeignHandoff | null;
}

/** What the window's Open hands to the foreign browser. */
export type ForeignHandoff =
  | { token: number; kind: "opened"; archive: ForeignArchiveSummary }
  | { token: number; kind: "password"; wrong: boolean };

export const ShellContext = createContext<Shell | null>(null);

/**
 * The slots drawn inside the More menu rather than on the title bar (release
 * 0.9.10: a calmer bar), and the key that reaches each action without the
 * menu. Key names stay in English in every language (src/i18n/README.md).
 */
export const MENU_SLOTS: Partial<Record<SlotName, Shortcut>> = {
  new: { shown: "Ctrl+N", aria: "Control+N" },
  extract: { shown: "Ctrl+E", aria: "Control+E" },
  remove: { shown: "Delete", aria: "Delete" },
};

/** A key as the menu shows it, and as `aria-keyshortcuts` spells it. */
export interface Shortcut {
  shown: string;
  aria: string;
}

/** Set while a `ToolButton` is drawn into a menu slot: it is then a menu item. */
const MenuItemContext = createContext<{ shortcut: Shortcut } | null>(null);

export function useShell(): Shell | null {
  return use(ShellContext);
}

/**
 * Draw `children` into a toolbar (or window) slot. Outside a window, as in a
 * component's own tests, they are drawn in place.
 */
export function ToolSlot({
  name,
  owner = "mochi",
  children,
}: {
  name: SlotName;
  owner?: Owner | "any";
  children: ReactNode;
}) {
  const shell = useShell();
  if (shell === null) return <>{children}</>;
  if (owner !== "any" && owner !== shell.active) return null;
  const el = shell.slots[name];
  if (!el) return null;
  const shortcut = MENU_SLOTS[name];
  return createPortal(
    shortcut === undefined ? children : <MenuItemContext value={{ shortcut }}>{children}</MenuItemContext>,
    el,
  );
}

/** The empty element a `ToolSlot` draws into. Its ref is stable per name. */
export function SlotHost({ name, className }: { name: SlotName; className?: string }) {
  const shell = useShell();
  const register = shell?.register;
  const ref = useCallback(
    (el: HTMLElement | null) => {
      register?.(name, el);
    },
    [register, name],
  );
  return <span ref={ref} className={className ?? "tool-cell"} data-slot={name} />;
}

/**
 * One toolbar button: an icon and its word, the word shown whenever the bar has
 * room for every word (owner request 2026-11-10, `useToolbarLabels`). The label
 * is the accessible name either way, and the tooltip unless `title` holds a
 * longer one (or why the button is off). Drawn into a menu slot (`MENU_SLOTS`)
 * it is a menu item instead: icon, label and its key, with the same test id,
 * state and action, so the owning component does not need to know where it is.
 */
export function ToolButton({
  icon,
  label,
  title,
  testId,
  disabled,
  onClick,
  expanded,
  pressed,
  primary,
}: {
  icon: ReactNode;
  label: string;
  title?: string;
  testId?: string;
  disabled?: boolean;
  onClick: () => void;
  expanded?: boolean;
  pressed?: boolean;
  primary?: boolean;
}) {
  const menu = use(MenuItemContext);
  if (menu !== null) {
    return (
      <button
        type="button"
        className="menu-item"
        data-testid={testId}
        title={title ?? label}
        disabled={disabled}
        aria-pressed={pressed}
        aria-keyshortcuts={menu.shortcut.aria}
        onClick={onClick}
      >
        {icon}
        <span className="menu-label">{label}</span>
        <span className="menu-key" aria-hidden="true">
          {menu.shortcut.shown}
        </span>
      </button>
    );
  }
  return (
    <button
      type="button"
      className={primary ? "tool primary" : "tool"}
      data-testid={testId}
      title={title ?? label}
      disabled={disabled}
      aria-expanded={expanded}
      aria-pressed={pressed}
      onClick={onClick}
    >
      {icon}
      {/* Beside the icon when the bar has room, visually hidden when it does not
          (`useToolbarLabels`); the accessible name either way. */}
      <span className="tool-label">{label}</span>
    </button>
  );
}
