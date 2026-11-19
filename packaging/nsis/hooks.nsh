; NSIS installer hooks for MOCHI (plan D6, decision O10). Tauri includes this file
; into its generated installer (bundle.windows.nsis.installerHooks) and calls the
; NSIS_HOOK_* macros below. Everything is written under HKCU: the installer is
; per-user (installMode currentUser) and never elevates.
;
; What is registered, and nothing else (the uninstall hook removes each of it):
;
;   .mochi          the association itself comes from bundle.fileAssociations
;                   (Tauri's APP_ASSOCIATE; ProgID "MOCHI.Archive"). Its leftovers
;                   are cleaned here, because Tauri's APP_UNASSOCIATE leaves the
;                   ".mochi" key and a "<ProgID>_backup" value behind.
;   Install key     Tauri's HKCU\Software\<publisher>\<product> install-location
;                   key, which Tauri's own uninstaller keeps unless the user
;                   ticks "delete application data" (removed here, not on update).
;   Explorer menu  flat, no "MOCHI" cascade (owner decision 2026-10-20, amending the
;                   cascade of 2026-10-16), in the classic context menu, which Windows 11
;                   shows under "Show more options" (the top-level Windows 11 menu needs
;                   an IExplorerCommand handler in a SIGNED sparse package, and there is
;                   no code-signing certificate: 1.x, O10). Two parts:
;                     1. A shell extension, crates/mochi-shellext (a classic IContextMenu
;                        COM DLL, in-process, registered per user): on every file and
;                        folder, "Add to archive..." (--add) and a label that changes with
;                        the selection, Add to "<Name>.mochi" (--add-now; <Name> is what
;                        the app will name the archive, WinRAR style); and, when the
;                        selection holds a folder, "Find in MOCHI archives" (--find, given
;                        the folders only; also alone on a folder's background). Registered
;                        as HKCU\Software\Classes\CLSID\{...}\InprocServer32 (ThreadingModel
;                        Apartment) and ContextMenuHandlers\MOCHI under "*", "Directory"
;                        and "Directory\Background".
;                        The DLL starts the app with the paths of the whole selection in
;                        ONE process; it parses nothing and writes nothing itself.
;                     2. Three static verbs on .mochi files only (flat, next to the above):
;                        Open with MOCHI / Extract here / Extract to a new folder, one
;                        process per item as before.
;                   The app parses and validates every argument in Rust
;                   (src-tauri/src/launch.rs). A path is never joined into a shell string;
;                   "--" ends the options so that a file named like an option stays a
;                   path. No command line carries a passphrase.
;                   (Earlier versions registered four flat verbs, then the cascade
;                   "MochiMenu"; both are removed on install and uninstall.)
;   The DLL file    Explorer keeps a loaded shell extension locked, so the DLL is
;                   installed under a VERSIONED name, mochi-shellext-<version>.dll, and
;                   the registration points at that name. An update installs the new
;                   file beside the locked old one; anything of an older version, or a
;                   same-name leftover, is deleted if it can be, and otherwise renamed
;                   aside (a loaded DLL may be renamed, not deleted) and the renamed
;                   file is scheduled for deletion at the next restart. The scheduled
;                   name is never one that a later install uses, so a restart cannot
;                   delete the new DLL. Until Explorer restarts (or the user signs out)
;                   an already-running Explorer keeps the old DLL, which launches the
;                   same app with the same arguments.
;   Opt-in          ZIP, 7z, RAR, tar, tar.gz/tgz (and so .gz, Windows keys on the
;                   last extension): offered under "Open with" for those types
;                   only if the user says yes (a question at install time, default
;                   NO; /ASSOCIATE_FOREIGN on the command line says yes, silent
;                   installs say no). It never writes a type's default value and
;                   never touches the user's choice; Windows lets only the user
;                   make MOCHI the default (Settings > Default apps).
;   The CLI         The `mochi` command-line program is installed as $INSTDIR\bin\mochi.exe
;                   (a resource; packaging/build-installer.sh). That folder, which holds
;                   only it, is appended to the user's PATH (HKCU\Environment, value
;                   "Path"), so a new terminal finds `mochi`. The install directory itself
;                   is never put on PATH: it also holds uninstall.exe. An uninstall removes
;                   exactly that entry and nothing else; an update keeps it.
;                   NSIS strings hold NSIS_MAX_STRLEN - 1 characters (1023 in the standard
;                   build Tauri uses). For a longer value ReadRegStr returns "" with the
;                   error flag set, exactly as for a missing one: treating that as "no
;                   Path yet" would REPLACE the user's whole PATH. So a Path that exists
;                   but cannot be read whole, or would not fit with the entry added, is
;                   never written: the CLI is still installed, and the installer log says
;                   PATH was left alone.
;   CLI path        Where the CLI is, for programs that call it (they need not rely on
;                   PATH, which a running program does not see change):
;                   HKCU\Software\Microsoft\Windows\CurrentVersion\App Paths\mochi.exe,
;                   default value the full path of mochi.exe, "Path" its folder. App Paths is
;                   Windows' own registration of a program's location (it also lets Win+R
;                   start `mochi`). Per user, no elevation. An uninstall removes the key; an
;                   update rewrites it.
;
; Windows Server runners cannot show Explorer's menu; the keys are checked by
; packaging/smoke-nsis.ps1 and the menu itself is on the release checklist
; (docs/packaging.md).

!define MOCHI_ASSOC_PROGID "MOCHI.Archive"
!define MOCHI_FOREIGN_PROGID "MOCHI.Foreign"
!define MOCHI_CLASSES "Software\Classes"

!define MOCHI_SHELLEXT_CLSID "{508772cd-d924-4b90-8b87-37fd5d593932}"
!define MOCHI_SHELLEXT_DLL "mochi-shellext-${VERSION}.dll"

; One static verb: its key under the given Classes path, the menu text, the argument.
!macro MochiWriteItem SHELLKEY ITEM LABEL OPTION
  WriteRegStr HKCU "${MOCHI_CLASSES}\${SHELLKEY}\shell\${ITEM}" "" "${LABEL}"
  WriteRegStr HKCU "${MOCHI_CLASSES}\${SHELLKEY}\shell\${ITEM}" "Icon" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\",0"
  WriteRegStr HKCU "${MOCHI_CLASSES}\${SHELLKEY}\shell\${ITEM}\command" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" ${OPTION} -- $\"%1$\""
!macroend

!macro MochiDeleteVerb SHELLKEY VERB
  DeleteRegKey HKCU "${MOCHI_CLASSES}\${SHELLKEY}\shell\${VERB}"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\${SHELLKEY}\shell"
!macroend

; What earlier versions registered and the current one does not: the four flat verbs
; of 0.1.0 on every file and folder, and the cascade "MochiMenu" of 0.9.x.
!macro MochiDeleteLegacyVerbs
  !insertmacro MochiDeleteVerb "*" "MochiAdd"
  !insertmacro MochiDeleteVerb "Directory" "MochiAdd"
  !insertmacro MochiDeleteVerb "SystemFileAssociations\.mochi" "MochiMenu"
  !insertmacro MochiDeleteVerb "*" "MochiMenu"
  !insertmacro MochiDeleteVerb "Directory" "MochiMenu"
!macroend

; The shell extension's registration (per user, no elevation).
!macro MochiRegisterShellExt
  WriteRegStr HKCU "${MOCHI_CLASSES}\CLSID\${MOCHI_SHELLEXT_CLSID}" "" "${PRODUCTNAME} Explorer menu"
  WriteRegStr HKCU "${MOCHI_CLASSES}\CLSID\${MOCHI_SHELLEXT_CLSID}\InprocServer32" "" "$INSTDIR\${MOCHI_SHELLEXT_DLL}"
  WriteRegStr HKCU "${MOCHI_CLASSES}\CLSID\${MOCHI_SHELLEXT_CLSID}\InprocServer32" "ThreadingModel" "Apartment"
  WriteRegStr HKCU "${MOCHI_CLASSES}\*\shellex\ContextMenuHandlers\MOCHI" "" "${MOCHI_SHELLEXT_CLSID}"
  WriteRegStr HKCU "${MOCHI_CLASSES}\Directory\shellex\ContextMenuHandlers\MOCHI" "" "${MOCHI_SHELLEXT_CLSID}"
  ; The empty space of an open folder: only "Find in MOCHI archives" is offered there.
  WriteRegStr HKCU "${MOCHI_CLASSES}\Directory\Background\shellex\ContextMenuHandlers\MOCHI" "" "${MOCHI_SHELLEXT_CLSID}"
!macroend

!macro MochiUnregisterShellExt
  DeleteRegKey HKCU "${MOCHI_CLASSES}\*\shellex\ContextMenuHandlers\MOCHI"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\*\shellex\ContextMenuHandlers"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\*\shellex"
  DeleteRegKey HKCU "${MOCHI_CLASSES}\Directory\shellex\ContextMenuHandlers\MOCHI"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\Directory\shellex\ContextMenuHandlers"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\Directory\shellex"
  DeleteRegKey HKCU "${MOCHI_CLASSES}\Directory\Background\shellex\ContextMenuHandlers\MOCHI"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\Directory\Background\shellex\ContextMenuHandlers"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\Directory\Background\shellex"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\Directory\Background"
  DeleteRegKey HKCU "${MOCHI_CLASSES}\CLSID\${MOCHI_SHELLEXT_CLSID}"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\CLSID"
!macroend

; Delete a file; if it is locked (a DLL Explorer has loaded), rename it aside to the
; first free <name>.staleN and schedule that name for deletion at the next restart.
; Uses $R3. Never schedules the original name, which a later install may use again.
!macro MochiRetireFile PATH
  ; One id per expansion: __LINE__ differs on every line of a macro, so each
  ; label would get a different suffix from the jumps that name it.
  !define MOCHI_RID ${__LINE__}
  Delete "${PATH}"
  IfFileExists "${PATH}" 0 mochi_retired_${MOCHI_RID}
  StrCpy $R3 1
  mochi_try_${MOCHI_RID}:
  IfFileExists "${PATH}.stale$R3" 0 mochi_rename_${MOCHI_RID}
  IntOp $R3 $R3 + 1
  IntCmp $R3 20 mochi_retired_${MOCHI_RID} mochi_try_${MOCHI_RID} mochi_retired_${MOCHI_RID}
  mochi_rename_${MOCHI_RID}:
  Rename "${PATH}" "${PATH}.stale$R3"
  Delete /REBOOTOK "${PATH}.stale$R3"
  mochi_retired_${MOCHI_RID}:
  !undef MOCHI_RID
!macroend

; Retire every mochi-shellext-*.dll in $INSTDIR except KEEP (empty: all). Uses $R3, $R5-$R6.
!macro MochiSweepShellExt KEEP
  !define MOCHI_SID ${__LINE__}
  FindFirst $R5 $R6 "$INSTDIR\mochi-shellext-*.dll"
  mochi_sweep_loop_${MOCHI_SID}:
  StrCmp $R6 "" mochi_sweep_done_${MOCHI_SID}
  StrCmp $R6 "${KEEP}" mochi_sweep_next_${MOCHI_SID}
  !insertmacro MochiRetireFile "$INSTDIR\$R6"
  mochi_sweep_next_${MOCHI_SID}:
  FindNext $R5 $R6
  Goto mochi_sweep_loop_${MOCHI_SID}
  mochi_sweep_done_${MOCHI_SID}:
  FindClose $R5
  !undef MOCHI_SID
!macroend

; Offer MOCHI under "Open with" for one extension (never the default value).
!macro MochiOfferFor EXT
  WriteRegStr HKCU "${MOCHI_CLASSES}\.${EXT}\OpenWithProgids" "${MOCHI_FOREIGN_PROGID}" ""
!macroend

!macro MochiWithdrawFor EXT
  DeleteRegValue HKCU "${MOCHI_CLASSES}\.${EXT}\OpenWithProgids" "${MOCHI_FOREIGN_PROGID}"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\.${EXT}\OpenWithProgids"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\.${EXT}"
!macroend

!macro MochiRegisterForeign
  WriteRegStr HKCU "${MOCHI_CLASSES}\${MOCHI_FOREIGN_PROGID}" "" "Archive (opened with MOCHI)"
  WriteRegStr HKCU "${MOCHI_CLASSES}\${MOCHI_FOREIGN_PROGID}\DefaultIcon" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\",0"
  WriteRegStr HKCU "${MOCHI_CLASSES}\${MOCHI_FOREIGN_PROGID}\shell" "" "open"
  WriteRegStr HKCU "${MOCHI_CLASSES}\${MOCHI_FOREIGN_PROGID}\shell\open" "" "Open with ${PRODUCTNAME}"
  WriteRegStr HKCU "${MOCHI_CLASSES}\${MOCHI_FOREIGN_PROGID}\shell\open\command" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" --open -- $\"%1$\""
  !insertmacro MochiOfferFor "zip"
  !insertmacro MochiOfferFor "7z"
  !insertmacro MochiOfferFor "rar"
  !insertmacro MochiOfferFor "tar"
  !insertmacro MochiOfferFor "tgz"
  !insertmacro MochiOfferFor "gz"
!macroend

!macro MochiUnregisterForeign
  !insertmacro MochiWithdrawFor "zip"
  !insertmacro MochiWithdrawFor "7z"
  !insertmacro MochiWithdrawFor "rar"
  !insertmacro MochiWithdrawFor "tar"
  !insertmacro MochiWithdrawFor "tgz"
  !insertmacro MochiWithdrawFor "gz"
  DeleteRegKey HKCU "${MOCHI_CLASSES}\${MOCHI_FOREIGN_PROGID}"
!macroend

!define MOCHI_CLI_DIR "$INSTDIR\bin"
!define MOCHI_APP_PATHS "Software\Microsoft\Windows\CurrentVersion\App Paths\mochi.exe"

; The user's Path into $R0, and in $R5 whether it could be read: "ok", "absent" (no such
; value), or "unreadable" (it exists but ReadRegStr failed: too long for an NSIS string,
; or not a string type). Never treat "unreadable" as empty. Uses $R0, $R5, $R6.
!macro MochiReadPath
  ClearErrors
  ReadRegStr $R0 HKCU "Environment" "Path"
  ${IfNot} ${Errors}
    StrCpy $R5 "ok"
  ${Else}
    StrCpy $R5 "absent"
    StrCpy $R6 0
    ${Do}
      ClearErrors
      EnumRegValue $R0 HKCU "Environment" $R6
      ${If} ${Errors}
      ${OrIf} $R0 == ""
        ${ExitDo}
      ${EndIf}
      ${If} $R0 == "Path"
        StrCpy $R5 "unreadable"
        ${ExitDo}
      ${EndIf}
      IntOp $R6 $R6 + 1
    ${Loop}
    StrCpy $R0 ""
  ${EndIf}
