; The Windows installer of Omnigraph Studio (spec 027), built by script/bundle-windows with
; Inno Setup 6:
;
;   ISCC -DVersion=0.4.1 -DNumericVersion=0.4.1 -DSourceDir=<folder> -O<out> -F<name> omnigraph-studio.iss
;
; SourceDir holds what the zip holds: omnigraph-studio.exe, LICENSE and THIRD-PARTY-NOTICES.md.
;
; `omnigraph studio` finds Studio by the uninstall key this AppId names,
; Software\Microsoft\Windows\CurrentVersion\Uninstall\com.modernrelay.omnigraph-studio_is1, in
; HKCU (or HKLM after an /ALLUSERS install). Never change the AppId: it is how the CLI finds
; Studio and how a newer installer recognises the copy it upgrades.

#ifndef Version
  #error Pass -DVersion=<version>
#endif
#ifndef NumericVersion
  #error Pass -DNumericVersion=<major.minor.patch>
#endif
#ifndef SourceDir
  #error Pass -DSourceDir=<folder with omnigraph-studio.exe>
#endif

[Setup]
AppId=com.modernrelay.omnigraph-studio
AppName=Omnigraph Studio
AppVersion={#Version}
AppVerName=Omnigraph Studio {#Version}
AppPublisher=ModernRelay
AppPublisherURL=https://github.com/ModernRelay/omnigraph-studio
AppSupportURL=https://github.com/ModernRelay/omnigraph-studio/issues
AppUpdatesURL=https://github.com/ModernRelay/omnigraph-studio/releases
AppCopyright=ModernRelay. Licensed under GPL-3.0-or-later.
VersionInfoVersion={#NumericVersion}
VersionInfoProductName=Omnigraph Studio
VersionInfoProductTextVersion={#Version}
VersionInfoDescription=Omnigraph Studio installer
; Per user, without administrator rights: %LOCALAPPDATA%\Programs and HKCU. /ALLUSERS on the
; command line installs for everyone, into Program Files and HKLM, and asks for admin.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline
DefaultDirName={autopf}\Omnigraph Studio
DisableDirPage=auto
DisableProgramGroupPage=yes
DefaultGroupName=Omnigraph Studio
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 1809, as the zip (spec 001, "Shipping").
MinVersion=10.0.17763
UninstallDisplayName=Omnigraph Studio
UninstallDisplayIcon={app}\omnigraph-studio.exe
; An upgrade closes running copies through the Restart Manager instead of failing on a locked
; executable.
CloseApplications=yes
RestartApplications=no
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
SetupLogging=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\omnigraph-studio.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
; A debug bundle has no notices unless cargo-about is installed.
Source: "{#SourceDir}\THIRD-PARTY-NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{autoprograms}\Omnigraph Studio"; Filename: "{app}\omnigraph-studio.exe"
Name: "{autodesktop}\Omnigraph Studio"; Filename: "{app}\omnigraph-studio.exe"; Tasks: desktopicon

[Registry]
; `start omnigraph-studio` and ShellExecute open Studio by its executable's name.
Root: HKA; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\omnigraph-studio.exe"; ValueType: string; ValueName: ""; ValueData: "{app}\omnigraph-studio.exe"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\omnigraph-studio.exe"; ValueType: string; ValueName: "Path"; ValueData: "{app}"

[Run]
; Offered on the last page; a silent install (the CLI's, the updater's) never opens Studio.
Filename: "{app}\omnigraph-studio.exe"; Description: "{cm:LaunchProgram,Omnigraph Studio}"; Flags: nowait postinstall skipifsilent

; Uninstalling leaves %APPDATA%\Omnigraph Studio: Studio wrote it, and a reinstall finds the
; connections and conversations it holds.
