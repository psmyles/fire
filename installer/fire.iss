; Fire — Inno Setup installer script.
;
; This script is NOT meant to be compiled directly. Build it through
; scripts\build-installer.ps1, which parses product.json (the single source of product
; metadata) and writes product.generated.iss with the matching #define directives. That keeps
; the product name / version / publisher in exactly one place: bump product.json, re-run the
; script, and the value flows into the application (via build.rs) and this installer alike.
;
; product.generated.iss is produced by the build script (and git-ignored). If you are seeing a
; "could not open include file" error, you compiled this directly instead of via the script.
#include "product.generated.iss"

; Guards: confirm the generated include actually defined what we need (it always should).
#ifndef MyAppName
  #error product.generated.iss did not define MyAppName — re-run scripts\build-installer.ps1
#endif
#ifndef MyAppVersion
  #error product.generated.iss did not define MyAppVersion — re-run scripts\build-installer.ps1
#endif
#ifndef MyExeSource
  #error product.generated.iss did not define MyExeSource — re-run scripts\build-installer.ps1
#endif
#ifndef MyIconSource
  #error product.generated.iss did not define MyIconSource — re-run scripts\build-installer.ps1
#endif
#ifndef MyOutputDir
  #define MyOutputDir "dist"
#endif

; "Fire.Image" was the original single shared ProgID. It is now legacy: kept only so the
; installer can delete it on upgrade (the Registry section below removes it). Each format now
; gets its OWN ProgID ("Fire.png", "Fire.tga", …) whose friendly type name — the class key's
; default value, which the shell reads for Explorer's "Type" column — names the format ("PNG
; image", "Truevision TGA image", …) instead of one undifferentiated "Fire Image". Versionless
; on purpose: there is only ever one installed Fire.
#define FireProgId "Fire.Image"