!macroend

; Append MOCHI_CLI_DIR to the user's Path unless it is there already. Uses $R0-$R6.
!macro MochiPathAdd
  !insertmacro MochiReadPath
  StrLen $R1 $R0
  StrLen $R2 "${MOCHI_CLI_DIR}"
  IntOp $R1 $R1 + $R2
  IntOp $R1 $R1 + 2
  ${If} $R5 == "unreadable"
  ${OrIf} $R1 >= ${NSIS_MAX_STRLEN}
    DetailPrint "PATH left unchanged: it is too long to edit safely. Add ${MOCHI_CLI_DIR} yourself to use mochi in a terminal."
  ${Else}
    ${WordReplace} ";$R0;" ";${MOCHI_CLI_DIR};" ";" "+" $R3
    ${If} $R3 == ";$R0;"
      ${If} $R0 == ""
        StrCpy $R0 "${MOCHI_CLI_DIR}"
      ${Else}
        StrCpy $R4 $R0 1 -1
        ${If} $R4 == ";"
          StrCpy $R0 $R0 -1
        ${EndIf}
        StrCpy $R0 "$R0;${MOCHI_CLI_DIR}"
      ${EndIf}
      WriteRegExpandStr HKCU "Environment" "Path" $R0
      SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
    ${EndIf}
  ${EndIf}
