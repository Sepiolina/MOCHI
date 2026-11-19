// The start screen and the plain wording of the main path (D10). Technical
// terms (snapshot, commit, hash) stay in the "Technical details" areas and in
// help text; the main path says "version", "files", and "folder".
export const start = {
  "start.heading": "What would you like to do?",
  "start.create": "Create archive",
  "start.open": "Open archive",
  "start.dropZone": "Drop files or an archive here",
  "start.settings": "Settings",
  "start.dropHint": "Files and folders go into a new archive; an archive opens.",
  "start.note":
    "An archive packs files into one file. It is not a second copy of them: keep an independent copy of anything you cannot lose.",
  "start.recent": "Recent archives",
  "start.recentOpen": "Open {name}",
  "start.recentFolder": "in {folder}",
  "start.forgetRecent": "Clear this list",
  "start.openRecentFailed": "Could not open that archive",
  "start.openDroppedFailed": "Could not open the dropped archive",
  "start.extractOpen": "Open and extract…",
  "start.showInFolder": "Show in folder",
  "start.showInFolderFailed": "Could not show the folder",
  "start.technical": "Technical details",
} as const;
