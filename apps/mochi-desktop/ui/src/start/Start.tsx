import { t } from "../i18n";
import type { RecentView } from "../ipc/start";
import { CloseIcon, FolderIcon, PlusIcon } from "../icons";

export interface StartProps {
  busy: boolean;
  recents: RecentView[];
  onCreate: () => void;
  onOpen: () => void;
  onOpenRecent: (index: number) => void;
  onForgetRecent: () => void;
}

/**
 * The empty window (D10): the mascot over a place to drop files, Create and
 * Open, and the recent archives as a short quiet list. What an archive is not
 * (a second copy) is in Help and in the drop zone's tooltip; the technical
 * tools are in the toolbar's More menu. The mascot is a CSS background on an
 * empty element, never an `img`; names from the recent list come from file
 * names on disk, so they are rendered as text only.
 */
export function Start({ busy, recents, onCreate, onOpen, onOpenRecent, onForgetRecent }: StartProps) {
  return (
    <div className="start" data-testid="start-screen">
      <div className="drop-zone" data-testid="start-drop-hint" title={t("start.dropHint")}>
        <span className="mascot-hero" aria-hidden="true" />
        <p className="drop-zone-title">{t("start.dropZone")}</p>
      </div>
      <div className="big-actions">
        <button type="button" className="big" data-testid="start-create" onClick={onCreate} disabled={busy}>
          <span className="big-title">
            <PlusIcon />
            {t("start.create")}
          </span>
        </button>
        <button type="button" className="big primary" data-testid="open-archive" onClick={onOpen} disabled={busy}>
          <span className="big-title">
            <FolderIcon />
            {t("start.open")}
          </span>
        </button>
      </div>
      {recents.length > 0 && (
        <section aria-labelledby="recent-heading" className="recent" data-testid="recent">
          <div className="recent-head">
            <h3 id="recent-heading">{t("start.recent")}</h3>
            <button
              type="button"
              className="icon-button"
              data-testid="recent-forget"
              aria-label={t("start.forgetRecent")}
              title={t("start.forgetRecent")}
              onClick={onForgetRecent}
              disabled={busy}
            >
              <CloseIcon />
            </button>
          </div>
          <ul className="recent-list">
            {recents.map((r) => (
              <li key={r.index}>
                <button
                  type="button"
                  className="recent-item"
                  data-testid="recent-open"
                  aria-label={t("start.recentOpen", { name: r.name })}
                  title={t("start.recentFolder", { folder: r.folder })}
                  onClick={() => onOpenRecent(r.index)}
                  disabled={busy}
                >
                  <span className="recent-name">{r.name}</span>
                  <span className="meta">{r.folder}</span>
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
