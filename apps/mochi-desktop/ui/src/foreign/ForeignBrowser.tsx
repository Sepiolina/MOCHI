import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { useJobFocus } from "../a11y/focus";
import { Hint } from "../a11y/Hint";
import { Panel } from "../a11y/Panel";
import { ProgressMeter } from "../a11y/ProgressMeter";
import { errorText, formatSize, shortSize } from "../browser/format";
import { EntryCells, ListHead } from "../browser/ListHead";
import { CheckIcon, CloseIcon, ExtractIcon, ExtractToIcon, KeyIcon } from "../icons";
import { ExtractChoices } from "../extract/ExtractChoices";
import { ExtractToDialog } from "../extract/ExtractToDialog";
import { useNewFolderRun } from "../extract/useNewFolderRun";
import { availableActions, tipFor } from "../shell/availability";
import { ToolButton, ToolSlot, useShell } from "../shell/Shell";
import { formatNumber, t, tc } from "../i18n";
import { Name } from "../browser/Name";
import { VirtualList } from "../browser/VirtualList";
import { Crumbs, NavButtons, UpRow } from "../browser/FolderNav";
import { folderKeys, useFolderPath } from "../browser/useFolderPath";
import { dragOutForeign } from "../ipc/dragout";
import {
  closeForeignArchive,
  foreignChildren,
  foreignSearch,
  openForeignArchive,
  planExtractForeign,
  planExtractForeignTo,
} from "../ipc/foreign";
import type { ForeignArchiveSummary, ForeignExtractPlan, ForeignHit, ForeignRow } from "../ipc/foreign";
import { cancelJob, isIpcError, startJob } from "../ipc/jobs";
import { discardExtractFolder, showInFolder } from "../ipc/start";
import { subscribeLaunches } from "../ipc/launch";
import type { JobEvent, JobOutcome } from "../ipc/jobs";
import type { NameText } from "../ipc/browse";
import { useDragOut } from "../list/dragOut";
import { ancestorsOf } from "../list/keys";
import { useSelection } from "../list/selection";
import type { Selection } from "../list/selection";
import { copyNames, listMenuItems, useListMenu } from "../list/ContextMenu";
import { listKeys, rowHandlers } from "../list/rows";
import { ResultBar } from "../result/Result";
import { SearchResults } from "../search/SearchResults";
import type { HitRow, SearchView } from "../search/SearchResults";
import { foundText, IDLE, running } from "../search/useArchiveSearch";
import { describeOutcome, extractNote, needsPassword, progressText, testNote, weakNote } from "./messages";

/** Row height of the list, in pixels (the same as the MOCHI browser's). */
const ROW_HEIGHT = 26;

const rowKey = (r: ForeignRow) => r.key;

type Ask = { reason: "ask" | "wrong" };

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
    }
  | { state: "done"; outcome: JobOutcome };

function kindLabel(row: Pick<ForeignRow, "kind">): string {
  switch (row.kind) {
    case "directory":
      return t("foreign.kindFolder");
    case "file":
      return t("foreign.kindFile");
    case "link":
      return t("foreign.kindLink");
    case "special":
      return t("foreign.kindSpecial");
  }
}

function sizeText(row: ForeignRow): string {
  if (row.kind === "directory") {
    return t("browser.itemsCount", { n: formatNumber(row.child_dirs + row.child_files) });
  }
  return row.size === null ? "" : shortSize(row.size);
}

/** A foreign search hit, as the results list shows it. */
type ForeignHitRow = HitRow & { row: ForeignRow };

function hitRow(h: ForeignHit): ForeignHitRow {
  const at = h.path.text.lastIndexOf("/");
  return {
    key: h.row.key,
    name: h.row.name,
    path: h.path,
    folder: at < 0 ? "" : h.path.text.slice(0, at),
    kind: h.row.kind,
    size: sizeText(h.row),
    row: h.row,
  };
}

