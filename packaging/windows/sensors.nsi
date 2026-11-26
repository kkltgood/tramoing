; Optional temperature component. The main installer never carries this payload.
Unicode true
!include "LogicLib.nsh"
!include "x64.nsh"
!ifndef SENSOR_VERSION
  !error "SENSOR_VERSION is required"
!endif
!ifndef SENSOR_PAYLOAD
  !define SENSOR_PAYLOAD "..\..\dist\windows-x64\sensors"
!endif
!ifndef SENSOR_OUTPUT
  !define SENSOR_OUTPUT "..\..\dist\Tramoing-Sensors-${SENSOR_VERSION}-windows-x64-setup.exe"
!endif
!ifdef SENSOR_TEST_MODE
  RequestExecutionLevel user
!else
  RequestExecutionLevel admin
!endif
Name "Tramoing temperature component ${SENSOR_VERSION}"
OutFile "${SENSOR_OUTPUT}"
InstallDir "$PROGRAMFILES64\Tramoing"
InstallDirRegKey HKLM "Software\Tramoing" "InstallDir"
SetCompressor /SOLID lzma
SilentInstall silent
AutoCloseWindow true
VIProductVersion "${SENSOR_VERSION}.0"
VIAddVersionKey "ProductName" "Tramoing temperature component"
VIAddVersionKey "FileVersion" "${SENSOR_VERSION}"
VIAddVersionKey "FileDescription" "Optional Tramoing temperature component"
VIAddVersionKey "LegalCopyright" "MIT and bundled third-party licenses"
Var HadPrevious
Var Committed
Var InstallMutex
Var RolledBack

Function .onInit
  ; The elevated readiness probe must not inherit runtime injection settings.
  System::Call 'kernel32::SetEnvironmentVariableW(w "DOTNET_STARTUP_HOOKS", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "DOTNET_ADDITIONAL_DEPS", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "DOTNET_SHARED_STORE", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "DOTNET_ROOT", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "DOTNET_ROOT_X64", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "DOTNET_ROOT_X86", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "DOTNET_ROOT(x86)", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "DOTNET_ROLL_FORWARD", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "DOTNET_ROLL_FORWARD_TO_PRERELEASE", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "COR_ENABLE_PROFILING", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "COR_PROFILER", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "COR_PROFILER_PATH", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "COR_PROFILER_PATH_32", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "COR_PROFILER_PATH_64", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "CORECLR_ENABLE_PROFILING", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "CORECLR_PROFILER", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "CORECLR_PROFILER_PATH", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "CORECLR_PROFILER_PATH_32", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "CORECLR_PROFILER_PATH_64", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "DOTNET_DIAGNOSTICPORTS", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "DOTNET_EnableDiagnostics", w "0")'
  StrCpy $HadPrevious 0
  StrCpy $Committed 0
  StrCpy $RolledBack 0
  ${IfNot} ${RunningX64}
    SetErrorLevel 1
    Abort "This component requires Windows x64."
  ${EndIf}
  ${If} ${IsNativeARM64}
    SetErrorLevel 1
    Abort "Built-in temperature sensors are not supported on Windows ARM64."
  ${EndIf}
  IfFileExists "$INSTDIR\tramoing.exe" 0 invalid_install
  IfFileExists "$INSTDIR\tramoing-helper.exe" 0 invalid_install
  ; Never follow stale junctions while extracting, renaming or removing payloads.
  Push "$INSTDIR"
  Call RejectReparsePoint
  Push "$INSTDIR\sensors"
  Call RejectReparsePoint
  Push "$INSTDIR\sensors.next"
  Call RejectReparsePoint
  Push "$INSTDIR\sensors.previous"
  Call RejectReparsePoint
  IfFileExists "$INSTDIR\sensors.next" recovery_needed
  IfFileExists "$INSTDIR\sensors.previous" recovery_needed
  !ifndef SENSOR_TEST_MODE
    System::Call 'kernel32::CreateMutexW(p 0, i 0, w "Global\TramoingSensorInstall") p.r0 ?e'
    Pop $1
    StrCpy $InstallMutex $0
    ${If} $0 == 0
    ${OrIf} $1 == 183
      SetErrorLevel 1
      Abort "A temperature component installation is already running."
    ${EndIf}
  !endif
  Return
  invalid_install:
    SetErrorLevel 1
    Abort "Install Tramoing before installing its temperature component."
  recovery_needed:
    SetErrorLevel 1
    Abort "A previous component installation needs recovery; sensors.previous was preserved."
