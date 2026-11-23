; Tramoing 的 Windows 安装包脚本（NSIS 3）。
;
; 由 packaging/windows/build.ps1 调用，也可以在 Windows 上手动执行：
;   makensis /DAPP_VERSION=0.1.0 /DAPP_ARCH=x64 packaging\windows\tramoing.nsi
;
; 安装内容：
;   tramoing.exe          托盘主程序（普通权限）
;   tramoing-helper.exe   采集助手（ETW 需要管理员权限）
;
; 关键点：助手不是 Windows 服务，而是登录时以最高权限启动的计划任务
; （由 `tramoing-helper.exe --install` 注册，卸载时用 --uninstall 摘掉）。
;
; 当前名称统一使用 Tramoing；旧名称仅用于识别和清理旧版安装。

Unicode true

; 下面用到 ${If} / ${EndIf}，必须显式引 LogicLib（NSIS 不自带）
!include "LogicLib.nsh"
; Modern UI 2：经典页面集里没有「完成」页（Page 只有 directory / components /
; instfiles），装完窗口直接消失 —— 既看不到「装好了」，也没机会顺手把程序拉起来。
!include "MUI2.nsh"

!define APP_NAME "Tramoing"
!define APP_EXE "tramoing.exe"
!define HELPER_EXE "tramoing-helper.exe"
!define BUNDLE_ID "Tramoing"
!define DATA_DIR "Tramoing"
!define TASK_NAME "TramoingHelper"
!include "legacy.nsh"
!define PUBLISHER "guolongkang"

!ifndef APP_VERSION
  !define APP_VERSION "0.1.0"
!endif
!ifndef APP_ARCH
  !define APP_ARCH "x64"
!endif
!if "${APP_ARCH}" != "x64"
!if "${APP_ARCH}" != "arm64"
  !error "APP_ARCH must be x64 or arm64"
!endif
!endif

; 覆盖安装时正在运行的 exe 会被 Windows 锁住，File 直接覆盖会弹 NSIS 原生
; 错误框（中止/重试/忽略），用户点「忽略」就会装出「主程序新版、助手旧版」
; 的半成品。所以写文件前先把旧 exe 删掉，让 File 变成全新写入；真删不掉就
; 中止并给出明确指引，绝不静默跳过。
!macro PREPARE_OVERWRITE FILE LABEL
  IfFileExists "${FILE}" 0 ${LABEL}_done
    ClearErrors
    Delete "${FILE}"
    IfErrors 0 ${LABEL}_done
      ; taskkill 返回后文件句柄可能还没释放，再给一次机会
      Sleep 800
      ClearErrors
      Delete "${FILE}"
      IfErrors 0 ${LABEL}_done
        Abort "无法替换文件：${FILE}$\r$\n$\r$\n请先在任务管理器里结束 tramoing.exe / tramoing-helper.exe（或重启电脑），然后重新运行安装包。$\r$\n$\r$\n不要选「忽略」，否则新版本不会被完整安装。"
  ${LABEL}_done:
!macroend

Name "${APP_NAME}"
BrandingText "${APP_NAME} ${APP_VERSION} (${APP_ARCH})"
OutFile "..\..\dist\${APP_NAME}-${APP_VERSION}-windows-${APP_ARCH}-setup.exe"
InstallDir "$PROGRAMFILES64\${APP_NAME}"
InstallDirRegKey HKLM "Software\${BUNDLE_ID}" "InstallDir"
; 装到 Program Files、注册计划任务都需要管理员权限，一次 UAC 换全程静默
RequestExecutionLevel admin
SetCompressor /SOLID lzma
ShowInstDetails show
ShowUninstDetails show

VIProductVersion "${APP_VERSION}.0"
VIAddVersionKey "ProductName" "${APP_NAME}"
VIAddVersionKey "FileDescription" "${APP_NAME} 安装程序 (${APP_ARCH})"
VIAddVersionKey "FileVersion" "${APP_VERSION}"
VIAddVersionKey "ProductVersion" "${APP_VERSION}"
VIAddVersionKey "LegalCopyright" "MIT"

; System plugin calls in the upgrade callbacks must follow SetCompressor.
!include "upgrade.nsh"

; ---------- 安装界面 ----------
; 图标同时管三处：安装包自身、卸载程序、以及对话框左上角那个小图标
!define MUI_ICON "..\..\assets\tramoing.ico"
!define MUI_UNICON "..\..\assets\tramoing.ico"
!define MUI_ABORTWARNING
; 完成页的「立即运行」：默认勾上，点「完成」时调用 LaunchApp
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "立即运行 ${APP_NAME}"
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchApp
!define MUI_FINISHPAGE_NOREBOOTSUPPORT

!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipSetupPageOnUpgrade
!insertmacro MUI_PAGE_DIRECTORY
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipSetupPageOnUpgrade
!insertmacro MUI_PAGE_COMPONENTS
!define MUI_PAGE_CUSTOMFUNCTION_SHOW ShowUpgradeProgress
!insertmacro MUI_PAGE_INSTFILES
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipSetupPageOnUpgrade
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH

; 界面语言：产品只有中文文案，装英文包反而中英混排
!insertmacro MUI_LANGUAGE "SimpChinese"

; ---------- 安装 ----------

; 首次安装的完成页、升级成功后的自动重启共用。
;
; 安装程序是提权跑的，直接 `Exec` 会把托盘程序也拉成管理员 —— 它自己不需要
; 管理员（采集是计划任务里的助手干的，见 SEC_HELPER）。把路径交给已经在跑的
; explorer，由它以登录用户的普通权限启动，效果和自己点开始菜单一样。
Function LaunchApp
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\${APP_EXE}"'
FunctionEnd

; 升级准备必须排在文件写入之前：NSIS 按脚本顺序执行选中的 section。
; 隐藏 section（名字以 - 开头）+ SectionIn RO，保证每次安装都会跑。
Section "-升级准备：关闭旧进程" SEC_PRE
  SectionIn RO
  Call WaitForUpdateParent
  DetailPrint "正在关闭旧的 Tramoing 进程..."

  ; 计划任务在登录时以最高权限拉起助手，先让它退下来，避免拷贝期间
  ; 任务调度器又把旧 exe 拉起来重新占住文件。
  nsExec::ExecToLog '"$SYSDIR\schtasks.exe" /End /TN "${TASK_NAME}"'
  Pop $0
  nsExec::ExecToLog '"$SYSDIR\schtasks.exe" /End /TN "${LEGACY_TASK_NAME}"'
  Pop $0

  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /IM "${HELPER_EXE}" /F'
  Pop $0
  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /IM "${APP_EXE}" /F'
  Pop $0
  ; 改名前的 exe 也可能还在跑，一样停掉
  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /IM "${LEGACY_APP_EXE}" /F'
  Pop $0
  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /IM "${LEGACY_HELPER_EXE}" /F'
  Pop $0

  ; taskkill 返回后进程对象可能还没完全释放句柄，等一下再删
  Sleep 700
  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /IM "${HELPER_EXE}" /F'
  Pop $0
  Sleep 300

  !insertmacro PREPARE_OVERWRITE "$INSTDIR\${APP_EXE}" prep_main
  !insertmacro PREPARE_OVERWRITE "$INSTDIR\${HELPER_EXE}" prep_helper
  !insertmacro PREPARE_OVERWRITE "$INSTDIR\uninstall.exe" prep_uninst
SectionEnd

Section "托盘程序（必需）" SEC_MAIN
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "..\..\dist\windows-${APP_ARCH}\${APP_EXE}"
  File "..\..\LICENSE"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk" "$INSTDIR\uninstall.exe"

  ; 清理旧名快捷方式；登录项只在原本启用时迁移，不改变用户的开关选择。
  Delete "$SMPROGRAMS\${LEGACY_START_MENU}\${LEGACY_START_MENU}.lnk"
  Delete "$SMPROGRAMS\${LEGACY_START_MENU}\卸载 ${LEGACY_START_MENU}.lnk"
  RMDir "$SMPROGRAMS\${LEGACY_START_MENU}"
  !insertmacro MIGRATE_LEGACY_RUN "Software\Microsoft\Windows\CurrentVersion\Run"

  ; 控制面板「程序和功能」里的条目
  WriteRegStr HKLM "Software\${BUNDLE_ID}" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "NoModify" 1
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}" "NoRepair" 1

  ; 覆盖安装时 exe 的图标变了，但 shell 的图标缓存不一定跟着刷 —— 旧版本装出来
  ; 是个空白方块，不通知的话开始菜单可能还挂着那张缓存图。SHCNE_ASSOCCHANGED
  ; 是安装程序里最常用的一招，代价只是让资源管理器重读一次图标。
  System::Call 'shell32::SHChangeNotify(i 0x8000000, i 0, i 0, i 0)'
SectionEnd

Section "流量采集助手（按进程统计必需）" SEC_HELPER
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "..\..\dist\windows-${APP_ARCH}\${HELPER_EXE}"

  DetailPrint "注册登录自启任务（最高权限）..."
  StrCpy $0 -1
  ExecWait '"$INSTDIR\${HELPER_EXE}" --install' $0
  ; 助手把失败原因写进日志而不是控制台，这里只能提示去日志里看
  ${If} $0 != 0
    MessageBox MB_ICONEXCLAMATION|MB_OK "采集助手安装或启动失败（错误码 $0）。$\r$\n$\r$\n托盘程序仍可启动，但无法统计流量。请以管理员身份运行一次 tramoing-helper.exe --install 后重试。$\r$\n日志：%LOCALAPPDATA%\${DATA_DIR}\helper.log"
    ${If} $UpgradeMode == 1
      SetAutoClose false
      SetErrorLevel 1
      Abort "采集助手更新失败，请按上述提示修复后重新运行程序。"
    ${EndIf}
  ${EndIf}
  ; 只有新版助手安装成功后才清掉旧执行文件和卸载注册项。
  ${If} $0 == 0
    Delete "$INSTDIR\${LEGACY_APP_EXE}"
    Delete "$INSTDIR\${LEGACY_HELPER_EXE}"
    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${LEGACY_BUNDLE_ID}"
    DeleteRegKey HKLM "Software\${LEGACY_BUNDLE_ID}"
  ${EndIf}
SectionEnd

; ---------- 卸载 ----------

Section "Uninstall"
  ; 先停任务再删文件，顺序反了会删不掉正在运行的 exe
  ExecWait 'taskkill /IM ${APP_EXE} /F'
  ExecWait '"$SYSDIR\schtasks.exe" /End /TN "${TASK_NAME}"'
  ExecWait '"$SYSDIR\schtasks.exe" /End /TN "${LEGACY_TASK_NAME}"'
  ExecWait 'taskkill /IM ${HELPER_EXE} /F'
  StrCpy $0 -1
  ExecWait '"$INSTDIR\${HELPER_EXE}" --clear-network-blocks' $0
  ${If} $0 != 0
    MessageBox MB_ICONEXCLAMATION|MB_OK "无法清理断网规则或移除助手，已保留程序。请先恢复联网后重试卸载。$\r$\n日志：%LOCALAPPDATA%\${DATA_DIR}\helper.log"
    Abort "卸载未完成，原程序已保留。"
  ${EndIf}
  ExecWait '"$INSTDIR\${HELPER_EXE}" --uninstall'
  ExecWait 'taskkill /IM ${HELPER_EXE} /F'
  ExecWait 'taskkill /IM ${APP_EXE} /F'
  ; 改名前的旧程序如果还在运行，一起停掉。
  ExecWait 'taskkill /IM ${LEGACY_APP_EXE} /F'
  ExecWait 'taskkill /IM ${LEGACY_HELPER_EXE} /F'

  ; 托盘程序自己的开机自启（设置里打开时写在 HKCU）。
  ; 同时清理未完成迁移的旧登录项。
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${BUNDLE_ID}"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${LEGACY_BUNDLE_ID}"

  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"

  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\${HELPER_EXE}"
  Delete "$INSTDIR\LICENSE"
  ; 覆盖安装过旧版的目录里可能还留着老名字的 exe
  Delete "$INSTDIR\${LEGACY_APP_EXE}"
  Delete "$INSTDIR\${LEGACY_HELPER_EXE}"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${BUNDLE_ID}"
  DeleteRegKey HKLM "Software\${BUNDLE_ID}"
  ; 改名前的旧注册项也一起清理。
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${LEGACY_BUNDLE_ID}"
  DeleteRegKey HKLM "Software\${LEGACY_BUNDLE_ID}"

  ; 快捷方式和 exe 都删了，同样让 shell 立刻丢掉缓存
  System::Call 'shell32::SHChangeNotify(i 0x8000000, i 0, i 0, i 0)'

  ; 历史数据默认保留：卸载工具不该顺手把用户一个月的流量记录删了
  MessageBox MB_YESNO|MB_ICONQUESTION "是否同时删除历史数据、设置和旧版迁移备份？$\r$\n（%LOCALAPPDATA%\${DATA_DIR} 及旧版数据目录）" /SD IDNO IDNO KeepData
  RMDir /r "$LOCALAPPDATA\${DATA_DIR}"
  RMDir /r "$LOCALAPPDATA\${LEGACY_BUNDLE_ID}"
  KeepData:
SectionEnd
