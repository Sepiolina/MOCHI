// The words of the native dialogs (plan D7 i18n). Rust shows the dialogs, so
// the webview only passes it short labels in the language the window is
// showing; Rust checks them (length, no control characters) and keeps its
// English default for anything absent or refused. The file extensions a
// dialog matches are Rust's and never cross.
import { t } from "../i18n";
import type { ArchiveFormat } from "./generated/ArchiveFormat";
import type { DialogText } from "./generated/DialogText";

export type { DialogText };

/** Open any archive MOCHI reads. */
export const openArchiveDialog = (): DialogText => ({
  title: t("dialog.openArchive"),
  filter: t("dialog.filterArchives"),
  mochi_archive: t("dialog.filterMochi"),
  all_files: t("dialog.filterAll"),
});

/** Open a foreign archive (read-only). */
export const openForeignDialog = (): DialogText => ({ title: t("dialog.openForeign") });

/** Choose files or a folder to add. */
export const sourcesDialog = (folder: boolean): DialogText => ({
  title: t(folder ? "dialog.chooseFolder" : "dialog.chooseFiles"),
});

/** Save a new archive of `format`. */
export function destinationDialog(format: ArchiveFormat): DialogText {
  const filter = {
    mochi: t("dialog.filterMochi"),
    zip: t("dialog.filterZip"),
    tar: t("dialog.filterTar"),
    tar_gz: t("dialog.filterTarGz"),
    tar_zst: t("dialog.filterTarZst"),
  }[format];
  return { title: t("dialog.saveArchiveAs"), filter };
}

/** Choose the folder to extract into. */
export const extractFolderDialog = (): DialogText => ({ title: t("dialog.extractTo") });

/** Choose where the folder a quick extraction made goes. */
export const moveFolderDialog = (): DialogText => ({ title: t("dialog.moveFolder") });

/** Choose the archive to test or repair. */
export const checkSubjectDialog = (): DialogText => ({
  title: t("dialog.chooseSubject"),
  mochi_archive: t("dialog.filterMochi"),
  all_files: t("dialog.filterAll"),
});

/** Choose an independent copy for a repair. */
export const repairCopyDialog = (): DialogText => ({
  title: t("dialog.chooseCopy"),
  mochi_archive: t("dialog.filterMochi"),
  all_files: t("dialog.filterAll"),
});

/** Save the new archive a repair writes. */
export const repairOutputDialog = (): DialogText => ({
  title: t("dialog.saveArchiveAs"),
  mochi_archive: t("dialog.filterMochi"),
});

/** Save an exported report. */
export const reportDialog = (): DialogText => ({
  title: t("dialog.exportReport"),
  filter: t("dialog.filterJson"),
});

/** Save an exported crash log. */
export const crashLogDialog = (): DialogText => ({
  title: t("dialog.exportCrashLog"),
  filter: t("dialog.filterText"),
});