FunctionEnd

Function RejectReparsePoint
  Exch $0
  Push $1
  System::Call 'kernel32::GetFileAttributesW(w r0) i.r1'
  ${If} $1 != -1
    IntOp $1 $1 & 0x400
    ${If} $1 != 0
      SetErrorLevel 1
      Abort "Temperature component paths must not be reparse points."
    ${EndIf}
  ${EndIf}
  Pop $1
  Pop $0
FunctionEnd

Function StopWorker
  !ifndef SENSOR_TEST_MODE
    nsExec::ExecToLog /TIMEOUT=15000 '"$SYSDIR\schtasks.exe" /End /TN "TramoingHelper"'
    Pop $0
    nsExec::ExecToLog /TIMEOUT=15000 '"$SYSDIR\taskkill.exe" /IM tramoing-helper.exe /F'
    Pop $0
    nsExec::ExecToLog /TIMEOUT=15000 '"$SYSDIR\taskkill.exe" /IM tramoing-sensors.exe /F'
    Pop $0
    Sleep 300
  !endif
FunctionEnd

Function RestartHelper
  !ifdef SENSOR_TEST_MODE
    StrCpy $0 0
    IfFileExists "$INSTDIR\fail-restart" 0 +2
      StrCpy $0 1
  !else
    nsExec::ExecToLog /TIMEOUT=20000 '"$SYSDIR\schtasks.exe" /Run /TN "TramoingHelper"'
    Pop $0
  !endif
FunctionEnd

Function Rollback
  ${If} $RolledBack == 1
    Return
  ${EndIf}
  StrCpy $RolledBack 1
  SetOutPath "$INSTDIR"
  Call StopWorker
  ${If} $Committed == 1
    RMDir /r "$INSTDIR\sensors"
  ${EndIf}
  ${If} $HadPrevious == 1
    ClearErrors
    Rename "$INSTDIR\sensors.previous" "$INSTDIR\sensors"
    ; A locked old directory is deliberately preserved for manual recovery.
  ${EndIf}
  RMDir /r "$INSTDIR\sensors.next"
  StrCpy $Committed 0
  StrCpy $HadPrevious 0
  Call RestartHelper
  SetErrorLevel 1
FunctionEnd

Function .onInstFailed
  Call Rollback
FunctionEnd

Function .onGUIEnd
  ${If} $InstallMutex != ""
    System::Call 'kernel32::CloseHandle(p $InstallMutex)'
  ${EndIf}
FunctionEnd

Section
  ClearErrors
  SetOutPath "$INSTDIR\sensors.next"
  SetOverwrite on
  File /r "${SENSOR_PAYLOAD}\*"
  IfErrors failed
  IfFileExists "$INSTDIR\sensors.next\tramoing-sensors.exe" 0 failed
  IfFileExists "$INSTDIR\sensors.next\component.json" 0 failed
  ; Release the working-directory handle before renaming the staging directory.
  SetOutPath "$INSTDIR"
  Call StopWorker
  IfFileExists "$INSTDIR\sensors" 0 no_previous
    ClearErrors
    Rename "$INSTDIR\sensors" "$INSTDIR\sensors.previous"
    IfErrors failed
    StrCpy $HadPrevious 1
  no_previous:
  ClearErrors
  Rename "$INSTDIR\sensors.next" "$INSTDIR\sensors"
  IfErrors failed
  StrCpy $Committed 1
  !ifndef SENSOR_TEST_MODE
    ; Missing/incompatible .NET must not replace a working older component.
    nsExec::ExecToLog /TIMEOUT=15000 '"$INSTDIR\sensors\tramoing-sensors.exe" --probe'
    Pop $0
    ${If} $0 != 0
      Goto failed
    ${EndIf}
  !endif
  Call RestartHelper
  ${If} $0 != 0
    Goto failed
  ${EndIf}
  RMDir /r "$INSTDIR\sensors.previous"
  SetErrorLevel 0
  Goto done
  failed:
    Call Rollback
    SetErrorLevel 1
    Abort "Temperature component installation failed; the previous component will be restored."
  done:
SectionEnd
