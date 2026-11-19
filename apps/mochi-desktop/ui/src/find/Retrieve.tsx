import { useCallback, useRef, useState } from "react";
import type { FormEvent } from "react";

import { Panel } from "../a11y/Panel";
import { errorText, shortSize } from "../browser/format";
import { progressText } from "../extract/messages";
import { FolderIcon } from "../icons";
import { formatNumber, t, tc, tn } from "../i18n";
import { findChooseRetrieveFolder } from "../ipc/find";
import type { FindHit, FindQuery, RetrieveArchiveView, RetrieveLayout, RetrieveReportView } from "../ipc/find";
import { cancelJob, pauseJob, startJob } from "../ipc/jobs";
import type { JobEvent, JobOutcome } from "../ipc/jobs";
import { TransferProgress } from "../progress/TransferProgress";
import { useTransferRate } from "../progress/useTransferRate";
import { baseName } from "./paths";

type Conflict = "add_new" | "replace";

type JobView =
  | { state: "idle" }
  | { state: "starting" }
  | {
      state: "running";
      jobId: number;
      phase: string;
      completed: number;
      total: number | null;
      cancelRequested: boolean;
      paused: boolean;
    }
  | { state: "done"; outcome: JobOutcome };

/** What to retrieve: the rows ticked, or every match of a search (any number, never listed). */
export type RetrieveWhat =
  | { kind: "items"; hits: readonly FindHit[] }
  | { kind: "all"; run: number; query: FindQuery; count: number; archives: number; bytes: string };

/** How many distinct archives `hits` come from. */
export function archivesOf(hits: readonly FindHit[]): number {
  return new Set(hits.map((h) => h.archive)).size;
}

/**
 * Retrieve the chosen hits of MOCHI Find into one folder (spec Annex B.2.21):
 * where to, how to lay the archives out, what to do with a name that is there
 * already, then the job's progress and what it did, archive by archive. The
 * folder is chosen in Rust's dialog; the webview names the hits only by scan
 * number, archive position and key. The report says what was extracted, never
 * that anything was verified or is safe (spec §23.3).
 */
