import { useEffect, useId, useState } from "react";

import { errorText } from "../browser/format";
import { t } from "../i18n";
import { getSpeedSettings, setSpeedSettings } from "../ipc/speed";
import type { SpeedSettingsView } from "../ipc/speed";
import { SettingGroup, SettingRow } from "./Row";

/**
 * Release 0.9.11: whether writes compress on every processor core (the
 * default) or on one. Only a speed choice: the archive is the same bytes
 * either way. Also fast extract (off by default): extraction skips the
 * per-file flush, which the note says costs durability after a power
 * failure. Rust saves both and applies them to the next job.
 */
export function Speed() {
  const [state, setState] = useState<SpeedSettingsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const noteId = useId();
  const fastNoteId = useId();
  const coresId = useId();
  const fastId = useId();
  const flushId = useId();
  const flushNoteId = useId();
  const historyId = useId();
  const historyNoteId = useId();
  const waitId = useId();
  const waitNoteId = useId();

  useEffect(() => {
    let live = true;
    getSpeedSettings().then(
      (s) => {
        if (live) setState(s);
      },
      (e: unknown) => {
        if (live) setError(errorText(t("settings.speedLoadFailed"), e));
      },
    );
    return () => {
      live = false;
    };
  }, []);

  const change = async (
    allCores: boolean,
    fastExtract: boolean,
    trustNtfsFlush?: boolean,
    localHistory?: boolean,
    waitForWriter?: boolean,
  ) => {
    setError(null);
    setBusy(true);
    try {
      setState(await setSpeedSettings(allCores, fastExtract, trustNtfsFlush, localHistory, waitForWriter));
    } catch (e) {
      setError(errorText(t("settings.speedSaveFailed"), e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <SettingGroup>
      <SettingRow
        title={t("settings.speedAllCoresTitle", { n: state?.cores ?? "…" })}
        titleFor={coresId}
        description={t("settings.speedAllCoresShort")}
        descriptionId={noteId}
        more={t("settings.speedNote")}
      >
        <input
          id={coresId}
          type="checkbox"
          className="switch"
          data-testid="speed-all-cores"
          aria-describedby={noteId}
          checked={state?.all_cores ?? true}
          disabled={state === null || busy}
          onChange={(e) => void change(e.target.checked, state?.fast_extract ?? false)}
        />
      </SettingRow>
      <SettingRow
        title={t("settings.fastExtractTitle")}
        titleFor={fastId}
        description={t("settings.fastExtractShort")}
        descriptionId={fastNoteId}
        more={t("settings.fastExtractNote")}
      >
        <input
          id={fastId}
          type="checkbox"
          className="switch"
          data-testid="speed-fast-extract"
          aria-describedby={fastNoteId}
          checked={state?.fast_extract ?? false}
          disabled={state === null || busy}
          onChange={(e) => void change(state?.all_cores ?? true, e.target.checked)}
        />
      </SettingRow>
      {state?.windows === true && (
        // Spec Annex B.2.20 (D30): off by default, which is the documented-only policy B.
        <SettingRow
          title={t("settings.ntfsFlushTitle")}
          titleFor={flushId}
          description={t("settings.ntfsFlushShort")}
          descriptionId={flushNoteId}
          more={t("settings.ntfsFlushNote")}
        >
          <input
            id={flushId}
            type="checkbox"
            className="switch"
            data-testid="speed-ntfs-flush"
            aria-describedby={flushNoteId}
            checked={state.trust_ntfs_flush}
            disabled={busy}
            onChange={(e) => void change(state.all_cores, state.fast_extract, e.target.checked)}
          />
        </SettingRow>
      )}
      {/* Spec Annex B.2.22: on by default, as in the command-line tool. */}
      <SettingRow
        title={t("settings.localHistoryTitle")}
        titleFor={historyId}
        description={t("settings.localHistoryShort")}
        descriptionId={historyNoteId}
        more={t("settings.localHistoryNote")}
      >
        <input
          id={historyId}
          type="checkbox"
          className="switch"
          data-testid="settings-local-history"
          aria-describedby={historyNoteId}
          checked={state?.local_history ?? true}
          disabled={state === null || busy}
          onChange={(e) =>
            void change(state?.all_cores ?? true, state?.fast_extract ?? false, undefined, e.target.checked)
          }
        />
      </SettingRow>
      {/* Spec Annex B.2.23: off by default, the spec's fail-on-conflict. */}
      <SettingRow
        title={t("settings.waitForWriterTitle")}
        titleFor={waitId}
        description={t("settings.waitForWriterShort")}
        descriptionId={waitNoteId}
        more={t("settings.waitForWriterNote")}
      >
        <input
          id={waitId}
          type="checkbox"
          className="switch"
          data-testid="settings-wait-for-writer"
          aria-describedby={waitNoteId}
          checked={state?.wait_for_writer ?? false}
          disabled={state === null || busy}
          onChange={(e) =>
            void change(state?.all_cores ?? true, state?.fast_extract ?? false, undefined, undefined, e.target.checked)
          }
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
