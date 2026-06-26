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

; ProgID that all Fire-associated image types point at (one shared class, so registration and
; cleanup stay uniform). Versionless on purpose — there is only ever one installed Fire.
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
Compression=lzma2
SolidCompression=yes
OutputDir={#MyOutputDir}
OutputBaseFilename={#MyAppName}-{#MyAppVersion}-Setup

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#MyExeSource}"; DestDir: "{app}"; DestName: "{#MyAppExe}"; Flags: ignoreversion

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
; --- shortcuts ---
Name: "desktopicon";    Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Registry]
; --- the shared ProgID (always registered; uninstalled wholesale) ---
Root: HKCU; Subkey: "Software\Classes\{#FireProgId}"; ValueType: string; ValueName: ""; ValueData: "{#MyAppName} Image"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\{#FireProgId}\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"
Root: HKCU; Subkey: "Software\Classes\{#FireProgId}\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"
Root: HKCU; Subkey: "Software\Classes\{#FireProgId}\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""

; --- Default Programs capabilities (so Fire appears in Settings > Default apps) ---
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "{#MyAppName} — fast image viewer"
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities"; ValueType: string; ValueName: "ApplicationIcon"; ValueData: "{app}\{#MyAppExe},0"
Root: HKCU; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "{#MyAppName}"; ValueData: "Software\{#MyAppName}\Capabilities"; Flags: uninsdeletevalue

; --- per-format associations -------------------------------------------------------------
; For every extension of a selected format we write three things:
;   1) OpenWithProgids — adds Fire to the file's "Open with" list (additive, always safe).
;   2) the .ext default ProgID — makes Fire the default where Windows has no protected
;      UserChoice yet (it never overrides an explicit user choice, by design).
;   3) Capabilities\FileAssociations — lists the type under Fire in Default apps.

; PNG
Root: HKCU; Subkey: "Software\Classes\.png\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\png
Root: HKCU; Subkey: "Software\Classes\.png"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\png
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".png"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\png

; JPEG
Root: HKCU; Subkey: "Software\Classes\.jpg\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jpg"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jpeg\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jpeg"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jpe\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jpe"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jfif\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\Classes\.jfif"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jpg"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jpeg"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jpe"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\jpeg
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jfif"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\jpeg

; GIF
Root: HKCU; Subkey: "Software\Classes\.gif\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\gif
Root: HKCU; Subkey: "Software\Classes\.gif"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\gif
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".gif"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\gif

; BMP
Root: HKCU; Subkey: "Software\Classes\.bmp\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\Classes\.bmp"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\Classes\.dib\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\Classes\.dib"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".bmp"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\bmp
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".dib"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\bmp

; TIFF
Root: HKCU; Subkey: "Software\Classes\.tif\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\Classes\.tif"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\Classes\.tiff\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\Classes\.tiff"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tif"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\tiff
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tiff"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\tiff

; WebP
Root: HKCU; Subkey: "Software\Classes\.webp\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\webp
Root: HKCU; Subkey: "Software\Classes\.webp"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\webp
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".webp"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\webp

; ICO
Root: HKCU; Subkey: "Software\Classes\.ico\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\ico
Root: HKCU; Subkey: "Software\Classes\.ico"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\ico
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".ico"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\ico

; TGA
Root: HKCU; Subkey: "Software\Classes\.tga\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\tga
Root: HKCU; Subkey: "Software\Classes\.tga"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\tga
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tga"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\tga

; QOI
Root: HKCU; Subkey: "Software\Classes\.qoi\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\qoi
Root: HKCU; Subkey: "Software\Classes\.qoi"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\qoi
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".qoi"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\qoi

; Netpbm
Root: HKCU; Subkey: "Software\Classes\.ppm\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.ppm"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.pgm\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.pgm"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.pbm\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.pbm"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.pnm\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\Classes\.pnm"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".ppm"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".pgm"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".pbm"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\netpbm
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".pnm"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\netpbm

; Farbfeld
Root: HKCU; Subkey: "Software\Classes\.ff\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\farbfeld
Root: HKCU; Subkey: "Software\Classes\.ff"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\farbfeld
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".ff"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\farbfeld

; JPEG XL
Root: HKCU; Subkey: "Software\Classes\.jxl\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jxl
Root: HKCU; Subkey: "Software\Classes\.jxl"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\jxl
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".jxl"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\jxl

; Radiance HDR
Root: HKCU; Subkey: "Software\Classes\.hdr\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\hdr
Root: HKCU; Subkey: "Software\Classes\.hdr"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\hdr
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".hdr"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\hdr

; OpenEXR
Root: HKCU; Subkey: "Software\Classes\.exr\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\exr
Root: HKCU; Subkey: "Software\Classes\.exr"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\exr
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".exr"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\exr

; Photoshop
Root: HKCU; Subkey: "Software\Classes\.psd\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\Classes\.psd"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\Classes\.psb\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\Classes\.psb"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".psd"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\psd
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".psb"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\psd

; HEIF / HEIC
Root: HKCU; Subkey: "Software\Classes\.heic\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\Classes\.heic"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\Classes\.heif\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\Classes\.heif"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".heic"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\heif
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".heif"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\heif

; AVIF
Root: HKCU; Subkey: "Software\Classes\.avif\OpenWithProgids"; ValueType: none; ValueName: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\avif
Root: HKCU; Subkey: "Software\Classes\.avif"; ValueType: string; ValueName: ""; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: assoc\avif
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".avif"; ValueData: "{#FireProgId}"; Flags: uninsdeletevalue; Tasks: assoc\avif

[Run]
Filename: "{app}\{#MyAppExe}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent
