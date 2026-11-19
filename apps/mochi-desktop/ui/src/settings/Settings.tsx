import { useId, useRef, useState } from "react";
import type { KeyboardEvent, ReactNode } from "react";

import { t } from "../i18n";
import { ExtractIcon, GearIcon, InfoIcon, NewArchiveIcon, RefreshIcon } from "../icons";
import { Updates } from "../update/Updates";
import { CrashReports } from "./CrashReports";
import { ExtractSettings } from "./ExtractSettings";
import { Language } from "./Language";
import { Motion } from "./Motion";
import { SettingGroup } from "./Row";
import { Speed } from "./Speed";

export type SettingsTab = "general" | "extraction" | "performance" | "updates" | "privacy";

const TABS: { id: SettingsTab; label: () => string; icon: ReactNode }[] = [
  { id: "general", label: () => t("settings.tabGeneral"), icon: <GearIcon /> },
  { id: "extraction", label: () => t("settings.extractHeading"), icon: <ExtractIcon /> },
  { id: "performance", label: () => t("settings.speedHeading"), icon: <NewArchiveIcon /> },
  { id: "updates", label: () => t("update.heading"), icon: <RefreshIcon /> },
  { id: "privacy", label: () => t("settings.crashHeading"), icon: <InfoIcon /> },
];

/**
 * Settings (owner request 2026-11-12: one long page read as a wall of text). A list of
 * sections on the left, one shown at a time on the right, each setting a row with its
 * name, a one-line description and its control; longer explanations are behind a small
 * "i". Every section stays mounted while another is shown, so its state (and the opted-in
 * update check at start-up) does not depend on which one is open. The tab list follows the
 * ARIA tabs pattern: arrow keys, Home and End move between sections.
 */
export function Settings() {
  const [tab, setTab] = useState<SettingsTab>("general");
  const baseId = useId();
  const tabsRef = useRef<Partial<Record<SettingsTab, HTMLButtonElement | null>>>({});

  const move = (e: KeyboardEvent<HTMLDivElement>) => {
    const i = TABS.findIndex((x) => x.id === tab);
    const next =
      e.key === "ArrowDown" || e.key === "ArrowRight"
        ? (i + 1) % TABS.length
        : e.key === "ArrowUp" || e.key === "ArrowLeft"
          ? (i - 1 + TABS.length) % TABS.length
          : e.key === "Home"
            ? 0
            : e.key === "End"
              ? TABS.length - 1
              : null;
    if (next === null) return;
    e.preventDefault();
    const id = TABS[next]?.id;
    if (id === undefined) return;
    setTab(id);
    tabsRef.current[id]?.focus();
  };

  const panel = (id: SettingsTab): ReactNode => {
    switch (id) {
      case "general":
        return (
          <>
            <Language />
            <SettingGroup>
              <Motion />
            </SettingGroup>
          </>
        );
      case "extraction":
        return <ExtractSettings />;
      case "performance":
        return <Speed />;
      case "updates":
        return <Updates />;
      case "privacy":
        return <CrashReports />;
    }
  };

  return (
    <section aria-labelledby="settings-heading" className="settings" data-testid="settings">
      <h2 id="settings-heading" tabIndex={-1}>
        {t("settings.heading")}
      </h2>
      <div className="settings-layout">
        <div
          role="tablist"
          aria-orientation="vertical"
          aria-label={t("settings.tabsLabel")}
          className="settings-tabs"
          onKeyDown={move}
        >
          {TABS.map((x) => (
            <button
              key={x.id}
              ref={(el) => {
                tabsRef.current[x.id] = el;
              }}
              type="button"
              role="tab"
              id={`${baseId}-tab-${x.id}`}
              aria-controls={`${baseId}-panel-${x.id}`}
              aria-selected={tab === x.id}
              tabIndex={tab === x.id ? 0 : -1}
              className="settings-tab"
              data-testid={`settings-tab-${x.id}`}
              onClick={() => setTab(x.id)}
            >
              {x.icon}
              <span>{x.label()}</span>
            </button>
          ))}
        </div>
        <div className="settings-panels">
          {TABS.map((x) => (
            <div
              key={x.id}
              role="tabpanel"
              id={`${baseId}-panel-${x.id}`}
              aria-labelledby={`${baseId}-tab-${x.id}`}
              className="settings-panel"
              data-testid={`settings-panel-${x.id}`}
              hidden={tab !== x.id}
            >
              <h3 className="settings-panel-title">{x.label()}</h3>
              {panel(x.id)}
            </div>
          ))}
        </div>
      </div>
    </section>
  );
}
