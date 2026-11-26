; No elevation, real application processes, scheduled tasks or system registry
; changes. Exercise the production upgrade callbacks with disposable files.
Unicode true
RequestExecutionLevel user
!include "MUI2.nsh"
!define APP_NAME "Tramoing upgrade regression"
!define APP_EXE "tramoing.exe"
!define BUNDLE_ID "${TEST_BUNDLE_ID}"
!define LEGACY_BUNDLE_ID "${TEST_BUNDLE_ID}-legacy"
!define INSTALL_REG_ROOT HKCU
!define TRAMOING_UPGRADE_TEST_TRACE
!include "..\upgrade.nsh"
!define TRAMOING_STOP_UI_TIMEOUT 1000
!include "..\stop-ui.nsh"

Name "${APP_NAME}"
OutFile "${TEST_OUTPUT}"
InstallDir "$EXEDIR\fresh"
InstallDirRegKey HKCU "Software\${BUNDLE_ID}" "InstallDir"

!define MUI_PAGE_CUSTOMFUNCTION_PRE DirectoryPre
!insertmacro MUI_PAGE_DIRECTORY
!define MUI_PAGE_CUSTOMFUNCTION_PRE ComponentsPre
!insertmacro MUI_PAGE_COMPONENTS
!define MUI_PAGE_CUSTOMFUNCTION_SHOW ShowUpgradeProgress
!insertmacro MUI_PAGE_INSTFILES
!define MUI_PAGE_CUSTOMFUNCTION_PRE FinishPre
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_LANGUAGE "SimpChinese"

Function DirectoryPre
  Call SkipSetupPageOnUpgrade
  WriteINIStr "$EXEDIR\result.ini" "test" "page" "directory"
  Quit
FunctionEnd

Function ComponentsPre
  Call SkipSetupPageOnUpgrade
  WriteINIStr "$EXEDIR\result.ini" "test" "page" "components"
  Quit
FunctionEnd

Function FinishPre
  Call SkipSetupPageOnUpgrade
  WriteINIStr "$EXEDIR\result.ini" "test" "page" "finish"
  Quit
FunctionEnd

Function LaunchApp
  WriteINIStr "$EXEDIR\result.ini" "test" "restarted" "1"
FunctionEnd

Function .onInstFailed
  WriteINIStr "$EXEDIR\result.ini" "test" "failed" "1"
  SetErrorLevel 1
  Quit
FunctionEnd

Section "Test payload"
  Call WaitForUpdateParent
  IfFileExists "$EXEDIR\stop-ui" 0 stop_ui_done
    !insertmacro STOP_UI "${TEST_UI_EXE}"
  stop_ui_done:
  ; Simulate a helper registration failure after entering the install section.
  IfFileExists "$EXEDIR\fail-install" 0 +3
    SetErrorLevel 1
    Abort "Simulated failure"
  CreateDirectory "$INSTDIR"
  FileOpen $0 "$INSTDIR\${APP_EXE}" w
  FileWrite $0 "new payload"
  FileClose $0
  !insertmacro MIGRATE_LEGACY_RUN "Software\${BUNDLE_ID}\Run"
  WriteINIStr "$EXEDIR\result.ini" "test" "installed" "1"
  WriteINIStr "$EXEDIR\result.ini" "test" "directory" "$INSTDIR"
SectionEnd
