import { useEffect, useId, useState } from "react";

import { Hint } from "../a11y/Hint";
import { errorText } from "../browser/format";
import { t } from "../i18n";
import { extractFolderDialog } from "../ipc/dialogs";
import { browseExtractFolder, getExtractSettings, setExtractSettings, setOpenFolderWhenDone } from "../ipc/start";
import type { ExtractSettingsView, ExtractStart } from "../ipc/start";
import { SettingGroup, SettingRow } from "./Row";

/**
 * Settings, Extraction (owner request 2026-11-12): which folder "Extract to..." starts in
 * (the Desktop by default, the folder used last, or a folder the user chooses here, which
 * suits a computer whose archives live on a network drive), and "Open the folder when done",
 * the choice the compact window shares. Rust keeps both; the folder is chosen with the
 * system's folder dialog and validated in Rust.
 */
export function ExtractSettings() {
  const [state, setState] = useState<ExtractSettingsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const startId = useId();
  const startNoteId = useId();
  const openId = useId();
  const openNoteId = useId();

  useEffect(() => {
    let live = true;
    getExtractSettings().then(
      (s) => {
        if (live) setState(s);
      },
      (e: unknown) => {
        if (live) setError(errorText(t("settings.extractLoadFailed"), e));
      },
    );
    return () => {
      live = false;
    };
  }, []);

  const act = async (work: () => Promise<void>) => {
    setError(null);
    setBusy(true);
    try {
      await work();
    } catch (e) {
      setError(errorText(t("settings.extractSaveFailed"), e));
    } finally {
      setBusy(false);
    }
  };

  /** Choose a folder with the system dialog, then start there. */
  const chooseFolder = () =>
    act(async () => {
      const picked = await browseExtractFolder(state?.custom ?? "", extractFolderDialog());
      if (picked !== null) setState(await setExtractSettings("custom", picked));
    });

  const pick = (start: ExtractStart) => {
    // "A folder of my choice" with none chosen yet asks for one first.
    if (start === "custom" && (state?.custom ?? "") === "") {
      void chooseFolder();
      return;
    }
    void act(async () => setState(await setExtractSettings(start, null)));
  };

  const toggleOpen = (open: boolean) =>
    void act(async () => {
      const now = await setOpenFolderWhenDone(open);
      setState((s) => (s === null ? s : { ...s, open_folder: now }));
    });

  const start = state?.start ?? "desktop";
  const custom = state?.custom ?? "";
  const options: { value: ExtractStart; label: string }[] = [
    { value: "desktop", label: t("settings.extractStartDesktop") },
    { value: "last_used", label: t("settings.extractStartLast") },
    { value: "custom", label: t("settings.extractStartCustom") },
  ];

  return (
    <SettingGroup>
      <fieldset className="setting-row setting-fieldset" data-testid="extract-start-row" aria-describedby={startNoteId}>
        {/* The legend is the fieldset's first child (its name); floated, it is laid out
            like the other rows' titles (styles.css, .setting-fieldset). */}
        <legend className="setting-title">
          {t("settings.extractStartTitle")} <Hint text={t("settings.extractStartMore")} />
        </legend>
        <p className="setting-description" id={startNoteId}>
          {t("settings.extractStartShort")}
        </p>
        <div className="setting-control setting-choices">
          {options.map((o) => (
            <label key={o.value} className="setting-choice">
              <input
                type="radio"
                name={startId}
                value={o.value}
                data-testid={`extract-start-${o.value}`}
                checked={start === o.value}
                disabled={state === null || busy}
                onChange={() => pick(o.value)}
              />
              {o.label}
            </label>
          ))}
        </div>
      </fieldset>
      <div className="setting-folder" data-testid="extract-custom" data-active={start === "custom"}>
        {/* The user's own folder text, shown as text. */}
        <span className="setting-folder-path" title={custom}>
          {custom === "" ? t("settings.extractStartNone") : custom}
        </span>
        <button
          type="button"
          data-testid="extract-custom-browse"
          disabled={state === null || busy}
          onClick={() => void chooseFolder()}
        >
          {t("settings.extractBrowse")}
        </button>
      </div>
      <SettingRow
        title={t("settings.extractOpenTitle")}
        titleFor={openId}
        description={t("settings.extractOpenShort")}
        descriptionId={openNoteId}
      >
        <input
          id={openId}
          type="checkbox"
          className="switch"
          data-testid="extract-open-folder"
          aria-describedby={openNoteId}
          checked={state?.open_folder ?? true}
          disabled={state === null || busy}
          onChange={(e) => toggleOpen(e.target.checked)}
        />
      </SettingRow>
      {error !== null && (
        <p role="alert" className="setting-error">
          {error}
        </p>
      )}
    </SettingGroup>
  );
}
