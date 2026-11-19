import type { KeyboardEvent } from "react";

import { BackIcon, ForwardIcon, UpIcon } from "../icons";
import { t } from "../i18n";
import { Name } from "./Name";
import type { Crumb } from "./Tree";

/** What the folder history offers; `useFolderPath` provides all of it. */
export type FolderNavState = {
  path: readonly Crumb[];
  canBack: boolean;
  canForward: boolean;
  back: () => void;
  forward: () => void;
  up: () => void;
};

/**
 * Back, Forward and Up, beside the path bar. Disabled (never hidden) when
 * they cannot act: Up at the root, Back and Forward without history, all
 * three while `disabled` (search results are on screen).
 */
export function NavButtons({
  nav,
  testIdPrefix,
  disabled = false,
}: {
  nav: FolderNavState;
  testIdPrefix: string;
  /** All three off, e.g. while search results (not a folder) are on screen. */
  disabled?: boolean;
}) {
  return (
    <div className="nav-buttons" role="group" aria-label={t("browser.navGroup")}>
      <button
        type="button"
        className="icon-button"
        data-testid={`${testIdPrefix}-back`}
        aria-label={t("browser.navBack")}
        title={t("browser.navBackTip")}
        disabled={disabled || !nav.canBack}
        onClick={nav.back}
      >
        <BackIcon />
      </button>
      <button
        type="button"
        className="icon-button"
        data-testid={`${testIdPrefix}-forward`}
        aria-label={t("browser.navForward")}
        title={t("browser.navForwardTip")}
        disabled={disabled || !nav.canForward}
        onClick={nav.forward}
      >
        <ForwardIcon />
      </button>
      <button
        type="button"
        className="icon-button"
        data-testid={`${testIdPrefix}-up`}
        aria-label={t("browser.navUp")}
        title={t("browser.navUpTip")}
        disabled={disabled || nav.path.length === 0}
        onClick={nav.up}
      >
        <UpIcon />
      </button>
    </div>
  );
}

/**
 * The ".." row at the top of a folder's list (not at the root). It is not a
 * row of the listing: it takes no index, cannot be selected, dragged, counted,
 * extracted or deleted. Double click, Enter or Space go to the parent folder;
 * the Down arrow moves into the first entry.
 */
export function UpRow({ onUp, testId }: { onUp: () => void; testId: string }) {
  const onKeyDown = (e: KeyboardEvent<HTMLButtonElement>) => {
    if (e.altKey || e.ctrlKey || e.metaKey || e.shiftKey) return;
    if (e.key === "ArrowDown") {
      const first = e.currentTarget.closest(".vtable")?.querySelector<HTMLElement>('[data-row-index="0"] button');
      if (first) {
        e.preventDefault();
        first.focus();
      }
    } else if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onUp();
    }
  };
  return (
    <button
      type="button"
      className="entry up-row"
      data-testid={testId}
      aria-label={t("browser.upRowLabel")}
      title={t("browser.navUpTip")}
      onDoubleClick={onUp}
      onKeyDown={onKeyDown}
    >
      <span className="kind">
        <UpIcon />
      </span>
      <span className="entry-name">{t("browser.upRowName")}</span>
      <span className="size" />
      <span className="type" />
    </button>
  );
}

/**
 * The path bar's crumbs: the archive's name for the root, then each folder
 * down to the current one. Crumb i goes to exactly the folder it names. The
 * root and the current folder keep their width when the bar is too narrow;
 * the ones between ellipsize, and every crumb carries its full name as a tooltip.
 */
export function Crumbs({
  rootName,
  path,
  onGo,
  label,
  testId,
}: {
  rootName: string;
  path: readonly Crumb[];
  onGo: (to: Crumb[]) => void;
  label: string;
  testId: string;
}) {
  return (
    <nav aria-label={label} className="crumbs" data-testid={testId}>
      <span className="crumb-part keep">
        <button
          type="button"
          className="crumb"
          data-testid="crumb-root"
          aria-label={t("browser.crumbRoot", { name: rootName })}
          aria-current={path.length === 0 ? "location" : undefined}
          title={t("browser.crumbRoot", { name: rootName })}
          onClick={() => onGo([])}
        >
          <span className="name">{rootName}</span>
        </button>
      </span>
      {path.map((c, i) => {
        const last = i === path.length - 1;
        return (
          <span key={c.key} className={last ? "crumb-part keep" : "crumb-part"}>
            <span className="crumb-sep" aria-hidden="true">
              /
            </span>
            <button
              type="button"
              className="crumb"
              data-testid="crumb"
              aria-current={last ? "location" : undefined}
              title={c.name.text}
              onClick={() => onGo(path.slice(0, i + 1))}
            >
              <Name name={c.name} />
            </button>
          </span>
        );
      })}
    </nav>
  );
}
