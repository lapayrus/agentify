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
  DetailPrint "Removing $INSTDIR from the user PATH"
  ExecWait '"$INSTDIR\agentify.exe" path remove' $0
  DetailPrint "agentify path remove exited with $0"
  DetailPrint "Removing the Agentify daemon login item"
  ExecWait '"$INSTDIR\agentify.exe" login-item remove' $0
  DetailPrint "agentify login-item remove exited with $0"
  DetailPrint "Removing the trusted root and Caddy data; Projects are left in place"
  ExecWait '"$INSTDIR\agentify.exe" uninstall' $0
  DetailPrint "agentify uninstall exited with $0"
!macroend
