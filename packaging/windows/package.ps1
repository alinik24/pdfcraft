<#
.SYNOPSIS
  Build, sign and package PdfCraft for Windows.

.DESCRIPTION
  Produces, in $env:DIST (default: dist/release):
    pdfcraft-<version>-windows-<arch>.msi            per-machine installer (WiX v5)
    pdfcraft-<version>-windows-<arch>-portable.zip   pdfcraft.exe + pdfcraft-cli.exe + models\ + portable.txt

  The binaries link the C runtime statically (+crt-static), so neither the MSI nor the portable
  zip needs the Visual C++ redistributable. Signing is delegated to sign.ps1 only when
  explicitly requested with -Sign.

  Needs: Rust (MSVC toolchain + the target), curl and the network (the OCR models), the Windows SDK (rc.exe, signtool.exe),
  and WiX v5: dotnet tool install --global wix --version 5.0.2

.EXAMPLE
  pwsh packaging/windows/package.ps1 -Arch x64
  pwsh packaging/windows/package.ps1 -Arch x86 -SkipBuild
  pwsh packaging/windows/package.ps1 -Arch arm64     # cross-compiled; needs the MSVC ARM64 build tools
#>
param(
  [ValidateSet('x64', 'x86', 'arm64')] [string] $Arch = 'x64',
  [switch] $SkipBuild,
  # A complete portable distribution without requiring WiX or creating an MSI.
  [switch] $PortableOnly,
  # Never select an identity from the build machine implicitly.
  [switch] $Sign
)
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$Commit = (& git -C $Root rev-parse HEAD).Trim()
if ($Commit -notmatch '^[a-f0-9]{40}$' -or (& git -C $Root status --porcelain)) {
  throw 'Release source must be a committed, clean Git checkout'
}

function Invoke-Native([string] $What, [scriptblock] $Block) {
  Write-Output "==> $What"
  & $Block
  if ($LASTEXITCODE -ne 0) { throw "$What failed with exit code $LASTEXITCODE" }
}

# The version lives in one place: [workspace.package] version in the root Cargo.toml.
$Version = $env:PDFCRAFT_VERSION
if (-not $Version) {
  $inPkg = $false
  foreach ($line in Get-Content (Join-Path $Root 'Cargo.toml')) {
    if ($line -match '^\s*\[') { $inPkg = ($line.Trim() -eq '[workspace.package]'); continue }
    if ($inPkg -and $line -match '^\s*version\s*=\s*"([^"]+)"') { $Version = $Matches[1]; break }
  }
}
if (-not $Version) { throw 'could not read [workspace.package] version from Cargo.toml' }
if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$') { throw 'Version must be a safe numeric release version with an optional prerelease suffix' }
# MSI ProductVersion is numeric (major.minor.build); pre-release tags are dropped there.
$MsiVersion = ($Version -split '-')[0]

$Target = switch ($Arch) { 'x64' { 'x86_64-pc-windows-msvc' } 'x86' { 'i686-pc-windows-msvc' } 'arm64' { 'aarch64-pc-windows-msvc' } }
$Dist = if ($env:DIST) { $env:DIST } else { Join-Path $Root 'dist\release' }
$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' }
New-Item -ItemType Directory -Force -Path $Dist | Out-Null

