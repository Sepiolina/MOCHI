import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";

import { Panel } from "../a11y/Panel";
import {
  CaretDownIcon,
  CheckIcon,
  CloseIcon,
  ExtractToIcon,
  ExtractIcon,
  FindIcon,
  FoldersIcon,
  GearIcon,
  HelpIcon,
  InfoIcon,
  KeyIcon,
  MoreIcon,
  OpenIcon,
  PlusIcon,
  ScanIcon,
  TickIcon,
  TrashIcon,
  WrenchIcon,
} from "../icons";
import { formatNumber, formatTime, t, tc, tn } from "../i18n";

import { closeArchive, entryDetails, listChildren, openArchive, openSnapshot } from "../ipc/browse";
import type {
  ArchiveSummary,
  OpenOutcome,
  CommitView,
  EntryDetails,
  EntryRow,
  NameText,
  SnapshotView,
} from "../ipc/browse";
import { dragOut } from "../ipc/dragout";
import { subscribeLaunches } from "../ipc/launch";
import { forgetRecentArchives, openDroppedArchive, openRecent, recentArchives } from "../ipc/start";
import type { RecentView } from "../ipc/start";
import { Start } from "../start/Start";
import { ArchiveInfo } from "../info/ArchiveInfo";
import { ScanArchive } from "../scan/ScanArchive";
import { errorText, formatMode, formatSize, shortSize } from "./format";
import { ListHead, EntryCells } from "./ListHead";
import { Name } from "./Name";
import { Crumbs, NavButtons, UpRow } from "./FolderNav";
import { Tree } from "./Tree";
import { VirtualList } from "./VirtualList";
import { folderKeys, useFolderPath } from "./useFolderPath";
import { Checker } from "../check/Checker";
import { EncryptionNotes, PassphraseForm } from "../crypt/PassphraseForm";
import { Rekey } from "../crypt/Rekey";
import { hiddenWithKey, notSigned, visibleWithoutKey } from "../crypt/messages";
import { useDragOut } from "../list/dragOut";
import { copyNames, listMenuItems, useListMenu } from "../list/ContextMenu";
import { ancestorsOf, splitPath } from "../list/keys";
import { useSelection } from "../list/selection";
import type { Selection } from "../list/selection";
import { listKeys, rowHandlers } from "../list/rows";
import { SearchBox } from "../search/SearchBox";
import { SearchResults } from "../search/SearchResults";
import type { HitRow } from "../search/SearchResults";
import { useArchiveSearch } from "../search/useArchiveSearch";
import { cancelUnlock, unlockArchive } from "../ipc/crypt";
import { Extractor } from "../extract/Extractor";
import { Finder } from "../find/Finder";
import { useReveal } from "../find/useReveal";
import { findOpenHit } from "../ipc/find";
import type { FindHit } from "../ipc/find";
import { Writer } from "../write/Writer";
import { availableActions, tipFor } from "../shell/availability";
import { SlotHost, ShellContext, ToolButton, ToolSlot } from "../shell/Shell";
import { TitleBar } from "../shell/TitleBar";
import { useToolbarLabels } from "../shell/useToolbarLabels";
import type { ForeignHandoff, Shell, SlotName } from "../shell/Shell";
import { ToolSheet } from "../shell/ToolSheet";
import type { ToolId } from "../shell/ToolSheet";

/** Row height of the file list, in pixels. */
export const ROW_HEIGHT = 26;

function profileText(a: ArchiveSummary): string {
  const parts = [t("browser.profileCore")];
  if (a.profile.tar_compatible) parts.push(t("browser.profileTar"));
  if (a.profile.encrypted) parts.push(t("browser.profileEncrypted"));
  if (a.profile.redundancy_percent !== null) {
    parts.push(t("browser.profileRecovery", { percent: a.profile.redundancy_percent }));
  }
  return parts.join(t("common.listSeparator"));
}

/** Retention wording for one commit. Unknown is said as unknown. */
function retentionText(c: CommitView): string {
  const notes: string[] = [];
  if (c.retained === null) notes.push(t("browser.retentionUnknown"));
  else if (c.expired) notes.push(t("browser.retentionExpired"));
  else if (!c.retained) notes.push(t("browser.retentionNotRetained"));
  return notes.join(t("browser.retentionSeparator"));
}

/** The snapshot timeline: every commit, newest first. */
function Timeline({
  commits,
  selected,
  onSelect,
}: {
  commits: CommitView[];
  selected: number;
  onSelect: (seq: number) => void;
}) {
  const head = commits.length - 1;
  return (
    <>
      <div className="popover-head" aria-hidden="true">
        <span>{t("browser.timeline")}</span>
        <span className="popover-count">{formatNumber(commits.length)}</span>
      </div>
      <ol className="timeline" aria-label={t("browser.timeline")} data-testid="timeline" reversed>
        {[...commits].reverse().map((c) => {
          const current = c.seq === selected;
          return (
            <li key={c.seq}>
              <button
                type="button"
                aria-current={current ? "true" : undefined}
                className={current ? "commit current" : "commit"}
                data-testid={`commit-${c.seq}`}
                onClick={() => onSelect(c.seq)}
              >
                <span className="commit-mark" aria-hidden="true">
                  {current && <TickIcon />}
                </span>
                <span className="commit-text">
                  <span className="seq">
                    {t(c.seq === head ? "browser.commitHead" : "browser.commit", { seq: c.seq })}
                  </span>
                  <span className="meta" title={c.time ?? undefined}>
                    {c.time === null ? t("browser.noTime") : formatTime(c.time)}
                  </span>
                  {retentionText(c) && <span className="meta commit-note">{retentionText(c)}</span>}
                  {c.holds.map((h) => (
                    <span className="meta commit-note" key={h.text}>
                      {t("browser.hold")}
                      <Name name={h} />
                    </span>
                  ))}
                </span>
              </button>
            </li>
          );
        })}
      </ol>
    </>
  );
}

