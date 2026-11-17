; Traffic Monitor 的 Windows 安装包脚本（NSIS 3）。
;
; 由 packaging/windows/build.ps1 调用，也可以在 Windows 上手动执行：
;   makensis /DAPP_VERSION=0.1.0 packaging\windows\traffic-monitor.nsi
;
; 安装内容：
;   traffic-monitor.exe  托盘主程序（普通权限）
;   tm-helper.exe        采集助手（ETW 需要管理员权限）
;
; 关键点：助手不是 Windows 服务，而是登录时以最高权限启动的计划任务
; （由 `tm-helper.exe --install` 注册，卸载时用 --uninstall 摘掉）。

Unicode true

!define APP_NAME "Traffic Monitor"
!define APP_EXE "traffic-monitor.exe"
!define HELPER_EXE "tm-helper.exe"
!define BUNDLE_ID "TrafficMonitor"
!define PUBLISHER "guolongkang"

!ifndef APP_VERSION
  !define APP_VERSION "0.1.0"
!endif

Name "${APP_NAME}"
BrandingText "${APP_NAME} ${APP_VERSION}"
OutFile "..\..\dist\TrafficMonitor-${APP_VERSION}-setup.exe"
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
    MessageBox MB_ICONEXCLAMATION|MB_OK "采集助手注册失败（错误码 $0）。$\r$\n$\r$\n托盘程序仍可启动，但拿不到采样数据，面板会停在加载状态。请以管理员身份运行一次 tm-helper.exe --install 后重试。$\r$\n日志：%LOCALAPPDATA%\TrafficMonitor\helper.log"
  ${EndIf}
SectionEnd

; ---------- 卸载 ----------

Section "Uninstall"
  ; 先停任务再删文件，顺序反了会删不掉正在运行的 exe
  ExecWait '"$INSTDIR\${HELPER_EXE}" --uninstall'
  ExecWait 'taskkill /IM ${HELPER_EXE} /F'
  ExecWait 'taskkill /IM ${APP_EXE} /F'

  ; 托盘程序自己的开机自启（设置里打开时写在 HKCU）
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${BUNDLE_ID}"

  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"

  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\${HELPER_EXE}"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}"
  DeleteRegKey HKLM "Software\${BUNDLE_ID}"

  ; 历史数据默认保留：卸载工具不该顺手把用户一个月的流量记录删了
  MessageBox MB_YESNO|MB_ICONQUESTION "是否同时删除历史数据和设置？$\r$\n（%LOCALAPPDATA%\TrafficMonitor）" /SD IDNO IDNO KeepData
  RMDir /r "$LOCALAPPDATA\TrafficMonitor"
  KeepData:
SectionEnd
