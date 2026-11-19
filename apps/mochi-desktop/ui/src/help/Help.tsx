import { useId, useState } from "react";

import { t } from "../i18n";
import { isIpcError } from "../ipc/jobs";
import { openUserGuide } from "../ipc/help";

type View = { state: "idle" } | { state: "requested" } | { state: "failed"; text: string };

/**
 * Help (audit F8), opened from the toolbar's "?". It holds what the main path
 * leaves out: what an archive is not (a second copy), the keys of the file
 * list (the list itself is described by a hidden copy of them), and the User guide button.
 * Rust opens the guide bundled with the installer, offline, in the system's
 * default program for web pages. The label and the note say that the guide is
 * for the `mochi` command line before it is opened; the guide's own pages say
 * it again in a banner and name the desktop equivalents.
 */
export function Help() {
  const [view, setView] = useState<View>({ state: "idle" });
  const noteId = useId();

  const open = () => {
    setView({ state: "idle" });
    openUserGuide().then(
      () => setView({ state: "requested" }),
      (e: unknown) =>
        setView({
          state: "failed",
          text: isIpcError(e)
            ? t("help.failed", { code: e.code, message: e.message })
            : t("help.failedPlain"),
        }),
    );
  };

  return (
    <section aria-labelledby="help-heading" className="help" data-testid="help">
      <h2 id="help-heading" tabIndex={-1}>
        {t("help.heading")}
      </h2>
      <p data-testid="start-note">{t("start.note")}</p>
      <p data-testid="help-keys">{t("help.keys")}</p>
      <p data-testid="help-shortcuts">{t("help.shortcuts")}</p>
      <div className="actions">
        <button type="button" data-testid="help-user-guide" aria-describedby={noteId} onClick={open}>
          {t("help.userGuide")}
        </button>
      </div>
      <p id={noteId} className="note">
        {t("help.userGuideNote")}
      </p>
      <p role="status" data-testid="help-status" data-state={view.state}>
        {view.state === "requested" && t("help.requested")}
        {view.state === "failed" && view.text}
      </p>
    </section>
  );
}
