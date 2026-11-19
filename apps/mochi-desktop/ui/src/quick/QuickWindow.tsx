import { useEffect, useRef, useState } from "react";

import { TransferProgress } from "../progress/TransferProgress";
import { useTransferRate } from "../progress/useTransferRate";
import { formatSize } from "../browser/format";
import { progressText as extractProgress } from "../extract/messages";
import { formatNumber, t, tc, useLocale } from "../i18n";
import {
  cancelQuick,
  closeQuick,
  moveQuickFolder,
  quickPause,
  renameQuickFolder,
  sendQuickPassphrase,
  setQuickOpenFolder,
  showQuickFolder,
  subscribeQuick,
  type QuickAction,
  type QuickEvent,
  type QuickFolder,
} from "../ipc/quick";
import { isIpcError } from "../ipc/jobs";
import { attentionHint, cautionCount, countParts, joinParts, logRecord } from "../result/details";
import type { DetailRow, ResultDetails } from "../result/details";
import { InlineDetails } from "../result/Result";
import { formatLabel } from "../write/format";
import { localCommitNote, progressText as writeProgress } from "../write/messages";
import { TitleBar } from "../shell/TitleBar";
import { AddForm } from "./AddForm";
import type { AddFormStart } from "./AddForm";
import "./quick.css";

interface Job {
  action: QuickAction;
  name: string;
  items: number;
}

type View =
  | { kind: "waiting" }
  | { kind: "form" }
  | { kind: "running"; progress: { phase: string; completed: number; total: number | null } | null }
  | { kind: "passphrase"; wrong: boolean }
  | {
      kind: "done";
      lines: string[];
      folder: boolean;
      failed: boolean;
      warn: boolean;
      details: ResultDetails | null;
      record: string | null;
      /** The folder was opened in the file manager when the job ended. */
      opened: boolean;
      /** The new folder "Extract to new folder" made: it can be renamed or moved. */
      movable: boolean;
      /** That folder's name, to start the rename field. */
      folderName: string;
    };

function title(job: Job): string {
  switch (job.action) {
    case "extract_here":
      return t("quick.titleExtractHere", { name: job.name });
    case "extract_to_folder":
      return t("quick.titleExtractToFolder", { name: job.name });
    case "add":
      return job.items > 1
        ? t("quick.titleAddMore", { name: job.name, n: job.items - 1 })
        : t("quick.titleAdd", { name: job.name });
  }
}

