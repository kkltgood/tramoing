; Old names are used only to detect and clean up installations from before the rename.
!ifndef TRAMOING_LEGACY_INCLUDED
!define TRAMOING_LEGACY_INCLUDED
!ifndef LEGACY_BUNDLE_ID
  !define LEGACY_BUNDLE_ID "TrafficMonitor"
!endif
!define LEGACY_APP_EXE "traffic-monitor.exe"
!define LEGACY_HELPER_EXE "tm-helper.exe"
!define LEGACY_TASK_NAME "TrafficMonitorHelper"
!define LEGACY_START_MENU "Traffic Monitor"

; RUN_KEY is injectable so the native test never writes real login items.
!macro MIGRATE_LEGACY_RUN RUN_KEY
  ReadRegStr $0 HKCU "${RUN_KEY}" "${LEGACY_BUNDLE_ID}"
  ${If} $0 != ""
    ClearErrors
    WriteRegStr HKCU "${RUN_KEY}" "${BUNDLE_ID}" '$\"$INSTDIR\${APP_EXE}$\"'
    ${IfNot} ${Errors}
      DeleteRegValue HKCU "${RUN_KEY}" "${LEGACY_BUNDLE_ID}"
    ${EndIf}
  ${EndIf}
!macroend
!endif
