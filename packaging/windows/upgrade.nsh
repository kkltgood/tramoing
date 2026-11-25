; Shared by the installer and its isolated native regression harness.
!ifndef TRAMOING_UPGRADE_INCLUDED
!define TRAMOING_UPGRADE_INCLUDED
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "${__FILEDIR__}\legacy.nsh"
!ifdef APP_ARCH
  !include "x64.nsh"
!endif

!ifndef INSTALL_REG_ROOT
  !define INSTALL_REG_ROOT HKLM
!endif

Var UpgradeMode
Var UpdateParentPid

Function .onInit
  !ifdef TRAMOING_SENSOR_COMPONENT
    Call InitializeSensorSelection
  !endif
  ; Reject incompatible packages before any installed files or processes change.
  !ifdef APP_ARCH
    !if "${APP_ARCH}" == "arm64"
      ${IfNot} ${IsNativeARM64}
        MessageBox MB_ICONSTOP|MB_OK "此安装包需要 Windows ARM64，请下载 Windows x64 安装包。" /SD IDOK
        SetErrorLevel 1
        Abort
      ${EndIf}
    !else
      ; Keep x64 upgrades working for existing clients under ARM64 emulation.
      ${IfNot} ${RunningX64}
        MessageBox MB_ICONSTOP|MB_OK "此安装包需要 64 位 Windows。" /SD IDOK
        SetErrorLevel 1
        Abort
      ${EndIf}
    !endif
  !endif
  StrCpy $UpgradeMode 0
  StrCpy $UpdateParentPid ""
  ${GetParameters} $0
  !ifdef TRAMOING_UPGRADE_TEST_TRACE
    WriteINIStr "$EXEDIR\result.ini" "trace" "arguments" "$0"
  !endif
  ClearErrors
  ${GetOptions} $0 "/UPDATE" $1
  ${IfNot} ${Errors}
    ; /D= is parsed by NSIS before .onInit, including spaces and Unicode.
    IfFileExists "$INSTDIR\${APP_EXE}" explicit_update invalid_update
    explicit_update:
      StrCpy $UpgradeMode 1
      ClearErrors
      ${GetOptions} $0 "/APP_PID=" $UpdateParentPid
      ${If} ${Errors}
        StrCpy $UpdateParentPid ""
      ${ElseIf} $UpdateParentPid <= 0
        Goto invalid_update
      ${EndIf}
      !ifdef TRAMOING_UPGRADE_TEST_TRACE
        WriteINIStr "$EXEDIR\result.ini" "trace" "accepted_parent" "$UpdateParentPid"
      !endif
      Return
    invalid_update:
      !ifdef TRAMOING_UPGRADE_TEST_TRACE
        WriteINIStr "$EXEDIR\result.ini" "trace" "invalid_update" "$INSTDIR; parent=$UpdateParentPid"
      !endif
      MessageBox MB_ICONSTOP|MB_OK "更新参数或安装目录无效，更新已停止。$\r$\n$INSTDIR" /SD IDOK
      SetErrorLevel 1
      Abort
  ${EndIf}

  ; Older clients launch setup without arguments. Detect their existing install
  ; too, so the first upgrade to this installer already skips the setup wizard.
  IfFileExists "$INSTDIR\${APP_EXE}" existing_install
  IfFileExists "$INSTDIR\${LEGACY_APP_EXE}" existing_install
  ReadRegStr $1 ${INSTALL_REG_ROOT} "Software\${LEGACY_BUNDLE_ID}" "InstallDir"
  ${If} $1 != ""
    IfFileExists "$1\${LEGACY_APP_EXE}" 0 fresh_install
    StrCpy $INSTDIR $1
    Goto existing_install
  ${EndIf}
  Goto fresh_install
  existing_install:
    StrCpy $UpgradeMode 1
  fresh_install:
FunctionEnd

; Call from the directory, components and finish page PRE callbacks.
Function SkipSetupPageOnUpgrade
  ${If} $UpgradeMode == 1
    Abort
  ${EndIf}
FunctionEnd

Function ShowUpgradeProgress
  ${If} $UpgradeMode == 1
    !insertmacro MUI_HEADER_TEXT "正在更新 ${APP_NAME}" "更新完成后将自动重新启动，保留设置和历史记录。"
    SetAutoClose true
  ${EndIf}
FunctionEnd

; This must run before stopping the collector or changing any installed file.
; The new client exits only after ShellExecute confirms that UAC was accepted.
Function WaitForUpdateParent
  !ifdef TRAMOING_UPGRADE_TEST_TRACE
    WriteINIStr "$EXEDIR\result.ini" "trace" "parent_pid" "$UpdateParentPid"
  !endif
  ${If} $UpdateParentPid != ""
    DetailPrint "正在等待程序保存历史并退出..."
    System::Call 'kernel32::OpenProcess(i 0x00100000, i 0, i $UpdateParentPid) p.r0'
    ${If} $0 P<> 0
      !ifdef TRAMOING_UPGRADE_TEST_TRACE
        WriteINIStr "$EXEDIR\result.ini" "trace" "parent_handle" "$0"
      !endif
      System::Call 'kernel32::WaitForSingleObject(p r0, i 15000) i.r1'
      System::Call 'kernel32::CloseHandle(p r0)'
      !ifdef TRAMOING_UPGRADE_TEST_TRACE
        WriteINIStr "$EXEDIR\result.ini" "trace" "wait_result" "$1"
      !endif
      ${If} $1 != 0
        SetAutoClose false
        SetErrorLevel 1
        Abort "程序尚未退出，更新已停止。请退出 Tramoing 后重试。"
      ${EndIf}
    ${Else}
      System::Call 'kernel32::GetLastError() i.r1'
      !ifdef TRAMOING_UPGRADE_TEST_TRACE
        WriteINIStr "$EXEDIR\result.ini" "trace" "open_error" "$1"
      !endif
      ; ERROR_INVALID_PARAMETER means the process has already exited.
      ${If} $1 != 87
        SetAutoClose false
        SetErrorLevel 1
        Abort "无法等待程序退出（错误 $1），更新已停止。"
      ${EndIf}
    ${EndIf}
  ${EndIf}
FunctionEnd

Function .onInstSuccess
  ${If} $UpgradeMode == 1
    Call LaunchApp
  ${EndIf}
FunctionEnd
!endif
