<#
.SYNOPSIS
    Build the Fire Windows installer (Inno Setup), sourcing all product metadata from product.json.

.DESCRIPTION
    product.json is the single source of truth for the product name, version, publisher, etc.
    This script:
      1. Reads product.json.
      2. Syncs the workspace version in Cargo.toml to match (so the crate version never drifts).
      3. Regenerates assets\fire.ico from assets\icon.png with ImageMagick (the one icon source).
      4. Builds fire.exe in release (build.rs embeds the same product.json values into the exe).
      5. Writes installer\product.generated.iss with #define directives for the Inno script.
      6. Compiles installer\fire.iss with ISCC, emitting the setup .exe into dist\.

    Bump the version (or any field) in product.json, re-run this script, and it flows everywhere.

.PARAMETER SkipBuild
    Skip the cargo release build (use an already-built target\release\fire.exe).

.PARAMETER SkipIcon
    Skip regenerating fire.ico from icon.png.

.EXAMPLE
    pwsh scripts\build-installer.ps1
#>
[CmdletBinding()]
param(
    [switch] $SkipBuild,
    [switch] $SkipIcon
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# --- paths -------------------------------------------------------------------
$RepoRoot     = Split-Path -Parent $PSScriptRoot
$ProductJson  = Join-Path $RepoRoot 'product.json'
$CargoToml    = Join-Path $RepoRoot 'Cargo.toml'
$IconPng      = Join-Path $RepoRoot 'assets\icon.png'
$IconIco      = Join-Path $RepoRoot 'assets\fire.ico'
$IssScript    = Join-Path $RepoRoot 'installer\fire.iss'
$GeneratedIss = Join-Path $RepoRoot 'installer\product.generated.iss'
$ExePath      = Join-Path $RepoRoot 'target\release\fire.exe'
$DistDir      = Join-Path $RepoRoot 'dist'

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

# --- 1. read product.json ----------------------------------------------------
Write-Step "Reading product.json"
if (-not (Test-Path $ProductJson)) { throw "product.json not found at $ProductJson" }
$product = Get-Content -Raw -LiteralPath $ProductJson | ConvertFrom-Json

# Check property existence before access (Set-StrictMode would throw on a missing property).
$names = $product.PSObject.Properties.Name
$missing = foreach ($field in 'productName','version','publisher','copyright','homepage') {
    if (($names -notcontains $field) -or [string]::IsNullOrWhiteSpace([string]$product.$field)) { $field }
}
if ($missing) { throw "product.json is missing required field(s): $($missing -join ', ')" }
Write-Host "    $($product.productName) $($product.version) — $($product.publisher)"

# --- 2. sync Cargo.toml workspace version ------------------------------------
Write-Step "Syncing Cargo.toml [workspace.package] version to $($product.version)"
$cargo = Get-Content -Raw -LiteralPath $CargoToml
# The only line-anchored `version = "..."` belongs to [workspace.package]; dependency versions
# are either `name = "x"` or live inside inline tables, so this regex targets it uniquely.
$cargoNew = [regex]::Replace($cargo, '(?m)^version\s*=\s*"[^"]*"', "version = `"$($product.version)`"")
if ($cargoNew -ne $cargo) {
    Set-Content -LiteralPath $CargoToml -Value $cargoNew -NoNewline -Encoding utf8
    Write-Host "    Cargo.toml updated."
} else {
    Write-Host "    Cargo.toml already in sync."
}

# --- 3. regenerate fire.ico from icon.png ------------------------------------
if (-not $SkipIcon) {
    Write-Step "Generating fire.ico from icon.png (ImageMagick)"
    $magickCmd = Get-Command magick -ErrorAction SilentlyContinue
    if (-not $magickCmd) { $magickCmd = Get-Command magick.exe -ErrorAction SilentlyContinue }
    if (-not $magickCmd) { throw "ImageMagick 'magick' not found on PATH. Install it or pass -SkipIcon." }
    $magick = $magickCmd.Source
    if (-not (Test-Path $IconPng)) { throw "Icon source not found at $IconPng" }
    & $magick $IconPng -background none -define icon:auto-resize=256,128,64,48,32,16 $IconIco
    if ($LASTEXITCODE -ne 0) { throw "magick failed to build fire.ico (exit $LASTEXITCODE)" }
    Write-Host "    Wrote $IconIco"
} else {
    Write-Step "Skipping icon regeneration (-SkipIcon)"
}

# --- 4. build the release exe ------------------------------------------------
if (-not $SkipBuild) {
    Write-Step "Building fire.exe (cargo build -p fire --release)"
    & cargo build -p fire --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
} else {
    Write-Step "Skipping cargo build (-SkipBuild)"
}
if (-not (Test-Path $ExePath)) { throw "Built exe not found at $ExePath (did the build run?)" }

# --- 4b. verify the license notices are present ------------------------------
# fire.iss installs these alongside the exe. A statically-linked fire.exe carries code from ~130
# other projects whose licenses require their notices to travel with the binary, so shipping an
# installer without them is a compliance bug, not a cosmetic one. Fail here with a clear message
# rather than letting ISCC report a missing source file.
Write-Step "Verifying license notices"
$noticeFiles = 'LICENSE', 'THIRD-PARTY-NOTICES.md', 'CREDITS.md'
$absent = $noticeFiles | Where-Object { -not (Test-Path (Join-Path $RepoRoot $_)) }
if ($absent) { throw "Missing license file(s) the installer must ship: $($absent -join ', ')" }
$licenseTexts = @(Get-ChildItem -Path (Join-Path $RepoRoot 'licenses') -Filter '*.txt' -ErrorAction SilentlyContinue)
if ($licenseTexts.Count -eq 0) { throw "No license texts found in $RepoRoot\licenses\ (installer expects licenses\*.txt)" }
Write-Host "    $($noticeFiles.Count) notice files + $($licenseTexts.Count) license texts."

# --- 5. write the generated ISPP include -------------------------------------
Write-Step "Writing installer\product.generated.iss"
# Escape double quotes for ISPP string literals (doubled), so values with quotes survive.
function ConvertTo-IssString([string] $s) { '"' + ($s -replace '"', '""') + '"' }

# Each element is parenthesized: inside @(...) a bare `string + (expr)` is mis-parsed as two
# array elements (the `+` becomes a unary plus), which would split the directive from its value.
$lines = @(
    "; AUTO-GENERATED by scripts\build-installer.ps1 from product.json. Do not edit or commit.",
    ("#define MyAppName "      + (ConvertTo-IssString $product.productName)),
    ("#define MyAppVersion "   + (ConvertTo-IssString $product.version)),
    ("#define MyAppPublisher " + (ConvertTo-IssString $product.publisher)),
    ("#define MyAppURL "       + (ConvertTo-IssString $product.homepage)),
    ("#define MyAppCopyright " + (ConvertTo-IssString $product.copyright)),
    ("#define MyAppExe "       + (ConvertTo-IssString 'fire.exe')),
    ("#define MyExeSource "    + (ConvertTo-IssString $ExePath)),
    ("#define MyIconSource "   + (ConvertTo-IssString $IconIco)),
    ("#define MyOutputDir "    + (ConvertTo-IssString $DistDir))
)
Set-Content -LiteralPath $GeneratedIss -Value ($lines -join "`r`n") -Encoding utf8

# --- 6. compile the installer ------------------------------------------------
Write-Step "Locating ISCC (Inno Setup compiler)"
$isccCmd = Get-Command iscc.exe -ErrorAction SilentlyContinue
$iscc = if ($isccCmd) { $isccCmd.Source } else { $null }
if (-not $iscc) {
    foreach ($c in @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles}\Inno Setup 6\ISCC.exe")) {
        if (Test-Path $c) { $iscc = $c; break }
    }
}
if (-not $iscc) { throw "ISCC.exe not found. Install Inno Setup 6 or add ISCC.exe to PATH." }
Write-Host "    $iscc"

New-Item -ItemType Directory -Force -Path $DistDir | Out-Null

Write-Step "Compiling installer\fire.iss"
& $iscc $IssScript
if ($LASTEXITCODE -ne 0) { throw "ISCC failed (exit $LASTEXITCODE)" }

$setup = Join-Path $DistDir ("{0}-{1}-Setup.exe" -f $product.productName, $product.version)
Write-Host ""
Write-Step "Done"
if (Test-Path $setup) {
    Write-Host "    Installer: $setup" -ForegroundColor Green
} else {
    Write-Host "    Installer written to $DistDir" -ForegroundColor Green
}
