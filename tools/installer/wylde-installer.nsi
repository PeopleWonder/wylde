; wylde-installer.nsi -- per-user (no-UAC) NSIS installer for the Wylde alpha.
;
; This script is NOT meant to be run by hand. `tools/installer/build-installer.ps1`
; stages the app tree into a build dir, then invokes makensis with the
; /D defines below. See docs/installer.md for the full picture.
;
; DESIGN (see docs/installer.md and Core/GUI/installer/README.md):
;   * Per-user install, NO elevation. `RequestExecutionLevel user` means
;     SmartScreen may warn (unsigned alpha) but Windows never raises a UAC
;     prompt. Install root is %LOCALAPPDATA%\Programs\Wylde -- the same
;     per-user convention VS Code uses, and the location the gpui-era
;     installer README already committed to.
;   * The Start-menu shortcut launches launch_wylde.ps1 (which boots the
;     Lifecycle daemon FIRST, waits for its pipe, then starts wylde-gui.exe).
;     It deliberately does NOT point straight at wylde-gui.exe -- doing so
;     leaves the backend down and every required-service panel shows a stub
;     (this is the exact bug Core/GUI/installer/fix_desktop_shortcut.ps1 was
;     written to undo).
;   * The self-updater (wylde-updater, Phase 12.5) swaps the running binary
;     in place on the NEXT launch. Windows can't overwrite a running .exe, so
;     the updater renames-aside; that only needs the install dir to be
;     user-writable, which %LOCALAPPDATA% always is. The installer does not
;     have to do anything special for the updater beyond installing here and
;     dropping version.txt.

;--------------------------------------------------------------------
; Build-time defines (overridable from build-installer.ps1 via /D...)
;--------------------------------------------------------------------
!ifndef VERSION
  !define VERSION "0.2.0"
!endif
; VIProductVersion (below) demands a strictly numeric X.X.X.X string, so a
; SemVer pre-release tag like "0.1.0-alpha.1" can't be fed to it directly.
; build-installer.ps1 passes the numeric core via /DVI_VERSION (the part
; before any "-suffix"); this default keeps a hand `makensis` run working.
!ifndef VI_VERSION
  !define VI_VERSION "0.2.0"
!endif
!ifndef STAGE_DIR
  ; Default matches build-installer.ps1's staging location.
  !define STAGE_DIR "stage"
!endif
!ifndef OUT_FILE
  !define OUT_FILE "WyldeSetup-${VERSION}.exe"
!endif
!ifndef ICON_FILE
  !define ICON_FILE "${STAGE_DIR}\Core\GUI\assets\icons\icon.ico"
!endif
!ifndef LICENSE_FILE
  !define LICENSE_FILE "${STAGE_DIR}\LICENSE"
!endif

!define PRODUCT_NAME      "Wylde"
!define PRODUCT_PUBLISHER "Wylde"
!define PRODUCT_WEB       "https://wyldebot.com"
; The exe whose embedded icon the launcher shortcut should show.
!define GUI_EXE_REL       "Core\GUI\target\release\wylde-gui.exe"
; Per-user uninstall registry key (HKCU -> no admin needed).
!define UNINST_KEY        "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}"
!define RUN_KEY           "Software\Microsoft\Windows\CurrentVersion\Run"

;--------------------------------------------------------------------
; Includes
;--------------------------------------------------------------------
!include "MUI2.nsh"
!include "FileFunc.nsh"   ; ${GetSize}

;--------------------------------------------------------------------
; General
;--------------------------------------------------------------------
Name "${PRODUCT_NAME} ${VERSION}"
OutFile "${OUT_FILE}"
Unicode true

; Per-user: no UAC. SetShellVarContext is "current" by default under this
; exec level, so $SMPROGRAMS / $DESKTOP resolve to the user's own folders
; and HKCU is the implicit registry root for our keys below.
RequestExecutionLevel user

; %LOCALAPPDATA%\Programs\Wylde. $LOCALAPPDATA is always user-writable, which
; is what the self-updater's rename-aside swap needs.
InstallDir "$LOCALAPPDATA\Programs\${PRODUCT_NAME}"
; If a previous per-user install recorded its location, reuse it.
InstallDirRegKey HKCU "Software\${PRODUCT_NAME}" "InstallDir"

ShowInstDetails show
ShowUnInstDetails show
SetCompressor /SOLID lzma

;--------------------------------------------------------------------
; UI
;--------------------------------------------------------------------
!define MUI_ICON   "${ICON_FILE}"
!define MUI_UNICON "${ICON_FILE}"
!define MUI_ABORTWARNING

; Offer to launch right after install via the daemon-first launcher.
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Launch Wylde now"
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchWylde

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${LICENSE_FILE}"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

;--------------------------------------------------------------------
; Version info on the produced setup .exe
;--------------------------------------------------------------------
; VIProductVersion must be numeric X.X.X.X — use the numeric-only VI_VERSION,
; never the (possibly pre-release-suffixed) display VERSION.
VIProductVersion "${VI_VERSION}.0"
VIAddVersionKey "ProductName"     "${PRODUCT_NAME}"
VIAddVersionKey "ProductVersion"  "${VERSION}"
VIAddVersionKey "CompanyName"     "${PRODUCT_PUBLISHER}"
VIAddVersionKey "FileDescription" "${PRODUCT_NAME} per-user installer"
VIAddVersionKey "FileVersion"     "${VERSION}"
VIAddVersionKey "LegalCopyright"  "GPL-3.0-or-later"

