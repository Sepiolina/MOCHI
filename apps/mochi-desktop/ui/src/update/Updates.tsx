import { useCallback, useEffect, useId, useRef, useState } from "react";

import { formatSize } from "../browser/format";
import { cancelJob, isIpcError, startJob } from "../ipc/jobs";
import type { JobEvent } from "../ipc/jobs";
import { checkForUpdate, getUpdateSettings, setUpdateSettings } from "../ipc/update";
import type { UpdateOffer, UpdateSettingsView } from "../ipc/update";
import { t } from "../i18n";
import type { UpdateFailureKind } from "../ipc/update";
import { SettingGroup, SettingRow } from "../settings/Row";

/** One sentence per kind of failure; the network's own words go in as a parameter, as text. */
export function failureText(kind: UpdateFailureKind, detail: string): string {
  switch (kind) {
    case "offline":
      return t("update.failOffline");
    case "server":
      return t("update.failServer", { detail });
    case "signature":
      return t("update.failSignature");
    case "install":
      return t("update.failInstall", { detail });
    case "unavailable":
      return t("update.failUnavailable", { detail });
    case "other":
      return t("update.failOther", { detail });
  }
}

type View =
  | { state: "idle" }
  | { state: "checking" }
  | { state: "up_to_date"; current: string }
  | { state: "available"; offer: UpdateOffer }
  | { state: "installing"; jobId: number | null; done: number; total: number | null; cancelRequested: boolean; offer: UpdateOffer }
  | { state: "installed"; version: string }
  | { state: "cancelled" }
  | { state: "failed"; text: string };

/**
 * Settings: "Check for updates". Nothing here talks to the network by itself:
 * the explicit button asks, the start-up check exists only if the user turned
 * it on (off by default, and Rust refuses an automatic check while it is off),
 * and nothing is downloaded or installed until the user confirms the version
 * shown. Release notes are remote text: rendered as a text node, never HTML.
 */
