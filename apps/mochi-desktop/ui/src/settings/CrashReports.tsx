import { useEffect, useId, useState } from "react";
import { Hint } from "../a11y/Hint";

import { Live } from "../a11y/Live";
import { errorText, formatSize } from "../browser/format";
import { t, tc } from "../i18n";
import {
  clearCrashLog,
  exportCrashLog,
  getCrashSettings,
  readCrashLog,
  setCrashSettings,
} from "../ipc/crash";
import type { CrashLogView, CrashSettingsView } from "../ipc/crash";
import { SettingGroup, SettingRow } from "./Row";

/**
 * Plan D7 crash-report policy: opt-in, local, never sent. Both switches start
 * off (the state comes from Rust, which persists them). The log text is shown
 * as text only; saving a copy goes through a native dialog shown by Rust.
 */
export function CrashReports() {
  const [state, setState] = useState<CrashSettingsView | null>(null);
  const [log, setLog] = useState<CrashLogView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const detailsHintId = useId();
  const enableId = useId();
  const detailsId = useId();

  useEffect(() => {
    let live = true;
    getCrashSettings().then(
      (s) => {
        if (live) setState(s);
      },
      (e: unknown) => {
        if (live) setError(errorText(t("settings.loadFailed"), e));
      },
    );
    return () => {
      live = false;
    };
  }, []);

  /** Run one action with the controls held, showing its failure as an alert. */
  const act = async (failed: string, work: () => Promise<void>) => {
    setError(null);
    setNote(null);
    setBusy(true);
    try {
      await work();
    } catch (e) {
      setError(errorText(failed, e));
    } finally {
      setBusy(false);
    }
  };

  const change = (enabled: boolean, includeDetails: boolean) =>
    act(t("settings.saveFailed"), async () => {
      const next = await setCrashSettings(enabled, includeDetails);
      setState(next);
      setNote(t(next.enabled ? "settings.enabledNow" : "settings.disabledNow"));
    });

  const show = () =>
    act(t("settings.readFailed"), async () => {
      setLog(await readCrashLog());
    });

  const save = () =>
    act(t("settings.exportFailed"), async () => {
      const name = await exportCrashLog();
      if (name !== null) setNote(t("settings.exported", { name }));
    });

  const clear = () =>
    act(t("settings.clearFailed"), async () => {
      setState(await clearCrashLog());
      setLog(null);
      setNote(t("settings.cleared"));
    });

  const enabled = state?.enabled ?? false;
  const entries = state?.entries ?? 0;

  return (
    <>
      <p className="setting-intro" data-testid="crash-policy">
        {t("settings.crashPolicy")} <Hint text={[t("settings.crashCollected"), t("settings.crashLimits")]} />
      </p>
      <SettingGroup>
        <SettingRow title={t("settings.enable")} titleFor={enableId}>
          <input
            id={enableId}
            type="checkbox"
            className="switch"
            data-testid="crash-enabled"
            checked={enabled}
            disabled={state === null || busy}
            onChange={(e) => void change(e.target.checked, e.target.checked && (state?.include_details ?? false))}
          />
        </SettingRow>
        <SettingRow title={t("settings.details")} titleFor={detailsId}>
          <input
            id={detailsId}
            type="checkbox"
            className="switch"
            data-testid="crash-details"
            checked={state?.include_details ?? false}
            disabled={!enabled || busy}
            aria-describedby={enabled ? detailsHintId : undefined}
            onChange={(e) => void change(true, e.target.checked)}
          />
        </SettingRow>
        {/* What the message can hold matters once it can be turned on. */}
        {enabled && (
          <p className="setting-warning" id={detailsHintId} data-testid="crash-details-warning">
            {t("settings.detailsWarning")}
          </p>
        )}
        <div className="setting-row setting-stack">
          <div className="setting-text">
            {state && (
              <p className="setting-description" data-testid="crash-state">
                {t("settings.state", {
                  entries: tc(entries, "settings.entryOne", "settings.entryMany"),
                  size: formatSize(String(state.bytes)),
                })}
              </p>
            )}
          </div>
          <div className="setting-control actions">
            <button
              type="button"
              data-testid="crash-show"
              disabled={busy || state === null || entries === 0}
              onClick={() => (log === null ? void show() : setLog(null))}
            >
              {t(log === null ? "settings.show" : "settings.hide")}
            </button>
            <button
              type="button"
              data-testid="crash-export"
              disabled={busy || state === null || entries === 0}
              onClick={() => void save()}
            >
              {t("settings.export")}
            </button>
            <button
              type="button"
              data-testid="crash-clear"
              disabled={busy || state === null || entries === 0}
              onClick={() => void clear()}
            >
              {t("settings.clear")}
            </button>
          </div>
        </div>
      </SettingGroup>
      {error && (
        <p role="alert" className="error" data-testid="crash-error">
          {error}
        </p>
      )}
      <Live data-testid="crash-status">{note}</Live>
      {log && (
        <div data-testid="crash-log">
          {log.truncated && <p className="note">{t("settings.logTruncated")}</p>}
          {/* The log is text from Rust, never markup. It is a tab stop so that
              the keyboard can scroll it. */}
          <pre className="json" tabIndex={0} aria-label={t("settings.logLabel")} data-testid="crash-log-text">
            {log.text === "" ? t("settings.logEmpty") : log.text}
          </pre>
        </div>
      )}
    </>
  );
}
