import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";

import { Panel } from "../a11y/Panel";
import { Hint } from "../a11y/Hint";
import { errorText } from "../browser/format";
import { formatNumber, t } from "../i18n";
import { extractFolderDialog } from "../ipc/dialogs";
import { browseExtractFolder, extractWizardSetup, previewExtractTo, setOpenFolderWhenDone } from "../ipc/start";
import type { ExtractToPreview, PreviewArchive } from "../ipc/start";

/** How long typing pauses before the folder is looked at again. */
const PREVIEW_DELAY_MS = 180;

/**
 * WinRAR's "Extract to..." window: an editable destination folder with
 * Browse..., the folders used lately (newest first), and a choice to extract
 * into a new folder named after the archive (on by default). Enter confirms.
 *
 * Owner request 2026-11-10: the folder starts at the Desktop; one that does not exist yet
 * is created on OK; a line under the field says, as the folder is typed, what will happen
 * (the folder exists, it will be created, or the new folder gets a number because the
 * archive's name is taken there); and "Open the folder when done" (the choice the compact
 * window shares) opens the folder in the file manager once the extraction ends.
 *
 * The folder text is the user's; Rust validates it (`folders::folder_to_extract_into`)
 * and `onSubmit` rejects with Rust's message if it will not do, which is shown
 * here and keeps the window open. The same window serves MOCHI and foreign
 * archives: the caller plans and extracts.
 */
