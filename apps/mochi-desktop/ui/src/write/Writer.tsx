import { useCallback, useEffect, useRef, useState } from "react";

import { useJobFocus } from "../a11y/focus";
import { Hint } from "../a11y/Hint";
import { Panel } from "../a11y/Panel";
import { errorText } from "../browser/format";
import { formatNumber, t, tn } from "../i18n";
import { Name } from "../browser/Name";
import type { ArchiveSummary, NameText } from "../ipc/browse";
import { cancelJob, pauseJob, startJob } from "../ipc/jobs";
import type { IpcError, JobEvent, JobRequest } from "../ipc/jobs";
import { showInFolder, stageDroppedArchive } from "../ipc/start";
import {
  chooseDestination,
  clearStaged,
  setArchiveFormat,
  stageSources,
  stagedSources,
  unstageSource,
  watchDrops,
} from "../ipc/write";
import type { ArchiveFormat, CompressionPreset, SourcePick, StagedView } from "../ipc/write";
import { CloseIcon, NewArchiveIcon, PlusIcon, TrashIcon } from "../icons";
import { availableActions, tipFor } from "../shell/availability";
import { ToolButton, ToolSlot } from "../shell/Shell";
import { EncryptionNotes } from "../crypt/PassphraseForm";
import {
  hiddenWithKey,
  noGenericTools,
  RECOVERY_CHOICES,
  recoveryNotes,
  recoveryLabel,
  visibleWithoutKey,
} from "../crypt/messages";
import { TransferProgress } from "../progress/TransferProgress";
import { useTransferRate } from "../progress/useTransferRate";
import { logRecord } from "../result/details";
import { ResultBar } from "../result/Result";
import { commitView, deleteNote, exportView, failureText, outcomeResult, progressText, tarNotes } from "./messages";
import type { Operation, WriteResult } from "./messages";
import { FormatSelect, PRESETS, presetsShown } from "./FormatFields";
import { fitPreset, formatLabel, hasLevel, isMochi } from "./format";

export { PRESETS };

/** An entry the list has selected, for the remove flow. */
export type SelectedEntry = { key: string; name: NameText };

export interface WriterProps {
  /** The open archive, if any: what "Add" adds to. */
  archive: ArchiveSummary | null;
  /** Whether the snapshot on screen is the latest; only it can be changed. */
  viewingHead: boolean;
  /** What the list has selected (Remove removes all of it). */
  selected: readonly SelectedEntry[];
  /**
   * A commit was published. `archive` is the archive as it now stands (the
   * same handle for an add or remove, a new one for a create), `null` if it
   * could not be reopened.
   */
  onCommitted: (archive: ArchiveSummary | null) => void;
  /**
   * Whether to show the buttons that start a create, add, or remove. The
   * start screen starts a create itself (`createRequest`), so it hides them
   * while no archive is open.
   */
  showToolbar?: boolean;
  /** Changes when the start screen asks for a new archive. */
  createRequest?: number;
  /** Changes when the list asks to remove its selection (the Delete key). */
  removeRequest?: number;
  /** Changes when the list's menu asks to add files (ignored when Add is off). */
  addRequest?: number;
  /** Told when the create panel opens or closes (the start screen steps aside). */
  onCreating?: (creating: boolean) => void;
  /** A single archive file was dropped on the window; Rust holds it. */
  onArchiveDropped?: () => void;
}

type Mode = "closed" | "create" | "add" | "remove";

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
  | {
      state: "done";
      result: WriteResult;
      committed: boolean;
      archive: ArchiveSummary | null;
      /** A ZIP or tar file was written: no archive to open; "Show in folder" uses Rust's memory. */
      exported: boolean;
      /** A job that failed or stopped unexpectedly (not a cancel): the card keeps its last progress. */
      failure: { text: string; phase: string; completed: number; total: number | null } | null;
    }
  | { state: "error"; text: string };

/** What the card says about the running job: the operation, how many items, and what it writes to. */
interface JobMeta {
  op: Operation;
  items: number;
  name: string;
  format: ArchiveFormat;
}