/** The final view for a terminal event; `null` for an event that is not one. */
function finished(event: QuickEvent): View | null {
  switch (event.event) {
    case "done": {
      const parts = joinParts(countParts(event.files, event.directories, event.bytes));
      const add = event.action === "add";
      // A plain ZIP or tar file (plan O31): what was written, and no commit status.
      const plain = add && event.format !== "mochi";
      const rows: DetailRow[] = [
        { label: t("quick.rowLocation"), value: event.location.text },
        { label: t("quick.rowFiles"), value: formatNumber(event.files) },
        { label: t("quick.rowFolders"), value: formatNumber(event.directories) },
        { label: t("quick.rowSize"), value: formatSize(String(event.bytes)) },
      ];
      if (plain) {
        rows.unshift({ label: t("quick.rowFormat"), value: formatLabel(event.format) });
        rows.push({ label: t("quick.rowFileSize"), value: formatSize(String(event.output_bytes)) });
      }
      if (add && !plain) {
        rows.push({
          label: t("quick.rowStatus"),
          value: t("write.statusCommitted"),
          ...(event.status ? { sub: event.status } : {}),
        });
      }
      if (event.durability_note) {
        rows.push({
          label: t("quick.rowFolderEntry"),
          value: t("quick.notConfirmed"),
          sub: t("quick.degradedMark"),
          note: event.durability_note,
          caution: true,
        });
      }
      if (event.skipped > 0) {
        rows.push({
          label: t(add ? "quick.rowNotStored" : "quick.rowNotExtracted"),
          value: tc(event.skipped, "common.itemOne", "common.itemMany"),
          ...(event.note ? { note: t("quick.firstReason", { note: event.note }) } : {}),
          caution: true,
        });
      }
      const head = plain
        ? t("quick.doneExport", { parts, name: event.name.text })
        : add
          ? t("quick.doneAdd", { name: event.name.text, parts })
          : t("quick.doneExtract", { parts, name: event.name.text });
      const line =
        event.skipped > 0
          ? `${head}${t("common.statusSeparator")}${t(add ? "quick.lineNotAdded" : "quick.lineNotExtracted", { n: formatNumber(event.skipped) })}`
          : head;
      const record = [
        plain ? `export ${event.format}` : add ? `add commit ${event.status ?? ""}` : "extract",
        `files ${event.files}, folders ${event.directories}, bytes ${event.bytes}`,
        `skipped ${event.skipped}`,
        event.durability_note
          ? `directory entry not confirmed durable (${event.durability_note})`
          : "directory entry confirmed",
      ].join("; ");
      return {
        kind: "done",
        lines: [line],
        folder: true,
        failed: false,
        warn: event.skipped > 0,
        details: {
          rows,
          notes: [plain ? t("quick.exportNote") : add ? localCommitNote() : t("quick.extractNote")],
        },
        record,
        opened: event.opened_folder,
        movable: event.movable,
        folderName: event.name.text,
      };
    }
    case "fallback":
      return {
        kind: "done",
        lines: [
          t(event.conflicts === 1 ? "quick.fallbackOne" : "quick.fallbackMany", {
            n: event.conflicts,
            destination: event.destination.text,
          }),
        ],
        folder: false,
        failed: false,
        warn: false,
        details: null,
        record: null,
        opened: false,
        movable: false,
        folderName: "",
      };
    case "cancelled":
      return {
        kind: "done",
        lines: [t(event.action === "add" ? "write.cancelledCreate" : "quick.cancelledExtract")],
        folder: false,
        failed: false,
        warn: false,
        details: null,
        record: null,
        opened: false,
        movable: false,
        folderName: "",
      };
    case "failed":
      return {
        kind: "done",
        lines: [t("common.failed", { code: event.code, message: event.message })],
        folder: false,
        failed: true,
        warn: false,
        details: null,
        record: null,
        opened: false,
        movable: false,
        folderName: "",
      };
    case "aborted":
      return {
        kind: "done",
        lines: [t("common.stoppedUnexpectedly", { message: event.message })],
        folder: false,
        failed: true,
        warn: false,
        details: null,
        record: null,
        opened: false,
        movable: false,
        folderName: "",
      };
    default:
      return null;
  }
}

/**
 * The compact window of "Extract here", "Extract to new folder", and "Add to archive" (plan
 * D6, owner decision 2026-10-16): progress while Rust runs the job, then a short result with
 * "Show in folder" and Close. An extraction opens its folder when done unless "Open the folder
 * when done" is off, and the folder "Extract to new folder" made can be renamed or moved (owner
 * request 2026-11-09; Rust does both, by folder name or its own dialog). It names no path; an archive that is encrypted asks for its
 * passphrase here, once.
 */
