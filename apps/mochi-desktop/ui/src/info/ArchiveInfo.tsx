import { useEffect, useId, useRef, useState } from "react";
import type { ReactNode } from "react";

import { Panel } from "../a11y/Panel";
import { errorText, formatSize, shortSize } from "../browser/format";
import { formatNumber, t } from "../i18n";
import { archiveInfo } from "../ipc/browse";
import { startJob } from "../ipc/jobs";
import type { JobEvent } from "../ipc/jobs";
import type { ArchiveInfoView, ArchiveSummary } from "../ipc/browse";

/** Longest comment, in UTF-8 bytes (spec Annex B.2.19; Rust checks it too). */
export const MAX_COMMENT_BYTES = 16 * 1024;

/** A comment as it will be saved: Windows line ends become LF; empty means none. */
export function normalizeComment(text: string): string | null {
  const t = text.replace(/\r\n?/g, "\n");
  return t === "" ? null : t;
}

function utf8Length(text: string): number {
  return new TextEncoder().encode(text).length;
}

/** Packed size as a whole percentage of total size; `null` when nothing is stored. */
export function ratioPercent(packed: string, total: string): number | null {
  let p: bigint;
  let n: bigint;
  try {
    p = BigInt(packed);
    n = BigInt(total);
  } catch {
    return null;
  }
  if (n <= 0n) return null;
  // Rounded to the nearest whole percent, in integers (sizes may exceed 2^53).
  return Number((p * 200n + n) / (2n * n));
}

function count(decimal: string): string {
  try {
    return formatNumber(BigInt(decimal));
  } catch {
    return decimal;
  }
}

function Row({ label, value, tip, testId }: { label: string; value: ReactNode; tip?: string; testId: string }) {
  return (
    <>
      <dt title={tip}>{label}</dt>
      <dd data-testid={testId} title={tip}>
        {value}
      </dd>
    </>
  );
}

/**
 * The archive Info window (owner request 2026-11-14, after WinRAR's): one version's
 * folders, files, sizes and ratio, and what the writer recorded. Facts only: nothing
 * here says whether the archive is intact (that is Test), and a value the archive did
 * not record is said to be not recorded, never guessed.
 */
export function ArchiveInfo({
  archive,
  seq,
  onClose,
  onCommitted,
}: {
  archive: ArchiveSummary;
  seq: number;
  onClose: () => void;
  /** A comment was saved as a new commit: the archive as it now stands. */
  onCommitted?: (archive: ArchiveSummary) => void;
}) {
  const [view, setView] = useState<ArchiveInfoView | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    // Mounted once per archive and version (the caller keys it), so nothing to reset.
    let live = true;
    archiveInfo(archive.handle, seq).then(
      (v) => {
        if (live) setView(v);
      },
      (e: unknown) => {
        if (live) setError(errorText(t("info.loadFailed"), e));
      },
    );
    return () => {
      live = false;
    };
  }, [archive.handle, seq]);

  const ratio = view ? ratioPercent(view.packed_size, view.total_size) : null;
  const p = archive.profile;
  const head = archive.commits[archive.commits.length - 1]?.seq ?? seq;

  return (
    <Panel heading={t("info.heading")} testId="archive-info" className="info-dialog" onClose={onClose}>
      <div className="info-body">
        <div className="info-meter" aria-hidden="true">
          <div className="info-meter-track">
            <div
              className="info-meter-fill"
              style={{ height: `${String(Math.min(ratio ?? 0, 100))}%` }}
            />
          </div>
          <span className="info-meter-label">{ratio === null ? t("info.ratioNone") : t("info.ratioValue", { percent: ratio })}</span>
        </div>
        <div className="info-facts">
          <p className="info-kind">
            <strong>{t("info.kind")}</strong>
            <span className="info-name" title={archive.file_name}>
              {archive.file_name}
            </span>
          </p>
          {error !== null && (
            <p role="alert" className="error" data-testid="archive-info-error">
              {error}
            </p>
          )}
          {view === null && error === null && (
            <p className="note" role="status">
              {t("common.loading")}
            </p>
          )}
          {view !== null && (
            <>
              <dl className="info-list">
                <Row label={t("info.version")} value={t("info.versionValue", { seq: view.seq })} testId="info-version" />
                <Row
                  label={t("info.encryption")}
                  value={p.encrypted ? t("info.present") : t("info.absent")}
                  testId="info-encryption"
                />
                <Row
                  label={t("info.recovery")}
                  value={
                    p.redundancy_percent === null
                      ? t("info.absent")
                      : t("info.recoveryValue", { percent: p.redundancy_percent })
                  }
                  testId="info-recovery"
                />
              </dl>
              <hr />
              <dl className="info-list">
                <Row label={t("info.folders")} value={count(view.folders)} testId="info-folders" />
                <Row label={t("info.files")} value={count(view.files)} testId="info-files" />
                <Row label={t("info.totalSize")} value={shortSize(view.total_size)} tip={formatSize(view.total_size)} testId="info-total" />
                <Row
                  label={t("info.packedSize")}
                  value={shortSize(view.packed_size)}
                  tip={`${formatSize(view.packed_size)}\n${t("info.packedTip")}`}
                  testId="info-packed"
                />
                <Row
                  label={t("info.ratio")}
                  value={ratio === null ? t("info.ratioNone") : t("info.ratioValue", { percent: ratio })}
                  tip={t("info.ratioTip")}
                  testId="info-ratio"
                />
                <Row label={t("info.chunks")} value={count(view.chunks)} testId="info-chunks" />
                {view.file_size !== null && (
                  <Row
                    label={t("info.fileSize")}
                    value={shortSize(view.file_size)}
                    tip={`${formatSize(view.file_size)}\n${t("info.fileSizeTip")}`}
                    testId="info-file-size"
                  />
                )}
              </dl>
              <hr />
              <dl className="info-list">
                <Row
                  label={t("info.dictionary")}
                  value={
                    view.chunk_size === null
                      ? t("info.notRecorded")
                      : t("info.dictionaryValue", { size: shortSize(view.chunk_size) })
                  }
                  tip={t("info.dictionaryTip")}
                  testId="info-dictionary"
                />
                <Row
                  label={t("info.level")}
                  value={view.zstd_level === null ? t("info.notRecorded") : t("info.levelValue", { level: view.zstd_level })}
                  testId="info-level"
                />
              </dl>
              <hr />
              <CommentSection
                archive={archive}
                comment={view.comment}
                latest={seq === head}
                onCommitted={onCommitted}
              />
            </>
          )}
        </div>
      </div>
      <div className="actions dialog-actions">
        <button type="button" className="primary" data-testid="archive-info-ok" onClick={onClose}>
          {t("info.ok")}
        </button>
      </div>
    </Panel>
  );
}