$env:PDFCRAFT_BUILD_SHA = $Commit
if (-not $env:PDFCRAFT_BUILD_DATE) { $env:PDFCRAFT_BUILD_DATE = (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd') }

Write-Output "PdfCraft $Version for Windows $Arch ($Target)"

if (-not $SkipBuild) {
  # Static CRT: no VC++ redistributable needed. Scoped to the target so host build scripts and
  # proc-macros are unaffected.
  $previousFlags = $env:CARGO_ENCODED_RUSTFLAGS
  $nativeFlagsName = "CFLAGS_$Target"
  $previousNativeFlags = [Environment]::GetEnvironmentVariable($nativeFlagsName)
  $flags = @('-C', 'target-feature=+crt-static')
  $remaps = @()
  if ($env:USERPROFILE) { $remaps += @{ source = $env:USERPROFILE; target = '/build/user' } }
  if ($env:CARGO_HOME) { $remaps += @{ source = $env:CARGO_HOME; target = '/build/cargo' } }
  $remaps += @{ source = $Root; target = '/build/pdfcraft' }
  if ($env:CRAFT_FONTS_DIR) { $remaps += @{ source = $env:CRAFT_FONTS_DIR; target = '/build/craft-fonts' } }
  foreach ($remap in $remaps) {
    $prefix = [IO.Path]::GetFullPath($remap.source)
    $flags += "--remap-path-prefix=$prefix=$($remap.target)"
    $flags += "--remap-path-prefix=$($prefix.Replace('\', '/'))=$($remap.target)"
  }
  # Explicit --target keeps these flags off host build scripts and proc macros.
  # Encoded arguments also support source paths containing spaces.
  # Rust remapping does not affect C __FILE__ strings in native dependencies.
  # MSVC's trim option removes the user-profile prefix from those diagnostics.
  # Keep this scoped to the target and restore the caller's flags after building.
  if ($Arch -ne 'arm64' -and $env:USERPROFILE) {
    $trimFlag = '/d1trimfile:' + [IO.Path]::GetFullPath($env:USERPROFILE).TrimEnd('\') + '\'
    if ($env:CC_SHELL_ESCAPED_FLAGS -eq '1') {
      $trimFlag = "'" + $trimFlag.Replace("'", "'\''") + "'"
    } elseif ($trimFlag -match '\s') {
      throw 'For a user-profile path with spaces, set CC_SHELL_ESCAPED_FLAGS=1 and shell-quote existing native compiler flags.'
    }
    [Environment]::SetEnvironmentVariable($nativeFlagsName, "$previousNativeFlags $trimFlag".Trim())
  }
  $env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]0x1f
  $env:PDFCRAFT_BUILD_LABEL = "html-review $($Commit.Substring(0, 12))"
  # Fail the build (rather than warn) if the icon/VERSIONINFO can't be embedded.
  $env:PDFCRAFT_REQUIRE_WINRES = '1'
  try {
    Invoke-Native "cargo build ($Target)" { cargo build --release --locked -p pdfcraft -p pdfcraft-cli --features pdfcraft-cli/html-review --target $Target -j 2 }
  } finally {
    $env:CARGO_ENCODED_RUSTFLAGS = $previousFlags
    [Environment]::SetEnvironmentVariable($nativeFlagsName, $previousNativeFlags)
  }
}

$Bin = Join-Path $TargetDir "$Target\release"
if ($SkipBuild) {
  $builtVersion = & (Join-Path $Bin 'pdfcraft-cli.exe') --version
  if ($LASTEXITCODE -ne 0 -or $builtVersion -notlike "*html-review $($Commit.Substring(0, 12))*") {
    throw 'SkipBuild requires a previously built binary labeled with this source commit'
  }
}

# Check both binaries' PE headers before packaging. Machine (COFF header) must match -Arch, so an
# x64 build can never ship labelled arm64 (#61). Subsystem (optional header): 2 = Windows GUI,
# 3 = console. The app must be GUI (no console window opens with it, #57); the CLI must stay
# console so its output reaches the terminal.
function Get-PeHeader([string] $Path) {
  $bytes = [System.IO.File]::ReadAllBytes($Path)
  $pe = [BitConverter]::ToInt32($bytes, 0x3C)
  return @{ Machine = [BitConverter]::ToUInt16($bytes, $pe + 4); Subsystem = [BitConverter]::ToUInt16($bytes, $pe + 0x5C) }
}
$Machine = switch ($Arch) { 'x64' { 0x8664 } 'x86' { 0x14C } 'arm64' { 0xAA64 } }
foreach ($check in @(@('pdfcraft.exe', 2), @('pdfcraft-cli.exe', 3))) {
  $h = Get-PeHeader (Join-Path $Bin $check[0])
  if ($h.Machine -ne $Machine) { throw "$($check[0]) is for machine 0x$('{0:X}' -f $h.Machine), expected 0x$('{0:X}' -f $Machine) ($Arch)" }
  if ($h.Subsystem -ne $check[1]) { throw "$($check[0]) has PE subsystem $($h.Subsystem), expected $($check[1])" }
  Write-Output "ok $($check[0]): $Arch, PE subsystem $($h.Subsystem)"
}
$StageId = [Guid]::NewGuid().ToString('N')
$Stage = Join-Path $TargetDir "windows-package\$Arch-$StageId"
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
Copy-Item (Join-Path $Bin 'pdfcraft.exe'), (Join-Path $Bin 'pdfcraft-cli.exe') $Stage
# The document icon embeds Lucide artwork: carry its ISC notice and the project licences
# beside the binaries in both distributions.
foreach ($f in 'LICENSE-MIT', 'LICENSE-APACHE', 'NOTICE') {
  Copy-Item (Join-Path $Root $f) $Stage
}
Copy-Item (Join-Path $Root 'assets\app-icon\LICENSE-document.txt') $Stage

if ($Sign) {
  & (Join-Path $PSScriptRoot 'sign.ps1') (Join-Path $Stage 'pdfcraft.exe') (Join-Path $Stage 'pdfcraft-cli.exe')
}

# OCR models (#103): the app looks in models\ beside pdfcraft.exe. `cargo xtask models` fetches every
# ATTRIBUTION.toml `kind = "model"` file (verified by SHA-256) with its licence and ATTRIBUTION.txt;
# the MSI and the portable zip take the whole folder, so new models ship without naming them here.
$Models = Join-Path $Stage 'models'
New-Item -ItemType Directory -Force -Path $Models | Out-Null
Push-Location $Root
try { Invoke-Native 'cargo xtask models' { cargo xtask models $Models } } finally { Pop-Location }
Remove-Item -Force (Join-Path $Models '*.part') -ErrorAction SilentlyContinue
if (-not (Test-Path (Join-Path $Models 'ATTRIBUTION.txt')) -or -not (Get-ChildItem (Join-Path $Models '*.LICENCE.txt'))) {
  throw "no OCR models in $Models after cargo xtask models"
}

# ---- MSI ---------------------------------------------------------------------------------------
$Msi = $null
if (-not $PortableOnly) {
$Msi = Join-Path $Dist "pdfcraft-$Version-windows-$Arch.msi"
Invoke-Native 'wix build' {
  wix build (Join-Path $PSScriptRoot 'pdfcraft.wxs') -arch $Arch `
    (Join-Path $PSScriptRoot 'installer-ui.wxs') `
    -d "Version=$MsiVersion" -d "BinDir=$Stage" -d "ModelsDir=$Models" -d "IconPath=$(Join-Path $Root 'assets\app-icon\pdfcraft.ico')" `
    -o $Msi
}
# Inspect the built MSI, not just the XML, before signing/publishing it. In a child process, so
# its Windows Installer database handle is gone before signtool opens the MSI.
Invoke-Native 'test-msi' { & (Get-Process -Id $PID).Path -NoProfile -File (Join-Path $PSScriptRoot 'test-msi.ps1') $Msi }
# wix writes its debug symbols (.wixpdb) next to the MSI; keep them out of the release assets.
Remove-Item -Force -ErrorAction SilentlyContinue ([IO.Path]::ChangeExtension($Msi, '.wixpdb'))
if ($Sign) { & (Join-Path $PSScriptRoot 'sign.ps1') $Msi }
}

# ---- portable zip ------------------------------------------------------------------------------
$Portable = Join-Path $TargetDir "windows-package\$StageId\pdfcraft-$Version-windows-$Arch-portable"
New-Item -ItemType Directory -Force -Path $Portable | Out-Null
Copy-Item (Join-Path $Stage '*.exe') $Portable
foreach ($f in 'NOTICE', 'LICENSE-document.txt') {
  Copy-Item (Join-Path $Stage $f) $Portable
}
Copy-Item -Recurse $Models (Join-Path $Portable 'models')
foreach ($f in 'README.md', 'LICENSE', 'LICENSE-MIT', 'LICENSE-APACHE') {
  $p = Join-Path $Root $f
  if (Test-Path $p) { Copy-Item $p $Portable }
}
# Builds made with craft-fonts (CRAFT_FONTS_DIR, set for every release) embed its fonts: ship their
# licences, fonts\<family>\OFL.txt -> OFL-<family>.txt.
if ($env:CRAFT_FONTS_DIR) {
  Get-ChildItem -Path (Join-Path $env:CRAFT_FONTS_DIR 'fonts') -Directory -ErrorAction SilentlyContinue | ForEach-Object {
    $ofl = Join-Path $_.FullName 'OFL.txt'
    if (Test-Path $ofl) { Copy-Item $ofl (Join-Path $Portable "OFL-$($_.Name).txt") }
  }
}
# portable.txt beside pdfcraft.exe switches on portable mode: settings, logs, recovery files and new
# digital IDs go to PdfCraftData\ next to the exe instead of %APPDATA% (#157; see
# crates/ui-egui/src/portable.rs).
Copy-Item (Join-Path $PSScriptRoot 'portable.txt') $Portable
$DocDir = Join-Path $Portable 'docs'
New-Item -ItemType Directory -Force -Path $DocDir | Out-Null
foreach ($f in 'html-installation.md', 'html-review.md', 'agent-review-architecture.md') {
  Copy-Item -LiteralPath (Join-Path $Root "docs\$f") -Destination $DocDir
}
$SourceDirty = [bool](& git -C $Root status --porcelain)
if ($SourceDirty -or (& git -C $Root rev-parse HEAD) -ne $Commit) { throw 'Release source changed during packaging' }
[ordered]@{
  version = $Version; commit = $env:PDFCRAFT_BUILD_SHA; target = $Target
  html_review = $true; static_crt_requested = $true; signed = ((Get-AuthenticodeSignature (Join-Path $Stage 'pdfcraft.exe')).Status -eq 'Valid')
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Portable 'BUILD-INFO.json')
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'README-portable.txt') -Destination $Portable
$Zip = Join-Path $Dist "pdfcraft-$Version-windows-$Arch-portable.zip"
if (Test-Path -LiteralPath $Zip) { throw "Release archive already exists: $Zip. Use a new version or archive it explicitly." }
Compress-Archive -Path $Portable -DestinationPath $Zip

# Smoke-test the CLI when this machine can run it. An ARM64 build made on an x64 runner can't run
# here; .github/workflows/windows-arm64.yml installs and runs it on ARM64 instead.
$HostArch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
if ($Arch -ne 'arm64' -or $HostArch -eq 'arm64') {
  Invoke-Native 'pdfcraft-cli --version' { & (Join-Path $Stage 'pdfcraft-cli.exe') --version }
} else {
  Write-Output "skipping pdfcraft-cli --version: an $Arch build doesn't run on this $HostArch machine"
}
foreach ($Artifact in @($Msi, $Zip)) {
  if ($Artifact) { Get-Item -LiteralPath $Artifact | Format-Table Name, Length }
}