export function QuickWindow() {
  useLocale();
  const [job, setJob] = useState<Job | null>(null);
  const [view, setView] = useState<View>({ kind: "waiting" });
  // The Add form: what was chosen (updated when another item joins) and the form's starting values.
  const [prompt, setPrompt] = useState<{ items: number; subject: string; start: AddFormStart } | null>(null);
  const [cancelling, setCancelling] = useState(false);
  const [paused, setPaused] = useState(false);
  // "Open the folder when done": the saved choice, as Rust sent it when the job began.
  const [openFolder, setOpenFolder] = useState(true);
  // After a rename or a move: where the folder is now.
  const [moved, setMoved] = useState<string | null>(null);
  const [folderName, setFolderName] = useState("");
  const [busy, setBusy] = useState(false);
  const rate = useTransferRate();
  const { sample: sampleRate, reset: resetRate } = rate;
  const [problem, setProblem] = useState<string | null>(null);
  const passphraseRef = useRef<HTMLInputElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);

  useEffect(
    () =>
      subscribeQuick((event) => {
        if (event.event === "started") {
          setJob({ action: event.action, name: event.subject.text, items: event.items });
          setOpenFolder(event.open_folder);
          setMoved(null);
          setPrompt(null);
          setView({ kind: "running", progress: null });
          setCancelling(false);
          setPaused(false);
          resetRate();
          setProblem(null);
          return;
        }
        if (event.event === "add_prompt") {
          setPrompt((prev) => ({
            items: event.items,
            subject: event.subject.text,
            // Only the first values start the form; later ones just change the count.
            start: prev?.start ?? { name: event.name, folder: event.folder.text, format: event.format },
          }));
          setView({ kind: "form" });
          return;
        }
        if (event.event === "need_passphrase") {
          setView({ kind: "passphrase", wrong: event.wrong });
          return;
        }
        if (event.event === "progress") {
          sampleRate(event.phase, event.completed, event.total, Date.now());
          setView({
            kind: "running",
            progress: { phase: event.phase, completed: event.completed, total: event.total },
          });
          return;
        }
        const end = finished(event);
        if (end) {
          setView(end);
          if (end.kind === "done") setFolderName(end.folderName);
          // The technical record goes to the console, once per result.
          if (end.kind === "done" && end.record) logRecord(end.record);
        }
      }),
    [sampleRate, resetRate],
  );

  useEffect(() => {
    if (view.kind === "passphrase") passphraseRef.current?.focus();
    if (view.kind === "done") closeRef.current?.focus();
  }, [view]);

  const report = (e: unknown) =>
    setProblem(isIpcError(e) ? t("common.failed", { code: e.code, message: e.message }) : null);

  const cancel = () => {
    setCancelling(true);
    cancelQuick().catch(report);
  };

  const togglePause = () => {
    if (cancelling) return;
    const next = !paused;
    setPaused(next);
    rate.pause(next, Date.now());
    quickPause(next).catch((e: unknown) => {
      setPaused(!next);
      rate.pause(!next, Date.now());
      report(e);
    });
  };

  const toggleOpenFolder = (open: boolean) => {
    setOpenFolder(open);
    setQuickOpenFolder(open).catch((e: unknown) => {
      setOpenFolder(!open);
      report(e);
    });
  };

  // Rename and Move to…: Rust acts on the folder it made and says where it is now.
  const relocate = (work: () => Promise<QuickFolder | null>, key: "quick.renamed" | "quick.moved") => {
    if (busy) return;
    setBusy(true);
    setProblem(null);
    work()
      .then((now) => {
        if (now) {
          setFolderName(now.name.text);
          setMoved(t(key, { location: now.location.text }));
        }
      })
      .catch(report)
      .finally(() => setBusy(false));
  };

  const submitPassphrase = () => {
    const input = passphraseRef.current;
    if (!input || input.value === "") return;
    const value = input.value;
    // Not kept: the field is emptied as it is sent.
    input.value = "";
    setView({ kind: "running", progress: null });
    sendQuickPassphrase(value).catch(report);
  };

  const heading =
    view.kind === "form" && prompt
      ? prompt.items > 1
        ? t("quick.formTitleMore", { name: prompt.subject, n: prompt.items - 1 })
        : t("quick.formTitle", { name: prompt.subject })
      : job
        ? title(job)
        : t("quick.waiting");
  const addProgress = job?.action === "add";
  const extracting = job !== null && job.action !== "add";
  const openBox = extracting && (
    <label className="quick-check" data-testid="quick-open-when-done-label">
      <input
        type="checkbox"
        data-testid="quick-open-when-done"
        checked={openFolder}
        onChange={(e) => toggleOpenFolder(e.currentTarget.checked)}
      />
      {t("quick.openWhenDone")}
    </label>
  );

  return (
    <div className="quick-window">
      {/* The same title bar as the archive windows (release 0.9.10), without maximize. */}
      <TitleBar name={null} maximizable={false} />
      <main className="quick" data-testid="quick" aria-labelledby="quick-title">
        <h1 id="quick-title" data-testid="quick-title">
          {heading}
        </h1>
        {view.kind === "waiting" && <p data-testid="quick-waiting">{t("common.starting")}</p>}
        {view.kind === "form" && prompt && <AddForm start={prompt.start} />}
        {view.kind === "running" && (
          <>
            <TransferProgress
              title={addProgress ? t("transfer.titleCreate") : t("transfer.titleExtract")}
              phase={view.progress?.phase ?? ""}
              completed={view.progress?.completed ?? 0}
              total={view.progress?.total ?? null}
              rate={rate.state}
              text={
                view.progress
                  ? (addProgress ? writeProgress : extractProgress)(
                      view.progress.phase,
                      view.progress.completed,
                      view.progress.total,
                    )
                  : t("common.starting")
              }
              state={view.progress === null ? "starting" : cancelling ? "cancelling" : paused ? "paused" : "running"}
              onPause={togglePause}
              onResume={togglePause}
              testIds={{ root: "quick-progress", text: "quick-progress-text", pause: "quick-pause" }}
            />
            {openBox}
            <div className="actions">
              <button type="button" data-testid="quick-cancel" disabled={cancelling} onClick={cancel}>
                {cancelling ? t("quick.cancelling") : t("common.cancel")}
              </button>
            </div>
          </>
        )}
        {view.kind === "passphrase" && (
          <form
            data-testid="quick-passphrase"
            onSubmit={(e) => {
              e.preventDefault();
              submitPassphrase();
            }}
          >
            <p role={view.wrong ? "alert" : "note"} data-testid="quick-passphrase-note">
              {t(view.wrong ? "quick.passphraseWrong" : "quick.passphraseNeeded", { name: job?.name ?? "" })}
            </p>
            <label>
              {t("quick.passphrase")}
              <input ref={passphraseRef} type="password" autoComplete="off" data-testid="quick-passphrase-input" />
            </label>
            <div className="actions">
              <button type="submit" data-testid="quick-passphrase-submit">
                {t("quick.passphraseSubmit")}
              </button>
              <button type="button" data-testid="quick-passphrase-cancel" onClick={cancel}>
                {t("common.cancel")}
              </button>
            </div>
          </form>
        )}
        {view.kind === "done" && (
          <>
            <div
              role={view.failed ? "alert" : "status"}
              data-testid="quick-result"
              data-state={view.failed ? "failed" : "finished"}
            >
              {view.lines.map((line, i) => (
                <p key={line} className={view.warn && i === 0 ? "warning" : i === 0 ? undefined : "note"}>
                  {line}
                </p>
              ))}
              {view.details && cautionCount(view.details) > 0 && (
                <span className="sr-only" data-testid="quick-attention">
                  {attentionHint(cautionCount(view.details))}
                </span>
              )}
            </div>
            {view.opened && (
              <p className="note" data-testid="quick-opened">
                {t("quick.openedFolder")}
              </p>
            )}
            {view.details && <InlineDetails prefix="quick" details={view.details} />}
            {view.movable && (
              <form
                className="quick-rename"
                data-testid="quick-rename"
                onSubmit={(e) => {
                  e.preventDefault();
                  relocate(() => renameQuickFolder(folderName), "quick.renamed");
                }}
              >
                <label>
                  {t("quick.folderName")}
                  <span className="quick-row">
                    <input
                      type="text"
                      data-testid="quick-folder-name"
                      autoComplete="off"
                      spellCheck={false}
                      value={folderName}
                      onChange={(e) => setFolderName(e.currentTarget.value)}
                    />
                    <button type="submit" data-testid="quick-rename-submit" disabled={busy || folderName.trim() === ""}>
                      {t("quick.rename")}
                    </button>
                    <button
                      type="button"
                      data-testid="quick-move"
                      disabled={busy}
                      onClick={() => relocate(moveQuickFolder, "quick.moved")}
                    >
                      {t("quick.moveTo")}
                    </button>
                  </span>
                </label>
                {moved && (
                  <p role="status" className="note" data-testid="quick-moved">
                    {moved}
                  </p>
                )}
              </form>
            )}
            {view.details !== null && openBox}
            <div className="actions">
              {view.folder && (
                <button type="button" data-testid="quick-show-folder" onClick={() => showQuickFolder().catch(report)}>
                  {t("quick.showFolder")}
                </button>
              )}
              <button type="button" ref={closeRef} data-testid="quick-close" onClick={() => closeQuick().catch(report)}>
                {t("common.close")}
              </button>
            </div>
          </>
        )}
        {problem && (
          <p role="alert" className="error" data-testid="quick-problem">
            {problem}
          </p>
        )}
      </main>
    </div>
  );
}