/**
 * The archive comment (spec Annex B.2.19): the comment of the version on screen, shown as
 * text exactly as written (archive text: never markup). On the latest version of an archive
 * that is not encrypted it can be edited; saving adds a version, and earlier versions keep
 * the comment they had.
 */
function CommentSection({
  archive,
  comment,
  latest,
  onCommitted,
}: {
  archive: ArchiveSummary;
  comment: string | null;
  latest: boolean;
  onCommitted?: ((archive: ArchiveSummary) => void) | undefined;
}) {
  const id = useId();
  const [draft, setDraft] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const liveRef = useRef(true);
  useEffect(() => {
    liveRef.current = true;
    return () => {
      liveRef.current = false;
    };
  }, []);
  const encrypted = archive.profile.encrypted;
  const editable = latest && !encrypted && onCommitted !== undefined;
  const bytes = draft === null ? 0 : utf8Length(normalizeComment(draft) ?? "");
  const tooLong = bytes > MAX_COMMENT_BYTES;

  const onEvent = (e: JobEvent) => {
    if (!liveRef.current || e.event !== "finished") return;
    setSaving(false);
    const o = e.outcome;
    if (o.status === "completed" && o.commit?.archive) {
      setDraft(null);
      onCommitted?.(o.commit.archive);
    } else if (o.status === "failed") {
      setError(errorText(t("info.commentFailed"), o));
    } else if (o.status !== "cancelled") {
      setError(t("info.commentFailed"));
    }
  };

  const save = () => {
    if (draft === null || tooLong) return;
    setSaving(true);
    setError(null);
    startJob(
      { kind: "set_archive_comment", handle: archive.handle, comment: normalizeComment(draft) },
      onEvent,
    ).catch((e: unknown) => {
      if (!liveRef.current) return;
      setSaving(false);
      setError(errorText(t("info.commentFailed"), e));
    });
  };

  return (
    <section className="info-comment" aria-labelledby={`${id}-h`} data-testid="info-comment">
      <div className="info-comment-head">
        <h4 id={`${id}-h`}>{t("info.comment")}</h4>
        {editable && draft === null && (
          <button type="button" data-testid="info-comment-edit" onClick={() => setDraft(comment ?? "")}>
            {t("info.commentEdit")}
          </button>
        )}
      </div>
      {draft === null ? (
        <>
          {comment === null ? (
            <p className="note" data-testid="info-comment-text">
              {encrypted ? t("info.commentEncrypted") : t("info.commentEmpty")}
            </p>
          ) : (
            <div className="info-comment-text" data-testid="info-comment-text">
              {comment}
            </div>
          )}
          {!latest && !encrypted && <p className="note">{t("info.commentOlder")}</p>}
        </>
      ) : (
        <div className="info-comment-editor">
          <label htmlFor={`${id}-text`} className="sr-only">
            {t("info.commentLabel")}
          </label>
          <textarea
            id={`${id}-text`}
            data-testid="info-comment-input"
            rows={6}
            value={draft}
            disabled={saving}
            aria-describedby={`${id}-hint ${id}-count`}
            onChange={(e) => setDraft(e.target.value)}
          />
          <div className="info-comment-meta">
            <span id={`${id}-hint`} className="note">
              {t("info.commentHint")}
            </span>
            <span id={`${id}-count`} className={tooLong ? "error" : "note"} data-testid="info-comment-count">
              {tooLong ? t("info.commentTooLong") : t("info.commentBytes", { n: formatNumber(bytes) })}
            </span>
          </div>
          {saving && (
            <p role="status" className="note">
              {t("info.commentSaving")}
            </p>
          )}
          {error !== null && (
            <p role="alert" className="error" data-testid="info-comment-error">
              {error}
            </p>
          )}
          <div className="actions">
            <button type="button" disabled={saving} onClick={() => setDraft(null)}>
              {t("info.commentCancel")}
            </button>
            <button
              type="button"
              className="primary"
              data-testid="info-comment-save"
              disabled={saving || tooLong}
              onClick={save}
            >
              {t("info.commentSave")}
            </button>
          </div>
        </div>
      )}
    </section>
  );
}