export function Updates() {
  const [settings, setSettings] = useState<UpdateSettingsView | null>(null);
  const [view, setView] = useState<View>({ state: "idle" });
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const askedRef = useRef(false);
  const startupId = useId();
  const startupNoteId = useId();
  const notesId = useId();

  const runCheck = useCallback(async (automatic: boolean) => {
    setError(null);
    setView({ state: "checking" });
    try {
      const result = await checkForUpdate(automatic);
      switch (result.result) {
        case "skipped":
          setView({ state: "idle" });
          break;
        case "up_to_date":
          setView({ state: "up_to_date", current: result.current_version });
          break;
        case "available":
          setView({ state: "available", offer: result.offer });
          break;
        case "failed":
          setView({ state: "failed", text: failureText(result.kind, result.detail) });
          break;
      }
    } catch (e) {
      setView({
        state: "failed",
        text: failureText("other", isIpcError(e) ? `${e.code}: ${e.message}` : t("update.unknownError")),
      });
    }
  }, []);

  useEffect(() => {
    let live = true;
    getUpdateSettings().then(
      (s) => {
        if (!live) return;
        setSettings(s);
        // Only an opted-in start-up check; the explicit button is separate.
        if (s.check_on_start && !askedRef.current) {
          askedRef.current = true;
          void runCheck(true);
        }
      },
      () => {
        if (live) setError(t("update.loadFailed"));
      },
    );
    return () => {
      live = false;
    };
  }, [runCheck]);

  const toggle = async (on: boolean) => {
    setError(null);
    setSaving(true);
    try {
      setSettings(await setUpdateSettings(on));
    } catch {
      setError(t("update.saveFailed"));
    } finally {
      setSaving(false);
    }
  };

  const onEvent = useCallback((event: JobEvent) => {
    setView((prev) => {
      if (prev.state !== "installing") return prev;
      switch (event.event) {
        case "started":
          return { ...prev, jobId: event.job_id };
        case "progress":
          return { ...prev, done: event.completed, total: event.total };
        case "finished": {
          const o = event.outcome;
          if (o.status === "completed") return { state: "installed", version: prev.offer.version };
          if (o.status === "cancelled") return { state: "cancelled" };
          return { state: "failed", text: t("update.installFailed", { message: o.message }) };
        }
      }
    });
  }, []);

  const install = (offer: UpdateOffer) => {
    setError(null);
    setView({ state: "installing", jobId: null, done: 0, total: null, cancelRequested: false, offer });
    startJob({ kind: "install_update", version: offer.version }, onEvent).catch((e: unknown) => {
      setView({
        state: "failed",
        text: t("update.installFailed", { message: isIpcError(e) ? e.message : t("update.couldNotStart") }),
      });
    });
  };

  const cancel = () => {
    if (view.state !== "installing" || view.jobId === null) return;
    setView({ ...view, cancelRequested: true });
    cancelJob(view.jobId).catch(() => {
      // The job's own `finished` event says how it ended.
    });
  };

  const busy = view.state === "checking" || view.state === "installing";

  return (
    <div role="group" aria-label={t("update.heading")} data-testid="updates">
      <SettingGroup>
        <SettingRow
          title={settings ? <span data-testid="update-version">{t("update.version", { current: settings.current_version })}</span> : "…"}
          more={[t("update.intro"), t("update.signing")]}
        >
          <button type="button" data-testid="update-check" onClick={() => void runCheck(false)} disabled={busy}>
            {t("update.check")}
          </button>
        </SettingRow>
        <SettingRow
          title={t("update.startup")}
          titleFor={startupId}
          description={settings ? (settings.check_on_start ? t("update.startupOn") : t("update.startupOff")) : undefined}
          descriptionId={startupNoteId}
        >
          <input
            id={startupId}
            type="checkbox"
            className="switch"
            data-testid="update-startup"
            aria-describedby={settings ? startupNoteId : undefined}
            checked={settings?.check_on_start ?? false}
            disabled={settings === null || saving}
            onChange={(e) => void toggle(e.target.checked)}
          />
        </SettingRow>
      </SettingGroup>
      {error && <p role="alert">{error}</p>}
      <p role="status" data-testid="update-status" data-state={view.state}>
        {view.state === "checking" && t("update.checking")}
        {view.state === "up_to_date" && t("update.upToDate", { current: view.current })}
        {view.state === "cancelled" && t("update.cancelled")}
        {view.state === "installed" && t("update.installedRestart", { version: view.version })}
      </p>
      {view.state === "failed" && (
        <p role="alert" data-testid="update-failed">
          {view.text}
        </p>
      )}
      {(view.state === "available" || view.state === "installing") && (
        <div role="group" aria-label={t("update.dialogLabel")} data-testid="update-dialog">
          <h4>{t("update.available", { version: view.offer.version, current: view.offer.current_version })}</h4>
          {view.offer.date && <p>{view.offer.date}</p>}
          <h5 id={notesId}>{t("update.notesHeading")}</h5>
          {/* Remote text: a text node inside <pre>, never markup. */}
          <pre aria-labelledby={notesId} data-testid="update-notes" className="update-notes">
            {view.offer.notes ?? t("update.notesNone")}
          </pre>
          {view.offer.notes_truncated && <p>{t("update.notesCut")}</p>}
          {view.offer.install_mode === "manual" ? (
            <>
              <p>{t("update.manual")}</p>
              <p data-testid="update-page">{view.offer.download_page}</p>
              <div className="actions">
                <button type="button" onClick={() => setView({ state: "idle" })}>
                  {t("update.later")}
                </button>
              </div>
            </>
          ) : (
            <>
              {view.offer.needs_admin_prompt && <p>{t("update.adminPrompt")}</p>}
              <p>{t("update.closesApp")}</p>
              <div className="actions">
                <button
                  type="button"
                  data-testid="update-install"
                  disabled={view.state === "installing"}
                  onClick={() => install(view.offer)}
                >
                  {t("update.install")}
                </button>
                {view.state === "available" ? (
                  <button type="button" data-testid="update-later" onClick={() => setView({ state: "idle" })}>
                    {t("update.later")}
                  </button>
                ) : (
                  <button
                    type="button"
                    data-testid="update-cancel"
                    onClick={cancel}
                    disabled={view.jobId === null || view.cancelRequested}
                  >
                    {t("update.cancel")}
                  </button>
                )}
              </div>
            </>
          )}
          {view.state === "installing" && (
            <div className="progress">
              <progress data-testid="update-progress" value={view.done} max={view.total ?? undefined} />
              <span data-testid="update-progress-text">
                {view.total === null
                  ? t("update.installingBytes", { size: formatSize(String(view.done)) })
                  : t("update.installingOf", {
                      size: formatSize(String(view.done)),
                      total: formatSize(String(view.total)),
                    })}
              </span>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