export function ExtractToDialog({
  count,
  archive,
  onSubmit,
  onClose,
}: {
  /** Entries selected; 0 extracts everything. */
  count: number;
  /** The archive being extracted, for the preview (its name names the new folder). */
  archive: PreviewArchive | null;
  /** Plan and start with these choices; reject to keep the window open with the error. */
  onSubmit: (folder: string, newFolder: boolean) => Promise<void>;
  onClose: () => void;
}) {
  const [folder, setFolder] = useState("");
  const [recent, setRecent] = useState<string[]>([]);
  const [newFolder, setNewFolder] = useState(true);
  const [ready, setReady] = useState(false);
  const [working, setWorking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [openFolder, setOpenFolder] = useState(true);
  const [preview, setPreview] = useState<ExtractToPreview | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  // Set once the user has typed, chosen, or picked a folder: the starting folder, which may
  // arrive after that, never replaces what they put there.
  const editedRef = useRef(false);
  const choose = (value: string) => {
    editedRef.current = true;
    setFolder(value);
  };

  // The folder field starts with the Desktop, else the folder used last.
  useEffect(() => {
    let live = true;
    extractWizardSetup().then(
      (s) => {
        if (!live) return;
        if (!editedRef.current) setFolder(s.destination);
        setRecent(s.recent);
        setOpenFolder(s.open_folder);
        setReady(true);
      },
      (e: unknown) => {
        if (!live) return;
        setReady(true);
        setError(errorText(t("extract.toFailed"), e));
      },
    );
    return () => {
      live = false;
    };
  }, []);
  // Once it is filled in, the field takes focus with its text selected, so
  // typing replaces it and Enter accepts it.
  useEffect(() => {
    if (!ready) return;
    inputRef.current?.focus();
    inputRef.current?.select();
  }, [ready]);

  // What the folder would do, looked at again a moment after typing pauses. An answer
  // that comes back after the text changed again is dropped.
  // The archive is named by its kind and handle, not by the object the caller passes
  // (a new one on every render).
  const kind = archive?.kind ?? null;
  const handle = archive?.handle ?? null;
  useEffect(() => {
    if (!ready || kind === null || handle === null) return;
    let live = true;
    const timer = setTimeout(() => {
      previewExtractTo(folder, newFolder, { kind, handle }).then(
        (p) => {
          if (live) setPreview(p);
        },
        () => {
          if (live) setPreview(null);
        },
      );
    }, PREVIEW_DELAY_MS);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [ready, folder, newFolder, kind, handle]);

  const toggleOpenFolder = (open: boolean) => {
    setOpenFolder(open);
    setOpenFolderWhenDone(open).catch((e: unknown) => {
      setOpenFolder(!open);
      setError(errorText(t("extract.toFailed"), e));
    });
  };

  const browse = () => {
    setError(null);
    browseExtractFolder(folder, extractFolderDialog()).then(
      (picked) => {
        if (picked !== null) choose(picked);
      },
      (e: unknown) => setError(errorText(t("extract.toBrowseFailed"), e)),
    );
  };

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (working) return;
    setError(null);
    setWorking(true);
    onSubmit(folder, newFolder).then(
      () => undefined,
      (err: unknown) => {
        setWorking(false);
        setError(errorText(t("extract.toFailed"), err));
      },
    );
  };

  return (
    <Panel
      label={t("extract.toHeading")}
      heading={t("extract.toHeading")}
      testId="extract-to-dialog"
      fallbackId="extract-heading"
      onClose={working ? undefined : onClose}
    >
      <form onSubmit={submit} className="extract-to">
        <p className="note" data-testid="extract-to-what">
          {count === 0
            ? t("extract.toWhatAll")
            : count === 1
              ? t("extract.toWhatOne")
              : t("extract.toWhatMany", { n: formatNumber(count) })}
        </p>
        <label htmlFor="extract-to-folder">{t("extract.toFolder")}</label>
        <div className="extract-to-row">
          <input
            id="extract-to-folder"
            ref={inputRef}
            type="text"
            value={folder}
            data-testid="extract-to-folder"
            spellCheck={false}
            autoComplete="off"
            disabled={working}
            onChange={(e) => choose(e.target.value)}
          />
          <button type="button" data-testid="extract-to-browse" onClick={browse} disabled={working}>
            {t("extract.toBrowse")}
          </button>
        </div>
        <p
          className="note extract-to-preview"
          aria-live="polite"
          data-testid="extract-to-preview"
          data-state={preview?.state ?? "none"}
          title={preview?.state === "ready" ? preview.target : undefined}
        >
          {previewText(preview, newFolder)}
        </p>
        {recent.length > 0 && (
          <div className="extract-to-recent">
            <span id="extract-to-recent-label" className="meta">
              {t("extract.toRecent")}
            </span>
            <ul aria-labelledby="extract-to-recent-label" data-testid="extract-to-recent">
              {recent.map((r) => (
                <li key={r}>
                  <button
                    type="button"
                    className="link"
                    data-testid="extract-to-recent-item"
                    title={r}
                    disabled={working}
                    onClick={() => {
                      choose(r);
                      inputRef.current?.focus();
                    }}
                  >
                    {r}
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}
        <label className="extract-to-check">
          <input
            type="checkbox"
            checked={newFolder}
            data-testid="extract-to-new-folder"
            disabled={working}
            onChange={(e) => setNewFolder(e.target.checked)}
          />
          {t("extract.toNewFolder")}
        </label>
        <label className="extract-to-check">
          <input
            type="checkbox"
            checked={openFolder}
            data-testid="extract-to-open-folder"
            disabled={working}
            onChange={(e) => toggleOpenFolder(e.target.checked)}
          />
          {t("extract.toOpenFolder")}
        </label>
        {error && (
          <p role="alert" className="error" data-testid="extract-to-error">
            {error}
          </p>
        )}
        <div className="actions dialog-actions">
          <Hint testId="extract-to-note" text={t("extract.toNote")} />
          <button type="submit" className="primary" data-testid="extract-to-ok" disabled={working || !ready}>
            {t("extract.toOk")}
          </button>
          <button type="button" data-testid="extract-to-cancel" onClick={onClose} disabled={working}>
            {t("common.cancel")}
          </button>
        </div>
      </form>
    </Panel>
  );
}

/** The last part of a folder path as shown (either separator). */
function lastPart(path: string): string {
  const parts = path.split(/[\\/]/).filter((p) => p !== "");
  return parts.at(-1) ?? path;
}

/**
 * The line under the folder field: what OK will do with the folder typed. The field already
 * shows the path, so the line names only the folder that matters; the whole path is its tooltip.
 */
function previewText(p: ExtractToPreview | null, newFolder: boolean): string {
  if (p === null) return "";
  if (p.state === "invalid") return p.reason;
  const name = lastPart(p.target);
  if (p.taken !== null) return t("extract.toPreviewTaken", { name: p.taken, target: name });
  if (p.creates) return newFolder ? t("extract.toPreviewCreateWith", { target: name }) : t("extract.toPreviewCreate");
  return newFolder ? t("extract.toPreviewNew", { target: name }) : t("extract.toPreviewInto");
}
