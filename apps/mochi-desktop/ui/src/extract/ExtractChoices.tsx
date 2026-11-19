import { useId } from "react";
import type { ReactNode } from "react";

import { formatNumber, t } from "../i18n";
import type { ConflictChoice } from "../ipc/generated/ConflictChoice";
import type { ConflictWhy } from "../ipc/generated/ConflictWhy";
import type { ExtractPlan } from "../ipc/generated/ExtractPlan";
import type { NameText } from "../ipc/generated/NameText";

/**
 * What to do when the folder already has some of the names (owner decision 2026-11-11), for a
 * MOCHI archive and a ZIP/7z/RAR/tar alike: a title that says what is going on, one line of
 * explanation, the names as a plain list, and the choices as buttons.
 *
 * - **Add only new files** (the default, Enter): folders that are there are added to, files that
 *   are there keep their name and bytes. `add_new`.
 * - **Replace**: files that are there go to the Recycle Bin / Trash (so the user can get them
 *   back) and the archive's take their names. `replace`. Never a folder, never a link.
 * - **Extract to a new folder**: everything into a new folder named after the archive, inside
 *   this one; nothing that is there is touched.
 * - What cannot be extracted here whatever is chosen (a name this computer does not allow,
 *   something of another kind in the way) is listed under a closed disclosure, in plain words.
 *
 * With nothing in the way it is a plain confirmation ("Extract"). Archive names are text only.
 */