/** The last request of a run, kept to try it again. A passphrase is never kept: no request with one is stored. */
interface LastRun {
  op: Operation;
  request: JobRequest;
  meta: JobMeta;
}

const EMPTY: StagedView = { items: [], destination: null, default_destination: null, format: "mochi" };

function kindLabel(kind: "file" | "folder" | "other"): string {
  return t(kind === "folder" ? "write.kindFolder" : kind === "file" ? "write.kindFile" : "write.kindOther");
}

/**
 * Plan D2: create an archive, add to one, and remove entries from its current
 * snapshot. The webview names no path: sources come from native dialogs shown
 * by Rust or from files dropped on the window (Rust receives them), and the
 * jobs run in Rust over the shared job channel.
 */
export function Writer({
  archive,
  viewingHead,
  selected,
  onCommitted,
  showToolbar = true,
  createRequest = 0,
  removeRequest = 0,
  addRequest = 0,
  onCreating,
  onArchiveDropped,
}: WriterProps) {
  const [mode, setMode] = useState<Mode>("closed");
  const [staged, setStaged] = useState<StagedView>(EMPTY);
  const [problems, setProblems] = useState<IpcError[]>([]);
  const [dragging, setDragging] = useState(false);
  const [preset, setPreset] = useState<CompressionPreset>("balanced");
  const [tar, setTar] = useState(false);
  const [encrypt, setEncrypt] = useState(false);
  const [recovery, setRecovery] = useState<number | null>(null);
  // The passphrase is read from these inputs when the button is pressed and
  // goes straight into the request; it is never put in state.
  const passRef = useRef<HTMLInputElement>(null);
  const againRef = useRef<HTMLInputElement>(null);
  const [job, setJob] = useState<JobView>({ state: "idle" });
  const [meta, setMeta] = useState<JobMeta>({ op: "create", items: 0, name: "", format: "mochi" });
  const [lastRun, setLastRun] = useState<LastRun | null>(null);
  const rate = useTransferRate();
  const sampleRate = rate.sample;
  const [error, setError] = useState<string | null>(null);
  // The format of the next new archive lives in Rust (it names the default file).
  const format: ArchiveFormat = staged.format;

  // The drop listener is registered once; it reads the latest values here.
  const latestRef = useRef({ archive, viewingHead, busy: false });
  useEffect(() => {
    latestRef.current = {
      archive,
      viewingHead,
      busy: job.state === "starting" || job.state === "running",
    };
  });
  // Files dropped on an open archive go straight in (one commit, no panel).
  const runRef = useRef<(op: Operation, request: JobRequest, over?: Partial<JobMeta>) => void>(() => undefined);
  const operationRef = useRef<Operation>("add");
  const onCommittedRef = useRef(onCommitted);
  const onArchiveDroppedRef = useRef(onArchiveDropped);
  useEffect(() => {
    onCommittedRef.current = onCommitted;
    onArchiveDroppedRef.current = onArchiveDropped;
  });

  // An event heard from Rust is newer than the first look at the staged set: the answer to
  // that look must not replace it (a replayed announcement made before the window listened).
  const heardRef = useRef(false);
  useEffect(() => {
    stagedSources().then(
      (view) => {
        if (!heardRef.current) setStaged(view);
      },
      () => undefined,
    );
    watchDrops((event) => {
      if (event.event === "hover") {
        setDragging(event.active);
        return;
      }
      if (event.event === "archive") {
        // A ZIP (or other foreign archive) dropped on an open MOCHI archive is added to it,
        // as any file is; otherwise the dropped archive is opened.
        const now = latestRef.current;
        if (event.foreign && now.archive && !now.busy) {
          if (!now.viewingHead) {
            setError(t("write.onlyLatest"));
            return;
          }
          const handle = now.archive.handle;
          stageDroppedArchive().then(
            (view) => {
              setStaged(view);
              runRef.current("add", { kind: "add_to_archive", handle }, { items: view.items.length });
            },
            (e: unknown) => setError(errorText(t("write.addSelectionFailed"), e)),
          );
          return;
        }
        onArchiveDroppedRef.current?.();
        return;
      }
      heardRef.current = true;
      setStaged(event.staged);
      setProblems(event.problems);
      const now = latestRef.current;
      if (now.busy) return;
      // Dropped on an open archive: added at once. The shell's "Add to archive" asks first.
      if (now.archive && !event.from_shell && event.staged.items.length > 0 && event.problems.length === 0) {
        if (!now.viewingHead) {
          setError(t("write.onlyLatest"));
          return;
        }
        setMode((m) => (m === "create" ? m : "closed"));
        runRef.current("add", { kind: "add_to_archive", handle: now.archive.handle }, { items: event.staged.items.length });
        return;
      }
      setMode((m) => (m === "create" || m === "add" ? m : now.archive ? "add" : "create"));
    }).catch((e: unknown) => setError(errorText(t("write.dropUnavailable"), e)));
  }, []);

  const busy = job.state === "starting" || job.state === "running";
  const rememberFocus = useJobFocus(busy);

  const onEvent = useCallback((event: JobEvent) => {
    setJob((prev) => {
      switch (event.event) {
        case "started":
          return prev.state === "running"
            ? prev
            : {
                state: "running",
                jobId: event.job_id,
                phase: "",
                completed: 0,
                total: null,
                cancelRequested: false,
                paused: false,
              };
        case "progress":
          if (prev.state === "done") return prev;
          return {
            state: "running",
            jobId: event.job_id,
            phase: event.phase,
            completed: event.completed,
            total: event.total,
            cancelRequested: prev.state === "running" && prev.cancelRequested,
            paused: prev.state === "running" && prev.paused,
          };
        case "finished": {
          const o = event.outcome;
          const stopped = o.status === "failed" || o.status === "aborted";
          return {
            state: "done",
            result: outcomeResult(operationRef.current, o),
            committed: o.status === "completed" && o.commit !== undefined,
            archive: o.status === "completed" ? (o.commit?.archive ?? null) : null,
            exported: o.status === "completed" && o.export !== undefined,
            failure: stopped
              ? {
                  text: failureText(o),
                  phase: prev.state === "running" ? prev.phase : "",
                  completed: prev.state === "running" ? prev.completed : 0,
                  total: prev.state === "running" ? prev.total : null,
                }
              : null,
          };
        }
      }
    });
    if (event.event === "progress") sampleRate(event.phase, event.completed, event.total, Date.now());
    if (event.event === "finished") {
      const o = event.outcome;
      // The technical record goes to the console, once per result.
      if (o.status === "completed" && o.commit) {
        logRecord(commitView(o.commit).record);
      }
      if (o.status === "completed" && o.export) {
        logRecord(exportView(o.export).record);
      }
      stagedSources().then(setStaged, () => undefined);
      if (o.status === "completed" && o.commit) {
        setMode("closed");
        setProblems([]);
        onCommittedRef.current(o.commit.archive ?? null);
      }
      if (o.status === "completed" && o.export) {
        // A plain ZIP or tar file: there is no archive to open, only a result to show.
        setMode("closed");
        setProblems([]);
      }
    }
  }, [sampleRate]);

  const run = (op: Operation, request: JobRequest, over: Partial<JobMeta> = {}) => {
    rememberFocus();
    operationRef.current = op;
    const jobMeta: JobMeta = {
      op,
      items: staged.items.length,
      name:
        op === "create" || op === "export"
          ? (staged.destination ?? staged.default_destination ?? "")
          : (archive?.file_name ?? ""),
      format,
      ...over,
    };
    setMeta(jobMeta);
    // A request that carries a passphrase is not kept: trying again means typing it again.
    setLastRun("passphrase" in request ? null : { op, request, meta: jobMeta });
    rate.reset();
    setError(null);
    setJob({ state: "starting" });
    startJob(request, onEvent).catch((e: unknown) => {
      setJob({ state: "idle" });
      setError(errorText(t("write.startFailed"), e));
    });
  };
  useEffect(() => {
    runRef.current = run;
  });

  const stage = (pick: SourcePick) => {
    setError(null);
    stageSources(pick).then(
      (out) => {
        setStaged(out.staged);
        setProblems(out.problems);
      },
      (e: unknown) => setError(errorText(t("write.addSelectionFailed"), e)),
    );
  };

  const unstage = (index: number) => {
    unstageSource(index).then(setStaged, (e: unknown) => setError(errorText(t("write.removeFailed"), e)));
  };

  const open = (to: Mode) => {
    // Each new archive starts from the plain defaults: encryption and a
    // recovery record are chosen on purpose, never carried over.
    if (to === "create") {
      setEncrypt(false);
      setTar(false);
      setRecovery(null);
      setPreset("balanced");
      // And the plain default format, `.mochi`, if an earlier archive used another.
      if (staged.format !== "mochi") setArchiveFormat("mochi").then(setStaged, () => undefined);
    }
    setJob({ state: "idle" });
    setError(null);
    setProblems([]);
    setMode(to);
  };

  // The start screen asked for a new archive: open the panel, from the
  // request itself rather than from an effect.
  const [seenRequest, setSeenRequest] = useState(createRequest);
  if (seenRequest !== createRequest) {
    setSeenRequest(createRequest);
    open("create");
  }
  // Which of Add and Remove are on: one answer, shared with the menus (shell/availability.ts).
  const avail = availableActions(
    { kind: archive === null ? "none" : "mochi", viewingHead },
    { selected: selected.length, total: 0 },
  );
  const canAdd = avail.add.enabled;
  const canRemove = avail.remove.enabled;
  const [seenAdd, setSeenAdd] = useState(addRequest);
  if (seenAdd !== addRequest) {
    setSeenAdd(addRequest);
    if (canAdd && !busy) open("add");
  }
  const [seenRemove, setSeenRemove] = useState(removeRequest);
  if (seenRemove !== removeRequest) {
    setSeenRemove(removeRequest);
    if (canRemove && !busy) open("remove");
  }

  const onCreatingRef = useRef(onCreating);
  useEffect(() => {
    onCreatingRef.current = onCreating;
  });
  useEffect(() => {
    onCreatingRef.current?.(mode === "create");
  }, [mode]);

  const close = () => {
    setMode("closed");
    setProblems([]);
    setError(null);
  };

  const discard = () => {
    clearStaged().then(setStaged, () => undefined);
  };

  const create = () => {
    rememberFocus();
    setError(null);
    let passphrase: string | undefined;
    if (encrypt && isMochi(format)) {
      const first = passRef.current?.value ?? "";
      if (first === "") {
        setError(t("write.enterPassphrase"));
        return;
      }
      if (first !== (againRef.current?.value ?? "")) {
        setError(t("write.passphrasesDiffer"));
        return;
      }
      passphrase = first;
    }
    const go = (view: StagedView = staged) => {
      const over = { items: view.items.length, name: view.destination ?? view.default_destination ?? "" };
      if (!isMochi(format)) {
        // A plain ZIP or tar file: none of the MOCHI-only choices exist for it.
        run("export", { kind: "export_archive", format, level: preset }, over);
        return;
      }
      run(
        "create",
        {
          kind: "create_archive",
          preset,
          tar_compatible: tar,
          ...(passphrase === undefined ? {} : { passphrase }),
          ...(recovery === null ? {} : { recovery_percent: recovery }),
        },
        over,
      );
      // Rust holds it now; the page does not.
      if (passRef.current) passRef.current.value = "";
      if (againRef.current) againRef.current.value = "";
    };
    // With a place already chosen or defaulted (next to the first item,
    // never over an existing file) there is no dialog: one click creates.
    if (staged.destination !== null || staged.default_destination !== null) {
      go();
      return;
    }
    chooseDestination(format).then(
      (view) => {
        if (view === null) return; // the user cancelled the dialog
        setStaged(view);
        go(view);
      },
      (e: unknown) => setError(errorText(t("write.chooseLocationFailed"), e)),
    );
  };

  /** Let the user pick a different place (the dialog starts at the default). */
  const changeDestination = () => {
    setError(null);
    chooseDestination(format).then(
      (view) => {
        if (view !== null) setStaged(view);
      },
      (e: unknown) => setError(errorText(t("write.chooseLocationFailed"), e)),
    );
  };

  const [folderError, setFolderError] = useState<string | null>(null);
  const showFolder = (target: { kind: "archive"; handle: number } | { kind: "last_export" }) => {
    setFolderError(null);
    showInFolder(target).then(
      () => undefined,
      (e: unknown) => setFolderError(errorText(t("start.showInFolderFailed"), e)),
    );
  };

  const cancel = () => {
    if (job.state !== "running") return;
    const { jobId } = job;
    setJob({ ...job, cancelRequested: true });
    cancelJob(jobId).catch(() => undefined);
  };

  const togglePause = () => {
    if (job.state !== "running" || job.cancelRequested) return;
    const { jobId } = job;
    const paused = !job.paused;
    setJob({ ...job, paused });
    rate.pause(paused, Date.now());
    pauseJob(jobId, paused).catch(() => {
      // Rust could not (the job just ended): show what is true again.
      setJob((now) => (now.state === "running" && now.jobId === jobId ? { ...now, paused: !paused } : now));
      rate.pause(!paused, Date.now());
    });
  };

  /** Try the same request again; only after a failure, and never a request that held a passphrase. */
  const retry = () => {
    if (lastRun && !busy) run(lastRun.op, lastRun.request, lastRun.meta);
  };

  /** Choose the format of the new archive: MOCHI-only choices are dropped, never carried over. */
  const chooseFormat = (next: ArchiveFormat) => {
    setError(null);
    setPreset((p) => fitPreset(next, p));
    if (!isMochi(next)) {
      setEncrypt(false);
      setTar(false);
      setRecovery(null);
    }
    setArchiveFormat(next).then(setStaged, (e: unknown) => setError(errorText(t("write.chooseLocationFailed"), e)));
  };

  // A problem shows where the user is looking: in the open panel, else in the status bar.
  const errorLine = error && (
    <p role="alert" className="error status-line" data-testid="write-error" title={error}>
      {error}
    </p>
  );

  const stagedList = (
    <>
      {staged.items.length === 0 ? (
        <p className="drop-target" data-testid="staged-empty" title={t("write.dropHint")}>
          {t("write.nothingChosen")}
        </p>
      ) : (
        <ul className="staged" data-testid="staged-list" aria-label={t("write.chosenItems")}>
          {staged.items.map((item, i) => (
            <li key={item.name.text} data-testid="staged-item">
              <span className="kind-label" title={kindLabel(item.kind)}>
                {kindLabel(item.kind)}
              </span>{" "}
              <Name name={item.name} />
              <button
                type="button"
                className="icon-button unstage"
                data-testid="unstage"
                aria-label={t("write.unstageItem", { name: item.name.text })}
                title={t("write.unstage")}
                onClick={() => unstage(i)}
                disabled={busy}
              >
                <CloseIcon />
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="actions">
        <button type="button" data-testid="stage-files" onClick={() => stage("files")} disabled={busy}>
          {t("write.addFiles")}
        </button>
        <button type="button" data-testid="stage-folder" onClick={() => stage("folder")} disabled={busy}>
          {t("write.addFolder")}
        </button>
        <button type="button" data-testid="stage-clear" onClick={discard} disabled={busy || staged.items.length === 0}>
          {t("write.clear")}
        </button>
      </div>
      {problems.length > 0 && (
        <ul role="alert" className="error" data-testid="write-problems">
          {problems.map((p) => (
            <li key={`${p.code}:${p.message}`}>
              {t("write.notAdded", { code: p.code, message: p.message })}
            </li>
          ))}
        </ul>
      )}
    </>
  );

  // What a running job is doing: in the open dialog (which covers the window), else in the status bar.
  // A job that failed keeps its card, frozen at the last progress, with the error and "Try again".
  const failure = job.state === "done" ? job.failure : null;
  const cardTitle =
    meta.op === "create"
      ? t("transfer.titleCreate")
      : meta.op === "add"
        ? t("transfer.titleAdd")
        : meta.op === "delete"
          ? t("transfer.titleDelete")
          : t("transfer.titleExport", { format: formatLabel(meta.format) });
  const subtitle =
    meta.op === "create" || meta.op === "export" || meta.op === "add"
      ? meta.name === ""
        ? undefined
        : tn(meta.items, "transfer.subtitleAddOne", "transfer.subtitleAddMany", {
            n: formatNumber(meta.items),
            name: meta.name,
          })
      : tn(meta.items, "transfer.subtitleDeleteOne", "transfer.subtitleDeleteMany", {
          n: formatNumber(meta.items),
          name: meta.name,
        });
  const progressLine =
    (job.state === "starting" || job.state === "running" || failure !== null) && (
      <TransferProgress
        title={cardTitle}
        subtitle={subtitle}
        phase={job.state === "running" ? job.phase : (failure?.phase ?? "")}
        completed={job.state === "running" ? job.completed : (failure?.completed ?? 0)}
        total={job.state === "running" ? job.total : (failure?.total ?? null)}
        rate={rate.state}
        text={
          job.state === "running"
            ? (job.phase ? progressText(job.phase, job.completed, job.total) : t("common.starting")) +
              (job.cancelRequested ? t("common.cancelling") : "")
            : failure !== null
              ? failure.phase
                ? progressText(failure.phase, failure.completed, failure.total)
                : ""
              : t("common.starting")
        }
        state={
          failure !== null
            ? "error"
            : job.state !== "running"
              ? "starting"
              : job.cancelRequested
                ? "cancelling"
                : job.paused
                  ? "paused"
                  : "running"
        }
        errorText={failure?.text}
        onPause={togglePause}
        onResume={togglePause}
        onCancel={cancel}
        onRetry={lastRun !== null && !busy ? retry : undefined}
        testIds={{
          root: failure !== null ? "write-failed" : "write-running",
          text: "write-progress",
          pause: "write-pause",
          cancel: "write-cancel",
          retry: "write-retry",
        }}
      />
    );

  return (
    <section aria-labelledby="write-heading" className="writer" data-testid="writer">
      <h2 id="write-heading" tabIndex={-1} className="sr-only">
        {t("write.heading")}
      </h2>
      {dragging && (
        <p className="drop-overlay" data-testid="drop-overlay" role="note">
          {t("write.dropOverlay")}
        </p>
      )}
      {showToolbar && (
        <>
          <ToolSlot name="new" owner="any">
            <ToolButton
              icon={<NewArchiveIcon />}
              label={t("write.new")}
              testId="new-archive"
              onClick={() => open("create")}
              disabled={busy}
            />
          </ToolSlot>
          <ToolSlot name="add">
            <ToolButton
              icon={<PlusIcon />}
              label={t("write.add")}
              testId="add-to-archive"
              onClick={() => open("add")}
              disabled={busy || !canAdd}
              title={tipFor(avail.add, t("write.addTip"))}
            />
          </ToolSlot>
          <ToolSlot name="remove">
            <ToolButton
              icon={<TrashIcon />}
              label={t("write.remove")}
              testId="remove-entry"
              onClick={() => open("remove")}
              disabled={busy || !canRemove}
              title={tipFor(avail.remove, t("write.removeTip"))}
            />
          </ToolSlot>
        </>
      )}
      {mode === "create" && (
        <Panel
          label={t("write.panelNew")}
          heading={t("write.panelNew")}
          testId="write-panel"
          fallbackId="write-heading"
          onClose={busy ? undefined : close}
        >
          <section aria-labelledby="create-step-1" className="step" data-testid="create-step-files">
            <h4 id="create-step-1" className="sr-only">
              {t("write.step1")}
            </h4>
            {stagedList}
          </section>
          <section aria-labelledby="create-step-2" className="step save-row" data-testid="create-step-where">
            <h4 id="create-step-2">{t("write.step2")}</h4>
            {staged.destination !== null ? (
              <span data-testid="save-location" title={t("write.savedAs", { name: staged.destination })}>
                {staged.destination}
              </span>
            ) : staged.default_destination !== null ? (
              <span data-testid="save-location" title={t("write.saveNote")}>
                {staged.default_destination}
              </span>
            ) : (
              <span className="note" data-testid="save-location">
                {t("write.saveUnknown")}
              </span>
            )}
            <button
              type="button"
              data-testid="change-destination"
              onClick={changeDestination}
              disabled={busy || staged.items.length === 0}
            >
              {t("write.change")}
            </button>
          </section>
          <section aria-labelledby="create-step-format" className="step format-row" data-testid="create-step-format">
            <h4 id="create-step-format" className="sr-only">
              {t("write.format")}
            </h4>
            <FormatSelect value={format} onChange={chooseFormat} disabled={busy} testId="create-format" />
            {!isMochi(format) && <Hint testId="plain-note" text={t("write.plainNote")} />}
          </section>
          <details className="options" data-testid="create-options">
            <summary>{t("write.options")}</summary>
            {hasLevel(format) ? (
              <fieldset disabled={busy}>
                <legend>{t("write.compression")}</legend>
                {presetsShown(format).map((p) => (
                  <label key={p.id}>
                    <input
                      type="radio"
                      name="preset"
                      data-testid={`preset-${p.id}`}
                      checked={preset === p.id}
                      onChange={() => setPreset(p.id)}
                    />{" "}
                    <span title={p.hint()}>{p.label()}</span>
                  </label>
                ))}
              </fieldset>
            ) : (
              <p className="note" data-testid="no-level">
                {t("write.levelNone")}
              </p>
            )}
            {isMochi(format) && (
              <>
                <fieldset disabled={busy}>
                  <legend>{t("write.encryption")}</legend>
                  <label>
                    <input
                      type="checkbox"
                      data-testid="encrypt"
                      checked={encrypt}
                      disabled={tar}
                      onChange={(e) => {
                        setEncrypt(e.target.checked);
                        if (e.target.checked) setTar(false);
                      }}
                    />{" "}
                    {t("write.encrypt")}
                  </label>
                  {tar && <Hint text={noGenericTools()} />}
                  {encrypt && (
                    <div data-testid="encrypt-panel">
                      <label>
                        {t("write.passphrase")}{" "}
                        <input
                          ref={passRef}
                          type="password"
                          autoComplete="off"
                          spellCheck={false}
                          data-testid="create-passphrase"
                        />
                      </label>
                      <label>
                        {t("write.repeat")}{" "}
                        <input
                          ref={againRef}
                          type="password"
                          autoComplete="off"
                          spellCheck={false}
                          data-testid="create-passphrase-again"
                        />
                      </label>
                      <EncryptionNotes creating />
                      <Hint
                        testId="visible-without-key"
                        text={[hiddenWithKey(), t("write.visibleIntro"), ...visibleWithoutKey(), t("write.fixedAtCreation")]}
                      />
                    </div>
                  )}
                </fieldset>
                <fieldset disabled={busy}>
                  <legend>{t("write.recoveryRecord")}</legend>
                  <label>
                    <input
                      type="radio"
                      name="recovery"
                      data-testid="recovery-none"
                      checked={recovery === null}
                      onChange={() => setRecovery(null)}
                    />{" "}
                    {t("write.recoveryNone")}
                  </label>
                  {RECOVERY_CHOICES.map((p) => (
                    <label key={p}>
                      <input
                        type="radio"
                        name="recovery"
                        data-testid={`recovery-${p}`}
                        checked={recovery === p}
                        onChange={() => setRecovery(p)}
                      />{" "}
                      {recoveryLabel(p)}
                    </label>
                  ))}
                  <Hint testId="recovery-notes" text={recoveryNotes()} />
                </fieldset>
                <fieldset disabled={busy}>
                  <legend>{t("write.compatibility")}</legend>
                  <label>
                    <input
                      type="checkbox"
                      data-testid="tar-compatible"
                      checked={tar}
                      disabled={encrypt}
                      onChange={(e) => setTar(e.target.checked)}
                    />{" "}
                    {t("write.tarCompatible")}
                  </label>
                  <Hint testId="tar-notes" text={tarNotes()} />
                </fieldset>
              </>
            )}
          </details>
          {errorLine}
          {progressLine}
          <div className="actions dialog-actions">
            <button
              type="button"
              className="primary"
              data-testid="create-confirm"
              onClick={create}
              disabled={busy || staged.items.length === 0}
            >
              {t("write.createConfirm")}
            </button>
            <button type="button" data-testid="write-close" onClick={close} disabled={busy}>
              {t("write.close")}
            </button>
          </div>
        </Panel>
      )}

      {mode === "add" && archive && (
        <Panel
          label={t("write.panelAdd")}
          heading={t("write.addTo", { name: archive.file_name })}
          testId="write-panel"
          fallbackId="write-heading"
          onClose={busy ? undefined : close}
        >
          {stagedList}
          {errorLine}
          {progressLine}
          <div className="actions dialog-actions">
            <Hint text={t("write.addNote") + (archive.profile.tar_compatible ? t("write.addNoteTar") : "")} />
            <button
              type="button"
              className="primary"
              data-testid="add-confirm"
              onClick={() => run("add", { kind: "add_to_archive", handle: archive.handle })}
              disabled={busy || staged.items.length === 0}
            >
              {t("write.addConfirm")}
            </button>
            <button type="button" data-testid="write-close" onClick={close} disabled={busy}>
              {t("write.close")}
            </button>
          </div>
        </Panel>
      )}

      {mode === "remove" && archive && selected.length > 0 && (
        <Panel
          label={t("write.panelRemove")}
          heading={t("write.removeHeading")}
          testId="write-panel"
          fallbackId="write-heading"
          onClose={busy ? undefined : close}
        >
          <p>
            {selected.length === 1 && selected[0]
              ? t("write.removeQuestion", { name: selected[0].name.text })
              : t("write.removeQuestionMany", { n: formatNumber(selected.length) })}
          </p>
          <p className="note" data-testid="delete-note">
            {deleteNote()}
          </p>
          {errorLine}
          {progressLine}
          <div className="actions dialog-actions">
            <button
              type="button"
              className="primary"
              data-testid="delete-confirm"
              onClick={() =>
                run(
                  "delete",
                  {
                    kind: "delete_from_archive",
                    handle: archive.handle,
                    keys: selected.map((e) => e.key),
                  },
                  { items: selected.length },
                )
              }
              disabled={busy}
            >
              {t("write.removeConfirm")}
            </button>
            <button type="button" data-testid="delete-keep" onClick={close} disabled={busy}>
              {t("write.keep")}
            </button>
          </div>
        </Panel>
      )}

      <ToolSlot name="status" owner="any">
      {mode === "closed" && errorLine}
      {mode === "closed" && progressLine}
      <ResultBar
        prefix="write"
        statusTestId="write-status"
        statusAttrs={{ "data-committed": job.state === "done" ? String(job.committed) : undefined }}
        lines={job.state !== "done" ? [] : job.result.kind === "text" ? job.result.lines : [job.result.line]}
        headlineTestId="write-headline"
        warn={job.state === "done" && job.result.kind !== "text" && job.result.warn}
        details={job.state === "done" && job.result.kind !== "text" ? job.result.details : null}
        onShowFolder={
          job.state === "done" && job.archive !== null
            ? () => {
                if (job.archive) showFolder({ kind: "archive", handle: job.archive.handle });
              }
            : job.state === "done" && job.exported
              ? () => showFolder({ kind: "last_export" })
              : undefined
        }
        onDismiss={() => {
          setJob({ state: "idle" });
          setFolderError(null);
        }}
        fallbackId="write-heading"
      />
      {folderError && (
        <p role="alert" className="error status-line" data-testid="write-folder-error">
          {folderError}
        </p>
      )}
      </ToolSlot>
    </section>
  );
}
