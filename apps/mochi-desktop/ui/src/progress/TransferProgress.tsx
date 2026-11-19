import type { ReactNode } from "react";

import { t } from "../i18n";
import { CloseIcon, ErrorIcon, PauseIcon, PlayIcon, RefreshIcon, TransferIcon } from "../icons";
import { transferView } from "./messages";
import type { RateState } from "./rate";

export type TransferState = "starting" | "running" | "paused" | "cancelling" | "error";

export interface TransferTestIds {
  root: string;
  /** The size line, and the spoken value of the bar. */
  text: string;
  pause: string;
  /** Not needed when `onCancel` is not given (the compact window has its own Cancel button). */
  cancel?: string;
  retry?: string;
}

export interface TransferProgressProps {
  /** The header while running ("Creating archive…"). Paused and error states have their own. */
  title: string;
  /** A line of text under the percentage; names in it come from the user or an archive. */
  subtitle?: ReactNode;
  phase: string;
  completed: number;
  total: number | null;
  rate: RateState;
  /** The size line ("296.7 MiB of 528.9 MiB"); also the bar's spoken value. */
  text: string;
  state: TransferState;
  /** Set in the error state: the whole red line. */
  errorText?: string | undefined;
  onPause?: () => void;
  onResume?: () => void;
  onCancel?: () => void;
  /** Offers "Try again" in the error state. */
  onRetry?: (() => void) | undefined;
  testIds: TransferTestIds;
}

/**
 * The card a running job shows, after the Windows file-transfer dialog: a header, how much is
 * done, a thick bar with Pause / Resume and Cancel, how much is left and how fast. The bar is a
 * `<progress>` with a name and a spoken value; the text is deliberately not a live region (the
 * screen's own status region announces the start and the end). Colour never carries the state
 * alone: the header, the time line and the error line say it in words. Nothing here is green,
 * since nothing here is a verification (spec §23.3).
 */
export function TransferProgress({
  title,
  subtitle,
  phase,
  completed,
  total,
  rate,
  text,
  state,
  errorText,
  onPause,
  onResume,
  onCancel,
  onRetry,
  testIds,
}: TransferProgressProps) {
  const view = transferView(rate, phase, completed, total);
  const paused = state === "paused";
  const failed = state === "error";
  const header = failed ? t("transfer.titleError") : paused ? t("transfer.titlePaused") : title;
  const timeLine = failed ? null : paused ? t("transfer.paused") : view.timeLine;
  const label = paused ? t("transfer.resume") : t("transfer.pause");
  const idle = state === "starting" || state === "cancelling";
  return (
    <div className="transfer" data-state={state} data-testid={testIds.root}>
      <p className="transfer-title">
        <TransferIcon />
        <span>{header}</span>
      </p>
      {view.heading !== null && <p className="transfer-percent">{view.heading}</p>}
      {subtitle !== undefined && <p className="transfer-subtitle">{subtitle}</p>}
      <div className="transfer-bar">
        <progress
          aria-label={t("common.progress")}
          aria-valuetext={text}
          value={view.fraction === null ? undefined : view.fraction}
          max={view.fraction === null ? undefined : 1}
        />
        {!failed && (
          <>
            <button
              type="button"
              className="icon-button transfer-pause"
              data-testid={testIds.pause}
              aria-label={label}
              title={label}
              onClick={paused ? onResume : onPause}
              disabled={idle}
            >
              {paused ? <PlayIcon /> : <PauseIcon />}
            </button>
            {onCancel && (
              <button
                type="button"
                className="icon-button"
                data-testid={testIds.cancel}
                aria-label={t("common.cancel")}
                title={t("common.cancel")}
                onClick={onCancel}
                disabled={idle}
              >
                <CloseIcon />
              </button>
            )}
          </>
        )}
      </div>
      <p className="transfer-size" data-testid={testIds.text}>
        {text}
      </p>
      {timeLine !== null && <p className="transfer-time">{timeLine}</p>}
      {failed && errorText !== undefined && (
        <p className="transfer-error">
          <ErrorIcon />
          <span>{errorText}</span>
        </p>
      )}
      {failed && onRetry && testIds.retry !== undefined && (
        <button type="button" className="transfer-retry" data-testid={testIds.retry} onClick={onRetry}>
          <RefreshIcon />
          <span>{t("transfer.retry")}</span>
        </button>
      )}
    </div>
  );
}
