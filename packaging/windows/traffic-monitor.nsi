; Tramoing 的 Windows 安装包脚本（NSIS 3）。
;
; 由 packaging/windows/build.ps1 调用，也可以在 Windows 上手动执行：
;   makensis /DAPP_VERSION=0.1.0 packaging\windows\traffic-monitor.nsi
;
; 安装内容：
;   tramoing.exe          托盘主程序（普通权限）
;   tramoing-helper.exe   采集助手（ETW 需要管理员权限）
;
; 关键点：助手不是 Windows 服务，而是登录时以最高权限启动的计划任务
; （由 `tramoing-helper.exe --install` 注册，卸载时用 --uninstall 摘掉）。
;
; 命名约定（2026-11-18 产品改名 Tramoing）：
;   对外可见的名字（安装目录、快捷方式、控制面板条目、安装包文件名）用 APP_NAME=Tramoing；
;   **内部标识一律不动**：exe 名、数据目录 %LOCALAPPDATA%\TrafficMonitor、
;   HKCU Run 值、计划任务名、ETW 会话名 —— 改了会让旧版的历史数据和已注册的
;   计划任务变成孤儿，测试时就分不清「程序有问题」还是「迁移没做」。

Unicode true

; 下面用到 ${If} / ${EndIf}，必须显式引 LogicLib（NSIS 不自带）
!include "LogicLib.nsh"

!define APP_NAME "Tramoing"
!define APP_EXE "tramoing.exe"
!define HELPER_EXE "tramoing-helper.exe"
!define BUNDLE_ID "Tramoing"
; 旧版的标识，卸载时顺手清掉，避免残留两条 Run 项 / 两套注册表键
!define LEGACY_BUNDLE_ID "TrafficMonitor"
;; 数据目录沿用旧名，历史记录和设置不能因为改名丢掉
!define DATA_DIR "TrafficMonitor"
!define PUBLISHER "guolongkang"

!ifndef APP_VERSION
  !define APP_VERSION "0.1.0"
!endif

Name "${APP_NAME}"
BrandingText "${APP_NAME} ${APP_VERSION}"
OutFile "..\..\dist\${APP_NAME}-${APP_VERSION}-setup.exe"
InstallDir "$PROGRAMFILES64\${APP_NAME}"
InstallDirRegKey HKLM "Software\${BUNDLE_ID}" "InstallDir"
; 装到 Program Files、注册计划任务都需要管理员权限，一次 UAC 换全程静默
RequestExecutionLevel admin
SetCompressor /SOLID lzma
ShowInstDetails show
ShowUninstDetails show

VIProductVersion "${APP_VERSION}.0"
VIAddVersionKey "ProductName" "${APP_NAME}"
VIAddVersionKey "FileDescription" "${APP_NAME} 安装程序"
VIAddVersionKey "FileVersion" "${APP_VERSION}"
VIAddVersionKey "ProductVersion" "${APP_VERSION}"
VIAddVersionKey "LegalCopyright" "MIT"

Page directory
Page components
Page instfiles

UninstPage uninstConfirm
UninstPage instfiles

; ---------- 安装 ----------

Section "托盘程序（必需）" SEC_MAIN
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "..\..\dist\windows\${APP_EXE}"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk" "$INSTDIR\uninstall.exe"

  ; 覆盖安装旧版（当时叫 Traffic Monitor）时，把旧开始菜单项清掉，
  ; 否则开始菜单里会同时躺着两个名字
  Delete "$SMPROGRAMS\Traffic Monitor\Traffic Monitor.lnk"
  Delete "$SMPROGRAMS\Traffic Monitor\卸载 Traffic Monitor.lnk"
  RMDir "$SMPROGRAMS\Traffic Monitor"

  ; 控制面板「程序和功能」里的条目
  WriteRegStr HKLM "Software\${BUNDLE_ID}" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "NoModify" 1
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "NoRepair" 1
SectionEnd

Section "流量采集助手（按进程统计必需）" SEC_HELPER
  SetOutPath "$INSTDIR"
  File "..\..\dist\windows\${HELPER_EXE}"

  DetailPrint "注册登录自启任务（最高权限）..."
  ExecWait '"$INSTDIR\${HELPER_EXE}" --install' $0
  ; 助手把失败原因写进日志而不是控制台，这里只能提示去日志里看
  ${If} $0 != 0
    MessageBox MB_ICONEXCLAMATION|MB_OK "采集助手注册失败（错误码 $0）。$\r$\n$\r$\n托盘程序仍可启动，但拿不到采样数据，面板会停在加载状态。请以管理员身份运行一次 tramoing-helper.exe --install 后重试。$\r$\n日志：%LOCALAPPDATA%\${DATA_DIR}\helper.log"
  ${EndIf}
SectionEnd

; ---------- 卸载 ----------

Section "Uninstall"
  ; 先停任务再删文件，顺序反了会删不掉正在运行的 exe
  ExecWait '"$INSTDIR\${HELPER_EXE}" --uninstall'
  ExecWait 'taskkill /IM ${HELPER_EXE} /F'
  ExecWait 'taskkill /IM ${APP_EXE} /F'
  ; 旧版（改名前的 traffic-monitor.exe / tm-helper.exe）如果还在这台机器上跑，一起停掉
  ExecWait 'taskkill /IM traffic-monitor.exe /F'
  ExecWait 'taskkill /IM tm-helper.exe /F'

  ; 托盘程序自己的开机自启（设置里打开时写在 HKCU）。
  ; 程序内部仍用旧值名 TrafficMonitor（改了旧机器上的自启项会变成孤儿），
  ; 所以两个名字都删。
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${BUNDLE_ID}"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${LEGACY_BUNDLE_ID}"

  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"

  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\${HELPER_EXE}"
  ; 覆盖安装过旧版的目录里可能还留着老名字的 exe
  Delete "$INSTDIR\traffic-monitor.exe"
  Delete "$INSTDIR\tm-helper.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}"
  DeleteRegKey HKLM "Software\${BUNDLE_ID}"
  ; 旧版（Traffic Monitor）的条目也顺手清掉
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${LEGACY_BUNDLE_ID}"
  DeleteRegKey HKLM "Software\${LEGACY_BUNDLE_ID}"

  ; 历史数据默认保留：卸载工具不该顺手把用户一个月的流量记录删了
  MessageBox MB_YESNO|MB_ICONQUESTION "是否同时删除历史数据和设置？$\r$\n（%LOCALAPPDATA%\${DATA_DIR}）" /SD IDNO IDNO KeepData
  RMDir /r "$LOCALAPPDATA\${DATA_DIR}"
  KeepData:
SectionEnd
