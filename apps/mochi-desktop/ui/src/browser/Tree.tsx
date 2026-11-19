import { useEffect, useState } from "react";

import { formatNumber, t } from "../i18n";
import { listChildren } from "../ipc/browse";
import type { EntryRow } from "../ipc/browse";
import { Name } from "./Name";

/** Subdirectories fetched per "show more". */
const TREE_PAGE = 200;

/** A directory on the path from the root: its key and its name. */
export type Crumb = Pick<EntryRow, "key" | "name">;

type Ctx = {
  handle: number;
  seq: number;
  current: string | null;
  onOpen: (path: Crumb[]) => void;
  onError: (e: unknown) => void;
};

/** Subdirectories of one directory, a page at a time. */
function Subdirs({ dir, ancestors, ctx }: { dir: string | null; ancestors: Crumb[]; ctx: Ctx }) {
  const [rows, setRows] = useState<EntryRow[]>([]);
  const [total, setTotal] = useState<number | null>(null);
  const { handle, seq, onError } = ctx;

  useEffect(() => {
    let live = true;
    listChildren(handle, seq, dir, true, 0, TREE_PAGE).then(
      (p) => {
        if (!live) return;
        setRows(p.entries);
        setTotal(p.total);
      },
      (e: unknown) => {
        if (live) onError(e);
      },
    );
    return () => {
      live = false;
    };
  }, [handle, seq, dir, onError]);

  const more = () => {
    listChildren(handle, seq, dir, true, rows.length, TREE_PAGE).then(
      (p) => setRows((prev) => [...prev, ...p.entries]),
      onError,
    );
  };

  if (total === null) return null;
  return (
    <ul>
      {rows.map((r) => (
        <TreeNode key={r.key} row={r} ancestors={ancestors} ctx={ctx} />
      ))}
      {rows.length < total && (
        <li>
          <button type="button" className="link" onClick={more}>
            {t("browser.showMoreFolders", { n: formatNumber(total - rows.length) })}
          </button>
        </li>
      )}
    </ul>
  );
}

// The folder tree is a nested list of buttons, not an ARIA `tree`: every
// control is an ordinary tab stop, so keyboard use needs no roving-focus
// widget contract. The expand button says which folder it opens and whether
// it is open (`aria-expanded`).
function TreeNode({ row, ancestors, ctx }: { row: EntryRow; ancestors: Crumb[]; ctx: Ctx }) {
  const [expanded, setExpanded] = useState(false);
  const path = [...ancestors, { key: row.key, name: row.name }];
  const hasDirs = row.child_dirs > 0;
  const current = ctx.current === row.key;
  return (
    <li>
      <span className="tree-row">
        <button
          type="button"
          className="twisty"
          aria-label={t(expanded ? "browser.collapseFolder" : "browser.expandFolder", { name: row.name.text })}
          aria-expanded={hasDirs ? expanded : undefined}
          disabled={!hasDirs}
          onClick={() => setExpanded((x) => !x)}
        >
          {hasDirs ? (expanded ? "▾" : "▸") : " "}
        </button>
        <button
          type="button"
          className={current ? "tree-label current" : "tree-label"}
          aria-current={current ? "true" : undefined}
          onClick={() => ctx.onOpen(path)}
        >
          <Name name={row.name} />
        </button>
      </span>
      {expanded && <Subdirs dir={row.key} ancestors={path} ctx={ctx} />}
    </li>
  );
}

/** The folder tree of one snapshot, loaded lazily (directories only). */
export function Tree(props: Ctx) {
  return (
    <nav aria-label={t("browser.folders")} className="tree" data-testid="tree">
      <button
        type="button"
        className={props.current === null ? "tree-label current" : "tree-label"}
        aria-current={props.current === null ? "true" : undefined}
        onClick={() => props.onOpen([])}
      >
        {t("browser.archiveRoot")}
      </button>
      <Subdirs dir={null} ancestors={[]} ctx={props} />
    </nav>
  );
}
