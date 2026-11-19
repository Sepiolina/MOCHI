// Help, User guide (audit F8). The guide is the command-line guide from docs/user, bundled
// with the installer and opened offline; the labels say so before it is opened.
export const help = {
  "help.heading": "Help",
  "help.toolbar": "Help",
  "help.keys":
    "In the list, the arrow keys move between entries, Page Up, Page Down, Home and End jump, Space selects, and Enter opens a folder.",
  "help.shortcuts":
    "Ctrl+O opens an archive, Ctrl+F searches its names, Ctrl+A selects the folder, Escape selects nothing, Backspace goes up, Alt+Left and Alt+Right go back and forward through the folders you opened, and Delete removes the selection. Ctrl or Shift with a click selects more than one entry. Drag entries out of the list into a folder to extract them; drop files on the window to add them. Ctrl+Shift+F finds names inside many archives under folders you choose. Ctrl+N starts a new archive, Ctrl+E extracts, Alt+Enter shows or hides details, and F1 opens this Help; these and the other actions are also in the More menu.",
  "help.userGuide": "User guide (command line)",
  "help.userGuideNote":
    "Written for the mochi command line, not for this app; its first lines point to the matching screens. Opens the copy installed with MOCHI in your default web browser. It needs no network.",
  "help.requested": "The user guide was handed to your default program for web pages.",
  "help.failed": "Could not open the user guide ({code}): {message}",
  "help.failedPlain": "Could not open the user guide.",
} as const;
