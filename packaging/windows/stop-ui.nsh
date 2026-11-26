; Used for both installation and uninstallation. Keep UI shutdown before
; collector shutdown, so a responsive UI can save and delete its tray icon.
!ifndef TRAMOING_STOP_UI_INCLUDED
!define TRAMOING_STOP_UI_INCLUDED
!include "LogicLib.nsh"
!define TRAMOING_STOP_UI_SCRIPT "${__FILEDIR__}\stop-ui.ps1"
!ifndef TRAMOING_STOP_UI_TIMEOUT
!define TRAMOING_STOP_UI_TIMEOUT 10000
!endif
!macro STOP_UI PROCESS_NAME
  InitPluginsDir
  File /oname=$PLUGINSDIR\stop-ui.ps1 "${TRAMOING_STOP_UI_SCRIPT}"
  nsExec::ExecToLog '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\stop-ui.ps1" -ProcessName "${PROCESS_NAME}" -TimeoutMilliseconds ${TRAMOING_STOP_UI_TIMEOUT}'
  Pop $0
  ${If} $0 != 0
    SetAutoClose false
    SetErrorLevel 1
    Abort "无法关闭 Tramoing，文件尚未改动。请先退出程序后重试。"
  ${EndIf}
!macroend
!endif
