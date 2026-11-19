// Updates (plan D7, T13). A failure is never styled as success; an unsigned
// installer is never called verified, trusted, safe, or secure; the update's own
// signature is what authenticates an update. Release notes and the detail of a
// failure come from the network: they are parameters or text nodes, never markup.
export const update = {
  "update.heading": "Updates",
  "update.intro":
    "MOCHI never checks for updates, and never downloads or installs one, unless you ask. A check is one request to GitHub (github.com/immerh8/MOCHI/releases) that carries no file names or archive contents.",
  "update.check": "Check for updates",
  "update.checking": "Checking for updates…",
  "update.startup": "Check for updates when MOCHI starts (off unless you turn it on)",
  "update.startupOn": "MOCHI will check once each time it starts. It still asks before downloading anything.",
  "update.startupOff": "MOCHI will not check for updates by itself.",
  "update.signing":
    "MOCHI's installers are not code-signed with a Windows publisher certificate, so Windows SmartScreen shows \"unknown publisher\" when you run one: choose More info, then Run anyway. An update is authenticated by MOCHI's own update signature, which this app checks before it installs anything; an update whose signature does not match is refused.",
  "update.loadFailed": "Could not read the update settings.",
  "update.saveFailed": "Could not save the update setting.",
  "update.version": "You are running version {current}.",
  "update.upToDate": "No newer version was found. You are running version {current}.",
  "update.available": "Version {version} is available. You are running version {current}.",
  "update.dialogLabel": "A new version is available",
  "update.later": "Not now",
  "update.install": "Download and install",
  "update.cancel": "Cancel download",
  "update.adminPrompt": "Ubuntu will ask for your password to install the package.",
  "update.closesApp": "On Windows, MOCHI closes when the installer starts.",
  "update.manual":
    "This copy of MOCHI was not installed from a MOCHI package, so it cannot update itself. Download the new version from:",
  "update.notesHeading": "Release notes",
  "update.notesNone": "The release has no notes.",
  "update.notesCut": "The notes are cut here; the release page has the rest.",
  "update.installingBytes": "Downloaded {size}",
  "update.installingOf": "Downloaded {size} of {total}",
  "update.cancelled": "Cancelled. The update was not installed.",
  "update.installedRestart": "Version {version} was installed. Close MOCHI and open it again to use it.",
  "update.installFailed": "The update was not installed: {message}",
  "update.failOffline":
    "Could not reach the update server. Check your connection and try again. Nothing was changed.",
  "update.failServer": "The update server did not give a usable answer. Nothing was changed. ({detail})",
  "update.failSignature": "The update's signature did not match, so it was refused. Nothing was installed.",
  "update.failInstall": "The update was downloaded but not installed. ({detail})",
  "update.failUnavailable": "Updates are not available in this build. ({detail})",
  "update.failOther": "The check for updates did not finish. ({detail})",
  "update.couldNotStart": "could not start",
  "update.unknownError": "unknown error",
} as const;