function Details({ details }: { details: EntryDetails }) {
  const a = details.attributes;
  return (
    <section aria-labelledby="details-heading" className="details" data-testid="details">
      <h3 id="details-heading">
        <Name name={details.path} />
      </h3>
      <dl>
        <dt className="sr-only">{t("browser.detailPath")}</dt>
        <dd className="sr-only" data-testid="details-path">
          <Name name={details.path} />
        </dd>
        <dt>{t("browser.detailKind")}</dt>
        <dd>{details.kind === "directory" ? t("common.folder") : t("common.file")}</dd>
        {details.kind === "file" && (
          <>
            <dt>{t("browser.detailSize")}</dt>
            <dd>{formatSize(details.size)}</dd>
          </>
        )}
        {a.state === "recorded" ? (
          <>
            <dt>{t("browser.detailModified")}</dt>
            <dd>{a.mtime ?? t("browser.detailNotRecorded")}</dd>
            {a.posix && (
              <>
                <dt>{t("browser.detailPermissions")}</dt>
                <dd>
                  {t("browser.detailPosix", {
                    mode: formatMode(a.posix.mode),
                    uid: a.posix.uid,
                    gid: a.posix.gid,
                  })}
                </dd>
              </>
            )}
            {a.windows && (
              <>
                <dt>{t("browser.detailWindows")}</dt>
                <dd>{a.windows.join(t("common.listSeparator")) || t("browser.detailNoneSet")}</dd>
              </>
            )}
          </>
        ) : (
          <>
            <dt>{t("browser.detailAttributes")}</dt>
            <dd role="note">
              {t("browser.detailAttributesError", { code: a.error.code, message: a.error.message })}
            </dd>
          </>
        )}
      </dl>
      <details className="technical" data-testid="details-technical">
        <summary>{t("start.technical")}</summary>
        <dl>
          {details.kind === "file" && (
            <>
              <dt>{t("browser.detailHash")}</dt>
              <dd className="hash">{details.content_hash}</dd>
            </>
          )}
          <dt>{t("browser.detailVersionId")}</dt>
          <dd className="hash">{details.file_version_id}</dd>
        </dl>
      </details>
    </section>
  );
}

type Open = { archive: ArchiveSummary; snapshot: SnapshotView };

/**
 * Covers the window below the title bar while an archive is read (owner report
 * 2026-11-12): it says which file, takes the pointer and the keyboard so nothing behind it
 * can be started meanwhile, and leaves the window's own buttons (minimize, close) usable.
 * It fades in after a moment, so a quick local open does not flash. The name is the user's
 * own file name, shown as text.
 */
function OpeningNotice({ fileName }: { fileName: string }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    ref.current?.focus();
  }, []);
  return (
    <div
      className="opening-layer"
      data-testid="opening-notice"
      // Keys stay here: focus does not leave for the window behind, and no shortcut of
      // the window runs while the archive is read.
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === "Tab") e.preventDefault();
      }}
    >
      <div className="opening-card" role="status" aria-live="polite" ref={ref} tabIndex={-1} data-testid="opening-card">
        <span className="spinner" aria-hidden="true" />
        <div>
          <p className="opening-title">{t("browser.openingFile", { name: fileName })}</p>
          <p className="note">{t("browser.openingNote")}</p>
        </div>
      </div>
    </div>
  );
}

/** An entry the list can select: a folder row or a search hit. */
export type Picked = { key: string; name: NameText };

const rowKey = (r: EntryRow) => r.key;

// Keys shown beside the More menu's own items (key names stay in English in every language).
const KEY_INFO = "Alt+Enter";
const KEY_HELP = "F1";

