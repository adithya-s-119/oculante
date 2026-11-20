Unicode true

!include "MUI2.nsh"
!include "build-config.nsh"

Name "Pictor"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\Pictor"
InstallDirRegKey HKCU "Software\Pictor" "InstallDir"
RequestExecutionLevel user

SetCompressor /SOLID lzma
SetCompressorDictSize 64
BrandingText "Pictor ${VERSION}"
Icon "${ICON}"
UninstallIcon "${ICON}"
ShowInstDetails show
ShowUninstDetails show

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=1033 "ProductName" "Pictor"
VIAddVersionKey /LANG=1033 "FileDescription" "Pictor Photo Library Setup"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "LegalCopyright" "MIT License"
VIAddVersionKey /LANG=1033 "CompanyName" "adithya-s-119"

!define MUI_ABORTWARNING
!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_WELCOMEPAGE_TITLE "Welcome to Pictor"
!define MUI_WELCOMEPAGE_TEXT "Pictor is a fast, frosted-glass photo library and viewer.$\r$\n$\r$\nThis will install Pictor for the current user. No administrator rights are required.$\r$\n$\r$\nYour photos stay on your PC."
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${LICENSE}"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\pictor.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch Pictor"
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Pictor"

!macro RegisterAssoc EXT
    WriteRegStr HKCU "Software\Classes\${EXT}\OpenWithProgids" "Pictor.Image" ""
!macroend

!macro UnregisterAssoc EXT
    DeleteRegValue HKCU "Software\Classes\${EXT}\OpenWithProgids" "Pictor.Image"
!macroend

Section "Pictor Photo Library" SEC_MAIN
    SectionIn RO

    SetOutPath "$INSTDIR"
    File "${SOURCE}\pictor.exe"
    File "${SOURCE}\libstdc++-6.dll"
    File "${SOURCE}\libgcc_s_seh-1.dll"
    File "${SOURCE}\libwinpthread-1.dll"
    File "${SOURCE}\README.md"
    File "${SOURCE}\LICENSE"
    File "${SOURCE}\THIRD-PARTY-NOTICES.txt"

    WriteUninstaller "$INSTDIR\Uninstall.exe"

    CreateDirectory "$SMPROGRAMS\Pictor"
    CreateShortcut "$SMPROGRAMS\Pictor\Pictor.lnk" "$INSTDIR\pictor.exe"
    CreateShortcut "$SMPROGRAMS\Pictor\Uninstall Pictor.lnk" "$INSTDIR\Uninstall.exe"

    WriteRegStr HKCU "Software\Pictor" "InstallDir" "$INSTDIR"
    WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "Pictor"
    WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
    WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "adithya-s-119"
    WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\pictor.exe"
    WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
    WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
    WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
    WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
    WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
    WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" 65000
SectionEnd

Section /o "Desktop shortcut" SEC_DESKTOP
    CreateShortcut "$DESKTOP\Pictor.lnk" "$INSTDIR\pictor.exe"
SectionEnd