;--------------------------------------------------------------------
; Helper: the daemon-first launch command used by shortcuts + autostart.
;--------------------------------------------------------------------
!macro LauncherArgs
  ; powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden
  ;   -File "<INSTDIR>\launch_wylde.ps1"
  ; (kept in a macro so the shortcut, the Run key, and the finish-page
  ;  launch never drift apart)
!macroend

Function LaunchWylde
  Exec '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "$INSTDIR\launch_wylde.ps1"'
FunctionEnd

;--------------------------------------------------------------------
; Sections
;--------------------------------------------------------------------
Section "Wylde (required)" SEC_CORE
  SectionIn RO                       ; cannot be unchecked
  SetOutPath "$INSTDIR"

  ; Lay down the entire staged app tree (binaries + service trees + docs +
  ; launcher). build-installer.ps1 decides exactly what STAGE_DIR contains;
  ; see docs/installer.md "What gets bundled".
  File /r "${STAGE_DIR}\*.*"

  ; version.txt -- the self-updater and support tooling read this to learn
  ; what's installed without parsing the binary.
  FileOpen $0 "$INSTDIR\version.txt" w
  FileWrite $0 "${VERSION}$\r$\n"
  FileClose $0

  ; Remember where we installed (for InstallDirRegKey + uninstaller).
  WriteRegStr HKCU "Software\${PRODUCT_NAME}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\${PRODUCT_NAME}" "Version"    "${VERSION}"

  ; --- Per-user uninstall entry (Settings -> Apps lists it, no admin) ---
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr   HKCU "${UNINST_KEY}" "DisplayName"     "${PRODUCT_NAME}"
  WriteRegStr   HKCU "${UNINST_KEY}" "DisplayVersion"  "${VERSION}"
  WriteRegStr   HKCU "${UNINST_KEY}" "Publisher"       "${PRODUCT_PUBLISHER}"
  WriteRegStr   HKCU "${UNINST_KEY}" "URLInfoAbout"    "${PRODUCT_WEB}"
  WriteRegStr   HKCU "${UNINST_KEY}" "DisplayIcon"     "$INSTDIR\${GUI_EXE_REL}"
  WriteRegStr   HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr   HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr   HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1

  ; EstimatedSize (KB) so Apps shows a real footprint.
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "Start Menu shortcut" SEC_STARTMENU
  ; Daemon-first launcher (see header note + LaunchWylde). Icon is pulled
  ; from the gpui binary so the tile looks right.
  CreateShortcut "$SMPROGRAMS\${PRODUCT_NAME}.lnk" \
    "$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" \
    '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "$INSTDIR\launch_wylde.ps1"' \
    "$INSTDIR\${GUI_EXE_REL}" 0 SW_SHOWNORMAL "" "Launch ${PRODUCT_NAME} (daemon-first)"
SectionEnd

Section "Desktop shortcut" SEC_DESKTOP
  CreateShortcut "$DESKTOP\${PRODUCT_NAME}.lnk" \
    "$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" \
    '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "$INSTDIR\launch_wylde.ps1"' \
    "$INSTDIR\${GUI_EXE_REL}" 0 SW_SHOWNORMAL "" "Launch ${PRODUCT_NAME} (daemon-first)"
SectionEnd

Section /o "Start Wylde when I sign in" SEC_AUTOSTART
  ; Unchecked by default (the /o). Writes the same daemon-first command to
  ; the per-user Run key. Aaron's stack already supports autostart via the
  ; auto-launch crate; this is the installer-managed equivalent.
  WriteRegStr HKCU "${RUN_KEY}" "${PRODUCT_NAME}" \
    '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "$INSTDIR\launch_wylde.ps1"'
SectionEnd

;--------------------------------------------------------------------
; Component descriptions
;--------------------------------------------------------------------
LangString DESC_CORE      ${LANG_ENGLISH} "The Wylde app, backend service binaries, and runtime files. Required."
LangString DESC_STARTMENU ${LANG_ENGLISH} "Add a Wylde shortcut to the Start menu."
LangString DESC_DESKTOP   ${LANG_ENGLISH} "Add a Wylde shortcut to the desktop."
LangString DESC_AUTOSTART ${LANG_ENGLISH} "Launch Wylde automatically when you sign in to Windows."

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_CORE}      $(DESC_CORE)
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_STARTMENU} $(DESC_STARTMENU)
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_DESKTOP}   $(DESC_DESKTOP)
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_AUTOSTART} $(DESC_AUTOSTART)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

;--------------------------------------------------------------------
; Uninstaller
;--------------------------------------------------------------------
Section "Uninstall"
  ; Shortcuts
  Delete "$SMPROGRAMS\${PRODUCT_NAME}.lnk"
  Delete "$DESKTOP\${PRODUCT_NAME}.lnk"

  ; Autostart + per-user registry
  DeleteRegValue HKCU "${RUN_KEY}" "${PRODUCT_NAME}"
  DeleteRegKey   HKCU "${UNINST_KEY}"
  DeleteRegKey   HKCU "Software\${PRODUCT_NAME}"

  ; Files. RMDir /r is recursive; the install root is entirely ours
  ; (%LOCALAPPDATA%\Programs\Wylde), so this is safe.
  RMDir /r "$INSTDIR"
SectionEnd