/** Plan D1: open an archive and browse its snapshots. Read-only. */
export function Browser({
  foreign,
  tools,
  help,
}: {
  /** The foreign-archive browser (D8); it draws into the stage slot. */
  foreign?: ReactNode;
  /** The tools the More menu opens, by id ("settings", "check", ...). */
  tools?: Partial<Record<ToolId, ReactNode>>;
  help?: ReactNode;
}) {
  const [open, setOpen] = useState<Open | null>(null);
  const [busy, setBusy] = useState(false);
  // The file Rust is reading for this window now (owner report 2026-11-12: on a network
  // drive, or for a very large archive, the window seemed to do nothing). While it is set the
  // window says so and takes no other action.
  const [opening, setOpening] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const folder = useFolderPath();
  const { path, focusList, reveal } = folder;
  const [counted, setCounted] = useState<{ id: string; total: number } | null>(null);
  const [shownDetails, setShownDetails] = useState<{ id: string; details: EntryDetails } | null>(null);
  const [waiting, setWaiting] = useState<{ pending: number; fileName: string } | null>(null);
  // Set when the shell asked to extract (D6): the Extractor then shows its plan.
  const [autoExtract, setAutoExtract] = useState<{
    token: number;
    handle: number;
    defaultFolder?: boolean;
  } | null>(null);
  const [recents, setRecents] = useState<RecentView[]>([]);
  const [createRequest, setCreateRequest] = useState(0);
  const [removeRequest, setRemoveRequest] = useState(0);
  const [addRequest, setAddRequest] = useState(0);
  const [creating, setCreating] = useState(false);
  const [extractAfterUnlock, setExtractAfterUnlock] = useState(false);
  // The window around the list (toolbar, panels that open and close).
  const [infoOpen, setInfoOpen] = useState(false);
  const [foldersOpen, setFoldersOpen] = useState(false);
  const [foldersShown, setFoldersShown] = useState(false);
  const [versionsOpen, setVersionsOpen] = useState(false);
  const [moreOpen, setMoreOpen] = useState(false);
  const [tool, setTool] = useState<ToolId | null>(null);
  const [foreignOpen, setForeignOpen] = useState(false);
  // The foreign archive's file name, for the title bar.
  const [foreignName, setForeignName] = useState<string | null>(null);
  const onForeignOpen = useCallback((isOpen: boolean, name?: string | null) => {
    setForeignOpen(isOpen);
    setForeignName(isOpen ? (name ?? null) : null);
  }, []);
  const [closeForeignToken, setCloseForeignToken] = useState(0);
  const [openForeignToken, setOpenForeignToken] = useState(0);
  const [testRequest, setTestRequest] = useState(0);
  // Changes when the shell asked for MOCHI Find (`--find`); the Find tool then scans the folders Rust holds.
  const [findLaunch, setFindLaunch] = useState(0);
  // The entry a Find hit asked this window to show: its folder, with the entry selected.
  const [revealRequest, setRevealRequest] = useState<{
    handle: number;
    key: string;
    path: NameText;
    kind: "file" | "directory";
  } | null>(null);
  const [query, setQuery] = useState("");
  const [foreignHandoff, setForeignHandoff] = useState<ForeignHandoff | null>(null);
  const [clearSearch, setClearSearch] = useState(0);
  const [slots, setSlots] = useState<Partial<Record<SlotName, HTMLElement>>>({});
  const register = useCallback((name: SlotName, el: HTMLElement | null) => {
    setSlots((prev) => {
      // A slot element lives as long as the window; an unmount needs no undo.
      if (el === null || prev[name] === el) return prev;
      return { ...prev, [name]: el };
    });
  }, []);
  const clearQuery = useCallback(() => setClearSearch((n) => n + 1), []);

  const refreshRecents = useCallback(() => {
    recentArchives().then(setRecents, () => setRecents([]));
  }, []);
  useEffect(() => {
    if (open === null) refreshRecents();
  }, [open, refreshRecents]);

  const handle = open?.archive.handle ?? null;
  const seq = open?.snapshot.seq ?? null;
  const dir = path.at(-1)?.key ?? null;
  const listingId = `${handle ?? ""}:${seq ?? ""}:${dir ?? ""}`;
  const total = counted?.id === listingId ? counted.total : null;

  const fail = useCallback((what: string) => (e: unknown) => setError(errorText(what, e)), []);
  const listError = useCallback((e: unknown) => setError(errorText(t("browser.listFailed"), e)), []);

  // The current directory's size, which the list needs before it pages.
  useEffect(() => {
    if (handle === null || seq === null) return;
    let live = true;
    listChildren(handle, seq, dir, false, 0, 1).then(
      (p) => {
        if (live) setCounted({ id: listingId, total: p.total });
      },
      (e: unknown) => {
        if (live) listError(e);
      },
    );
    return () => {
      live = false;
    };
  }, [handle, seq, dir, listingId, listError]);

  const loadPage = useCallback(
    (offset: number, limit: number) =>
      handle === null || seq === null
        ? Promise.resolve([])
        : listChildren(handle, seq, dir, false, offset, limit).then((p) => p.entries),
    [handle, seq, dir],
  );

  // What the toolbar's search found in the snapshot on screen.
  const searching = open !== null && !foreignOpen && query !== "";
  const search = useArchiveSearch(searching ? handle : null, seq, query);
  const hits = useMemo(() => (search.state === "done" ? search.hits : []), [search]);
  const loadHits = useCallback(
    (offset: number, limit: number) => Promise.resolve(hits.slice(offset, offset + limit)),
    [hits],
  );

  const folderSel = useSelection<EntryRow>(loadPage, listError);
  const hitSel = useSelection<HitRow>(loadHits, listError);
  const selected: Picked[] = (searching ? hitSel.rows : folderSel.rows).flatMap((r) =>
    r.key === null ? [] : [{ key: r.key, name: r.name }],
  );
  const focusKey = (searching ? hitSel.focus : folderSel.focus)?.key ?? null;
  const clearFolderSel = folderSel.clear;
  const clearHitSel = hitSel.clear;
  // A hit opened from MOCHI Find: select its row and scroll to it once its folder is listed.
  const revealer = useReveal({ handle, dir, listingId, total, loadPage, select: folderSel.ensure });

  // Moving to another folder drops the selection; `navigate` is the one way
  // to move (path bar, tree, double click, Backspace, history).
  useEffect(() => {
    folder.onMove(clearFolderSel);
  });
  const navigate = folder.go;
  const resetPath = folder.reset;

  // A new search, or none, starts with nothing selected among the hits.
  useEffect(() => {
    clearHitSel();
  }, [search, clearHitSel]);

  // The Info panel shows the entry clicked last.
  const detailsId = `${handle ?? ""}:${seq ?? ""}:${focusKey ?? ""}`;
  const details = shownDetails?.id === detailsId ? shownDetails.details : null;
  useEffect(() => {
    if (handle === null || seq === null || focusKey === null) return;
    let live = true;
    entryDetails(handle, seq, focusKey).then(
      (d) => {
        if (live) setShownDetails({ id: `${handle}:${seq}:${focusKey}`, details: d });
      },
      (e: unknown) => {
        if (live) fail(t("browser.detailsFailed"))(e);
      },
    );
    return () => {
      live = false;
    };
  }, [handle, seq, focusKey, fail]);

  /** Show an archive that is open in Rust, replacing the one on screen. */
  const show = async (archive: ArchiveSummary) => {
    setCloseForeignToken((n) => n + 1);
    // The same handle comes back when this window already had the file open in Rust: keep it.
    if (open && open.archive.handle !== archive.handle) await closeArchive(open.archive.handle);
    const head = archive.commits[archive.commits.length - 1]?.seq ?? 0;
    const snapshot = await openSnapshot(archive.handle, head);
    setOpen({ archive, snapshot });
    clearQuery();
    resetPath();
  };

  /** Wait for the user's choice of archive, then show it (and plan an extract if asked). */
  const openWith = async (get: () => Promise<OpenOutcome | null>, extract: boolean, what: string) => {
    setError(null);
    setBusy(true);
    try {
      if (waiting) {
        await cancelUnlock(waiting.pending).catch(() => false);
        setWaiting(null);
      }
      const outcome = await get();
      if (outcome === null) return;
      // This window keeps what it shows: Rust opened the archive in a new window (or brought
      // forward the window that already has it).
      if (outcome.status === "elsewhere") return;
      if (outcome.status === "needs_passphrase") {
        setWaiting({ pending: outcome.pending, fileName: outcome.file_name });
        setExtractAfterUnlock(extract);
        return;
      }
      // A ZIP, 7z, RAR, or tar: the foreign browser shows it (read-only).
      if (outcome.status === "foreign") {
        if (open) {
          await closeArchive(open.archive.handle).catch(() => false);
          setOpen(null);
        }
        const archive = outcome.archive;
        setForeignHandoff((h) => ({ token: (h?.token ?? 0) + 1, kind: "opened", archive }));
        return;
      }
      if (outcome.status === "foreign_password") {
        const wrong = outcome.wrong;
        setForeignHandoff((h) => ({ token: (h?.token ?? 0) + 1, kind: "password", wrong }));
        return;
      }
      await show(outcome.archive);
      if (extract) {
        setAutoExtract((a) => ({
          token: (a?.token ?? 0) + 1,
          handle: outcome.archive.handle,
          defaultFolder: true,
        }));
      }
    } catch (e) {
      setError(errorText(what, e));
    } finally {
      setBusy(false);
    }
  };

  const doOpen = (extract = false) => openWith(openArchive, extract, t("browser.openFailed"));
  const doOpenRecent = (index: number) =>
    openWith(() => openRecent(index), false, t("start.openRecentFailed"));
  const doOpenDropped = () => openWith(openDroppedArchive, false, t("start.openDroppedFailed"));
  /** The passphrase goes to Rust once; a wrong one leaves the prompt up. */
  const unlock = async (passphrase: string) => {
    if (!waiting) return;
    const archive = await unlockArchive(waiting.pending, passphrase);
    setWaiting(null);
    try {
      await show(archive);
      if (extractAfterUnlock) {
        setAutoExtract((a) => ({ token: (a?.token ?? 0) + 1, handle: archive.handle, defaultFolder: true }));
      }
    } catch (e) {
      setError(errorText(t("browser.openFailed"), e));
    }
  };

  const giveUp = () => {
    if (waiting) void cancelUnlock(waiting.pending).catch(() => false);
    setWaiting(null);
    setExtractAfterUnlock(false);
  };

  /** A commit was published: show the archive as it now stands, at its head. */
  const adopt = useCallback(
    async (archive: ArchiveSummary | null) => {
      if (archive === null) return;
      try {
        const head = archive.commits[archive.commits.length - 1]?.seq ?? 0;
        const snapshot = await openSnapshot(archive.handle, head);
        if (open && open.archive.handle !== archive.handle) {
          await closeArchive(open.archive.handle).catch(() => false);
        }
        setOpen({ archive, snapshot });
        resetPath();
      } catch (e) {
        setError(errorText(t("browser.adoptFailed"), e));
      }
    },
    [open, resetPath],
  );

  // The shell opened an archive, or could not (D6). Rust has done the opening
  // and chosen any extract folder; this only shows the result. The listener is
  // registered once and reads the latest handlers through a ref.
  type Launched = (archive: ArchiveSummary, extract: boolean) => Promise<void>;
  const launchedRef = useRef<Launched | null>(null);
  // The handle on screen, kept current between renders: two launches in quick
  // succession must each close the archive the one before opened.
  const shownRef = useRef<number | null>(null);
  const showLaunched: Launched = async (archive, extract) => {
    setError(null);
    try {
      const head = archive.commits[archive.commits.length - 1]?.seq ?? 0;
      const snapshot = await openSnapshot(archive.handle, head);
      const previous = shownRef.current;
      shownRef.current = archive.handle;
      if (previous !== null && previous !== archive.handle) {
        await closeArchive(previous).catch(() => false);
      }
      setOpen({ archive, snapshot });
      resetPath();
      if (extract) setAutoExtract((a) => ({ token: (a?.token ?? 0) + 1, handle: archive.handle }));
    } catch (e) {
      setError(errorText(t("browser.openFailed"), e));
    }
  };
  useEffect(() => {
    launchedRef.current = showLaunched;
    shownRef.current = open?.archive.handle ?? null;
  });
  useEffect(
    () =>
      subscribeLaunches((event) => {
        if (event.event === "opening") setOpening(event.file_name);
        else if (event.event === "opening_ended") setOpening(null);
        else if (event.event === "problem") setError(event.message);
        else if (event.event === "opened") void launchedRef.current?.(event.archive, event.extract);
        else if (event.event === "find") {
          setMoreOpen(false);
          setTool("find");
          setFindLaunch((n) => n + 1);
        } else if (event.event === "reveal") {
          // A hit of MOCHI Find: go to the entry once this window shows that archive.
          setRevealRequest({ handle: event.handle, key: event.key, path: event.path, kind: event.kind });
        }
        // An Encrypted archive handed to this (new) window: ask for its passphrase here.
        else if (event.event === "needs_passphrase") {
          setWaiting({ pending: event.pending, fileName: event.file_name });
          setExtractAfterUnlock(false);
        }
        // Foreign archives are the foreign browser's to show.
      }),
    [],
  );

  const doClose = async () => {
    if (!open) return;
    await closeArchive(open.archive.handle).catch(() => false);
    setOpen(null);
    setError(null);
    clearQuery();
    resetPath();
  };

  const pickSnapshot = async (to: number) => {
    if (!open || to === open.snapshot.seq) return;
    setError(null);
    setBusy(true);
    try {
      const snapshot = await openSnapshot(open.archive.handle, to);
      setOpen({ ...open, snapshot });
      clearQuery();
      resetPath();
    } catch (e) {
      setError(errorText(t("browser.openCommitFailed", { seq: to }), e));
    } finally {
      setBusy(false);
    }
  };

  /**
   * A hit of MOCHI Find. Rust opens its archive where archives open: here if this window is
   * empty, the window that already has it (brought forward), else a new window (it checks that
   * the file is the one that was scanned). Whichever window shows it is told to go to the
   * entry (the `reveal` event, applied below).
   */
  const openFound = async (hit: FindHit) => {
    setTool(null);
    setError(null);
    setBusy(true);
    try {
      const outcome = await findOpenHit(hit.run, hit.archive, hit.key);
      if (outcome.status === "opened") await show(outcome.archive);
      else if (outcome.status !== "elsewhere") setError(t("find.openFailed"));
    } catch (e) {
      setError(errorText(t("find.openFailed"), e));
    } finally {
      setBusy(false);
    }
  };

  // Applied once, when this window shows the request's archive.
  const startReveal = revealer.reveal;
  const revealedRef = useRef<unknown>(null);
  useEffect(() => {
    if (revealRequest === null || revealRequest === revealedRef.current) return;
    if (open === null || open.archive.handle !== revealRequest.handle) return;
    const r = revealRequest;
    revealedRef.current = r;
    const above = ancestorsOf(r.key, r.path);
    if (r.kind === "directory") {
      navigate([...above, { key: r.key, name: splitPath(r.path).name }], true);
    } else {
      navigate(above, true);
      startReveal({ handle: r.handle, dir: above.at(-1)?.key ?? null, key: r.key });
    }
  }, [revealRequest, open, navigate, startReveal]);

  // The More menu's tools open in a window over the list, one at a time.
  const moreButtonRef = useRef<HTMLButtonElement>(null);
  // The archive Info window (owner request 2026-11-14).
  const [archiveInfoOpen, setArchiveInfoOpen] = useState(false);
  // Scan for viruses (owner request 2026-11-14).
  const [scanOpen, setScanOpen] = useState(false);
  const showTool = (id: ToolId) => {
    setMoreOpen(false);
    setTool(id);
  };
  const doTest = () => {
    showTool("check");
    setTestRequest((n) => n + 1);
  };

  const showStart = open === null && !creating && waiting === null && !foreignOpen;
  const active = foreignOpen ? "foreign" : "mochi";
  const canInspect = open !== null || foreignOpen;

  // The start screen steps aside while the create panel is open; when the
  // panel closes without an archive, keyboard focus goes back to its button.
  const wasCreatingRef = useRef(false);
  useEffect(() => {
    if (wasCreatingRef.current && !creating && open === null) {
      document.querySelector<HTMLElement>('[data-testid="start-create"]')?.focus();
    }
    wasCreatingRef.current = creating;
  }, [creating, open]);

  // The version list and the More menu close on Escape and when the pointer
  // goes down outside them.
  const moreRef = useRef<HTMLDivElement>(null);
  const versionsRef = useRef<HTMLDivElement>(null);
  const versionsButtonRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  // Words beside the toolbar's icons whenever they fit (owner request 2026-11-10).
  const toolbarRef = useRef<HTMLDivElement>(null);
  useToolbarLabels(toolbarRef);
  // A choice closes the menu. Some items are drawn into it from other components
  // (a portal), whose clicks do not bubble through this element in React's tree, so
  // this listens on the element itself.
  useEffect(() => {
    const menu = menuRef.current;
    if (!menu) return;
    const onChoice = (e: Event) => {
      if (e.target instanceof Element && e.target.closest("button")) {
        moreButtonRef.current?.focus();
        setMoreOpen(false);
      }
    };
    menu.addEventListener("click", onChoice, true);
    return () => menu.removeEventListener("click", onChoice, true);
  }, []);
  useEffect(() => {
    if (!moreOpen && !versionsOpen) return;
    const onPointer = (e: PointerEvent) => {
      const target = e.target instanceof Node ? e.target : null;
      if (moreOpen && !moreRef.current?.contains(target)) setMoreOpen(false);
      // The versions button is outside the list; its own press toggles it.
      if (versionsOpen && !versionsRef.current?.contains(target) && !versionsButtonRef.current?.contains(target)) {
        setVersionsOpen(false);
      }
    };
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (moreOpen) moreButtonRef.current?.focus();
      setMoreOpen(false);
      setVersionsOpen(false);
    };
    document.addEventListener("pointerdown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [moreOpen, versionsOpen]);

  // Ctrl+Shift+F: Find in archives (names inside every .mochi under some folders).
  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.altKey || !e.shiftKey) return;
      if (e.key === "f" || e.key === "F") {
        e.preventDefault();
        setMoreOpen(false);
        setTool("find");
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);

  // The keys of the actions that sit in the More menu: Ctrl+N new archive, Ctrl+E extract,
  // Alt+Enter details, F1 help (Delete removes; the list handles it). A key presses the very
  // button the menu shows, so it is on and off exactly when the button is.
  const keysRef = useRef({ slots, canInspect });
  useEffect(() => {
    keysRef.current = { slots, canInspect };
  });
  useEffect(() => {
    const press = (name: SlotName) => {
      const button = keysRef.current.slots[name]?.querySelector<HTMLButtonElement>("button");
      if (button && !button.disabled) button.click();
    };
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.defaultPrevented) return;
      const ctrl = e.ctrlKey || e.metaKey;
      const key = e.key.toLowerCase();
      if (ctrl && !e.altKey && !e.shiftKey && (key === "n" || key === "e")) {
        e.preventDefault();
        setMoreOpen(false);
        press(key === "n" ? "new" : "extract");
      } else if (e.altKey && !ctrl && !e.shiftKey && e.key === "Enter") {
        e.preventDefault();
        if (keysRef.current.canInspect) setInfoOpen((v) => !v);
      } else if (e.key === "F1" && !ctrl && !e.altKey && !e.shiftKey) {
        e.preventDefault();
        setMoreOpen(false);
        setTool((v) => (v === "help" ? null : "help"));
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);

  // Desktop keys: Ctrl+F finds, Ctrl+O opens.
  const openRef = useRef(doOpen);
  useEffect(() => {
    openRef.current = doOpen;
  });
  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.altKey || e.shiftKey) return;
      if (e.key === "f" || e.key === "F") {
        const field = document.querySelector<HTMLInputElement>('[data-testid="search-query"]');
        if (field && !field.disabled) {
          e.preventDefault();
          field.focus();
          field.select();
        }
      } else if (e.key === "o" || e.key === "O") {
        e.preventDefault();
        void openRef.current();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);

  const shell: Shell = {
    active,
    register,
    slots,
    setForeignOpen: onForeignOpen,
    closeForeignToken,
    openForeignToken,
    infoOpen,
    query,
    clearQuery,
    foreignHandoff,
  };

  // Drag entries out of the list into a folder on the computer.
  const dragFrom =
    <T extends { key: string | null }>(sel: Selection<T>) =>
    (row: T) => {
      if (handle === null || seq === null || row.key === null) return null;
      const inSel = sel.rows.some((x) => x.key === row.key);
      const keys = inSel ? sel.rows.flatMap((x) => (x.key === null ? [] : [x.key])) : [row.key];
      return dragOut(handle, seq, keys).catch((e: unknown) => setError(errorText(t("browser.dragFailed"), e)));
    };
  const folderDrag = useDragOut(dragFrom(folderSel));
  const hitDrag = useDragOut(dragFrom(hitSel));

  const viewingHead = open !== null && open.snapshot.seq === (open.archive.commits.at(-1)?.seq ?? -1);
  // Which actions are on, for the toolbar, the More menu, the list's menu, and the Delete key.
  const avail = availableActions(
    {
      kind: foreignOpen ? "foreign" : open !== null ? "mochi" : "none",
      viewingHead,
      encrypted: open?.archive.profile.encrypted ?? false,
      locked: waiting !== null,
    },
    { selected: selected.length, total: searching ? hits.length : (total ?? 0) },
  );
  const listKeyHandlers = {
    all: () => (searching ? hitSel.all(hits.length) : total !== null && folderSel.all(total)),
    none: () => {
      const sel = searching ? hitSel : folderSel;
      if (sel.rows.length === 0) return false;
      sel.clear();
      return true;
    },
    up: () => {
      if (!searching && path.length > 0) folder.up();
    },
    remove: () => {
      if (avail.remove.enabled) setRemoveRequest((n) => n + 1);
    },
  };

  // The list's right-click menu. Names are the full in-archive paths, as text.
  const [extractToRequest, setExtractToRequest] = useState(0);
  const listMenu = useListMenu(
    listMenuItems({
      available: avail,
      selectAll: () => listKeyHandlers.all(),
      copyNames: () => {
        const names = searching
          ? hitSel.rows.map((h) => h.path.text)
          : folderSel.rows.map((r) => [...path.map((c) => c.name.text), r.name.text].join("/"));
        copyNames(names).catch((e: unknown) => setError(errorText(t("browser.copyFailed"), e)));
      },
      extractTo: () => setExtractToRequest((n) => n + 1),
      add: () => setAddRequest((n) => n + 1),
      remove: () => setRemoveRequest((n) => n + 1),
    }),
  );

  const openHit = (hit: HitRow) => {
    if (hit.key === null) return;
    const above = ancestorsOf(hit.key, hit.path);
    clearQuery();
    if (hit.kind === "directory") navigate([...above, { key: hit.key, name: hit.name }], true);
    else navigate(above, true);
  };

  const statusText =
    total === null
      ? t("common.loading")
      : tc(total, "common.itemOne", "common.itemMany") +
        (selected.length > 0 ? t("common.statusSeparator") + t("common.selectedCount", { n: selected.length }) : "");

  // The title bar's title: the file name of the archive on screen (untrusted text).
  const titleName = foreignOpen ? foreignName : (open?.archive.file_name ?? null);

  return (
    <ShellContext value={shell}>
      <section aria-labelledby="browser-heading" className="browser">
        <TitleBar name={titleName} heading>
          <div className="toolbar" ref={toolbarRef} role="group" aria-label={t("toolbar.label")} inert={opening !== null}>
            {/* The primary actions stay on the bar; the rest are in More, each with its key. */}
            <span className="tool-cell">
              <ToolButton
                icon={<OpenIcon />}
                label={t("browser.open")}
                testId="toolbar-open"
                title={t("toolbar.openTip")}
                disabled={busy}
                onClick={() => void doOpen()}
              />
            </span>
            <span className="toolbar-sep" aria-hidden="true" />
            <SlotHost name="add" />
            <SlotHost name="extractTo" />
            <SlotHost name="test" />
            <span className="tool-cell">
              <ToolButton
                icon={<FindIcon />}
                label={t("toolbar.find")}
                testId="toolbar-find"
                title={t("toolbar.findTip")}
                expanded={tool === "find"}
                onClick={() => showTool("find")}
              />
            </span>
            <span className="tool-cell">
              <ToolButton
                icon={<InfoIcon />}
                label={t("info.toolbar")}
                testId="toolbar-archive-info"
                title={open ? t("info.tip") : t("toolbar.openFirst")}
                disabled={open === null || foreignOpen}
                expanded={archiveInfoOpen}
                onClick={() => setArchiveInfoOpen(true)}
              />
            </span>
            <span className="tool-cell">
              <ToolButton
                icon={<ScanIcon />}
                label={t("scan.toolbar")}
                testId="toolbar-scan"
                title={open ? t("scan.tip") : t("toolbar.openFirst")}
                disabled={open === null || foreignOpen}
                expanded={scanOpen}
                onClick={() => setScanOpen(true)}
              />
            </span>
            <span className="toolbar-spacer" />
            <SearchBox disabled={!canInspect} onQuery={setQuery} clearToken={clearSearch} />
            <div className="more" ref={moreRef}>
              <button
                ref={moreButtonRef}
                type="button"
                className="tool"
                data-testid="toolbar-more"
                title={t("toolbar.more")}
                aria-expanded={moreOpen}
                aria-controls="more-menu"
                onClick={() => setMoreOpen((v) => !v)}
              >
                <MoreIcon />
                <span className="sr-only">{t("toolbar.more")}</span>
              </button>
              <div id="more-menu" className="menu" ref={menuRef} hidden={!moreOpen}>
                {/* Drawn here by the components that own them (Shell.tsx `MENU_SLOTS`). */}
                <SlotHost name="new" className="menu-cell" />
                <SlotHost name="extract" className="menu-cell" />
                <SlotHost name="remove" className="menu-cell" />
                <button
                  type="button"
                  className="menu-item"
                  data-testid="toolbar-info"
                  title={canInspect ? t("toolbar.infoTip") : t("toolbar.openFirst")}
                  disabled={!canInspect}
                  aria-pressed={infoOpen}
                  aria-keyshortcuts="Alt+Enter"
                  onClick={() => setInfoOpen((v) => !v)}
                >
                  <InfoIcon />
                  <span className="menu-label">{t("toolbar.info")}</span>
                  <span className="menu-key" aria-hidden="true">
                    {KEY_INFO}
                  </span>
                </button>
                <hr />
                <button type="button" className="menu-item" data-testid="start-extract" onClick={() => void doOpen(true)} disabled={busy}>
                  <ExtractIcon />
                  {t("start.extractOpen")}
                </button>
                <button type="button" className="menu-item" data-testid="menu-foreign" onClick={() => setOpenForeignToken((n) => n + 1)}>
                  <OpenIcon />
                  {t("toolbar.foreign")}
                </button>
                <hr />
                <button
                  type="button"
                  className="menu-item"
                  data-testid="menu-passphrases"
                  disabled={!avail.passphrases.enabled}
                  title={tipFor(avail.passphrases, t("app.sectionPassphrases"))}
                  onClick={() => showTool("passphrases")}
                >
                  <KeyIcon />
                  {t("app.sectionPassphrases")}
                </button>
                <button type="button" className="menu-item" data-testid="menu-repair" onClick={() => showTool("check")}>
                  <WrenchIcon />
                  {t("toolbar.repair")}
                </button>
                <hr />
                <button type="button" className="menu-item" data-testid="start-settings" onClick={() => showTool("settings")}>
                  <GearIcon />
                  {t("start.settings")}
                </button>
                <button
                  type="button"
                  className="menu-item"
                  data-testid="toolbar-help"
                  title={t("toolbar.helpTip")}
                  aria-keyshortcuts="F1"
                  onClick={() => showTool("help")}
                >
                  <HelpIcon />
                  <span className="menu-label">{t("help.toolbar")}</span>
                  <span className="menu-key" aria-hidden="true">
                    {KEY_HELP}
                  </span>
                </button>
              </div>
            </div>
          </div>
        </TitleBar>
        <h2 id="browser-heading" tabIndex={-1} className="sr-only">
          {open || foreignOpen ? t("browser.heading") : t("start.heading")}
        </h2>
        {/* What the toolbar's other buttons are while their owner has nothing
            to offer: the MOCHI ones when no MOCHI archive is open are drawn by
            their components; while a foreign archive is open, Add and Remove
            are off. */}
        <ToolSlot name="test" owner="mochi">
          <ToolButton
            icon={<CheckIcon />}
            label={t("toolbar.test")}
            testId="toolbar-test"
            title={open ? t("toolbar.testTip") : t("toolbar.openFirst")}
            disabled={open === null}
            onClick={doTest}
          />
        </ToolSlot>
        <ToolSlot name="add" owner="foreign">
          <ToolButton icon={<PlusIcon />} label={t("write.add")} testId="add-to-archive" title={tipFor(avail.add, t("write.addTip"))} disabled={!avail.add.enabled} onClick={() => setAddRequest((n) => n + 1)} />
        </ToolSlot>
        <ToolSlot name="remove" owner="foreign">
          <ToolButton icon={<TrashIcon />} label={t("write.remove")} testId="remove-entry" title={tipFor(avail.remove, t("write.removeTip"))} disabled={!avail.remove.enabled} onClick={() => setRemoveRequest((n) => n + 1)} />
        </ToolSlot>
        <ToolSlot name="extract" owner="mochi">
          {open === null && (
            <ToolButton
              icon={<ExtractIcon />}
              label={t("extract.toolbar")}
              testId="extract-all"
              title={tipFor(avail.extract, t("extract.allTip"))}
              disabled
              onClick={doTest}
            />
          )}
        </ToolSlot>
        <ToolSlot name="extractTo" owner="mochi">
          {open === null && (
            <ToolButton
              icon={<ExtractToIcon />}
              label={t("extract.to")}
              testId="extract-to"
              title={tipFor(avail.extractTo, t("extract.toTip"))}
              disabled
              onClick={doTest}
            />
          )}
        </ToolSlot>

        {open && !foreignOpen && (
          <div className="pathbar">
            <button
              type="button"
              className="icon-button"
              data-testid="toggle-folders"
              aria-label={t("browser.paneFolders")}
              title={t("browser.paneFolders")}
              aria-expanded={foldersOpen}
              onClick={() => {
                setFoldersOpen((v) => !v);
                setFoldersShown(true);
              }}
            >
              <FoldersIcon />
            </button>
            <NavButtons nav={folder} testIdPrefix="nav" disabled={searching} />
            <Crumbs
              rootName={open.archive.file_name}
              path={path}
              onGo={navigate}
              label={t("browser.currentFolder")}
              testId="crumbs"
            />
            <div className="path-meta" data-testid="archive-meta">
              {/* The root crumb and the window title already show the name: here it is for
                  assistive technology only, which hears it before the badge and the version. */}
              <span className="sr-only">{open.archive.file_name}</span>
              {open.archive.profile.encrypted && (
                <span
                  className="badge"
                  data-testid="encrypted-note"
                  title={[t("browser.profileEncrypted"), hiddenWithKey(), ...visibleWithoutKey(), notSigned()].join("\n")}
                >
                  <KeyIcon />
                  <span className="sr-only">{t("browser.profileEncrypted")}</span>
                </span>
              )}
            </div>
            <div className="versions" ref={versionsRef}>
              <button
                type="button"
                className="version-button"
                ref={versionsButtonRef}
                data-testid="toggle-versions"
                aria-expanded={versionsOpen}
                aria-controls="versions-pop"
                disabled={open.archive.commits.length < 2}
                title={tn(open.archive.commits.length, "browser.metaCommitOne", "browser.metaCommitMany", {
                  n: open.archive.commits.length,
                })}
                onClick={() => setVersionsOpen((v) => !v)}
              >
                {t("browser.versionShort", { seq: open.snapshot.seq })}
                {open.archive.commits.length > 1 && <CaretDownIcon />}
              </button>
              <div id="versions-pop" className="popover" hidden={!versionsOpen}>
                <Timeline
                  commits={open.archive.commits}
                  selected={open.snapshot.seq}
                  onSelect={(s) => {
                    setVersionsOpen(false);
                    void pickSnapshot(s);
                  }}
                />
              </div>
            </div>
            {open.archive.retention_error && (
              <span role="note" className="path-warn" title={t("browser.retentionError", {
                code: open.archive.retention_error.code,
                message: open.archive.retention_error.message,
              })}>
                !
                <span className="sr-only">
                  {t("browser.retentionError", {
                    code: open.archive.retention_error.code,
                    message: open.archive.retention_error.message,
                  })}
                </span>
              </span>
            )}
            {open.snapshot.catalog_source.source === "snapshot_manifest" && (
              <span role="note" className="path-warn" data-testid="catalog-note" title={t("browser.catalogRebuilt", { code: open.snapshot.catalog_source.image_error.code })}>
                !
                <span className="sr-only">
                  {t("browser.catalogRebuilt", { code: open.snapshot.catalog_source.image_error.code })}
                </span>
              </span>
            )}
            <button
              type="button"
              className="icon-button pathbar-close"
              data-testid="close-archive"
              aria-label={t("browser.close")}
              title={t("browser.close")}
              onClick={doClose}
              disabled={busy}
            >
              <CloseIcon />
            </button>
          </div>
        )}

        <div className="stage">
          <SlotHost name="stage" className="stage-foreign" />
          {/* Outside the MOCHI stage, which is hidden while a ZIP, 7z or RAR file is open: the
              New archive dialog (position: fixed) must still be visible then. */}
          <Writer
            archive={open?.archive ?? null}
            viewingHead={viewingHead}
            selected={selected}
            onCommitted={(a) => void adopt(a)}
            showToolbar
            createRequest={createRequest}
            removeRequest={removeRequest}
            addRequest={addRequest}
            onCreating={setCreating}
            onArchiveDropped={() => void doOpenDropped()}
          />
          <div className="stage-mochi" hidden={foreignOpen}>
            {waiting && (
              <Panel
                label={t("browser.passphraseGroup")}
                testId="unlock-panel"
                fallbackId="browser-heading"
                heading={t("browser.isEncrypted", { name: waiting.fileName })}
                onClose={giveUp}
              >
                <PassphraseForm
                  testId="unlock"
                  label={t("browser.unlockLabel")}
                  submitLabel={t("browser.unlockSubmit")}
                  onSubmit={unlock}
                  onCancel={giveUp}
                >
                  <EncryptionNotes creating={false} />
                </PassphraseForm>
              </Panel>
            )}
            {showStart && (
              <Start
                busy={busy}
                recents={recents}
                onCreate={() => setCreateRequest((n) => n + 1)}
                onOpen={() => void doOpen()}
                onOpenRecent={(i) => void doOpenRecent(i)}
                onForgetRecent={() => {
                  forgetRecentArchives().then(refreshRecents, refreshRecents);
                }}
              />
            )}
            {open && (
              <>
                <Extractor
                  archive={open.archive}
                  seq={open.snapshot.seq}
                  selected={selected}
                  autoPlan={autoExtract}
                  toRequest={extractToRequest}
                />
                <div className="workspace">
                  {foldersShown && (
                    <nav className="folders" aria-label={t("browser.paneFolders")} hidden={!foldersOpen}>
                      <Tree
                        key={`${open.archive.handle}:${open.snapshot.seq}`}
                        handle={open.archive.handle}
                        seq={open.snapshot.seq}
                        current={dir}
                        onOpen={navigate}
                        onError={listError}
                      />
                    </nav>
                  )}
                  <div
                    className="listing"
                    onKeyDown={(e) => {
                      if (!searching && folderKeys(e, folder)) return;
                      listKeys(e, listKeyHandlers);
                    }}
                    onMouseUp={(e) => {
                      // The mouse's back and forward buttons walk the folder history.
                      if (searching) return;
                      if (e.button === 3) folder.back();
                      else if (e.button === 4) folder.forward();
                    }}
                    {...listMenu.listProps}
                  >
                    <p id="entry-hint" className="sr-only">
                      {t("help.keys")}
                    </p>
                    {listMenu.menu}
                    {searching ? (
                      <SearchResults
                        search={search}
                        rowHeight={ROW_HEIGHT}
                        hintId="entry-hint"
                        testId="search-hits"
                        onMarquee={hitSel.marquee}
                        renderRow={(hit, index, tabIndex) => (
                          <button
                            type="button"
                            className={hitSel.has(hit) ? "entry selected" : "entry"}
                            data-testid="search-hit"
                            data-kind={hit.kind}
                            aria-pressed={hitSel.has(hit)}
                            tabIndex={tabIndex}
                            {...rowHandlers(hitSel, hit, index, () => openHit(hit), hitDrag, hit.kind === "directory")}
                          >
                            <EntryCells
                              kind={hit.kind}
                              name={hit.name}
                              kindLabel={hit.kind === "directory" ? t("common.folder") : t("common.file")}
                              size={hit.size}
                              folder={hit.folder}
                            />
                          </button>
                        )}
                      />
                    ) : (
                      total !== null && (
                        <VirtualList
                          key={listingId}
                          total={total}
                          rowHeight={ROW_HEIGHT}
                          loadPage={loadPage}
                          label={t("browser.folderContents")}
                          head={
                            <>
                              <ListHead />
                              {path.length > 0 && <UpRow testId="up-row" onUp={() => folder.up()} />}
                            </>
                          }
                          hintId="entry-hint"
                          focusFirst={focusList}
                          scrollToIndex={revealer.index}
                          revealKey={reveal}
                          keyOf={rowKey}
                          onReveal={folderSel.ensure}
                          testId="entries"
                          onMarquee={folderSel.marquee}
                          onError={listError}
                          renderRow={(row: EntryRow, index: number, tabIndex: 0 | -1) => (
                            <button
                              type="button"
                              className={folderSel.has(row) ? "entry selected" : "entry"}
                              data-testid="entry"
                              data-kind={row.kind}
                              aria-pressed={folderSel.has(row)}
                              tabIndex={tabIndex}
                              {...rowHandlers(
                                folderSel,
                                row,
                                index,
                                () => {
                                  if (row.kind === "directory") navigate([...path, { key: row.key, name: row.name }], true);
                                },
                                folderDrag,
                                row.kind === "directory",
                              )}
                            >
                              <EntryCells
                                kind={row.kind}
                                name={row.name}
                                kindLabel={row.kind === "directory" ? t("common.folder") : t("common.file")}
                                size={
                                  row.kind === "directory"
                                    ? t("browser.itemsCount", {
                                        n: formatNumber(row.child_dirs + row.child_files),
                                      })
                                    : shortSize(row.size)
                                }
                              />
                            </button>
                          )}
                        />
                      )
                    )}
                  </div>
                  <aside className="info" aria-label={t("toolbar.info")} hidden={!infoOpen} data-testid="info-panel">
                    {details && <Details details={details} />}
                    <details className="technical" data-testid="archive-technical">
                      <summary>{t("browser.aboutArchive")}</summary>
                      <p data-testid="archive-technical-text">
                        {profileText(open.archive)}
                        {t("browser.metaSeparator")}
                        {t("browser.metaArchiveId")} <span className="hash">{open.archive.archive_id}</span>
                      </p>
                      <ul>
                        {open.archive.commits.map((c) => (
                          <li key={c.seq}>
                            {t("browser.commitTechnical", { seq: c.seq })}
                            {t("browser.metaSeparator")}
                            {c.checkpoint ? t("browser.checkpoint") : t("browser.delta")}
                          </li>
                        ))}
                      </ul>
                    </details>
                  </aside>
                </div>
                <ToolSlot name="count" owner="mochi">
                  <span
                    className="statusbar-text"
                    data-testid="entry-count"
                    title={
                      searching && search.state === "done" && search.detail !== undefined
                        ? search.detail
                        : t("browser.entriesInVersion", { n: formatNumber(open.snapshot.entries) })
                    }
                  >
                    {searching ? search.statusText : statusText}
                  </span>
                  {/* The selection's own actions, one click away (owner request 2026-11-10).
                      The same actions as the More menu, the list's menu and their keys, with
                      the same availability; a button that cannot act is off with its reason,
                      never hidden, so nothing moves under the pointer. */}
                  {selected.length > 0 && (
                    <div className="selection-bar" role="group" aria-label={t("selection.label")} data-testid="selection-bar">
                      <button
                        type="button"
                        className="selection-action"
                        data-testid="selection-extract"
                        title={tipFor(avail.extractTo, t("selection.extractTip"))}
                        disabled={!avail.extractTo.enabled}
                        onClick={() => setExtractToRequest((n) => n + 1)}
                      >
                        <ExtractToIcon />
                        <span>{t("extract.to")}</span>
                      </button>
                      <button
                        type="button"
                        className="selection-action"
                        data-testid="selection-remove"
                        title={tipFor(avail.remove, t("selection.removeTip"))}
                        disabled={!avail.remove.enabled}
                        onClick={() => setRemoveRequest((n) => n + 1)}
                      >
                        <TrashIcon />
                        <span>{t("write.remove")}</span>
                      </button>
                      <button
                        type="button"
                        className="selection-action"
                        data-testid="selection-info"
                        title={t("toolbar.infoTip")}
                        aria-pressed={infoOpen}
                        onClick={() => setInfoOpen((v) => !v)}
                      >
                        <InfoIcon />
                        <span>{t("toolbar.info")}</span>
                      </button>
                      <button
                        type="button"
                        className="icon-button selection-clear"
                        data-testid="selection-clear"
                        aria-label={t("selection.clear")}
                        title={t("selection.clear")}
                        onClick={() => listKeyHandlers.none()}
                      >
                        <CloseIcon />
                      </button>
                    </div>
                  )}
                </ToolSlot>
              </>
            )}
          </div>
        </div>
        <div className="statusbar">
          <SlotHost name="count" className="statusbar-count" />
          <SlotHost name="status" className="statusbar-status" />
          {error && (
            <div className="statusbar-error">
              <p role="alert" className="statusbar-error-text" data-testid="browse-error" title={error}>
                {error}
              </p>
              <button
                type="button"
                className="icon-button"
                aria-label={t("result.dismiss")}
                title={t("result.dismiss")}
                onClick={() => setError(null)}
              >
                <CloseIcon />
              </button>
            </div>
          )}
        </div>
        {foreign}
        {opening !== null && <OpeningNotice fileName={opening} />}
        {scanOpen && open && !foreignOpen && (
          <ScanArchive
            key={`${String(open.archive.handle)}:${String(open.snapshot.seq)}`}
            archive={open.archive}
            seq={open.snapshot.seq}
            onClose={() => setScanOpen(false)}
          />
        )}
        {archiveInfoOpen && open && !foreignOpen && (
          <ArchiveInfo
            key={`${String(open.archive.handle)}:${String(open.snapshot.seq)}`}
            archive={open.archive}
            seq={open.snapshot.seq}
            onClose={() => setArchiveInfoOpen(false)}
            onCommitted={(a) => void adopt(a)}
          />
        )}
        <ToolSheet tool={tool} onClose={() => setTool(null)} returnFocus={moreButtonRef}>
          {{
            help,
            passphrases: (
              <Rekey
                key={open?.archive.handle ?? "none"}
                archive={open?.archive ?? null}
                onChanged={(a) => {
                  if (a === null) void doClose();
                  else void adopt(a);
                }}
              />
            ),
            check: <Checker archive={open?.archive ?? null} testRequest={testRequest} />,
            find: <Finder active={tool === "find"} launchToken={findLaunch} onOpen={(h) => void openFound(h)} />,
            ...tools,
          }}
        </ToolSheet>
      </section>
    </ShellContext>
  );
}
