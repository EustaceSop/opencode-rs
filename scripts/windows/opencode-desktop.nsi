; Per-user installer for the Rust desktop app, after the original's electron-builder NSIS target
; (packages/desktop/electron-builder.config.ts: nsis oneClick, perMachine false, protocols opencode).
; Built by scripts/package-desktop-installer.ps1, which passes STAGE, OUTFILE and VERSION.
;
; Switches: /S silent, /D=<dir> install directory (NSIS), /NOSYSTEM files only: no Start menu entries and no
; registry writes, so a test install into a scratch directory leaves the machine as it was.

Unicode true
ManifestDPIAware true
RequestExecutionLevel user
SetCompressor /SOLID lzma

!ifndef STAGE
  !error "define STAGE: the directory holding opencode-desktop.exe, opencode.exe and icon.ico"
!endif
!ifndef OUTFILE
  !error "define OUTFILE"
!endif
!ifndef VERSION
  !define VERSION "0.0.0"
!endif

!define PRODUCT "OpenCode"
!define APP_ID "ai.opencode.desktop.rs"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"
!define PROTOCOL_KEY "Software\Classes\opencode"
!define AUMID_KEY "Software\Classes\AppUserModelId\${APP_ID}"
!define OPEN_COMMAND '"$INSTDIR\opencode-desktop.exe" "%1"'

!include "FileFunc.nsh"
!include "LogicLib.nsh"

Name "${PRODUCT}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\opencode-desktop-rs"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
Icon "${STAGE}\icon.ico"
UninstallIcon "${STAGE}\icon.ico"
ShowInstDetails nevershow
ShowUninstDetails nevershow
VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${PRODUCT}"
VIAddVersionKey "FileDescription" "${PRODUCT} installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Licensed under the MIT License"

; One click, like oneClick: true.
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Var NoSystem

Function .onInit
  StrCpy $NoSystem 0
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/NOSYSTEM" $1
  ${IfNot} ${Errors}
    StrCpy $NoSystem 1
  ${EndIf}
FunctionEnd

Function un.onInit
  StrCpy $NoSystem 0
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/NOSYSTEM" $1
  ${IfNot} ${Errors}
    StrCpy $NoSystem 1
  ${EndIf}
FunctionEnd

Section "Install"
  SetShellVarContext current
  SetOutPath "$INSTDIR"
  File "${STAGE}\opencode-desktop.exe"
  File "${STAGE}\opencode.exe"
  File "${STAGE}\icon.ico"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  ${If} $NoSystem == 1
    Return
  ${EndIf}

  ; Shortcuts start in the home directory, as the original does `process.chdir(homedir())`.
  CreateDirectory "$SMPROGRAMS\${PRODUCT}"
  SetOutPath "$PROFILE"
  CreateShortcut "$SMPROGRAMS\${PRODUCT}\${PRODUCT}.lnk" "$INSTDIR\opencode-desktop.exe" "" "$INSTDIR\icon.ico" 0
  CreateShortcut "$SMPROGRAMS\${PRODUCT}\${PRODUCT} CLI.lnk" "$INSTDIR\opencode.exe" "" "$INSTDIR\opencode.exe" 0
  CreateShortcut "$SMPROGRAMS\${PRODUCT}\Uninstall ${PRODUCT}.lnk" "$INSTDIR\uninstall.exe" "" "$INSTDIR\uninstall.exe" 0

  ; opencode:// links; registered here only, never by the app at runtime. A handler another OpenCode registered
  ; (the Electron app's) is remembered and put back by the uninstaller.
  ReadRegStr $0 HKCU "${PROTOCOL_KEY}\shell\open\command" ""
  ${If} $0 != ""
  ${AndIf} $0 != '${OPEN_COMMAND}'
    WriteRegStr HKCU "${UNINSTALL_KEY}" "PreviousOpenCommand" $0
    ReadRegStr $1 HKCU "${PROTOCOL_KEY}\DefaultIcon" ""
    WriteRegStr HKCU "${UNINSTALL_KEY}" "PreviousDefaultIcon" $1
  ${EndIf}
  WriteRegStr HKCU "${PROTOCOL_KEY}" "" "URL:${PRODUCT}"
  WriteRegStr HKCU "${PROTOCOL_KEY}" "URL Protocol" ""
  WriteRegStr HKCU "${PROTOCOL_KEY}\DefaultIcon" "" "$INSTDIR\opencode-desktop.exe,0"
  WriteRegStr HKCU "${PROTOCOL_KEY}\shell\open\command" "" '${OPEN_COMMAND}'

  ; Name and icon of the app's toast notifications (the app's AppUserModelID).
  WriteRegStr HKCU "${AUMID_KEY}" "DisplayName" "${PRODUCT}"
  WriteRegStr HKCU "${AUMID_KEY}" "IconUri" "$INSTDIR\icon.ico"

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${PRODUCT}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\opencode-desktop.exe,0"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" "$0"
SectionEnd

; User data (sessions, `desktop-rs.json`) lives in the opencode data and state directories and is kept.
Section "Uninstall"
  SetShellVarContext current
  Delete "$INSTDIR\opencode-desktop.exe"
  Delete "$INSTDIR\opencode.exe"
  Delete "$INSTDIR\icon.ico"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  ${If} $NoSystem == 1
    Return
  ${EndIf}

  Delete "$SMPROGRAMS\${PRODUCT}\${PRODUCT}.lnk"
  Delete "$SMPROGRAMS\${PRODUCT}\${PRODUCT} CLI.lnk"
  Delete "$SMPROGRAMS\${PRODUCT}\Uninstall ${PRODUCT}.lnk"
  RMDir "$SMPROGRAMS\${PRODUCT}"

  ; Another OpenCode may have taken the scheme since; only this installation's registration is undone.
  ReadRegStr $0 HKCU "${PROTOCOL_KEY}\shell\open\command" ""
  ${If} $0 == '${OPEN_COMMAND}'
    ReadRegStr $1 HKCU "${UNINSTALL_KEY}" "PreviousOpenCommand"
    ${If} $1 != ""
      WriteRegStr HKCU "${PROTOCOL_KEY}\shell\open\command" "" $1
      ReadRegStr $2 HKCU "${UNINSTALL_KEY}" "PreviousDefaultIcon"
      WriteRegStr HKCU "${PROTOCOL_KEY}\DefaultIcon" "" $2
    ${Else}
      DeleteRegKey HKCU "${PROTOCOL_KEY}"
    ${EndIf}
  ${EndIf}
  DeleteRegKey HKCU "${AUMID_KEY}"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
