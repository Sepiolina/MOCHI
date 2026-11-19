import type { ComponentPropsWithRef } from "react";

/**
 * A polite live region that is in the page before it has anything to say
 * (screen readers announce changes to a region that already exists far more
 * reliably than a region inserted together with its text). Use it for results
 * and progress summaries (plan D7 accessibility). Errors use `role="alert"`.
 */
export function Live({ children, ...rest }: ComponentPropsWithRef<"div">) {
  return (
    <div role="status" aria-live="polite" aria-atomic="true" {...rest}>
      {children}
    </div>
  );
}