[Setup]
; A stable AppId keeps upgrades/uninstall pointing at the same entry across versions. Never
; change it once shipped.
AppId={{8E9B2C4D-3F1A-4B6E-9C2D-7A5F1E0B8D34}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
AppCopyright={#MyAppCopyright}
VersionInfoVersion={#MyAppVersion}
VersionInfoProductName={#MyAppName}
VersionInfoProductVersion={#MyAppVersion}
VersionInfoCompany={#MyAppPublisher}
VersionInfoCopyright={#MyAppCopyright}

; Per-user install (no admin prompt) to match Fire's HKCU file-association model. {autopf}
; resolves to the per-user Programs folder under lowest privileges.
PrivilegesRequired=lowest
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
UninstallDisplayName={#MyAppName} {#MyAppVersion}
UninstallDisplayIcon={app}\{#MyAppExe}
SetupIconFile={#MyIconSource}
; Tell the shell to refresh icons/associations when our [Registry] entries change.
ChangesAssociations=yes
WizardStyle=modern

; Fire's own MIT license, shown on the wizard's license page. Paths here are relative to this
; .iss file, so ".." is the repo root.
LicenseFile=..\LICENSE
Compression=lzma2
SolidCompression=yes
OutputDir={#MyOutputDir}
OutputBaseFilename={#MyAppName}-{#MyAppVersion}-Setup

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#MyExeSource}"; DestDir: "{app}"; DestName: "{#MyAppExe}"; Flags: ignoreversion

; --- license notices -----------------------------------------------------------------------
; fire.exe is statically linked, so the binary installed above contains code from ~130 other
; projects. Their licenses (MIT/BSD attribution, and LGPL-3.0 for libheif/libde265) require the
; notices to travel with the binary, so a user who only ever runs Setup.exe still receives them.
; Not optional and not behind a task: dropping these makes the installed copy non-compliant.
; Paths are relative to this .iss file, so ".." is the repo root.
Source: "..\LICENSE";                 DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\THIRD-PARTY-NOTICES.md";  DestDir: "{app}"; Flags: ignoreversion
Source: "..\CREDITS.md";              DestDir: "{app}"; Flags: ignoreversion
Source: "..\licenses\*.txt";          DestDir: "{app}\licenses"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExe}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExe}"; Tasks: desktopicon

[Tasks]
; --- file associations: the headline option, one entry per format plus an "All" master ---
; The parent "assoc" is the "All supported image formats" toggle: checking it checks every
; child (Inno propagates a parent check to children), and individual formats can be toggled.
; Unchecked by default so an install never silently steals associations the user didn't pick.
Name: "assoc";          Description: "All supported image formats"; GroupDescription: "Set {#MyAppName} as the default image viewer for:"; Flags: unchecked
Name: "assoc\png";      Description: "PNG image (.png)"
Name: "assoc\jpeg";     Description: "JPEG image (.jpg, .jpeg, .jpe, .jfif)"
Name: "assoc\gif";      Description: "GIF image (.gif)"
Name: "assoc\bmp";      Description: "Bitmap image (.bmp, .dib)"
Name: "assoc\tiff";     Description: "TIFF image (.tif, .tiff)"
Name: "assoc\webp";     Description: "WebP image (.webp)"
Name: "assoc\ico";      Description: "Icon (.ico)"
Name: "assoc\tga";      Description: "Truevision TGA (.tga)"
Name: "assoc\qoi";      Description: "QOI image (.qoi)"
Name: "assoc\netpbm";   Description: "Netpbm (.ppm, .pgm, .pbm, .pnm)"
Name: "assoc\farbfeld"; Description: "Farbfeld (.ff)"
Name: "assoc\jxl";      Description: "JPEG XL (.jxl)"
Name: "assoc\hdr";      Description: "Radiance HDR (.hdr)"
Name: "assoc\exr";      Description: "OpenEXR (.exr)"
Name: "assoc\psd";      Description: "Photoshop document (.psd, .psb)"
Name: "assoc\heif";     Description: "HEIF / HEIC image (.heic, .heif)"
Name: "assoc\avif";     Description: "AVIF image (.avif)"
Name: "assoc\raw";      Description: "Camera raw — embedded preview (.cr2, .cr3, .nef, .arw, .dng, .raf, .orf, .rw2, …)"
; --- shortcuts ---
Name: "desktopicon";    Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Registry]
; --- remove the legacy single shared ProgID from older installs -------------------------
; Every format now has its own ProgID (below); "Fire.Image" is obsolete. deletekey is a
; no-op on fresh installs (key absent) and, on upgrade, clears the old "Fire Image" type.
Root: HKCU; Subkey: "Software\Classes\{#FireProgId}"; ValueType: none; Flags: deletekey

; --- Default Programs capabilities (so Fire appears in Settings > Default apps) ---
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "{#MyAppName} — fast image viewer"
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities"; ValueType: string; ValueName: "ApplicationIcon"; ValueData: "{app}\{#MyAppExe},0"
Root: HKCU; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "{#MyAppName}"; ValueData: "Software\{#MyAppName}\Capabilities"; Flags: uninsdeletevalue

; --- per-format ProgIDs + associations ---------------------------------------------------
; For each selected format we register one ProgID class carrying:
;   - its friendly type name (the class key's default value) — this is what Explorer shows
;     in the "Type" column, so each format reads as e.g. "PNG image" / "Truevision TGA image";
;   - DefaultIcon (the Fire icon) and shell\open\command.
; And for every extension of that format:
;   - OpenWithProgids — adds Fire to the file's "Open with" list (additive, always safe).
;   - the .ext default ProgID — makes Fire the default where Windows has no protected
;     UserChoice yet (it never overrides an explicit user choice, by design).
;   - Capabilities\FileAssociations — lists the type under Fire in Default apps.

; PNG image — Fire.png
Root: HKCU; Subkey: "Software\Classes\Fire.png"; ValueType: string; ValueName: ""; ValueData: "PNG image"; Flags: uninsdeletekey; Tasks: assoc\png
Root: HKCU; Subkey: "Software\Classes\Fire.png\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\png
Root: HKCU; Subkey: "Software\Classes\Fire.png\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\png
Root: HKCU; Subkey: "Software\Classes\Fire.png\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\png
Root: HKCU; Subkey: "Software\Classes\.png\OpenWithProgids"; ValueType: none; ValueName: "Fire.png"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\png
Root: HKCU; Subkey: "Software\Classes\.png"; ValueType: string; ValueName: ""; ValueData: "Fire.png"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\png
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".png"; ValueData: "Fire.png"; Flags: uninsdeletevalue; Tasks: assoc\png

; JPEG image — Fire.jpeg
Root: HKCU; Subkey: "Software\Classes\Fire.jpeg"; ValueType: string; ValueName: ""; ValueData: "JPEG image"; Flags: uninsdeletekey; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\Fire.jpeg\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\Fire.jpeg\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\Fire.jpeg\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jpg\OpenWithProgids"; ValueType: none; ValueName: "Fire.jpeg"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jpg"; ValueType: string; ValueName: ""; ValueData: "Fire.jpeg"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jpeg\OpenWithProgids"; ValueType: none; ValueName: "Fire.jpeg"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jpeg"; ValueType: string; ValueName: ""; ValueData: "Fire.jpeg"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jpe\OpenWithProgids"; ValueType: none; ValueName: "Fire.jpeg"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jpe"; ValueType: string; ValueName: ""; ValueData: "Fire.jpeg"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jfif\OpenWithProgids"; ValueType: none; ValueName: "Fire.jpeg"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jfif"; ValueType: string; ValueName: ""; ValueData: "Fire.jpeg"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jpg"; ValueData: "Fire.jpeg"; Flags: uninsdeletevalue; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jpeg"; ValueData: "Fire.jpeg"; Flags: uninsdeletevalue; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jpe"; ValueData: "Fire.jpeg"; Flags: uninsdeletevalue; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jfif"; ValueData: "Fire.jpeg"; Flags: uninsdeletevalue; Tasks: assoc\jpeg

; GIF image — Fire.gif
Root: HKCU; Subkey: "Software\Classes\Fire.gif"; ValueType: string; ValueName: ""; ValueData: "GIF image"; Flags: uninsdeletekey; Tasks: assoc\gif
Root: HKCU; Subkey: "Software\Classes\Fire.gif\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\gif
Root: HKCU; Subkey: "Software\Classes\Fire.gif\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\gif
Root: HKCU; Subkey: "Software\Classes\Fire.gif\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\gif
Root: HKCU; Subkey: "Software\Classes\.gif\OpenWithProgids"; ValueType: none; ValueName: "Fire.gif"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\gif
Root: HKCU; Subkey: "Software\Classes\.gif"; ValueType: string; ValueName: ""; ValueData: "Fire.gif"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\gif
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".gif"; ValueData: "Fire.gif"; Flags: uninsdeletevalue; Tasks: assoc\gif

; Bitmap image — Fire.bmp
Root: HKCU; Subkey: "Software\Classes\Fire.bmp"; ValueType: string; ValueName: ""; ValueData: "Bitmap image"; Flags: uninsdeletekey; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\Classes\Fire.bmp\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\Classes\Fire.bmp\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\Classes\Fire.bmp\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\Classes\.bmp\OpenWithProgids"; ValueType: none; ValueName: "Fire.bmp"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\Classes\.bmp"; ValueType: string; ValueName: ""; ValueData: "Fire.bmp"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\Classes\.dib\OpenWithProgids"; ValueType: none; ValueName: "Fire.bmp"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\Classes\.dib"; ValueType: string; ValueName: ""; ValueData: "Fire.bmp"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".bmp"; ValueData: "Fire.bmp"; Flags: uninsdeletevalue; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".dib"; ValueData: "Fire.bmp"; Flags: uninsdeletevalue; Tasks: assoc\bmp

; TIFF image — Fire.tiff
Root: HKCU; Subkey: "Software\Classes\Fire.tiff"; ValueType: string; ValueName: ""; ValueData: "TIFF image"; Flags: uninsdeletekey; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\Classes\Fire.tiff\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\Classes\Fire.tiff\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\Classes\Fire.tiff\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\Classes\.tif\OpenWithProgids"; ValueType: none; ValueName: "Fire.tiff"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\Classes\.tif"; ValueType: string; ValueName: ""; ValueData: "Fire.tiff"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\Classes\.tiff\OpenWithProgids"; ValueType: none; ValueName: "Fire.tiff"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\Classes\.tiff"; ValueType: string; ValueName: ""; ValueData: "Fire.tiff"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tif"; ValueData: "Fire.tiff"; Flags: uninsdeletevalue; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tiff"; ValueData: "Fire.tiff"; Flags: uninsdeletevalue; Tasks: assoc\tiff

; WebP image — Fire.webp
Root: HKCU; Subkey: "Software\Classes\Fire.webp"; ValueType: string; ValueName: ""; ValueData: "WebP image"; Flags: uninsdeletekey; Tasks: assoc\webp
Root: HKCU; Subkey: "Software\Classes\Fire.webp\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\webp
Root: HKCU; Subkey: "Software\Classes\Fire.webp\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\webp
Root: HKCU; Subkey: "Software\Classes\Fire.webp\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\webp
Root: HKCU; Subkey: "Software\Classes\.webp\OpenWithProgids"; ValueType: none; ValueName: "Fire.webp"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\webp
Root: HKCU; Subkey: "Software\Classes\.webp"; ValueType: string; ValueName: ""; ValueData: "Fire.webp"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\webp
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".webp"; ValueData: "Fire.webp"; Flags: uninsdeletevalue; Tasks: assoc\webp

; Icon — Fire.ico
Root: HKCU; Subkey: "Software\Classes\Fire.ico"; ValueType: string; ValueName: ""; ValueData: "Icon"; Flags: uninsdeletekey; Tasks: assoc\ico
Root: HKCU; Subkey: "Software\Classes\Fire.ico\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\ico
Root: HKCU; Subkey: "Software\Classes\Fire.ico\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\ico
Root: HKCU; Subkey: "Software\Classes\Fire.ico\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\ico
Root: HKCU; Subkey: "Software\Classes\.ico\OpenWithProgids"; ValueType: none; ValueName: "Fire.ico"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\ico
Root: HKCU; Subkey: "Software\Classes\.ico"; ValueType: string; ValueName: ""; ValueData: "Fire.ico"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\ico
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".ico"; ValueData: "Fire.ico"; Flags: uninsdeletevalue; Tasks: assoc\ico

; Truevision TGA image — Fire.tga
Root: HKCU; Subkey: "Software\Classes\Fire.tga"; ValueType: string; ValueName: ""; ValueData: "Truevision TGA image"; Flags: uninsdeletekey; Tasks: assoc\tga
Root: HKCU; Subkey: "Software\Classes\Fire.tga\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\tga
Root: HKCU; Subkey: "Software\Classes\Fire.tga\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\tga
Root: HKCU; Subkey: "Software\Classes\Fire.tga\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\tga
Root: HKCU; Subkey: "Software\Classes\.tga\OpenWithProgids"; ValueType: none; ValueName: "Fire.tga"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\tga
Root: HKCU; Subkey: "Software\Classes\.tga"; ValueType: string; ValueName: ""; ValueData: "Fire.tga"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\tga
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tga"; ValueData: "Fire.tga"; Flags: uninsdeletevalue; Tasks: assoc\tga

; QOI image — Fire.qoi
Root: HKCU; Subkey: "Software\Classes\Fire.qoi"; ValueType: string; ValueName: ""; ValueData: "QOI image"; Flags: uninsdeletekey; Tasks: assoc\qoi
Root: HKCU; Subkey: "Software\Classes\Fire.qoi\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\qoi
Root: HKCU; Subkey: "Software\Classes\Fire.qoi\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\qoi
Root: HKCU; Subkey: "Software\Classes\Fire.qoi\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\qoi
Root: HKCU; Subkey: "Software\Classes\.qoi\OpenWithProgids"; ValueType: none; ValueName: "Fire.qoi"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\qoi
Root: HKCU; Subkey: "Software\Classes\.qoi"; ValueType: string; ValueName: ""; ValueData: "Fire.qoi"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\qoi
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".qoi"; ValueData: "Fire.qoi"; Flags: uninsdeletevalue; Tasks: assoc\qoi

; Netpbm image — Fire.netpbm
Root: HKCU; Subkey: "Software\Classes\Fire.netpbm"; ValueType: string; ValueName: ""; ValueData: "Netpbm image"; Flags: uninsdeletekey; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\Fire.netpbm\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\Fire.netpbm\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\Fire.netpbm\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.ppm\OpenWithProgids"; ValueType: none; ValueName: "Fire.netpbm"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.ppm"; ValueType: string; ValueName: ""; ValueData: "Fire.netpbm"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.pgm\OpenWithProgids"; ValueType: none; ValueName: "Fire.netpbm"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.pgm"; ValueType: string; ValueName: ""; ValueData: "Fire.netpbm"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.pbm\OpenWithProgids"; ValueType: none; ValueName: "Fire.netpbm"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.pbm"; ValueType: string; ValueName: ""; ValueData: "Fire.netpbm"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.pnm\OpenWithProgids"; ValueType: none; ValueName: "Fire.netpbm"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.pnm"; ValueType: string; ValueName: ""; ValueData: "Fire.netpbm"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".ppm"; ValueData: "Fire.netpbm"; Flags: uninsdeletevalue; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".pgm"; ValueData: "Fire.netpbm"; Flags: uninsdeletevalue; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".pbm"; ValueData: "Fire.netpbm"; Flags: uninsdeletevalue; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".pnm"; ValueData: "Fire.netpbm"; Flags: uninsdeletevalue; Tasks: assoc\netpbm

; Farbfeld image — Fire.farbfeld
Root: HKCU; Subkey: "Software\Classes\Fire.farbfeld"; ValueType: string; ValueName: ""; ValueData: "Farbfeld image"; Flags: uninsdeletekey; Tasks: assoc\farbfeld
Root: HKCU; Subkey: "Software\Classes\Fire.farbfeld\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\farbfeld
Root: HKCU; Subkey: "Software\Classes\Fire.farbfeld\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\farbfeld
Root: HKCU; Subkey: "Software\Classes\Fire.farbfeld\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\farbfeld
Root: HKCU; Subkey: "Software\Classes\.ff\OpenWithProgids"; ValueType: none; ValueName: "Fire.farbfeld"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\farbfeld
Root: HKCU; Subkey: "Software\Classes\.ff"; ValueType: string; ValueName: ""; ValueData: "Fire.farbfeld"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\farbfeld
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".ff"; ValueData: "Fire.farbfeld"; Flags: uninsdeletevalue; Tasks: assoc\farbfeld

; JPEG XL image — Fire.jxl
Root: HKCU; Subkey: "Software\Classes\Fire.jxl"; ValueType: string; ValueName: ""; ValueData: "JPEG XL image"; Flags: uninsdeletekey; Tasks: assoc\jxl
Root: HKCU; Subkey: "Software\Classes\Fire.jxl\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\jxl
Root: HKCU; Subkey: "Software\Classes\Fire.jxl\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\jxl
Root: HKCU; Subkey: "Software\Classes\Fire.jxl\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\jxl
Root: HKCU; Subkey: "Software\Classes\.jxl\OpenWithProgids"; ValueType: none; ValueName: "Fire.jxl"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jxl
Root: HKCU; Subkey: "Software\Classes\.jxl"; ValueType: string; ValueName: ""; ValueData: "Fire.jxl"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jxl
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jxl"; ValueData: "Fire.jxl"; Flags: uninsdeletevalue; Tasks: assoc\jxl

; Radiance HDR image — Fire.hdr
Root: HKCU; Subkey: "Software\Classes\Fire.hdr"; ValueType: string; ValueName: ""; ValueData: "Radiance HDR image"; Flags: uninsdeletekey; Tasks: assoc\hdr
Root: HKCU; Subkey: "Software\Classes\Fire.hdr\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\hdr
Root: HKCU; Subkey: "Software\Classes\Fire.hdr\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\hdr
Root: HKCU; Subkey: "Software\Classes\Fire.hdr\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\hdr
Root: HKCU; Subkey: "Software\Classes\.hdr\OpenWithProgids"; ValueType: none; ValueName: "Fire.hdr"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\hdr
Root: HKCU; Subkey: "Software\Classes\.hdr"; ValueType: string; ValueName: ""; ValueData: "Fire.hdr"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\hdr
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".hdr"; ValueData: "Fire.hdr"; Flags: uninsdeletevalue; Tasks: assoc\hdr

; OpenEXR image — Fire.exr
Root: HKCU; Subkey: "Software\Classes\Fire.exr"; ValueType: string; ValueName: ""; ValueData: "OpenEXR image"; Flags: uninsdeletekey; Tasks: assoc\exr
Root: HKCU; Subkey: "Software\Classes\Fire.exr\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\exr
Root: HKCU; Subkey: "Software\Classes\Fire.exr\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\exr
Root: HKCU; Subkey: "Software\Classes\Fire.exr\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\exr
Root: HKCU; Subkey: "Software\Classes\.exr\OpenWithProgids"; ValueType: none; ValueName: "Fire.exr"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\exr
Root: HKCU; Subkey: "Software\Classes\.exr"; ValueType: string; ValueName: ""; ValueData: "Fire.exr"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\exr
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".exr"; ValueData: "Fire.exr"; Flags: uninsdeletevalue; Tasks: assoc\exr

; Photoshop image — Fire.psd
Root: HKCU; Subkey: "Software\Classes\Fire.psd"; ValueType: string; ValueName: ""; ValueData: "Photoshop image"; Flags: uninsdeletekey; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\Classes\Fire.psd\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\Classes\Fire.psd\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\Classes\Fire.psd\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\Classes\.psd\OpenWithProgids"; ValueType: none; ValueName: "Fire.psd"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\Classes\.psd"; ValueType: string; ValueName: ""; ValueData: "Fire.psd"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\Classes\.psb\OpenWithProgids"; ValueType: none; ValueName: "Fire.psd"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\Classes\.psb"; ValueType: string; ValueName: ""; ValueData: "Fire.psd"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".psd"; ValueData: "Fire.psd"; Flags: uninsdeletevalue; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".psb"; ValueData: "Fire.psd"; Flags: uninsdeletevalue; Tasks: assoc\psd

; HEIF image — Fire.heif
Root: HKCU; Subkey: "Software\Classes\Fire.heif"; ValueType: string; ValueName: ""; ValueData: "HEIF image"; Flags: uninsdeletekey; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\Classes\Fire.heif\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\Classes\Fire.heif\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\Classes\Fire.heif\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\Classes\.heic\OpenWithProgids"; ValueType: none; ValueName: "Fire.heif"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\Classes\.heic"; ValueType: string; ValueName: ""; ValueData: "Fire.heif"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\Classes\.heif\OpenWithProgids"; ValueType: none; ValueName: "Fire.heif"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\Classes\.heif"; ValueType: string; ValueName: ""; ValueData: "Fire.heif"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".heic"; ValueData: "Fire.heif"; Flags: uninsdeletevalue; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".heif"; ValueData: "Fire.heif"; Flags: uninsdeletevalue; Tasks: assoc\heif

; AVIF image — Fire.avif
Root: HKCU; Subkey: "Software\Classes\Fire.avif"; ValueType: string; ValueName: ""; ValueData: "AVIF image"; Flags: uninsdeletekey; Tasks: assoc\avif
Root: HKCU; Subkey: "Software\Classes\Fire.avif\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\avif
Root: HKCU; Subkey: "Software\Classes\Fire.avif\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\avif
Root: HKCU; Subkey: "Software\Classes\Fire.avif\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\avif
Root: HKCU; Subkey: "Software\Classes\.avif\OpenWithProgids"; ValueType: none; ValueName: "Fire.avif"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\avif
Root: HKCU; Subkey: "Software\Classes\.avif"; ValueType: string; ValueName: ""; ValueData: "Fire.avif"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\avif
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".avif"; ValueData: "Fire.avif"; Flags: uninsdeletevalue; Tasks: assoc\avif

; Camera raw image — Fire.raw
Root: HKCU; Subkey: "Software\Classes\Fire.raw"; ValueType: string; ValueName: ""; ValueData: "Camera raw image"; Flags: uninsdeletekey; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\Fire.raw\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\Fire.raw\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\Fire.raw\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.cr2\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.cr2"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.cr3\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.cr3"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.crw\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.crw"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.nef\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.nef"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.nrw\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.nrw"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.arw\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.arw"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.srf\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.srf"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.sr2\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.sr2"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.raf\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.raf"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.orf\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.orf"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.rw2\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.rw2"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.pef\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.pef"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.srw\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.srw"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.dng\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.dng"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.x3f\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.x3f"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.3fr\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.3fr"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.fff\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.fff"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.iiq\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.iiq"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.erf\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.erf"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.mrw\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.mrw"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.dcr\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.dcr"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.kdc\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.kdc"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.mef\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.mef"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.mos\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.mos"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.rwl\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.rwl"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.gpr\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.gpr"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.raw\OpenWithProgids"; ValueType: none; ValueName: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\Classes\.raw"; ValueType: string; ValueName: ""; ValueData: "Fire.raw"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".cr2"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".cr3"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".crw"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".nef"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".nrw"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".arw"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".srf"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".sr2"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".raf"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".orf"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".rw2"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".pef"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".srw"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".dng"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".x3f"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".3fr"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".fff"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".iiq"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".erf"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".mrw"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".dcr"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".kdc"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".mef"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".mos"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".rwl"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".gpr"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".raw"; ValueData: "Fire.raw"; Flags: uninsdeletevalue; Tasks: assoc\raw

[Run]
Filename: "{app}\{#MyAppExe}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent
