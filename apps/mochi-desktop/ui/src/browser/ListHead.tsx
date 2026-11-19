import type { ReactNode } from "react";

import { CaretUpIcon, FileTypeIcon } from "../icons";
import { t } from "../i18n";
import type { NameText } from "../ipc/browse";
import { iconKindOf, typeLabelOf } from "./fileKind";
import { Name } from "./Name";

/**
 * The header of a file list: Name, Size, Type. It is for the eye only (the
 * list's rows say what each part is), so it is hidden from assistive
 * technology, and it sits outside the scrolling rows so it stays in view. The
 * listing arrives folders first, then files, each in name order, which is what
 * the caret shows; nothing here sorts.
 */
export function ListHead({ folder = false }: { folder?: boolean }) {
  return (
    <div className={folder ? "table-head with-folder" : "table-head"} aria-hidden="true">
      <span className="col-name">
        {t("browser.colName")}
        {!folder && <CaretUpIcon />}
      </span>
      {folder && <span className="col-folder">{t("browser.colFolder")}</span>}
      <span className="col-size">{t("browser.colSize")}</span>
      <span className="col-type">{t("browser.colType")}</span>
    </div>
  );
}

/**
 * The parts of one list row, in the columns of `ListHead`: a file-type icon,
 * the name (as text), a size or item count, and the type. `kindLabel` names the
 * kind for assistive technology; the icon is decorative. The name is only ever
 * rendered as text, and the icon and the type come from a fixed lookup.
 */
export function EntryCells({
  kind,
  name,
  kindLabel,
  size,
  folder,
  after,
}: {
  kind: "directory" | "file" | "link" | "special";
  name: NameText;
  kindLabel: string;
  size: string;
  /** The folder a search hit is in (escaped text), for the results' Folder column. */
  folder?: string;
  after?: ReactNode;
}) {
  return (
    <>
      <span className="kind" aria-label={kindLabel}>
        <FileTypeIcon kind={iconKindOf(kind, name.text)} />
      </span>
      <span className="entry-name">
        <Name name={name} />
        {after}
      </span>
      {folder !== undefined && (
        <span className="folder" title={folder}>
          {folder === "" ? "/" : folder}
        </span>
      )}
      <span className="size">{size}</span>
      <span className="type">{typeLabelOf(kind, name.text)}</span>
    </>
  );
}
