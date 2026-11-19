// Passphrases, encryption, the recovery record, rekeying, search coverage.
// Nothing here may say an archive is "safe", "secure", "backed up", or
// "preserved", or that a passphrase was "revoked" (spec §23.3; AGENTS.md).
export const crypt = {
  "crypt.lostPassphrase":
    "If you lose every passphrase for this archive, its contents are lost. There is no recovery: nothing in the file, in MOCHI, or in a repair can supply a passphrase. Keep the passphrase somewhere independent of the archive and of its copies.",
  "crypt.dedupLeak":
    "Identical content is stored once, also in an encrypted archive. Anyone who can compare the file before and after a change can learn whether the added content was already in it.",
  "crypt.hiddenWithKey": "Sealed: file names, attributes, file sizes, folder structure, and content.",
  "crypt.visible1": "The archive ID, the number of commits, and their parent links.",
  "crypt.visible2": "The number, kind, and sizes of stored objects, and whether a commit added data.",
  "crypt.visible3": "The key envelopes: their IDs and how costly it is to guess each passphrase.",
  "crypt.visible4": "Compressed sizes (content is compressed, then encrypted).",
  "crypt.notSigned":
    "The commit history is not signed. Someone who can rewrite the whole file can roll it back or withhold commits.",
  "crypt.noGenericTools":
    "Standard zstd and tar tools cannot unpack an encrypted archive, so it cannot also be TAR-compatible.",
  "crypt.wrongPassphrase": "The passphrase did not open this archive.",
  "crypt.wrongPassphraseNote":
    "This says nothing about the condition of the archive. Check the passphrase and try again.",
  "crypt.removeNotRevocation":
    "Removing a passphrase is not revocation. Anyone who knew it and kept a copy of the file can still open that copy, and the removed key envelope stays in this file's history. Only re-encrypting to a new file gives a new data key, and that does not recall copies already made.",
  "crypt.reencryptNote":
    "Re-encrypting writes a new file under a new data key and the new passphrase. The original file is not changed and still opens with its old passphrases; retire it yourself if you want it gone. Copies already made are not recalled.",
  "crypt.rekeyNeedsUnlock":
    "Open the archive with its passphrase first: changing passphrases needs the key.",
  "crypt.recovery1":
    "A recovery record is extra data that can rebuild bounded damage to this same file. It is not a second copy, and an archive that has one is not protected against losing the file.",
  "crypt.recovery2":
    "Within each group of stored data it can rebuild damaged blocks up to about the chosen percentage. Damage beyond that is reported and left as it is; nothing more is promised.",
  "crypt.recovery3": "It does not cover the newest commit's footer or the recovery data itself.",
  "crypt.recovery4": "The file grows by about the percentage you choose, and by more for small commits.",
  "crypt.recovery5": "It is fixed when the archive is created. Keep an independent copy as well.",
  "crypt.recoveryLabel": "{percent}% extra data",
  "crypt.rekeyAdded":
    "Committed commit {seq} ({status}): a passphrase was added to {file}. The data key and the data are unchanged.",
  "crypt.rekeyRemoved":
    "Committed commit {seq} ({status}): a passphrase was removed from {file}'s current key set. The data key and the data are unchanged.",
  "crypt.rekeyReencryptedOne":
    "Wrote {file}: {n} snapshot under a new data key and the new passphrase.",
  "crypt.rekeyReencryptedMany":
    "Wrote {file}: {n} snapshots under a new data key and the new passphrase.",
  "crypt.rekeyUnknownSeq": "?",
  "crypt.rekeyDefaultStatus": "LOCAL_COMMITTED",
  "crypt.rekeyDegraded": "Degraded: the directory entry is not confirmed durable ({note}).",
  "crypt.rekeyReopenError":
    "The archive could not be opened again with the key held ({code}): {message} Open it again with a passphrase that is still in its key set.",
  "crypt.rangeNone": "none",
  "crypt.rangeSeparator": ", ",
  "crypt.coverageSearchedOne":
    "Searched {searched} of {requested} requested snapshot (commits {ranges}); {examined} entries examined.",
  "crypt.coverageSearchedMany":
    "Searched {searched} of {requested} requested snapshots (commits {ranges}); {examined} entries examined.",
  "crypt.coverageComplete":
    "Coverage is complete for the snapshots requested: no matches means no matching names there.",
  "crypt.coveragePartial":
    "Coverage is PARTIAL. Some requested snapshots were not searched, so finding nothing is not proof that nothing matches.",
  "crypt.coverageUnavailable": "Not searched: commit {seq} ({code}): {message}",
  "crypt.coverageOpenedSeparately":
    "Commits {ranges} were opened one by one because the head's catalog was rebuilt from a snapshot manifest.",
  "crypt.coverageFromManifest":
    "The catalog image at the head was damaged; a snapshot manifest stood in for it.",
  "crypt.coverageContentNotSearched":
    "File contents were not searched, only names. Full-text search is not built.",
  // Passphrase form
  "crypt.enterPassphrase": "Enter a passphrase.",
  "crypt.passphrasesDiffer": "The two passphrases differ. Nothing was done.",
  "crypt.notDone": "Not done",
  "crypt.repeatIt": "Repeat it",
  // Rekey screen
  "rekey.heading": "Passphrases",
  "rekey.notEncrypted": "This archive is not encrypted, so it has no passphrases to change.",
  "rekey.openEncrypted": "Open an encrypted archive to change its passphrases.",
  "rekey.listFailed": "Could not list the key envelopes",
  "rekey.envelopes": "Key envelopes",
  "rekey.envelopeLine": " · written by commit {commit} · {kdf}",
  "rekey.envelopeOpened": " · opened this archive in this session",
  "rekey.add": "Add a passphrase…",
  "rekey.remove": "Remove a passphrase…",
  "rekey.removeLast": "The last passphrase cannot be removed",
  "rekey.reencrypt": "Re-encrypt to a new file…",
  "rekey.addGroup": "Add a passphrase",
  "rekey.newPassphrase": "New passphrase",
  "rekey.addSubmit": "Add passphrase",
  "rekey.removeGroup": "Remove a passphrase",
  "rekey.removeIntro":
    "Choose the key envelope to remove. If you do not know which one a passphrase belongs to, the one marked as opening this archive is the one you typed.",
  "rekey.openedThis": " (opened this archive)",
  "rekey.removeThis": "Remove this passphrase",
  "rekey.removeThisEnvelope": "Remove the passphrase of envelope {id}",
  "rekey.reencryptGroup": "Re-encrypt",
  "rekey.reencryptHeading": "Re-encrypt to a new file",
  "rekey.newFile": "New file: {path}",
  "rekey.newFilePassphrase": "Passphrase for the new file",
  "rekey.reencryptSubmit": "Write the new file",
  "rekey.working": "Working…",
  "rekey.cancelled": "Cancelled. Nothing was changed.",
  "rekey.failedNothing": "Nothing was changed.",
  "rekey.abortedHint": "Open the archive again to see what it holds.",
  "rekey.startFailed": "Could not start",
  "rekey.chooseFailed": "Could not choose a location",
  // Search
  "search.didNotRun": "The search did not run ({code}): {message}",
  "search.startFailed": "Could not start the search",
  "search.matches": "Matches",
  "search.label": "Search names in this archive (Ctrl+F)",
  "search.placeholder": "Search",
  "search.clear": "Clear search",
  "search.searching": "Searching…",
  "search.none": "No matches",
  "search.foundOne": "{n} match",
  "search.foundMany": "{n} matches",
  "search.firstShown": " (first {n} shown)",
  "search.partial": " · coverage PARTIAL",
} as const;
