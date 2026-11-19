// Which actions the window offers for the archive on screen and the selection.
// One pure function decides it; the toolbar buttons, the More menu, the
// right-click menu of both file lists, and the Delete key all read its answer,
// so a button and a menu item can never disagree. An action that cannot work is
// shown disabled (never hidden) with the reason as its tooltip.
import { t } from "../i18n";

/** What is on screen: nothing, a MOCHI archive, or a ZIP/7z/RAR/tar archive (read-only). */
export type ArchiveKind = "none" | "mochi" | "foreign";

export interface ArchiveContext {
  kind: ArchiveKind;
  /**
   * A MOCHI archive: the snapshot on screen is the latest. An older version is
   * a read-only view; changes are made to the latest only.
   */
  viewingHead?: boolean;
  /** A MOCHI archive is encrypted (and, being open, unlocked). */
  encrypted?: boolean;
  /**
   * An encrypted archive is waiting for its passphrase: nothing is open yet, so
   * every archive action is off, and the reason says why.
   */
  locked?: boolean;
  /** A job of this window is running (each action that starts one waits). */
  busy?: boolean;
}

export interface SelectionContext {
  /** Entries selected now. */
  selected: number;
  /** Entries in the list on screen. */
  total: number;
}

export type ActionId =
  | "newArchive"
  | "add"
  | "remove"
  | "extract"
  | "extractTo"
  | "test"
  | "copyNames"
  | "selectAll"
  | "info"
  | "search"
  | "passphrases";

/** Why an action is off. */
export type Reason =
  | "noArchive"
  | "locked"
  | "readOnly"
  | "oldVersion"
  | "noSelection"
  | "emptyList"
  | "busy"
  | "notEncrypted";

export interface Availability {
  enabled: boolean;
  /** `null` when enabled. */
  reason: Reason | null;
}

export type AvailableActions = Record<ActionId, Availability>;

const ON: Availability = { enabled: true, reason: null };

/** The first reason that applies wins; with none, the action is on. */
function first(...reasons: (Reason | null)[]): Availability {
  const reason = reasons.find((r) => r !== null) ?? null;
  return reason === null ? ON : { enabled: false, reason };
}

/**
 * The actions available for `archive` and `selection`.
 *
 * - Anything that reads an archive needs one open (`noArchive`, or `locked`
 *   while the passphrase is awaited).
 * - A foreign archive is read-only: Add and Remove are off (`readOnly`). So is
 *   an older version of a MOCHI archive (`oldVersion`).
 * - Actions on the selection (Extract selected, Remove, Copy names) need one.
 * - Starting a job is off while one runs (`busy`); the components that own a
 *   job pass it, the rest of the window does not know about their jobs.
 * - Repair is not listed: it chooses its own archive and works with none open.
 * - Passphrases are for an open encrypted MOCHI archive only.
 */
export function availableActions(archive: ArchiveContext, selection: SelectionContext): AvailableActions {
  const isMochi = archive.kind === "mochi";
  const closed: Reason | null =
    archive.kind !== "none" ? null : archive.locked === true ? "locked" : "noArchive";
  const busy: Reason | null = archive.busy === true ? "busy" : null;
  const none: Reason | null = selection.selected > 0 ? null : "noSelection";
  const readOnly: Reason | null =
    archive.kind === "foreign" ? "readOnly" : isMochi && archive.viewingHead !== true ? "oldVersion" : null;
  return {
    newArchive: first(busy),
    add: first(closed, readOnly, busy),
    remove: first(closed, readOnly, none, busy),
    extract: first(closed, busy),
    extractTo: first(closed, busy),
    test: first(closed, busy),
    copyNames: first(closed, none),
    selectAll: first(closed, selection.total > 0 ? null : "emptyList"),
    info: first(closed),
    search: first(closed),
    passphrases: first(
      closed ?? (isMochi ? null : "noArchive"),
      isMochi && archive.encrypted !== true ? "notEncrypted" : null,
      busy,
    ),
  };
}

/** The tooltip for a reason, from the message catalogue. */
export function reasonText(reason: Reason): string {
  switch (reason) {
    case "noArchive":
      return t("avail.noArchive");
    case "locked":
      return t("avail.locked");
    case "readOnly":
      return t("toolbar.readOnly");
    case "oldVersion":
      return t("write.onlyLatest");
    case "noSelection":
      return t("avail.noSelection");
    case "emptyList":
      return t("avail.emptyList");
    case "busy":
      return t("avail.busy");
    case "notEncrypted":
      return t("avail.notEncrypted");
  }
}

/** The tooltip of an action: its own text when on, the reason when off. */
export function tipFor(a: Availability, on: string): string {
  return a.reason === null ? on : reasonText(a.reason);
}
