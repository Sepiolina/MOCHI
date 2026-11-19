import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent as ReactKeyboardEvent, MouseEvent as ReactMouseEvent } from "react";

import { t } from "../i18n";
import { reasonText } from "../shell/availability";
import type { ActionId, AvailableActions } from "../shell/availability";

/** One entry of a context menu: a fixed label from the catalogue, never archive text. */
export interface MenuItem {
  id: string;
  label: string;
  disabled?: boolean;
  /** Why the item is disabled, as the tooltip. */
  title?: string | undefined;
  /** Draw a rule above this item. */
  separator?: boolean;
  run: () => void;
}

const EDITABLE =
  'textarea, input:not([type="button"]):not([type="submit"]):not([type="reset"]):not([type="checkbox"]):not([type="radio"]):not([type="range"]):not([type="file"]):not([type="color"]), [contenteditable=""], [contenteditable="true"]';

/** Whether the native menu (cut, copy, paste) is still wanted at `target`: text fields only. */
function editable(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest(EDITABLE) !== null;
}

/**
 * Keep the webview's own context menu (Refresh, Print, Save as, ...) from
 * showing anywhere except in editable text fields, where cut, copy, and paste
 * stay native. Returns the function that removes the handler.
 */
export function suppressNativeMenu(doc: Document = document): () => void {
  const onContextMenu = (e: MouseEvent) => {
    if (!editable(e.target)) e.preventDefault();
  };
  doc.addEventListener("contextmenu", onContextMenu);
  return () => doc.removeEventListener("contextmenu", onContextMenu);
}

export interface MenuState {
  x: number;
  y: number;
}

/** Where a keyboard-opened menu appears: under the focused row, else the list's corner. */
export function anchorOf(container: HTMLElement): MenuState {
  const active = document.activeElement;
  const el = active instanceof HTMLElement && container.contains(active) ? active : container;
  const r = el.getBoundingClientRect();
  return { x: r.left + Math.min(24, r.width / 2), y: r.bottom };
}

/** Whether `e` is the keyboard's way to a context menu: the Menu key or Shift+F10. */
export function isMenuKey(e: Pick<ReactKeyboardEvent, "key" | "shiftKey" | "ctrlKey" | "altKey" | "metaKey">): boolean {
  return e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey && !e.ctrlKey && !e.altKey && !e.metaKey);
}

/** A context menu opened at a point. Handlers for the area it belongs to come from `bind`. */
export function useContextMenu() {
  const [at, setAt] = useState<MenuState | null>(null);
  const openAt = useCallback((state: MenuState) => setAt(state), []);
  const close = useCallback(() => setAt(null), []);
  return { at, openAt, close };
}

/**
 * A context menu: a `role="menu"` of buttons at a point, kept inside the
 * window. Arrow keys, Home, and End move; Enter or Space chooses; Escape or
 * Tab closes and gives focus back to where it was. It also closes when the
 * pointer goes down outside it, on scroll, resize, and when the window loses
 * focus. Disabled items stay in the list (and are announced as disabled) but
 * cannot be chosen.
 */