/** Names in the open foreign archive that contain `query`, kept in Rust. */
function useForeignSearch(handle: number | null, query: string): SearchView<ForeignHitRow> {
  const [found, setFound] = useState<{ id: string; view: SearchView<ForeignHitRow> } | null>(null);
  const id = `${handle ?? ""}:${query}`;
  useEffect(() => {
    if (handle === null || query === "") return;
    let live = true;
    const done = (view: SearchView<ForeignHitRow>) => {
      if (live) setFound({ id: `${handle}:${query}`, view });
    };
    foreignSearch(handle, query, true).then(
      (r) =>
        done({ state: "done", hits: r.hits.map(hitRow), statusText: foundText(r.hits.length, r.total_hits, true) }),
      (e: unknown) => {
        const text = errorText(t("search.startFailed"), e);
        done({ state: "failed", text, statusText: text });
      },
    );
    return () => {
      live = false;
    };
  }, [handle, query]);
  if (handle === null || query === "") return IDLE;
  return found?.id === id ? found.view : running();
}

/**
 * Plan D8: open a ZIP, 7z, RAR, tar, tar.gz, or tar.zst archive read-only;
 * browse and search it, read and check its entries, extract from it or drag
 * entries out. Its format is named in the path bar; nothing here uses MOCHI's
 * verification wording or health dimensions. The archive and the folder come
 * from native dialogs shown by Rust; a password is typed here once and kept
 * in Rust.
 */