export function RetrieveDialog({ what, onClose }: { what: RetrieveWhat; onClose: () => void }) {
  const [folder, setFolder] = useState<string | null>(null);
  const [layout, setLayout] = useState<RetrieveLayout>("per_archive");
  const [conflict, setConflict] = useState<Conflict>("add_new");
  const [error, setError] = useState<string | null>(null);
  const [choosing, setChoosing] = useState(false);
  const [job, setJob] = useState<JobView>({ state: "idle" });
  const rate = useTransferRate();
  const { sample, reset, pause } = rate;
  const chooseRef = useRef<HTMLButtonElement>(null);

  const busy = job.state === "starting" || job.state === "running";
  const hits = what.kind === "items" ? what.hits : [];
  const files = hits.filter((h) => h.kind !== "directory").length;
  const folders = hits.length - files;
  const archives = what.kind === "items" ? archivesOf(hits) : what.archives;

  const choose = async () => {
    setError(null);
    setChoosing(true);
    try {
      const picked = await findChooseRetrieveFolder();
      if (picked !== null) setFolder(picked);
    } catch (e) {
      setError(errorText(t("find.retrieveChooseFailed"), e));
    } finally {
      setChoosing(false);
    }
  };

  const onEvent = useCallback(
    (event: JobEvent) => {
      setJob((prev) => {
        switch (event.event) {
          case "started":
            return {
              state: "running",
              jobId: event.job_id,
              phase: "",
              completed: 0,
              total: null,
              cancelRequested: false,
              paused: false,
            };
          case "progress":
            return prev.state === "running"
              ? { ...prev, phase: event.phase, completed: event.completed, total: event.total }
              : {
                  state: "running",
                  jobId: event.job_id,
                  phase: event.phase,
                  completed: event.completed,
                  total: event.total,
                  cancelRequested: false,
                  paused: false,
                };
          case "finished":
            return { state: "done", outcome: event.outcome };
        }
      });
      if (event.event === "progress") sample(event.phase, event.completed, event.total, Date.now());
    },
    [sample],
  );

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (folder === null || busy) return;
    if (what.kind === "items" && hits.length === 0) return;
    setError(null);
    reset();
    setJob({ state: "starting" });
    const request =
      what.kind === "all"
        ? { kind: "retrieve_found" as const, run: what.run, items: [], all: what.query, layout, on_conflict: conflict }
        : {
            kind: "retrieve_found" as const,
            run: hits[0]?.run ?? 0,
            items: hits.map((h) => ({ archive: h.archive, key: h.key })),
            layout,
            on_conflict: conflict,
          };
    startJob(request, onEvent).catch((err: unknown) => {
      setJob({ state: "idle" });
      setError(errorText(t("find.retrieveStartFailed"), err));
    });
  };

  const cancel = () => {
    if (job.state !== "running") return;
    setJob({ ...job, cancelRequested: true });
    void cancelJob(job.jobId).catch(() => false);
  };
  const togglePause = () => {
    if (job.state !== "running") return;
    const paused = !job.paused;
    setJob({ ...job, paused });
    pause(paused, Date.now());
    void pauseJob(job.jobId, paused).catch(() => false);
  };

  const outcome = job.state === "done" ? job.outcome : null;
  const report = outcome?.status === "completed" ? outcome.retrieve : undefined;

  return (
    <Panel
      heading={t("find.retrieveHeading")}
      testId="find-retrieve-dialog"
      fallbackId="finder-heading"
      className="find-retrieve"
      onClose={busy ? undefined : onClose}
    >
      {job.state === "idle" && (
        <form onSubmit={submit} className="find-retrieve-form">
          <p className="find-retrieve-what" data-testid="find-retrieve-what">
            {what.kind === "all"
              ? t("find.retrieveWhatAll", {
                  n: formatNumber(what.count),
                  archives: formatNumber(archives),
                  size: shortSize(what.bytes),
                })
              : t("find.retrieveWhat", {
                  files: formatNumber(files),
                  folders: formatNumber(folders),
                  archives: formatNumber(archives),
                })}
          </p>

          <fieldset className="find-field">
            <legend>{t("find.retrieveTo")}</legend>
            <div className="find-retrieve-folder">
              <FolderIcon />
              <span className="find-path" data-testid="find-retrieve-folder" title={folder ?? undefined}>
                {folder ?? t("find.retrieveNoFolder")}
              </span>
              <button
                ref={chooseRef}
                type="button"
                data-testid="find-retrieve-choose"
                disabled={choosing}
                onClick={() => void choose()}
              >
                {folder === null ? t("find.retrieveChoose") : t("find.retrieveChange")}
              </button>
            </div>
          </fieldset>

          <fieldset className="find-field find-choices">
            <legend>{t("find.retrieveLayout")}</legend>
            <label className="find-choice">
              <input
                type="radio"
                name="find-layout"
                data-testid="find-retrieve-per-archive"
                checked={layout === "per_archive"}
                onChange={() => setLayout("per_archive")}
              />
              <span>
                <span className="find-choice-title">{t("find.retrievePerArchive")}</span>
                <span className="find-choice-note">{t("find.retrievePerArchiveNote")}</span>
              </span>
            </label>
            <label className="find-choice">
              <input
                type="radio"
                name="find-layout"
                data-testid="find-retrieve-together"
                checked={layout === "together"}
                onChange={() => setLayout("together")}
              />
              <span>
                <span className="find-choice-title">{t("find.retrieveTogether")}</span>
                <span className="find-choice-note">{t("find.retrieveTogetherNote")}</span>
              </span>
            </label>
          </fieldset>

          <fieldset className="find-field find-choices">
            <legend>{t("find.retrieveExisting")}</legend>
            <label className="find-choice">
              <input
                type="radio"
                name="find-conflict"
                data-testid="find-retrieve-keep"
                checked={conflict === "add_new"}
                onChange={() => setConflict("add_new")}
              />
              <span>
                <span className="find-choice-title">{t("find.retrieveKeep")}</span>
                <span className="find-choice-note">{t("find.retrieveKeepNote")}</span>
              </span>
            </label>
            <label className="find-choice">
              <input
                type="radio"
                name="find-conflict"
                data-testid="find-retrieve-replace"
                checked={conflict === "replace"}
                onChange={() => setConflict("replace")}
              />
              <span>
                <span className="find-choice-title">{t("find.retrieveReplace")}</span>
                <span className="find-choice-note">{t("find.retrieveReplaceNote")}</span>
              </span>
            </label>
          </fieldset>

          <p className="find-retrieve-note">{t("find.retrieveNote")}</p>
          {error && (
            <p role="alert" className="error" data-testid="find-retrieve-error">
              {error}
            </p>
          )}
          <div className="actions dialog-actions">
            <button type="submit" className="primary" data-testid="find-retrieve-ok" disabled={folder === null}>
              {t("find.retrieveOk")}
            </button>
            <button type="button" data-testid="find-retrieve-cancel" onClick={onClose}>
              {t("common.cancel")}
            </button>
          </div>
        </form>
      )}

      {busy && (
        <TransferProgress
          title={t("find.retrieveRunning")}
          subtitle={folder ?? undefined}
          phase={job.state === "running" ? job.phase : ""}
          completed={job.state === "running" ? job.completed : 0}
          total={job.state === "running" ? job.total : null}
          rate={rate.state}
          text={
            job.state === "running" && job.phase
              ? progressText(job.phase, job.completed, job.total) + (job.cancelRequested ? t("common.cancelling") : "")
              : t("common.starting")
          }
          state={
            job.state !== "running" ? "starting" : job.cancelRequested ? "cancelling" : job.paused ? "paused" : "running"
          }
          onPause={togglePause}
          onResume={togglePause}
          onCancel={cancel}
          testIds={{ root: "find-retrieve-running", text: "find-retrieve-progress", pause: "find-retrieve-pause", cancel: "find-retrieve-stop" }}
        />
      )}

      {outcome !== null && (
        <div className="find-retrieve-done" data-testid="find-retrieve-done" data-status={outcome.status}>
          {report ? (
            <RetrieveReport report={report} />
          ) : (
            <p role="alert" className={outcome.status === "cancelled" ? "find-retrieve-stopped" : "error"} data-testid="find-retrieve-outcome">
              {outcome.status === "cancelled"
                ? t("find.retrieveCancelled")
                : outcome.status === "failed"
                  ? t("find.retrieveFailed", { code: outcome.code, message: outcome.message })
                  : t("find.retrieveNoResult")}
            </p>
          )}
          <div className="actions dialog-actions">
            {outcome.status !== "completed" && (
              <button type="button" data-testid="find-retrieve-again" onClick={() => setJob({ state: "idle" })}>
                {t("find.retrieveBack")}
              </button>
            )}
            <button type="button" className="primary" data-testid="find-retrieve-close" onClick={onClose}>
              {t("find.retrieveClose")}
            </button>
          </div>
        </div>
      )}
    </Panel>
  );
}

