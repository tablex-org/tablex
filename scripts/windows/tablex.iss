; Inno Setup 6 script of the tablex installer (scripts/package-windows-installer.ps1 compiles it).
;
; Per user: no administrator rights, no UAC prompt, the program goes to %LOCALAPPDATA%\Programs\tablex.
; It installs the files of the portable zip (tablex.exe, tablex-mcp.exe, LICENSE, licenses\, README.txt)
; from SourceDir, which is that zip unpacked. Nothing of the user's is in the install folder: the app keeps
; its data (saved connections, settings, logs, the driver store) in %APPDATA%\tablex, so an uninstall,
; which removes only what this installer put in place, leaves all of it.
;
; Defined on the command line (ISCC /D...):
;   AppVersion  the app version, major.minor.patch (apps/tablex-gpui/Cargo.toml)
;   SourceDir   the unpacked zip
;   IconFile    apps/tablex-gpui/assets/AppIcon.ico

#ifndef AppVersion
  #error "pass /DAppVersion=<version>"
#endif
#ifndef SourceDir
  #error "pass /DSourceDir=<the unpacked zip>"
#endif
#ifndef IconFile
  #error "pass /DIconFile=<AppIcon.ico>"
#endif

[Setup]
; The identity of tablex for Windows: a later version with the same AppId upgrades this one in place and
; shares its entry in Apps > Installed apps. Generated once. NEVER change it.
AppId={{D4E7242A-991A-46AC-819A-6FF566E5906C}
AppName=tablex
AppVersion={#AppVersion}
AppPublisher=tablex
AppPublisherURL=https://github.com/tablex-org/tablex
AppSupportURL=https://github.com/tablex-org/tablex/issues
AppUpdatesURL=https://github.com/tablex-org/tablex/releases
VersionInfoVersion={#AppVersion}
VersionInfoProductName=tablex
VersionInfoDescription=tablex setup
; With the lowest privileges {autopf} is {localappdata}\Programs, {autoprograms} the user's Start menu.
PrivilegesRequired=lowest
DefaultDirName={autopf}\tablex
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; A running tablex is closed (Restart Manager) before its files are replaced or removed.
CloseApplications=yes
RestartApplications=no
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\tablex.exe
UninstallDisplayName=tablex
OutputBaseFilename=tablex-{#AppVersion}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\tablex.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\tablex-mcp.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\tablex"; Filename: "{app}\tablex.exe"
Name: "{autodesktop}\tablex"; Filename: "{app}\tablex.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\tablex.exe"; Description: "{cm:LaunchProgram,tablex}"; Flags: nowait postinstall skipifsilent
