import type { ReactNode } from "react";

import { Hint } from "../a11y/Hint";

/**
 * One setting (owner request 2026-11-12: Settings read as a wall of text): its name and a
 * one-line description on the left, its control on the right. A longer explanation goes in
 * `more`, a small "i" whose tooltip holds it (screen readers read it as a note).
 *
 * `descriptionId` lets the control name the description with `aria-describedby`, so a
 * screen reader reads both.
 */
export function SettingRow({
  title,
  titleFor,
  description,
  descriptionId,
  more,
  children,
  testId,
}: {
  title: ReactNode;
  /** The control's id, when the title should be its label. */
  titleFor?: string;
  description?: ReactNode;
  descriptionId?: string;
  more?: string | readonly string[];
  children: ReactNode;
  testId?: string;
}) {
  return (
    <div className="setting-row" data-testid={testId}>
      <div className="setting-text">
        <div className="setting-title">
          {titleFor === undefined ? <span>{title}</span> : <label htmlFor={titleFor}>{title}</label>}
          {more !== undefined && <Hint text={more} />}
        </div>
        {description !== undefined && (
          <p className="setting-description" id={descriptionId}>
            {description}
          </p>
        )}
      </div>
      <div className="setting-control">{children}</div>
    </div>
  );
}

/** A group of settings under a small heading, drawn as one card. */
export function SettingGroup({ heading, children }: { heading?: string; children: ReactNode }) {
  return (
    <div className="setting-group">
      {heading !== undefined && <h3 className="setting-group-heading">{heading}</h3>}
      <div className="setting-card">{children}</div>
    </div>
  );
}
