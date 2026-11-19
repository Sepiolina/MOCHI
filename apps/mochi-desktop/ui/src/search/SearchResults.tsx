import type { ReactNode } from "react";

import { ListHead } from "../browser/ListHead";
import { VirtualList } from "../browser/VirtualList";
import { t } from "../i18n";
import type { NameText } from "../ipc/browse";
import type { MarqueeEvent } from "../list/selection";

/** One match, as both browsers list it: selectable, openable, draggable. */
export type HitRow = {
  /** Selects, extracts, drags; `null` for an entry whose name was refused. */
  key: string | null;
  /** The last component of the path (escaped). */
  name: NameText;
  /** The whole path (escaped). */
  path: NameText;
  /** The folder it is in, as text (empty at the top). */
  folder: string;
  kind: "directory" | "file" | "link" | "special";
  /** What the Size column shows. */
  size: string;
};

export type SearchView<H extends HitRow = HitRow> =
  | { state: "idle"; statusText: string }
  | { state: "running"; statusText: string }
  | { state: "done"; hits: H[]; statusText: string; detail?: string }
  | { state: "failed"; text: string; statusText: string };

/**
 * The matches of the toolbar's search, in place of the folder: Name, Folder,
 * Size, Type. Names are text only, as in the folder list.
 */
export function SearchResults<H extends HitRow>({
  search,
  rowHeight,
  hintId,
  testId,
  renderRow,
  onMarquee,
}: {
  search: SearchView<H>;
  rowHeight: number;
  hintId: string;
  testId: string;
  renderRow: (hit: H, index: number, tabIndex: 0 | -1) => ReactNode;
  /** The rubber band's reports (the hits' selection). */
  onMarquee?: ((e: MarqueeEvent) => void) | undefined;
}) {
  if (search.state === "failed") {
    return (
      <p role="alert" className="list-empty error" data-testid="search-failed">
        {search.text}
      </p>
    );
  }
  if (search.state !== "done") {
    return (
      <p className="list-empty" data-testid="search-running">
        {t("search.searching")}
      </p>
    );
  }
  if (search.hits.length === 0) {
    return (
      <p className="list-empty" data-testid="search-none">
        {t("search.none")}
      </p>
    );
  }
  const hits = search.hits;
  return (
    <VirtualList
      key={hits.length === 0 ? "none" : `${hits.length}:${hits[0]?.path.text ?? ""}`}
      total={hits.length}
      rowHeight={rowHeight}
      loadPage={(offset, limit) => Promise.resolve(hits.slice(offset, offset + limit))}
      label={t("search.matches")}
      head={<ListHead folder />}
      hintId={hintId}
      testId={testId}
      onError={() => undefined}
      onMarquee={onMarquee}
      renderRow={renderRow}
    />
  );
}