export function ContextMenu({
  at,
  items,
  label,
  onClose,
}: {
  at: MenuState;
  items: readonly MenuItem[];
  label: string;
  onClose: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<MenuState>(at);
  const [active, setActive] = useState(() => Math.max(0, items.findIndex((i) => !i.disabled)));
  const returnToRef = useRef<Element | null>(document.activeElement);
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    onCloseRef.current = onClose;
  });

  // Inside the viewport: flip or slide the menu if it would run off an edge.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const w = el.offsetWidth;
    const h = el.offsetHeight;
    const maxX = Math.max(0, window.innerWidth - w - 4);
    const maxY = Math.max(0, window.innerHeight - h - 4);
    setPos({ x: Math.min(Math.max(0, at.x), maxX), y: Math.min(Math.max(0, at.y), maxY) });
  }, [at.x, at.y]);

  const buttons = () => Array.from(ref.current?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]') ?? []);
  useEffect(() => {
    buttons()[active]?.focus();
  }, [active]);

  const dismiss = useCallback((restore: boolean) => {
    const back = returnToRef.current;
    onCloseRef.current();
    if (restore && back instanceof HTMLElement) back.focus();
  }, []);

  useEffect(() => {
    const onPointer = (e: PointerEvent) => {
      if (e.target instanceof Node && ref.current?.contains(e.target)) return;
      dismiss(false);
    };
    const away = () => dismiss(false);
    document.addEventListener("pointerdown", onPointer, true);
    window.addEventListener("scroll", away, true);
    window.addEventListener("wheel", away, { passive: true });
    window.addEventListener("resize", away);
    window.addEventListener("blur", away);
    return () => {
      document.removeEventListener("pointerdown", onPointer, true);
      window.removeEventListener("scroll", away, true);
      window.removeEventListener("wheel", away);
      window.removeEventListener("resize", away);
      window.removeEventListener("blur", away);
    };
  }, [dismiss]);

  const move = (step: 1 | -1) => {
    const n = items.length;
    for (let k = 1; k <= n; k++) {
      const i = (active + step * k + n * k) % n;
      if (!items[i]?.disabled) {
        setActive(i);
        return;
      }
    }
  };

  const choose = (item: MenuItem | undefined) => {
    if (!item || item.disabled) return;
    dismiss(true);
    item.run();
  };

  const onKeyDown = (e: ReactKeyboardEvent) => {
    // Keys in the menu are the menu's: the list around it must not see them
    // (Escape would drop the selection, Backspace would leave the folder).
    e.stopPropagation();
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        move(1);
        break;
      case "ArrowUp":
        e.preventDefault();
        move(-1);
        break;
      case "Home":
        e.preventDefault();
        setActive(Math.max(0, items.findIndex((i) => !i.disabled)));
        break;
      case "End": {
        e.preventDefault();
        const last = items.map((i) => i.disabled).lastIndexOf(false);
        if (last >= 0) setActive(last);
        break;
      }
      case "Enter":
      case " ":
        e.preventDefault();
        choose(items[active]);
        break;
      case "Escape":
        e.preventDefault();
        e.stopPropagation();
        dismiss(true);
        break;
      case "Tab":
        e.preventDefault();
        dismiss(true);
        break;
    }
  };

  return (
    <div
      ref={ref}
      className="menu context-menu"
      role="menu"
      aria-label={label}
      data-testid="context-menu"
      style={{ position: "fixed", left: pos.x, top: pos.y, right: "auto" }}
      onKeyDown={onKeyDown}
      onContextMenu={(e) => e.preventDefault()}
    >
      {items.map((item, i) => (
        <div key={item.id} role="none">
          {item.separator && <hr />}
          <button
            type="button"
            role="menuitem"
            className="menu-item"
            data-testid={`menu-${item.id}`}
            aria-disabled={item.disabled === true ? true : undefined}
            title={item.title}
            tabIndex={i === active ? 0 : -1}
            onClick={() => choose(item)}
            onPointerMove={() => item.disabled !== true && setActive(i)}
          >
            {item.label}
          </button>
        </div>
      ))}
    </div>
  );
}

/**
 * The menu of one file list: `listProps` go on the element around the rows (a
 * right-click anywhere in it, or the Menu key / Shift+F10, opens the menu),
 * and `menu` is drawn inside it while open.
 */
export function useListMenu(items: readonly MenuItem[]) {
  const { at, openAt, close } = useContextMenu();
  const listProps = {
    onContextMenu: (e: ReactMouseEvent<HTMLElement>) => {
      e.preventDefault();
      openAt({ x: e.clientX, y: e.clientY });
    },
    onKeyDownCapture: (e: ReactKeyboardEvent<HTMLElement>) => {
      if (!isMenuKey(e) || (e.target instanceof HTMLInputElement)) return;
      e.preventDefault();
      openAt(anchorOf(e.currentTarget));
    },
  };
  const menu = at !== null && <ContextMenu at={at} items={items} label={t("menu.label")} onClose={close} />;
  return { listProps, menu };
}

/** What a file list's menu does; the list supplies each action. */
export interface ListActions {
  /** Which items are on, from `availableActions` (one answer for menu, toolbar, and keys). */
  available: AvailableActions;
  selectAll: () => void;
  copyNames: () => void;
  extractTo: () => void;
  add: () => void;
  remove: () => void;
}

/**
 * The items of a file list's menu. Every item is always listed; the ones that
 * do not apply (Delete in a read-only archive, anything on an empty selection)
 * are disabled, announced as disabled, and say why in their tooltip.
 */
export function listMenuItems(a: ListActions): MenuItem[] {
  const item = (id: string, label: string, action: ActionId, run: () => void, separator = false): MenuItem => {
    const avail = a.available[action];
    return {
      id,
      label,
      disabled: !avail.enabled,
      title: avail.reason === null ? undefined : reasonText(avail.reason),
      separator,
      run,
    };
  };
  return [
    item("select-all", t("menu.selectAll"), "selectAll", a.selectAll),
    item("copy-names", t("menu.copyNames"), "copyNames", a.copyNames),
    item("extract-to", t("menu.extractTo"), "extractTo", a.extractTo),
    item("add", t("menu.add"), "add", a.add, true),
    item("delete", t("menu.delete"), "remove", a.remove),
  ];
}

/** Put full entry names on the clipboard, one per line. Rejects if the clipboard refuses. */
export function copyNames(names: readonly string[]): Promise<void> {
  if (!navigator.clipboard) return Promise.reject(new Error("the clipboard is not available"));
  return navigator.clipboard.writeText(names.join("\n"));
}
