# Packages the release binaries of the current Windows host into dist/, with the asset names and layout of
# docs/spec/07-peripheral-build.md (A.4.2, A.4.3, I.2):
#   dist/opencode-windows-<arch>/bin/opencode.exe + package.json   -> dist/opencode-windows-<arch>.zip
#   dist/opencode-windows-x64-baseline/...                         -> same binary under the baseline name (x64 only)
#   dist/opencode-desktop-win-<arch>.zip                           -> opencode-desktop.exe (no installer; see README)
# The archive root holds only the executable. Nothing is signed, uploaded or published.
#
# Usage: powershell -ExecutionPolicy Bypass -File scripts\package.ps1 [-SkipBuild] [-SkipDesktop]
param(
    [switch]$SkipBuild,
    [switch]$SkipDesktop
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root "dist"
$release = Join-Path $root "target\release"

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x64" }
    "ARM64" { "arm64" }
    default { throw "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}

$version = $env:OPENCODE_VERSION
if (-not $version) {
    $manifest = Get-Content (Join-Path $root "Cargo.toml") -Raw
    if ($manifest -notmatch '(?m)^version\s*=\s*"([^"]+)"') { throw "no workspace version in Cargo.toml" }
    $version = $Matches[1]
}

if (-not $SkipBuild) {
    $packages = @("-p", "opencode-cli")
    if (-not $SkipDesktop) { $packages += @("-p", "opencode-desktop") }
    Push-Location $root
    try {
        & cargo build --release --locked @packages
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
}

$cli = Join-Path $release "opencode.exe"
if (-not (Test-Path $cli)) { throw "missing $cli; build first or drop -SkipBuild" }

New-Item -ItemType Directory -Force $dist | Out-Null

function Write-Utf8NoBom([string]$Path, [string]$Text) {
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding $false))
}

function New-CliAsset([string]$Name) {
    $stage = Join-Path $dist $Name
    $bin = Join-Path $stage "bin"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force $bin | Out-Null
    Copy-Item $cli (Join-Path $bin "opencode.exe")
    $package = [ordered]@{
        name            = $Name
        version         = $version
        preferUnplugged = $true
        os              = @("win32")
        cpu             = @($arch)
    }
    Write-Utf8NoBom (Join-Path $stage "package.json") (($package | ConvertTo-Json -Depth 4) + "`n")
    $archive = Join-Path $dist "$Name.zip"
    if (Test-Path $archive) { Remove-Item -Force $archive }
    Compress-Archive -Path (Join-Path $bin "opencode.exe") -DestinationPath $archive
    $archive
}

$assets = @()
$assets += New-CliAsset "opencode-windows-$arch"
if ($arch -eq "x64") { $assets += New-CliAsset "opencode-windows-x64-baseline" }

if (-not $SkipDesktop) {
    $desktop = Join-Path $release "opencode-desktop.exe"
    if (-not (Test-Path $desktop)) { throw "missing $desktop; build first or pass -SkipDesktop" }
    $archive = Join-Path $dist "opencode-desktop-win-$arch.zip"
    if (Test-Path $archive) { Remove-Item -Force $archive }
    Compress-Archive -Path $desktop -DestinationPath $archive
    $assets += $archive
}

$sums = foreach ($asset in $assets) {
    $hash = (Get-FileHash -Algorithm SHA256 $asset).Hash.ToLower()
    "{0}  {1}" -f $hash, (Split-Path -Leaf $asset)
}
Write-Utf8NoBom (Join-Path $dist "SHA256SUMS") (($sums -join "`n") + "`n")

foreach ($asset in $assets) {
    "{0}  {1:N1} MB" -f (Split-Path -Leaf $asset), ((Get-Item $asset).Length / 1MB)
}