export function ForeignBrowser() {
  const [open, setOpen] = useState<ForeignArchiveSummary | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [ask, setAsk] = useState<Ask | null>(null);
  const folder = useFolderPath();
  const { path, reveal } = folder;
  const [total, setTotal] = useState<{ id: string; n: number } | null>(null);
  const [plan, setPlan] = useState<{ plan: ForeignExtractPlan; keys: string[]; what: NameText | null; n: number } | null>(
    null,
  );
  const [job, setJob] = useState<JobView>({ state: "idle" });
  // The last job was an extraction (its result can show the folder).
  const [extracted, setExtracted] = useState(false);
  // The "Extract to..." window is open.
  const [wizard, setWizard] = useState(false);
  // The plan's folder is a new one made inside the folder the user chose.
  const [madeFolder, setMadeFolder] = useState(false);
  // Set while the running job extracts into a folder Rust just made for it.
  const newFolderRun = useNewFolderRun();
  const passwordRef = useRef<HTMLInputElement>(null);

  const handle = open?.handle ?? null;
  const dir = path.at(-1)?.key ?? null;
  const listingId = `${handle ?? ""}:${dir ?? ""}`;
  const count = total?.id === listingId ? total.n : null;
  const jobBusy = job.state === "starting" || job.state === "running";
  const rememberFocus = useJobFocus(jobBusy);

  const listError = useCallback((e: unknown) => setError(errorText(t("foreign.listFailed"), e)), []);

  // The window shows an open archive in its main area: tell it when there is
  // one, and close this one when a MOCHI archive is shown instead. The More
  // menu's "Other formats" asks for the open dialog.
  const shell = useShell();
  const infoOpen = shell?.infoOpen ?? true;
  const setForeignOpen = shell?.setForeignOpen;
  const query = shell?.active === "foreign" ? shell.query : "";
  const clearQuery = shell?.clearQuery;
  const isOpen = open !== null;
  // The window's title bar shows the open archive's file name (as text).
  const openName = open?.file_name ?? null;
  useEffect(() => {
    setForeignOpen?.(isOpen, openName);
  }, [isOpen, openName, setForeignOpen]);
  useEffect(() => {
    if (handle === null) return;
    let live = true;
    foreignChildren(handle, dir, 0, 1).then(
      (p) => {
        if (live) setTotal({ id: listingId, n: p.total });
      },
      (e: unknown) => {
        if (live) listError(e);
      },
    );
    return () => {
      live = false;
    };
  }, [handle, dir, listingId, listError]);

  const loadPage = useCallback(
    (offset: number, limit: number) =>
      handle === null ? Promise.resolve([]) : foreignChildren(handle, dir, offset, limit).then((p) => p.entries),
    [handle, dir],
  );

  const searching = open !== null && query !== "";
  const search = useForeignSearch(searching ? handle : null, query);
  const hits: ForeignHitRow[] = useMemo(() => (search.state === "done" ? search.hits : []), [search]);
  const loadHits = useCallback(
    (offset: number, limit: number) => Promise.resolve(hits.slice(offset, offset + limit)),
    [hits],
  );
  const folderSel = useSelection<ForeignRow>(loadPage, listError);
  const hitSel = useSelection<ForeignHitRow>(loadHits, listError);
  const clearFolderSel = folderSel.clear;
  const clearHitSel = hitSel.clear;
  useEffect(() => {
    clearHitSel();
  }, [search, clearHitSel]);
  const selectedRows: ForeignRow[] = searching ? hitSel.rows.map((h) => h.row) : [...folderSel.rows];
  const focused: ForeignRow | null = searching ? (hitSel.focus?.row ?? null) : folderSel.focus;

  useEffect(() => {
    folder.onMove(clearFolderSel);
  });
  const navigate = folder.go;
  const resetPath = folder.reset;

  /** Show a freshly opened archive, closing the one on screen. */
  const show = async (summary: ForeignArchiveSummary) => {
    const previous = shownRef.current;
    shownRef.current = summary.handle;
    if (previous !== null && previous !== summary.handle) {
      await closeForeignArchive(previous).catch(() => false);
    }
    setAsk(null);
    setOpen(summary);
    setPlan(null);
    setJob({ state: "idle" });
    clearQuery?.();
    resetPath();
  };
  const launchedRef = useRef<{ show: typeof show; ask: (wrong: boolean) => void } | null>(null);
  const shownRef = useRef<number | null>(null);
  useEffect(() => {
    launchedRef.current = { show, ask: (wrong) => setAsk({ reason: wrong ? "wrong" : "ask" }) };
    shownRef.current = open?.handle ?? null;
  });
  // The shell's "Open with MOCHI" for a ZIP, 7z, RAR, or tar. Rust opened the
  // file through the same path as the open dialog, or remembers it for the
  // password retry; no path crosses.
  useEffect(
    () =>
      subscribeLaunches((event) => {
        if (event.event === "foreign_opened") {
          setError(null);
          void launchedRef.current?.show(event.archive);
        } else if (event.event === "foreign_password") {
          setError(null);
          launchedRef.current?.ask(event.wrong);
        }
      }),
    [],
  );

  /** Open: the dialog first, or (after a password prompt) the same file again. */
  const doOpen = async (retry: boolean) => {
    setError(null);
    setBusy(true);
    const password = retry ? (passwordRef.current?.value ?? "") : null;
    try {
      const summary = await openForeignArchive(retry, password);
      // The password has been sent; do not keep it in the field.
      if (passwordRef.current) passwordRef.current.value = "";
      if (summary === null) return;
      await show(summary);
    } catch (e) {
      if (passwordRef.current) passwordRef.current.value = "";
      const kind = isIpcError(e) ? needsPassword(e.code) : null;
      if (kind !== null) setAsk({ reason: kind });
      else {
        setAsk(null);
        setError(errorText(t("foreign.openFailed"), e));
      }
    } finally {
      setBusy(false);
    }
  };

  const doClose = async () => {
    if (!open) return;
    await closeForeignArchive(open.handle).catch(() => false);
    setOpen(null);
    setPlan(null);
    setError(null);
    setJob({ state: "idle" });
    clearQuery?.();
    resetPath();
  };

  // The window's own Open (or a drop) opened a foreign archive, or it needs its password.
  const handoff = shell?.foreignHandoff ?? null;
  const seenHandoffRef = useRef(0);
  useEffect(() => {
    if (handoff === null || handoff.token === seenHandoffRef.current) return;
    seenHandoffRef.current = handoff.token;
    setError(null);
    if (handoff.kind === "opened") void launchedRef.current?.show(handoff.archive);
    else launchedRef.current?.ask(handoff.wrong);
  }, [handoff]);

  const closeToken = shell?.closeForeignToken ?? 0;
  const openToken = shell?.openForeignToken ?? 0;
  const seenTokensRef = useRef({ close: closeToken, open: openToken });
  const actionsRef = useRef<{ close: () => Promise<void>; open: () => Promise<void> } | null>(null);
  useEffect(() => {
    actionsRef.current = { close: doClose, open: () => doOpen(false) };
  });
  useEffect(() => {
    const seen = seenTokensRef.current;
    if (closeToken !== seen.close) {
      seen.close = closeToken;
      void actionsRef.current?.close();
    }
    if (openToken !== seen.open) {
      seen.open = openToken;
      void actionsRef.current?.open();
    }
  }, [closeToken, openToken]);

  const onEvent = useCallback((event: JobEvent) => {
    setJob((prev) => {
      switch (event.event) {
        case "started":
          return prev.state === "running"
            ? prev
            : { state: "running", jobId: event.job_id, phase: "", completed: 0, total: null, cancelRequested: false };
        case "progress":
          if (prev.state === "done") return prev;
          return {
            state: "running",
            jobId: event.job_id,
            phase: event.phase,
            completed: event.completed,
            total: event.total,
            cancelRequested: prev.state === "running" && prev.cancelRequested,
          };
        case "finished":
          return { state: "done", outcome: event.outcome };
      }
    });
    if (event.event === "finished" && event.outcome.status === "completed") setPlan(null);
    if (event.event === "finished" && newFolderRun.take()) {
      // Nothing was extracted: do not leave an empty new folder behind.
      if (event.outcome.status !== "completed") void discardExtractFolder().catch(() => undefined);
    }
  }, [newFolderRun]);

  const begin = (request: Parameters<typeof startJob>[0]) => {
    rememberFocus();
    setError(null);
    setJob({ state: "starting" });
    setExtracted(request.kind === "extract_foreign");
    startJob(request, onEvent).catch((e: unknown) => {
      setJob({ state: "idle" });
      setError(errorText(t("foreign.startFailed"), e));
      if (newFolderRun.take()) void discardExtractFolder().catch(() => undefined);
    });
  };

  /**
   * OK in the "Extract to..." window: Rust checks the folder and, with
   * `newFolder`, makes a new one named after the archive inside it. With
   * nothing in the way the extraction starts at once; if some entries collide
   * the plan panel lists them first, and nothing is ever overwritten. A folder
   * Rust cannot use rejects, and the window shows why.
   */
  const submitTo = async (folder: string, newFolder: boolean) => {
    if (handle === null) return;
    const keys = selectedRows.flatMap((r) => (r.key === null ? [] : [r.key]));
    const p = await planExtractForeignTo(handle, keys, folder, newFolder);
    setError(null);
    setJob({ state: "idle" });
    setWizard(false);
    if (p.plan.existing_total > 0 || p.plan.conflicts_total > 0) {
      const only = selectedRows.length === 1 ? (selectedRows[0]?.name ?? null) : null;
      setPlan({ plan: p, keys, what: only, n: keys.length });
      setMadeFolder(newFolder);
      return;
    }
    if (newFolder) newFolderRun.set();
    // Nothing in the way: folders that are there are added to.
    begin({ kind: "extract_foreign", handle, keys, on_conflict: p.plan.merged_folders > 0 ? "add_new" : "stop" });
  };

  /** Close the plan panel; a new folder nobody extracted into is tidied up (Rust only removes an empty one). */
  const closePlan = () => {
    if (madeFolder) void discardExtractFolder().catch(() => undefined);
    setMadeFolder(false);
    setPlan(null);
  };

  const showFolder = () => {
    showInFolder({ kind: "extract_destination" }).then(
      () => undefined,
      (e: unknown) => setError(errorText(t("start.showInFolderFailed"), e)),
    );
  };

  const planFor = (keys: string[], what: NameText | null, n: number, pick: boolean) => {
    if (handle === null) return;
    setError(null);
    setJob({ state: "idle" });
    planExtractForeign(handle, keys, pick).then(
      (p) => {
        if (p !== null) {
          setPlan({ plan: p, keys, what, n });
          setMadeFolder(false);
        }
      },
      (e: unknown) => setError(errorText(t("foreign.folderFailed"), e)),
    );
  };

  /** Toolbar Extract: the selection if there is one, else everything. */
  const extractNow = () => {
    const keys = selectedRows.flatMap((r) => (r.key === null ? [] : [r.key]));
    const only = selectedRows.length === 1 ? (selectedRows[0]?.name ?? null) : null;
    planFor(keys, only, keys.length, true);
  };

  const cancel = () => {
    if (job.state !== "running") return;
    const { jobId } = job;
    setJob({ ...job, cancelRequested: true });
    cancelJob(jobId).catch(() => undefined);
  };

  // Drag entries out onto the desktop or a folder.
  const dragFrom =
    <T extends { key: string | null }>(sel: Selection<T>) =>
    (row: T) => {
      if (handle === null || row.key === null) return null;
      const inSel = sel.rows.some((x) => x.key === row.key);
      const keys = inSel ? sel.rows.flatMap((x) => (x.key === null ? [] : [x.key])) : [row.key];
      return dragOutForeign(handle, keys).catch((e: unknown) => setError(errorText(t("browser.dragFailed"), e)));
    };
  const folderDrag = useDragOut(dragFrom(folderSel));
  const hitDrag = useDragOut(dragFrom(hitSel));

  // What is on: a foreign archive is read-only, so Add and Delete are off.
  const avail = availableActions(
    { kind: "foreign", busy: jobBusy },
    { selected: selectedRows.length, total: searching ? hits.length : (count ?? 0) },
  );
  const listMenu = useListMenu(
    listMenuItems({
      available: avail,
      selectAll: () => (searching ? hitSel.all(hits.length) : count !== null && folderSel.all(count)),
      copyNames: () => {
        const names = searching
          ? hitSel.rows.map((h) => h.path.text)
          : folderSel.rows.map((r) => [...path.map((c) => c.name.text), r.name.text].join("/"));
        copyNames(names).catch((e: unknown) => setError(errorText(t("browser.copyFailed"), e)));
      },
      extractTo: () => setWizard(true),
      add: () => undefined,
      remove: () => undefined,
    }),
  );

  const openHit = (hit: ForeignHitRow) => {
    if (hit.key === null) return;
    const above = ancestorsOf(hit.key, hit.path);
    clearQuery?.();
    navigate(hit.kind === "directory" ? [...above, { key: hit.key, name: hit.name }] : above, true);
  };

  const described = job.state === "done" ? describeOutcome(job.outcome) : null;
  const selectedCount = selectedRows.length;

  // What a running job is doing: in the open dialog (which covers the window), else in the status bar.
  const progressLine = jobBusy && (
    <div className="progress" data-testid="foreign-running">
      {job.state === "running" && (
        <ProgressMeter
          completed={job.completed}
          total={job.total}
          text={
            (job.phase ? progressText(job.phase, job.completed, job.total) : t("common.starting")) +
            (job.cancelRequested ? t("common.cancelling") : "")
          }
          textTestId="foreign-progress"
        />
      )}
      <button
        type="button"
        className="icon-button"
        data-testid="foreign-cancel"
        aria-label={t("common.cancel")}
        title={t("common.cancel")}
        onClick={cancel}
        disabled={job.state !== "running" || job.cancelRequested}
      >
        <CloseIcon />
      </button>
    </div>
  );

  const view = open && (
    <>
      <div className="pathbar">
        <NavButtons nav={folder} testIdPrefix="foreign-nav" disabled={searching} />
        <div className="path-meta" data-testid="foreign-meta">
          <strong className="path-title" title={open.file_name}>
            {open.file_name}
          </strong>
          <span className="badge format-badge" title={t("foreign.meta", { format: open.format })}>
            {open.format}
            <span className="sr-only">{t("foreign.readOnly")}</span>
          </span>
          {open.opened_with_password && (
            <span className="badge" data-testid="foreign-password-badge" title={t("foreign.openedWithPassword")}>
              <KeyIcon />
              <span className="sr-only">{t("foreign.openedWithPassword")}</span>
            </span>
          )}
          {!open.complete && (
            <span
              role="note"
              className="badge badge-warn"
              data-testid="foreign-partial"
              title={t("foreign.partial", { reason: open.stop_reason?.text ?? t("foreign.partialDefault") })}
            >
              {t("foreign.partialBadge")}
              <span className="sr-only">
                {t("foreign.partial", { reason: open.stop_reason?.text ?? t("foreign.partialDefault") })}
              </span>
            </span>
          )}
          {open.weak_encryption && (
            <span role="note" className="path-warn" data-testid="foreign-weak" title={weakNote()}>
              !<span className="sr-only">{weakNote()}</span>
            </span>
          )}
        </div>
        <Crumbs
          rootName={open.file_name}
          path={path}
          onGo={navigate}
          label={t("foreign.currentFolder")}
          testId="foreign-crumbs"
        />
        <button
          type="button"
          className="icon-button pathbar-close"
          data-testid="foreign-close"
          aria-label={t("foreign.close")}
          title={t("foreign.close")}
          onClick={() => void doClose()}
          disabled={busy || jobBusy}
        >
          <CloseIcon />
        </button>
      </div>
      {open && wizard && plan === null && (
        <ExtractToDialog
          count={selectedCount}
          archive={{ kind: "foreign", handle: open.handle }}
          onSubmit={submitTo}
          onClose={() => setWizard(false)}
        />
      )}
      {plan && (
        <Panel
          label={t("foreign.planGroup")}
          heading={t("foreign.planHeading")}
          testId="foreign-plan"
          fallbackId="foreign-heading"
          onClose={jobBusy ? undefined : closePlan}
        >
          {!(plan.plan.plan.existing_total > 0 || plan.plan.plan.conflicts_total > 0) && (
            <dl>
              <dt>{t("foreign.folder")}</dt>
              <dd data-testid="foreign-destination" title={plan.plan.plan.destination}>
                {plan.plan.plan.destination}
              </dd>
              <dt>{t("foreign.what2")}</dt>
              <dd>
                {plan.what ? (
                  <Name name={plan.what} />
                ) : plan.n > 0 ? (
                  tc(plan.n, "common.itemOne", "common.itemMany")
                ) : (
                  t("foreign.everything")
                )}{" "}
                (
                {t(plan.plan.plan.entries === 1 ? "foreign.itemsOne" : "foreign.itemsMany", {
                  n: formatNumber(plan.plan.plan.entries),
                })}
                )
              </dd>
            </dl>
          )}
          {plan.plan.skipped_kinds > 0 && (
            <p role="note" className="note" data-testid="foreign-skipped-kinds">
              {t(plan.plan.skipped_kinds === 1 ? "foreign.skippedKindsOne" : "foreign.skippedKindsMany", {
                n: formatNumber(plan.plan.skipped_kinds),
              })}
            </p>
          )}
          {progressLine}
          <ExtractChoices
            plan={plan.plan.plan}
            busy={jobBusy}
            testIdPrefix="foreign"
            onChoose={(on_conflict) =>
              begin({ kind: "extract_foreign", handle: open.handle, keys: plan.keys, on_conflict })
            }
            onNewFolder={() => {
              const { keys } = plan;
              const folder = plan.plan.plan.destination;
              closePlan();
              planExtractForeignTo(open.handle, keys, folder, true).then(
                (p) => {
                  if (p.plan.existing_total > 0 || p.plan.conflicts_total > 0) {
                    setPlan({ plan: p, keys, what: null, n: keys.length });
                    setMadeFolder(true);
                    return;
                  }
                  newFolderRun.set();
                  begin({ kind: "extract_foreign", handle: open.handle, keys, on_conflict: "stop" });
                },
                (e: unknown) => setError(errorText(t("foreign.folderFailed"), e)),
              );
            }}
            onChangeFolder={() => planFor(plan.keys, plan.what, plan.n, true)}
            onCancel={closePlan}
            hint={<Hint text={extractNote()} />}
          />
        </Panel>
      )}

      <div className="workspace">
        <div
          className="listing"
          {...listMenu.listProps}
          onMouseUp={(e) => {
            // The mouse's back and forward buttons walk the folder history.
            if (searching) return;
            if (e.button === 3) folder.back();
            else if (e.button === 4) folder.forward();
          }}
          onKeyDown={(e) => {
            if (!searching && folderKeys(e, folder)) return;
            listKeys(e, {
              all: () => (searching ? hitSel.all(hits.length) : count !== null && folderSel.all(count)),
              none: () => {
                const sel = searching ? hitSel : folderSel;
                if (sel.rows.length === 0) return false;
                sel.clear();
                return true;
              },
              up: () => {
                if (!searching && path.length > 0) folder.up();
              },
            });
          }}
        >
          <p id="foreign-entry-hint" className="sr-only">
            {t("help.keys")}
          </p>
          {listMenu.menu}
          {searching ? (
            <SearchResults
              search={search}
              rowHeight={ROW_HEIGHT}
              hintId="foreign-entry-hint"
              testId="foreign-search-hits"
              onMarquee={hitSel.marquee}
              renderRow={(h: ForeignHitRow, index, tabIndex) => {
                return (
                  <button
                    type="button"
                    className={hitSel.has(h) ? "entry selected" : "entry"}
                    data-testid="search-hit"
                    data-kind={h.kind}
                    aria-pressed={hitSel.has(h)}
                    tabIndex={tabIndex}
                    {...rowHandlers(hitSel, h, index, () => openHit(h), hitDrag, h.kind === "directory")}
                  >
                    <EntryCells kind={h.kind} name={h.name} kindLabel={kindLabel(h)} size={h.size} folder={h.folder} />
                  </button>
                );
              }}
            />
          ) : (
            count !== null && (
              <VirtualList
                key={listingId}
                total={count}
                rowHeight={ROW_HEIGHT}
                loadPage={loadPage}
                label={t("foreign.folderContents")}
                head={
                  <>
                    <ListHead />
                    {path.length > 0 && <UpRow testId="foreign-up-row" onUp={() => folder.up()} />}
                  </>
                }
                hintId="foreign-entry-hint"
                focusFirst={folder.focusList}
                revealKey={reveal}
                keyOf={rowKey}
                onReveal={folderSel.ensure}
                testId="foreign-entries"
                onMarquee={folderSel.marquee}
                onError={listError}
                renderRow={(row: ForeignRow, index: number, tabIndex: 0 | -1) => (
                  <button
                    type="button"
                    className={folderSel.has(row) ? "entry selected" : "entry"}
                    data-testid="foreign-entry"
                    data-kind={row.kind}
                    data-refused={row.note ? "true" : undefined}
                    aria-pressed={folderSel.has(row)}
                    tabIndex={tabIndex}
                    {...rowHandlers(
                      folderSel,
                      row,
                      index,
                      () => {
                        if (row.kind === "directory" && row.key !== null)
                          navigate([...path, { key: row.key, name: row.name }], true);
                      },
                      folderDrag,
                      row.kind === "directory",
                    )}
                  >
                    <EntryCells
                      kind={row.kind}
                      name={row.name}
                      kindLabel={kindLabel(row)}
                      size={sizeText(row)}
                      after={
                        (row.encryption || row.note) && (
                          <span className="row-flag" title={[row.encryption, row.note].filter(Boolean).join(" · ")}>
                            {row.encryption && <span className="meta"> · {row.encryption}</span>}
                            {row.note && <span className="meta"> · {row.note}</span>}
                          </span>
                        )
                      }
                    />
                  </button>
                )}
              />
            )
          )}
        </div>
        <aside className="info" aria-label={t("foreign.info")} hidden={!infoOpen} data-testid="foreign-info">
          {focused && (
            <section className="details" aria-labelledby="foreign-details-heading" data-testid="foreign-details">
              <h3 id="foreign-details-heading">
                <Name name={focused.name} />
              </h3>
              <dl>
                <dt>{t("foreign.detailKind")}</dt>
                <dd>{kindLabel(focused)}</dd>
                {focused.size !== null && (
                  <>
                    <dt>{t("foreign.detailSize")}</dt>
                    <dd>{formatSize(focused.size)}</dd>
                  </>
                )}
                {focused.encryption && (
                  <>
                    <dt>{t("foreign.detailEncryption")}</dt>
                    <dd>{focused.encryption}</dd>
                  </>
                )}
                {focused.note && (
                  <>
                    <dt>{t("foreign.detailNote")}</dt>
                    <dd>{focused.note}</dd>
                  </>
                )}
              </dl>
            </section>
          )}
          {open.notes.length > 0 && (
            <ul className="staged warning" aria-label={t("foreign.limitNotes")} data-testid="foreign-notes">
              {open.notes.map((n) => (
                <li key={n}>{n}</li>
              ))}
            </ul>
          )}
        </aside>
      </div>
      <ToolSlot name="count" owner="foreign">
        <span className="statusbar-text" data-testid="foreign-count">
          {searching
            ? search.statusText
            : (count === null ? t("common.loading") : tc(count, "common.itemOne", "common.itemMany")) +
              (selectedCount > 0
                ? t("common.statusSeparator") + t("common.selectedCount", { n: selectedCount })
                : "")}
        </span>
      </ToolSlot>
    </>
  );

  return (
    <section aria-labelledby="foreign-heading" className="foreign" data-testid="foreign">
      <h2 id="foreign-heading" tabIndex={-1} className="sr-only">
        {t("foreign.heading")}
      </h2>
      {open && (
        <ToolSlot name="extract" owner="foreign">
          <ToolButton
            icon={<ExtractIcon />}
            label={t("extract.toolbar")}
            testId="foreign-extract-all"
            disabled={!avail.extract.enabled}
            onClick={extractNow}
            title={tipFor(
              avail.extract,
              selectedCount > 0
                ? t("extract.selectedTip", { n: formatNumber(selectedCount) })
                : t("foreign.extractAllTip"),
            )}
          />
        </ToolSlot>
      )}
      {open && (
        <ToolSlot name="extractTo" owner="foreign">
          <ToolButton
            icon={<ExtractToIcon />}
            label={t("extract.to")}
            testId="foreign-extract-to"
            disabled={!avail.extractTo.enabled}
            onClick={() => setWizard(true)}
            title={tipFor(
              avail.extractTo,
              selectedCount > 0
                ? t("extract.toSelectedTip", { n: formatNumber(selectedCount) })
                : t("extract.toTip"),
            )}
          />
        </ToolSlot>
      )}
      {open && (
        <ToolSlot name="test" owner="foreign">
          <ToolButton
            icon={<CheckIcon />}
            label={t("foreign.testToolbar")}
            testId="foreign-test"
            disabled={!avail.test.enabled}
            onClick={() => begin({ kind: "test_foreign", handle: open.handle })}
            title={tipFor(avail.test, testNote())}
          />
        </ToolSlot>
      )}
      <ToolSlot name="status" owner="any">
        {error && (
          <p role="alert" className="error status-line" data-testid="foreign-error" title={error}>
            {error}
          </p>
        )}
        {plan === null && progressLine}
        <ResultBar
          prefix="foreign"
          statusTestId="foreign-status"
          statusAttrs={{ "data-state": job.state === "done" ? job.outcome.status : undefined }}
          lines={described ? [described.headline, ...described.notes] : []}
          headlineTestId="foreign-headline"
          warn={described?.warn ?? false}
          details={described?.details ?? null}
          onShowFolder={
            job.state === "done" && job.outcome.status === "completed" && extracted ? showFolder : undefined
          }
          onDismiss={() => setJob({ state: "idle" })}
          fallbackId="foreign-heading"
        />
      </ToolSlot>
      <ToolSlot name="stage" owner="any">
        <div className="foreign-stage" data-testid="foreign-stage" hidden={!open && !ask}>
          {ask && (
            <Panel
              label={t("foreign.passwordGroup")}
              heading={t("foreign.passwordGroup")}
              testId="foreign-password"
              fallbackId="foreign-heading"
              onClose={() => setAsk(null)}
            >
              <form
                onSubmit={(e) => {
                  e.preventDefault();
                  void doOpen(true);
                }}
              >
                <p role="note" data-testid="foreign-password-note">
                  {t(ask.reason === "wrong" ? "foreign.wrongPassword" : "foreign.needPassword")}
                </p>
                <label>
                  {t("foreign.password")}{" "}
                  <input ref={passwordRef} type="password" autoComplete="off" data-testid="foreign-password-input" />
                </label>
                <div className="actions dialog-actions">
                  <button type="submit" className="primary" data-testid="foreign-password-submit" disabled={busy}>
                    {t("foreign.passwordSubmit")}
                  </button>
                  <button type="button" data-testid="foreign-password-cancel" onClick={() => setAsk(null)}>
                    {t("common.cancel")}
                  </button>
                </div>
              </form>
            </Panel>
          )}
          {view}
        </div>
      </ToolSlot>
    </section>
  );
}
