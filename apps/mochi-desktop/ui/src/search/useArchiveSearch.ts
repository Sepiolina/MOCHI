import { useEffect, useRef, useState } from "react";

import { errorText, shortSize } from "../browser/format";
import { formatNumber, t, tn } from "../i18n";
import { coverageLines } from "../crypt/messages";
import { cancelJob, startJob } from "../ipc/jobs";
import type { HitView } from "../ipc/crypt";
import { splitPath } from "../list/keys";
import type { HitRow, SearchView } from "./SearchResults";

/** No search: one value, so that effects keyed on the view do not run again. */
export const IDLE: { state: "idle"; statusText: string } = { state: "idle", statusText: "" };
export const running = (): { state: "running"; statusText: string } => ({ state: "running", statusText: t("search.searching") });

/** A MOCHI search hit as a list row. */
export function hitRow(h: HitView): HitRow {
  const { name, folder } = splitPath(h.path);
  return {
    key: h.key,
    name,
    path: h.path,
    folder,
    kind: h.kind,
    size: h.kind === "directory" ? "" : shortSize(h.size),
  };
}

/** "12 found", with the cut and a partial coverage said (spec §23.3 #8). */
export function foundText(shown: number, total: number, complete: boolean): string {
  return (
    tn(total, "search.foundOne", "search.foundMany", { n: formatNumber(total) }) +
    (total > shown ? t("search.firstShown", { n: formatNumber(shown) }) : "") +
    (complete ? "" : t("search.partial"))
  );
}

/**
 * Names in the snapshot on screen that contain `query` (spec §19: names only,
 * never file contents), through the search job. A new query cancels the
 * search before it; an empty one, or no archive, is no search.
 */
export function useArchiveSearch(handle: number | null, seq: number | null, query: string): SearchView {
  const [found, setFound] = useState<{ id: string; view: SearchView } | null>(null);
  const jobRef = useRef<number | null>(null);
  const id = `${handle ?? ""}:${seq ?? ""}:${query}`;

  useEffect(() => {
    if (handle === null || seq === null || query === "") return;
    let live = true;
    const done = (view: SearchView) => {
      if (live) setFound({ id: `${handle}:${seq}:${query}`, view });
    };
    const failed = (text: string) => done({ state: "failed", text, statusText: text });
    startJob(
      { kind: "search_archive", handle, query, ignore_case: true, scope: { scope: "commit", seq }, entry_kind: null },
      (e) => {
        if (e.event === "started") {
          jobRef.current = e.job_id;
          return;
        }
        if (e.event !== "finished") return;
        jobRef.current = null;
        const o = e.outcome;
        if (o.status === "completed" && o.search) {
          const r = o.search;
          done({
            state: "done",
            hits: r.hits.map(hitRow),
            statusText: foundText(r.hits.length, r.total_hits, r.coverage.complete),
            // What was searched and what was not (spec §19.3, §23.3 #8): the line's tooltip.
            detail: coverageLines(r.coverage).join("\n"),
          });
        } else if (o.status === "failed") {
          failed(t("search.didNotRun", { code: o.code, message: o.message }));
        } else if (o.status === "aborted") {
          failed(t("common.stoppedUnexpectedly", { message: o.message }));
        }
      },
    ).catch((err: unknown) => failed(errorText(t("search.startFailed"), err)));
    return () => {
      live = false;
      const job = jobRef.current;
      jobRef.current = null;
      if (job !== null) void cancelJob(job).catch(() => false);
    };
  }, [handle, seq, query]);

  if (handle === null || seq === null || query === "") return IDLE;
  return found?.id === id ? found.view : running();
}
