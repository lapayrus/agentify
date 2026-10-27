; NSIS hooks for Agentify's per-user installer.
;
; Tauri includes this file via `bundle.windows.nsis.installerHooks`. The
; installer itself does one thing beyond copying files: it stops the daemon that
; is being replaced before a sidecar is overwritten. The daemon's lifetime
; follows the app (ADR-0006), so no logon item is registered; a task left by
; v0.1.x is removed during the upgrade and again on uninstall.
;
; Nothing here elevates. The installer runs as the user (installMode
; "currentUser"), so no UAC prompt appears.
;
; The data root is never touched: `$LOCALAPPDATA\Agentify\` is resolved by the
; binaries themselves, which is what lets an install-over keep Projects and state.
;
; That is also why the program does not go where Tauri's per-user default puts it.
; `installMode: "currentUser"` defaults `$INSTDIR` to `$LOCALAPPDATA\Agentify` —
; the same directory as the data root — so an uninstall, or an install-over that
; clears the program directory, would take `state.db`, `projects\`, the rootfs
; cache, and the discovery file with it. Per-user programs live under
; `%LOCALAPPDATA%\Programs`, so the default is moved there and the two directories
; stay siblings. The hook only moves the *default*: a directory the user chose is
; left alone.

!macro NSIS_HOOK_PREINSTALL
  ${If} $INSTDIR == "$LOCALAPPDATA\${PRODUCTNAME}"
    StrCpy $INSTDIR "$LOCALAPPDATA\Programs\${PRODUCTNAME}"
    DetailPrint "Agentify installs to $INSTDIR; its data root stays in $LOCALAPPDATA\${PRODUCTNAME}"
    SetOutPath $INSTDIR
  ${EndIf}

  ; Stop the daemon of the version being replaced before a single sidecar is
  ; overwritten: Windows refuses to replace an executable that is still running.
  ; This release's CLI drains the daemon and answers 2 when an Operation is in
  ; flight, so the install is refused rather than killing work. An installation
  ; from an older build has a CLI without `daemon stop`; that case falls back to
  ; terminating the image, because the old build cannot report in-flight work.
  DetailPrint "Stopping a running Agentify daemon"
  ExecWait '"$INSTDIR\agentify.exe" daemon stop' $0
  DetailPrint "agentify daemon stop exited with $0"
  ${If} $0 == 2
    MessageBox MB_OK|MB_ICONEXCLAMATION "An Agentify operation is in flight. Let it finish, then run the installer again."
    Abort
  ${ElseIf} $0 != 0
    ExecWait 'taskkill /F /IM agentifyd.exe' $1
    DetailPrint "taskkill agentifyd exited with $1"
    Sleep 500
  ${EndIf}

  ; v0.1.x registered a per-user logon task. This build does not, so remove the
  ; old one: left in place it would start the new daemon at logon, outside the
  ; app that is supposed to own it.
  ExecWait '"$INSTDIR\agentify.exe" login-item remove' $0
  DetailPrint "agentify login-item remove exited with $0"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Agentify does not register a logon task; the daemon starts and stops with the app"
  DetailPrint "Putting $INSTDIR on the user PATH"
  ExecWait '"$INSTDIR\agentify.exe" path install' $0
  DetailPrint "agentify path install exited with $0"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Stop the daemon before any file is deleted. An Operation in flight is the one
  ; refusal: `daemon stop` answers 2, and the uninstall is abandoned.
  ExecWait '"$INSTDIR\agentify.exe" daemon stop' $0
  DetailPrint "agentify daemon stop exited with $0"
  ${If} $0 == 2
    MessageBox MB_OK|MB_ICONEXCLAMATION "An Agentify operation is in flight. Let it finish, then uninstall again."
    Abort
  ${EndIf}

  ; Question 1, default yes: what belongs to this installation rather than to
  ; its Projects.
  StrCpy $2 "1"
  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON1 /SD IDYES "Remove Agentify's daemon registrations, PATH entry, certificate trust, and caches?$\n$\nChoose No to keep them." IDYES machine_done
  StrCpy $2 "0"
  machine_done:

  ; Question 2, default no: Projects and their runtimes.
  StrCpy $3 "0"
  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 /SD IDNO "Delete your Projects and their runtimes?$\n$\nThis removes the state database, Project workspaces, WSL runtimes, and Checkpoints. Choose No to keep them." IDYES projects_yes
  Goto projects_done
  projects_yes:
    StrCpy $3 "1"
  projects_done:

  ; The CLI owns the inventory of what was installed; the prompts only choose
  ; among its two answers.
  ${If} $2 == "1"
  ${AndIf} $3 == "1"
    ExecWait '"$INSTDIR\agentify.exe" uninstall --purge' $0
  ${ElseIf} $2 == "1"
    ExecWait '"$INSTDIR\agentify.exe" uninstall' $0
  ${ElseIf} $3 == "1"
    ExecWait '"$INSTDIR\agentify.exe" uninstall --purge --keep-machine' $0
  ${Else}
    DetailPrint "Machine-level additions and Projects were kept"
  ${EndIf}
  DetailPrint "agentify uninstall exited with $0"
  ${If} $0 == 3
    MessageBox MB_OK|MB_ICONEXCLAMATION "Close any MCP clients connected to Agentify, then uninstall again."
    Abort
  ${EndIf}
!macroend