function archiveLine(a: RetrieveArchiveView): string {
  switch (a.state) {
    case "extracted":
      return a.not_extracted_total > 0
        ? t("find.retrieveArchivePartly", {
            files: formatNumber(a.files),
            skipped: formatNumber(a.not_extracted_total),
          })
        : tn(a.files, "find.retrieveArchiveDoneOne", "find.retrieveArchiveDoneMany", {
            n: formatNumber(a.files),
            size: shortSize(String(a.bytes)),
          });
    case "changed":
      return t("find.retrieveArchiveChanged");
    case "failed":
      return t("find.retrieveArchiveFailed", { message: a.message?.text ?? "" });
  }
}

/** What a retrieve did: the totals, then each archive (archive-supplied text as text only). */
export function RetrieveReport({ report }: { report: RetrieveReportView }) {
  return (
    <div className="find-report" data-testid="find-retrieve-report">
      <p className="find-report-head" data-testid="find-retrieve-summary">
        {tn(report.files, "find.retrieveSummaryOne", "find.retrieveSummaryMany", {
          n: formatNumber(report.files),
          size: shortSize(String(report.bytes)),
        })}
      </p>
      <p className="find-report-dest">
        <FolderIcon />
        <span className="find-path" title={report.destination}>
          {report.destination}
        </span>
      </p>
      {report.not_extracted_total > 0 && (
        <p className="find-report-warn" data-testid="find-retrieve-skipped">
          {tc(report.not_extracted_total, "find.retrieveSkippedOne", "find.retrieveSkippedMany")}
        </p>
      )}
      {report.archives_not_done > 0 && (
        <p className="find-report-warn" data-testid="find-retrieve-not-done">
          {tc(report.archives_not_done, "find.retrieveNotDoneOne", "find.retrieveNotDoneMany")}
        </p>
      )}
      <ul className="find-report-list" data-testid="find-retrieve-archives">
        {report.archives.map((a) => (
          <li key={a.archive_path} data-testid="find-retrieve-archive" data-state={a.state}>
            <span className="find-report-name" title={a.archive_path}>
              {baseName(a.archive_path)}
            </span>
            <span className="find-report-line">{archiveLine(a)}</span>
            {a.not_extracted.length > 0 && (
              <details>
                <summary>{t("find.retrieveWhichSkipped")}</summary>
                <ul>
                  {a.not_extracted.map((n) => (
                    <li key={n.path.text}>
                      <span className="find-path" title={n.path.text}>
                        {n.path.text}
                      </span>
                      <span className="find-reason">{n.reason.text}</span>
                    </li>
                  ))}
                </ul>
              </details>
            )}
          </li>
        ))}
      </ul>
      {report.files_not_flushed && <p className="find-report-note">{t("find.retrieveNotFlushed")}</p>}
      {report.opened_folder && <p className="find-report-note">{t("find.retrieveOpened")}</p>}
    </div>
  );
}