Section /o "Add Pictor to the Open with menu" SEC_ASSOC
    WriteRegStr HKCU "Software\Classes\Pictor.Image" "" "Pictor image"
    WriteRegStr HKCU "Software\Classes\Pictor.Image\DefaultIcon" "" "$INSTDIR\pictor.exe,0"
    WriteRegStr HKCU "Software\Classes\Pictor.Image\shell\open\command" "" '"$INSTDIR\pictor.exe" "%1"'
    WriteRegStr HKCU "Software\Classes\Applications\pictor.exe\shell\open\command" "" '"$INSTDIR\pictor.exe" "%1"'

    !insertmacro RegisterAssoc ".bmp"
    !insertmacro RegisterAssoc ".gif"
    !insertmacro RegisterAssoc ".heic"
    !insertmacro RegisterAssoc ".jpeg"
    !insertmacro RegisterAssoc ".jpg"
    !insertmacro RegisterAssoc ".jxl"
    !insertmacro RegisterAssoc ".png"
    !insertmacro RegisterAssoc ".svg"
    !insertmacro RegisterAssoc ".tif"
    !insertmacro RegisterAssoc ".tiff"
    !insertmacro RegisterAssoc ".webp"

    WriteRegStr HKCU "Software\Classes\Applications\pictor.exe\SupportedTypes" ".bmp" ""
    WriteRegStr HKCU "Software\Classes\Applications\pictor.exe\SupportedTypes" ".gif" ""
    WriteRegStr HKCU "Software\Classes\Applications\pictor.exe\SupportedTypes" ".heic" ""
    WriteRegStr HKCU "Software\Classes\Applications\pictor.exe\SupportedTypes" ".jpeg" ""
    WriteRegStr HKCU "Software\Classes\Applications\pictor.exe\SupportedTypes" ".jpg" ""
    WriteRegStr HKCU "Software\Classes\Applications\pictor.exe\SupportedTypes" ".jxl" ""
    WriteRegStr HKCU "Software\Classes\Applications\pictor.exe\SupportedTypes" ".png" ""
    WriteRegStr HKCU "Software\Classes\Applications\pictor.exe\SupportedTypes" ".svg" ""
    WriteRegStr HKCU "Software\Classes\Applications\pictor.exe\SupportedTypes" ".tif" ""
    WriteRegStr HKCU "Software\Classes\Applications\pictor.exe\SupportedTypes" ".tiff" ""
    WriteRegStr HKCU "Software\Classes\Applications\pictor.exe\SupportedTypes" ".webp" ""

    System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, i 0, i 0)'
SectionEnd

Section "Uninstall"
    Delete "$INSTDIR\pictor.exe"
    Delete "$INSTDIR\libstdc++-6.dll"
    Delete "$INSTDIR\libgcc_s_seh-1.dll"
    Delete "$INSTDIR\libwinpthread-1.dll"
    Delete "$INSTDIR\README.md"
    Delete "$INSTDIR\LICENSE"
    Delete "$INSTDIR\THIRD-PARTY-NOTICES.txt"
    Delete "$INSTDIR\Uninstall.exe"
    RMDir "$INSTDIR"

    Delete "$SMPROGRAMS\Pictor\Pictor.lnk"
    Delete "$SMPROGRAMS\Pictor\Uninstall Pictor.lnk"
    RMDir "$SMPROGRAMS\Pictor"
    Delete "$DESKTOP\Pictor.lnk"

    DeleteRegKey HKCU "Software\Classes\Pictor.Image"
    DeleteRegKey HKCU "Software\Classes\Applications\pictor.exe"
    !insertmacro UnregisterAssoc ".bmp"
    !insertmacro UnregisterAssoc ".gif"
    !insertmacro UnregisterAssoc ".heic"
    !insertmacro UnregisterAssoc ".jpeg"
    !insertmacro UnregisterAssoc ".jpg"
    !insertmacro UnregisterAssoc ".jxl"
    !insertmacro UnregisterAssoc ".png"
    !insertmacro UnregisterAssoc ".svg"
    !insertmacro UnregisterAssoc ".tif"
    !insertmacro UnregisterAssoc ".tiff"
    !insertmacro UnregisterAssoc ".webp"

    System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, i 0, i 0)'

    DeleteRegKey HKCU "${UNINST_KEY}"
    DeleteRegKey HKCU "Software\Pictor"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
    !insertmacro MUI_DESCRIPTION_TEXT ${SEC_MAIN} "The Pictor application and the runtime libraries it needs."
    !insertmacro MUI_DESCRIPTION_TEXT ${SEC_DESKTOP} "Create a Pictor shortcut on the desktop."
    !insertmacro MUI_DESCRIPTION_TEXT ${SEC_ASSOC} "Add Pictor to the Windows Open with menu for common image formats. This does not change your default viewer."
!insertmacro MUI_FUNCTION_DESCRIPTION_END
