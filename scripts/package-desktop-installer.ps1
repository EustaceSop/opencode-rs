# Builds the per-user Windows installer of the desktop app:
#   dist/opencode-desktop-win-<arch>.exe   (the asset name of the original's `windows-x64-nsis` download)
# It installs opencode-desktop.exe and opencode.exe under %LOCALAPPDATA%\Programs\opencode-desktop-rs, adds Start menu
# entries, registers opencode:// and uninstalls cleanly (scripts/windows/opencode-desktop.nsi). Nothing is signed.
#
# NSIS comes from -Nsis <dir>, $env:NSIS_DIR, makensis on PATH, or else a pinned portable NSIS zip that is downloaded
# and checked into target\tools\nsis-<version> (never installed system-wide).
#
# -TestInstall installs silently with /NOSYSTEM into a fresh temporary directory (files only: no registry, no Start
# menu), checks the payload, uninstalls the same way and removes the directory.
#
# Usage: powershell -ExecutionPolicy Bypass -File scripts\package-desktop-installer.ps1 [-SkipBuild] [-Nsis DIR] [-TestInstall]
param(
    [switch]$SkipBuild,
    [string]$Nsis = $env:NSIS_DIR,
    [switch]$TestInstall
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root "dist"
$release = Join-Path $root "target\release"
$script = Join-Path $PSScriptRoot "windows\opencode-desktop.nsi"
$icon = Join-Path $root "crates\opencode-desktop\assets\icons\icon.ico"

# NSIS 3.13 (released 2026-09-27); the SHA-1 matches the one SourceForge lists for this file.
$nsisVersion = "3.13"
$nsisSha256 = "ba63dffc4410ee89193e1cb5a41989991bd77c61068da17e3156d136b7b0b3d8"
$nsisUrl = "https://downloads.sourceforge.net/project/nsis/NSIS%203/$nsisVersion/nsis-$nsisVersion.zip"

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
if ($version -notmatch '^\d+\.\d+\.\d+$') { throw "version $version is not MAJOR.MINOR.PATCH" }

function Find-Makensis {
    if ($Nsis) {
        $candidate = Join-Path $Nsis "makensis.exe"
        if (-not (Test-Path $candidate)) { throw "no makensis.exe in $Nsis" }
        return $candidate
    }
    $onPath = Get-Command makensis -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    $tools = Join-Path $root "target\tools"
    $home_ = Join-Path $tools "nsis-$nsisVersion"
    $candidate = Join-Path $home_ "makensis.exe"
    if (Test-Path $candidate) { return $candidate }

    $download = Join-Path $tools "download-nsis-$nsisVersion"
    if (Test-Path $download) { Remove-Item -Recurse -Force $download }
    New-Item -ItemType Directory -Force $download | Out-Null
    $zip = Join-Path $download "nsis-$nsisVersion.zip"
    Write-Host "downloading NSIS $nsisVersion into $tools"
    $ProgressPreference = "SilentlyContinue"
    # SourceForge answers a browser-like client with an HTML page; a plain user agent gets the file.
    Invoke-WebRequest -UseBasicParsing -UserAgent "Wget" -Uri $nsisUrl -OutFile $zip
    $hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
    if ($hash -ne $nsisSha256) { throw "nsis-$nsisVersion.zip has SHA-256 $hash, expected $nsisSha256" }
    Expand-Archive -Path $zip -DestinationPath $download
    Move-Item (Join-Path $download "nsis-$nsisVersion") $home_
    Remove-Item -Recurse -Force $download
    if (-not (Test-Path $candidate)) { throw "the NSIS archive has no makensis.exe" }
    return $candidate
}

if (-not $SkipBuild) {
    Push-Location $root
    try {
        & cargo build --release --locked -p opencode-cli -p opencode-desktop
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
}

$stage = Join-Path $root "target\installer-stage"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null
foreach ($binary in @("opencode-desktop.exe", "opencode.exe")) {
    $built = Join-Path $release $binary
    if (-not (Test-Path $built)) { throw "missing $built; build first or drop -SkipBuild" }
    Copy-Item $built $stage
}
Copy-Item $icon $stage

New-Item -ItemType Directory -Force $dist | Out-Null
$installer = Join-Path $dist "opencode-desktop-win-$arch.exe"
if (Test-Path $installer) { Remove-Item -Force $installer }
$makensis = Find-Makensis
& $makensis /V2 /INPUTCHARSET UTF8 "/DSTAGE=$stage" "/DOUTFILE=$installer" "/DVERSION=$version" $script
if ($LASTEXITCODE -ne 0) { throw "makensis failed with exit code $LASTEXITCODE" }

if ($TestInstall) {
    $target = Join-Path ([System.IO.Path]::GetTempPath()) ("opencode-installer-test-" + [System.Guid]::NewGuid().ToString("N"))
    try {
        $run = Start-Process -FilePath $installer -ArgumentList @("/S", "/NOSYSTEM", "/D=$target") -Wait -PassThru
        if ($run.ExitCode -ne 0) { throw "silent install exited with $($run.ExitCode)" }
        $installed = @(Get-ChildItem $target | ForEach-Object Name | Sort-Object)
        $expected = @(@("icon.ico", "opencode-desktop.exe", "opencode.exe", "uninstall.exe") | Sort-Object)
        if (($installed -join ",") -ne ($expected -join ",")) { throw "installed files: $($installed -join ', ')" }
        foreach ($binary in @("opencode-desktop.exe", "opencode.exe")) {
            $a = (Get-FileHash (Join-Path $target $binary)).Hash
            $b = (Get-FileHash (Join-Path $stage $binary)).Hash
            if ($a -ne $b) { throw "$binary differs from the build" }
        }
        # `_?=` keeps the uninstaller in place instead of a copy in %TEMP%, so it cannot delete itself.
        $run = Start-Process -FilePath (Join-Path $target "uninstall.exe") -ArgumentList @("/S", "/NOSYSTEM", "_?=$target") -Wait -PassThru
        if ($run.ExitCode -ne 0) { throw "silent uninstall exited with $($run.ExitCode)" }
        $left = @(Get-ChildItem $target | ForEach-Object Name)
        if (($left -join ",") -ne "uninstall.exe") { throw "left after uninstall: $($left -join ', ')" }
        "test install: ok ($($expected -join ', ') installed and removed)"
    } finally {
        if (Test-Path $target) { Remove-Item -Recurse -Force $target }
    }
}

$hash = (Get-FileHash -Algorithm SHA256 $installer).Hash.ToLower()
"{0}  {1:N1} MB  sha256 {2}" -f $installer, ((Get-Item $installer).Length / 1MB), $hash
