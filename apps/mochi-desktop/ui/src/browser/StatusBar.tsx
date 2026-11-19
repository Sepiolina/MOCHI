import type { ReactNode } from "react";

/**
 * The one line under a file list: how many items, and how many are selected,
 * on the left; the primary action on the right (drawn into it by the
 * component that owns the action). `title` holds the longer fact (the size of
 * the whole version) for a pointer.
 */
export function StatusBar({
  text,
  title,
  testId,
  children,
}: {
  text: string;
  title?: string;
  testId?: string;
  children?: ReactNode;
}) {
  return (
    <div className="statusbar">
      <p className="statusbar-text" data-testid={testId} title={title}>
        {text}
      </p>
      <div className="statusbar-actions">{children}</div>
    </div>
  );
}
