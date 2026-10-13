<#
.SYNOPSIS
  Builds the Windows MSI (WiX). Run in PowerShell on Windows 10/11.

.DESCRIPTION
  Prerequisites: Visual Studio 2022 Build Tools (Desktop development with C++), Rust via rustup
  (rust-toolchain.toml pins the compiler), Node 24 + pnpm 11, cargo-about. Tauri downloads WiX
  itself. WebView2 is installed by the MSI through the evergreen bootstrapper.

  Signing is optional and secrets stay out of files: set WINDOWS_SIGN_COMMAND to the command Tauri
  runs per file, with "%1" standing for the file. Prefer a certificate-store selector, which needs
  no password at all, for example
    $env:WINDOWS_SIGN_COMMAND = 'signtool sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 /sha1 <thumbprint> "%1"'
  or an Azure Trusted Signing / smartcard command. Never put a password in the command: the
  command is written (and deleted again after the build) to
  packaging\generated\tauri.sign.windows.conf.json so Tauri can read it, so it must not contain
  secrets. Unset = unsigned build (SmartScreen will warn on download).

  Update artifacts: $env:OPENSVN_UPDATER = "1" with TAURI_SIGNING_PRIVATE_KEY and
  OPENSVN_UPDATER_PUBKEY set (see packaging/updater/README.md).

  Every native command is checked: a non-zero exit code aborts the script (Windows PowerShell 5.1
  does not do that by itself), and only installers produced by this run are collected.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\release\build-windows.ps1
#>
$ErrorActionPreference = "Stop"

function Invoke-Checked {
  param([string]$Step, [scriptblock]$Command)
  & $Command
  if ($LASTEXITCODE -ne 0) { throw "$Step failed (exit code $LASTEXITCODE)" }
}

$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$App = Join-Path $Root "apps\opensvn"
$Gen = Join-Path $Root "packaging\generated"
if (-not $env:CARGO_BUILD_JOBS) { $env:CARGO_BUILD_JOBS = "2" }
$BuildRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root "target" }

if ($env:OPENSVN_UPDATER -eq "1" -and -not $env:TAURI_SIGNING_PRIVATE_KEY) {
  throw "OPENSVN_UPDATER=1 needs TAURI_SIGNING_PRIVATE_KEY"
}

New-Item -ItemType Directory -Force $Gen | Out-Null
$Version = $null
Invoke-Checked "stamp-version" { $script:Version = (node (Join-Path $Root "scripts\release\stamp-version.mjs")).Trim() }
Write-Host "==> version $Version"

if ($env:SKIP_LICENSES -ne "1") {
  Push-Location $Root
  try {
    Invoke-Checked "cargo about" {
      cargo about generate --locked --format json --manifest-path apps/opensvn/src-tauri/Cargo.toml `
        -c packaging/licenses/about.toml -o packaging/generated/rust-licenses.json
    }
  } finally { Pop-Location }
  Push-Location $App
  try {
    Invoke-Checked "pnpm licenses" {
      pnpm licenses list --prod --json --long | Out-File -Encoding utf8 (Join-Path $Gen "npm-licenses.json")
    }
  } finally { Pop-Location }
  Invoke-Checked "render-licenses" { node (Join-Path $Root "scripts\release\render-licenses.mjs") }
}

$Config = @(
  "--config", (Join-Path $App "src-tauri\tauri.bundle.conf.json"),
  "--config", (Join-Path $Gen "tauri.version.conf.json")
)
$SignPath = Join-Path $Gen "tauri.sign.windows.conf.json"
if ($env:WINDOWS_SIGN_COMMAND) {
  $sign = @{ bundle = @{ windows = @{ signCommand = $env:WINDOWS_SIGN_COMMAND } } } | ConvertTo-Json -Depth 5
  Set-Content -Path $SignPath -Value $sign -Encoding utf8
  $Config += @("--config", $SignPath)
  Write-Host "==> signing with WINDOWS_SIGN_COMMAND"
} else {
  Write-Host "==> no WINDOWS_SIGN_COMMAND: building UNSIGNED"
}
if ($env:OPENSVN_UPDATER -eq "1") {
  Invoke-Checked "write-updater-config" { node (Join-Path $Root "scripts\release\write-updater-config.mjs") }
  $Config += @("--config", (Join-Path $Gen "tauri.updater.conf.json"))
}

try {
  Push-Location $Root
  try { Invoke-Checked "pnpm install" { pnpm install --frozen-lockfile } } finally { Pop-Location }
  $Started = Get-Date
  Push-Location $App
  try { Invoke-Checked "tauri build" { pnpm tauri build --bundles msi @Config } } finally { Pop-Location }
} finally {
  # The sign command file is transient: never leave it behind.
  Remove-Item -Force -ErrorAction SilentlyContinue $SignPath
}

$Out = Join-Path $Gen "out\$Version"
if (Test-Path $Out) { Remove-Item -Recurse -Force $Out }
New-Item -ItemType Directory -Force $Out | Out-Null
$Msi = Join-Path $BuildRoot "release\bundle\msi"
$Fresh = @(Get-ChildItem -Path $Msi -Filter "OpenSVN_${Version}_*.msi" -File |
    Where-Object { $_.LastWriteTime -ge $Started })
if ($Fresh.Count -ne 1) { throw "expected exactly one fresh OpenSVN_${Version}_*.msi in $Msi, found $($Fresh.Count)" }
Copy-Item $Fresh[0].FullName $Out -Force
if ($env:OPENSVN_UPDATER -eq "1") {
  $Sig = "$($Fresh[0].FullName).sig"
  if (-not (Test-Path $Sig) -or (Get-Item $Sig).Length -eq 0) { throw "missing signature $Sig (OPENSVN_UPDATER=1)" }
  Copy-Item $Sig $Out -Force
}
Get-ChildItem $Out -Filter *.msi | ForEach-Object { "$((Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower())  $($_.Name)" } | Set-Content (Join-Path $Out "SHA256SUMS")
Write-Host "==> artifacts in $Out"