export function ExtractChoices({
  plan,
  busy,
  testIdPrefix,
  onChoose,
  onNewFolder,
  onChangeFolder,
  onCancel,
  hint,
}: {
  plan: ExtractPlan;
  busy: boolean;
  /** "extract" (a MOCHI archive) or "foreign": the test ids' prefix. */
  testIdPrefix: "extract" | "foreign";
  /** Start the extraction with this choice. */
  onChoose: (choice: ConflictChoice) => void;
  /** Extract into a new folder named after the archive, inside this one, instead. */
  onNewFolder: (() => void) | null;
  onChangeFolder: (() => void) | null;
  onCancel: () => void;
  /** The quiet "i" note at the left of the buttons, if any. */
  hint?: ReactNode;
}) {
  const titleId = useId();
  const id = (s: string) => `${testIdPrefix}-${s}`;
  // The ids the specs already use for the plain confirmation and Cancel.
  const confirmId = testIdPrefix === "foreign" ? "foreign-extract-confirm" : "extract-confirm";
  const closeId = testIdPrefix === "foreign" ? "foreign-plan-close" : "extract-close";
  const folder = lastPart(plan.destination);
  const existing = plan.existing_total;
  const cannot = plan.conflicts_total;
  const firstExisting = plan.existing[0];

  const title =
    existing === 1 && firstExisting
      ? t("choice.titleExistingOne", { name: lastPart(firstExisting.text), folder })
      : existing > 1
        ? t("choice.titleExistingMany", { n: formatNumber(existing), folder })
        : cannot > 0
          ? t("choice.titleCannot", { folder })
          : t("choice.titleReady", { folder });

  // With nothing in the way: one plain confirmation. Folders that are there are added to.
  const plainChoice: ConflictChoice = plan.merged_folders > 0 || cannot > 0 ? "add_new" : "stop";

  return (
    <div className="choices" role="group" aria-labelledby={titleId} data-testid={id("choices")}>
      <p id={titleId} className="choices-title" data-testid={id("choices-title")} title={plan.destination}>
        {title}
      </p>
      {existing > 0 && (
        <p className="choices-sub">{onWindows() ? t("choice.subRecycleBin") : t("choice.subTrash")}</p>
      )}

      {existing > 0 && (
        <ul className="choices-list" aria-label={t("choice.existingList")} data-testid={id("existing")}>
          {plan.existing.map((n) => (
            <li key={n.text} data-testid={id("existing-item")} title={n.text}>
              <PathName name={n} />
            </li>
          ))}
          {existing > plan.existing.length && (
            <li className="choices-more">
              {t("common.andMore", { n: formatNumber(existing - plan.existing.length) })}
            </li>
          )}
        </ul>
      )}

      {plan.merged_folders > 0 && (
        <p className="choices-note" data-testid={id("merged-note")}>
          {plan.merged_folders === 1
            ? t("choice.mergedOne")
            : t("choice.mergedMany", { n: formatNumber(plan.merged_folders) })}
        </p>
      )}

      {cannot > 0 ? (
        <details className="choices-cannot" data-testid={id("conflicts")}>
          <summary>
            {cannot === 1 ? t("choice.cannotOne") : t("choice.cannotMany", { n: formatNumber(cannot) })}
          </summary>
          <ul aria-label={t("choice.cannotList")}>
            {plan.conflicts.map((c) => (
              <li key={`${c.code}:${c.path.text}`} data-testid={id("conflict")} title={c.reason}>
                <PathName name={c.path} />
                <span className="choices-why">{shortReason(c.why, c.reason)}</span>
              </li>
            ))}
            {cannot > plan.conflicts.length && (
              <li className="choices-more">
                {t("common.andMore", { n: formatNumber(cannot - plan.conflicts.length) })}
              </li>
            )}
          </ul>
        </details>
      ) : (
        <span className="sr-only" data-testid={id("no-conflicts")}>
          {t("extract.noConflicts")}
        </span>
      )}

      {/* Going elsewhere instead: quiet links, so the buttons stay three on one row. */}
      {((existing > 0 && onNewFolder) || onChangeFolder) && (
        <p className="choices-elsewhere">
          {existing > 0 && onNewFolder && (
            <button type="button" className="link" data-testid={id("new-folder")} onClick={onNewFolder} disabled={busy}>
              {t("choice.newFolder")}
            </button>
          )}
          {existing > 0 && onNewFolder && onChangeFolder && (
            <span className="choices-dot" aria-hidden="true">
              ·
            </span>
          )}
          {onChangeFolder && (
            <button type="button" className="link" data-testid={id("change-folder")} onClick={onChangeFolder} disabled={busy}>
              {t("extract.changeFolder")}
            </button>
          )}
        </p>
      )}

      <div className="actions dialog-actions choices-actions">
        {hint}
        <button type="button" data-testid={closeId} onClick={onCancel} disabled={busy}>
          {t("common.cancel")}
        </button>
        {existing > 0 && (
          <button
            type="button"
            className="destructive"
            data-testid={id("replace")}
            title={onWindows() ? t("choice.replaceTipRecycleBin") : t("choice.replaceTipTrash")}
            onClick={() => onChoose("replace")}
            disabled={busy}
          >
            {t("choice.replace")}
          </button>
        )}
        <button
          type="button"
          className="primary"
          data-testid={existing > 0 ? id("add-new") : confirmId}
          // Enter takes the safe choice.
          autoFocus
          onClick={() => onChoose(existing > 0 ? "add_new" : plainChoice)}
          disabled={busy}
        >
          {existing > 0 ? t("choice.addNew") : cannot > 0 ? t("choice.extractRest") : t("extract.confirm")}
        </button>
      </div>
    </div>
  );
}

/** An archive path with its folder part quieter than its name. Text only. */
function PathName({ name }: { name: NameText }) {
  const cut = name.text.lastIndexOf("/");
  if (cut < 0) return <span className="choices-name">{name.text}</span>;
  return (
    <>
      <span className="choices-dir">{name.text.slice(0, cut + 1)}</span>
      <span className="choices-name">{name.text.slice(cut + 1)}</span>
    </>
  );
}

/** The last part of a path, by either separator. */
function lastPart(path: string): string {
  const parts = path.split(/[\\/]/).filter((p) => p !== "");
  return parts.at(-1) ?? path;
}

/** The engine's reason in a few plain words; its full text is the row's tooltip. */
function shortReason(why: ConflictWhy, full: string): string {
  switch (why) {
    case "name_not_allowed":
      return t("choice.reasonName");
    case "other_kind":
      return t("choice.reasonInTheWay");
    case "folder_not_entered":
      return t("choice.reasonFolderNotEntered");
    case "same_name_in_archive":
      return t("choice.reasonSameName");
    case "other":
      return full;
  }
}

/** The Bin is called the Recycle Bin on Windows and the Trash on Ubuntu. */
function onWindows(): boolean {
  return typeof navigator !== "undefined" && /Windows/i.test(navigator.userAgent);
}
