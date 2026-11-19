import "@testing-library/jest-dom/vitest";
// Imported here so that Testing Library registers its own `afterEach(cleanup)`
// before ours; hooks run last-registered first, so the audit below sees the
// screen as the test left it, before cleanup removes it.
import "@testing-library/react";
import { afterEach } from "vitest";

import { audit } from "./audit";

// Plan D7: every screen any test ends on must pass the structural audit
// (accessible names, references, headings, roles). Tests that render broken
// markup on purpose (audit.test.tsx) clean up first.
afterEach(() => {
  const problems = audit(document.body);
  if (problems.length > 0) {
    throw new Error(`Accessibility problems on the screen this test ended on:\n  ${problems.join("\n  ")}`);
  }
});
