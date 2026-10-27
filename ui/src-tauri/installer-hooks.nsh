; NSIS hooks for Agentify's per-user installer.
;
; Tauri includes this file via `bundle.windows.nsis.installerHooks`. The installer
; itself does one thing beyond copying files: it registers the daemon's login
; item, so an installed Agentify starts its daemon at the next logon without the
; UI being open. Uninstalling removes it again — the same pair a person can run by
; hand with `agentify login-item install` / `remove`.
;
; Nothing here elevates. The installer runs as the user (installMode
; "currentUser") and the task it registers is a per-user Scheduled Task at
; LeastPrivilege, so no UAC prompt appears.
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
!macroend

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Registering the Agentify daemon login item"
  ExecWait '"$INSTDIR\agentify.exe" login-item install' $0
  DetailPrint "agentify login-item install exited with $0"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Removing the Agentify daemon login item"
  ExecWait '"$INSTDIR\agentify.exe" login-item remove' $0
  DetailPrint "agentify login-item remove exited with $0"
  DetailPrint "Removing the trusted root and Caddy data; Projects are left in place"
  ExecWait '"$INSTDIR\agentify.exe" uninstall' $0
  DetailPrint "agentify uninstall exited with $0"
!macroend
