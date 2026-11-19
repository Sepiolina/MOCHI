import { t } from "../i18n";

export interface ProgressMeterProps {
  completed: number;
  total: number | null;
  /** The words that go with the bar (also its spoken value). */
  text: string;
  /** `data-testid` of the text. */
  textTestId?: string;
  /** `data-testid` of the `<progress>` element. */
  barTestId?: string;
}

/**
 * A progress bar with a name and a spoken value, and the same words beside it.
 * The text is deliberately not a live region: announcing every tick would
 * drown the screen reader. The start and the end of a job are announced
 * through the screen's own status region instead.
 */
export function ProgressMeter({ completed, total, text, textTestId, barTestId }: ProgressMeterProps) {
  return (
    <>
      <progress
        aria-label={t("common.progress")}
        aria-valuetext={text}
        value={total === null ? undefined : completed}
        max={total ?? undefined}
        data-testid={barTestId}
      />
      <span data-testid={textTestId}>{text}</span>
    </>
  );
}