!macroend

; Remove every MOCHI_CLI_DIR entry from the user's Path, and nothing else. Uses $R0-$R6.
!macro MochiPathRemove
  !insertmacro MochiReadPath
  StrLen $R1 $R0
  IntOp $R1 $R1 + 2
  ${If} $R5 == "unreadable"
  ${OrIf} $R1 >= ${NSIS_MAX_STRLEN}
    DetailPrint "PATH left unchanged: it is too long to edit safely. Remove ${MOCHI_CLI_DIR} yourself if it is there."
  ${ElseIf} $R5 == "ok"
    StrCpy $R3 ";$R0;"
    ${Do}
      ${WordReplace} "$R3" ";${MOCHI_CLI_DIR};" ";" "+" $R4
      ${If} $R4 == $R3
        ${ExitDo}
      ${EndIf}
      StrCpy $R3 $R4
    ${Loop}
    ${If} $R3 != ";$R0;"
      StrCpy $R3 $R3 "" 1
      StrCpy $R3 $R3 -1
      ${If} $R3 == ""
        DeleteRegValue HKCU "Environment" "Path"
      ${Else}
        WriteRegExpandStr HKCU "Environment" "Path" $R3
      ${EndIf}
      SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  ; Same-version reinstall: the DLL may be loaded by Explorer and cannot be overwritten,
  ; but it can be renamed. Move it aside before the files are written.
  ${If} ${FileExists} "$INSTDIR\${MOCHI_SHELLEXT_DLL}"
    !insertmacro MochiRetireFile "$INSTDIR\${MOCHI_SHELLEXT_DLL}"
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Explorer menu: the shell extension (Add to archive..., Add to "<Name>.mochi") on
  ; every file and folder, three flat verbs on .mochi files; earlier layouts removed.
  !insertmacro MochiDeleteLegacyVerbs
  !insertmacro MochiRegisterShellExt
  !insertmacro MochiWriteItem "SystemFileAssociations\.mochi" "MochiOpen" "Open with ${PRODUCTNAME}" "--open"
  !insertmacro MochiWriteItem "SystemFileAssociations\.mochi" "MochiExtractHere" "Extract here" "--extract-here"
  !insertmacro MochiWriteItem "SystemFileAssociations\.mochi" "MochiExtractToFolder" "Extract to a new folder" "--extract-to-folder"
  ; Older versions of the DLL (locked ones are renamed aside and deleted at restart).
  !insertmacro MochiSweepShellExt "${MOCHI_SHELLEXT_DLL}"
  ; The CLI's folder on the user's PATH (an update finds it there already), and its path
  ; registered where programs can look it up.
  !insertmacro MochiPathAdd
  WriteRegStr HKCU "${MOCHI_APP_PATHS}" "" "${MOCHI_CLI_DIR}\mochi.exe"
  WriteRegStr HKCU "${MOCHI_APP_PATHS}" "Path" "${MOCHI_CLI_DIR}"

  ; The opt-in. An update keeps whatever the user chose at the first install.
  ${If} $UpdateMode <> 1
    StrCpy $R9 0
    ${GetOptions} $CMDLINE "/ASSOCIATE_FOREIGN" $R0
    ${IfNot} ${Errors}
      StrCpy $R9 1
    ${ElseIf} $PassiveMode <> 1
      ; Silent installs (/S) take the default, NO, without showing this.
      MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "Also offer ${PRODUCTNAME} in the $\"Open with$\" list for ZIP, 7z, RAR, TAR, TGZ and GZ files?$\r$\n$\r$\nThis does not make ${PRODUCTNAME} the default program for them; you can still choose that yourself in Windows Settings. You can undo this by uninstalling ${PRODUCTNAME}." /SD IDNO IDNO mochi_skip_foreign
      StrCpy $R9 1
      mochi_skip_foreign:
    ${EndIf}
    ${If} $R9 = 1
      !insertmacro MochiRegisterForeign
    ${EndIf}
  ${EndIf}
  !insertmacro UPDATEFILEASSOC
