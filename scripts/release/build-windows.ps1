<#
.SYNOPSIS
  Builds the Windows MSI (WiX). Run in PowerShell on Windows 10/11.

.DESCRIPTION
  Prerequisites: Visual Studio 2022 Build Tools (Desktop development with C++), Rust via rustup
  (rust-toolchain.toml pins the compiler), Node 24 + pnpm 11, cargo-about. Tauri downloads WiX
  itself. WebView2 is installed by the MSI through the evergreen bootstrapper.

  Signing is optional. To sign, set WINDOWS_SIGN_COMMAND to the command Tauri should run per
  file, with "%1" standing for the file, for example
    $env:WINDOWS_SIGN_COMMAND = 'signtool sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 /f C:\keys\cert.pfx /p <password> "%1"'
  or, for Azure Trusted Signing / smartcards, any command that signs "%1" in place. Unset = unsigned
  build (SmartScreen will warn on download).

  Update artifacts: $env:OPENSVN_UPDATER = "1" with TAURI_SIGNING_PRIVATE_KEY and
  OPENSVN_UPDATER_PUBKEY set (see packaging/updater/README.md).

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\release\build-windows.ps1
#>
$ErrorActionPreference = "Stop"
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$App = Join-Path $Root "apps\opensvn"
$Gen = Join-Path $Root "packaging\generated"
if (-not $env:CARGO_BUILD_JOBS) { $env:CARGO_BUILD_JOBS = "2" }
$BuildRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root "target" }

$Version = (node (Join-Path $Root "scripts\release\stamp-version.mjs")).Trim()
Write-Host "==> version $Version"

if ($env:SKIP_LICENSES -ne "1") {
  New-Item -ItemType Directory -Force $Gen | Out-Null
  Push-Location $Root
  cargo about generate --locked --format json --manifest-path apps/opensvn/src-tauri/Cargo.toml `
    -c packaging/licenses/about.toml -o packaging/generated/rust-licenses.json
  Pop-Location
  Push-Location $App
  pnpm licenses list --prod --json --long | Out-File -Encoding utf8 (Join-Path $Gen "npm-licenses.json")
  Pop-Location
  node (Join-Path $Root "scripts\release\render-licenses.mjs")
}

$Config = @(
  "--config", (Join-Path $App "src-tauri\tauri.bundle.conf.json"),
  "--config", (Join-Path $Gen "tauri.version.conf.json")
)
if ($env:WINDOWS_SIGN_COMMAND) {
  $sign = @{ bundle = @{ windows = @{ signCommand = $env:WINDOWS_SIGN_COMMAND } } } | ConvertTo-Json -Depth 5
  $signPath = Join-Path $Gen "tauri.sign.windows.conf.json"
  Set-Content -Path $signPath -Value $sign -Encoding utf8
  $Config += @("--config", $signPath)
  Write-Host "==> signing with WINDOWS_SIGN_COMMAND"
} else {
  Write-Host "==> no WINDOWS_SIGN_COMMAND: building UNSIGNED"
}
if ($env:OPENSVN_UPDATER -eq "1") {
  node (Join-Path $Root "scripts\release\write-updater-config.mjs")
  $Config += @("--config", (Join-Path $Gen "tauri.updater.conf.json"))
}

Push-Location $Root; pnpm install --frozen-lockfile; Pop-Location
Push-Location $App; pnpm tauri build --bundles msi @Config; Pop-Location

$Out = Join-Path $Gen "out\$Version"
New-Item -ItemType Directory -Force $Out | Out-Null
$Msi = Join-Path $BuildRoot "release\bundle\msi"
Copy-Item (Join-Path $Msi "*.msi") $Out -Force
if ($env:OPENSVN_UPDATER -eq "1") { Copy-Item (Join-Path $Msi "*.msi.sig") $Out -Force -ErrorAction SilentlyContinue }
Get-ChildItem $Out -Filter *.msi | ForEach-Object { "$((Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower())  $($_.Name)" } | Set-Content (Join-Path $Out "SHA256SUMS")
Write-Host "==> artifacts in $Out"
