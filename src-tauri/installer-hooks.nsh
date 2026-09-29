; Registers the Explorer preview handler + thumbnail provider (shell\babylon_shell.dll).

!define BABYLON_SHELL_DLL "$INSTDIR\shell\babylon_shell.dll"

; The installer is 32-bit, so go through Sysnative to reach the 64-bit regsvr32.
!macro BABYLON_REGSVR ARGS
  ${If} ${FileExists} "$WINDIR\Sysnative\regsvr32.exe"
    ExecWait '"$WINDIR\Sysnative\regsvr32.exe" /s ${ARGS} "${BABYLON_SHELL_DLL}"'
  ${Else}
    ExecWait '"$SYSDIR\regsvr32.exe" /s ${ARGS} "${BABYLON_SHELL_DLL}"'
  ${EndIf}
!macroend

; Explorer (thumbnails) or prevhost.exe (previews) may have the old DLL loaded. A loaded
; DLL can't be overwritten or deleted, but it can be renamed; the stale copy goes at reboot.
!macro BABYLON_RELEASE_DLL
  nsExec::Exec 'taskkill /f /im prevhost.exe'
  Pop $0
  ${If} ${FileExists} "${BABYLON_SHELL_DLL}"
    Delete "${BABYLON_SHELL_DLL}.old"
    Rename "${BABYLON_SHELL_DLL}" "${BABYLON_SHELL_DLL}.old"
    Delete /REBOOTOK "${BABYLON_SHELL_DLL}.old"
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro BABYLON_RELEASE_DLL
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro BABYLON_REGSVR ""
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro BABYLON_REGSVR "/u"
  !insertmacro BABYLON_RELEASE_DLL
!macroend