!macroend

!macro NSIS_HOOK_PREUNINSTALL
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  !insertmacro MochiDeleteLegacyVerbs
  !insertmacro MochiUnregisterShellExt
  !insertmacro MochiDeleteVerb "SystemFileAssociations\.mochi" "MochiOpen"
  !insertmacro MochiDeleteVerb "SystemFileAssociations\.mochi" "MochiExtractHere"
  !insertmacro MochiDeleteVerb "SystemFileAssociations\.mochi" "MochiExtractToFolder"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\SystemFileAssociations\.mochi"
  ; The DLL itself (Tauri's uninstaller cannot delete it while Explorer has it loaded).
  !insertmacro MochiSweepShellExt ""
  ; Parent keys this created (never removed if anything else lives in them).
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\SystemFileAssociations"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\*"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\Directory\Background"
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\Directory"

  ; Tauri's APP_UNASSOCIATE restores the previous default (empty if none) and
  ; leaves "<ProgID>_backup" behind: remove what is ours.
  DeleteRegValue HKCU "${MOCHI_CLASSES}\.mochi" "${MOCHI_ASSOC_PROGID}_backup"
  ReadRegStr $R0 HKCU "${MOCHI_CLASSES}\.mochi" ""
  ${If} $R0 == ""
    DeleteRegValue HKCU "${MOCHI_CLASSES}\.mochi" ""
  ${EndIf}
  DeleteRegKey /ifempty HKCU "${MOCHI_CLASSES}\.mochi"

  ${If} $UpdateMode <> 1
    !insertmacro MochiPathRemove
    DeleteRegKey HKCU "${MOCHI_APP_PATHS}"
    !insertmacro MochiUnregisterForeign
    ; Tauri's install-location key (written at install, ${MANUPRODUCTKEY}; with
    ; installMode currentUser that is HKCU). Its uninstaller removes it only when
    ; "delete application data" is ticked. An update (/UPDATE) keeps it: the next
    ; install reads it to find the folder. A reinstall writes it again.
    DeleteRegKey HKCU "${MANUPRODUCTKEY}"
    DeleteRegKey /ifempty HKCU "${MANUKEY}"
  ${EndIf}
  !insertmacro UPDATEFILEASSOC
!macroend
