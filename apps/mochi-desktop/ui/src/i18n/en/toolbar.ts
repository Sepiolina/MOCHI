// The toolbar and its menu. Labels are one or two words; the longer
// explanation is the button's tooltip.
export const toolbar = {
  "toolbar.label": "Actions",
  "toolbar.test": "Test",
  "toolbar.testTip": "Test this archive",
  "toolbar.readOnly": "Archives in other formats are read-only",
  "toolbar.info": "Details",
  "toolbar.infoTip": "Show or hide details (Alt+Enter)",
  "toolbar.openFirst": "Open an archive first",
  "toolbar.openTip": "Open an archive (Ctrl+O)",
  "toolbar.more": "More",
  "toolbar.repair": "Repair",
  "toolbar.foreign": "Other formats…",
  "toolbar.helpTip": "Help and keyboard keys (F1)",
  "toolbar.find": "Find",
  "toolbar.findTip": "Find names inside many archives (Ctrl+Shift+F)",
  // The window's own title bar (release 0.9.10) and its buttons.
  "window.controls": "Window",
  "window.minimize": "Minimize",
  "window.maximize": "Maximize",
  "window.restore": "Restore",
  "window.close": "Close",
  // Why a button or menu item is off (shell/availability.ts); the item stays visible.
  "avail.noArchive": "Open an archive first",
  "avail.locked": "Enter the archive's passphrase first",
  "avail.noSelection": "Select an entry first",
  "avail.emptyList": "There is nothing here to select",
  "avail.busy": "Wait for the current task to finish",
  "avail.notEncrypted": "Only an encrypted MOCHI archive has passphrases",
} as const;
