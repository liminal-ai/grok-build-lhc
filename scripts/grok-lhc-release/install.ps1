# grok-build-lhc Windows installer: the one owner of the managed store,
# checksums, receipts, and activation. Same contract as install.sh:
#   -AssetDir DIR   install a local candidate (release lane, offline)
#   -Download       fetch the release assets from GitHub Releases (or
#                   $env:GROK_LHC_RELEASE_BASE/download/... for a local test server)
# Layout: <store>\versions\<release>\{bin\grok.exe,release-manifest.json},
# <store>\current -> versions\<release> (directory junction, no privilege needed),
# <prefix>\bin\<name>.cmd launcher through current\bin\grok.exe (forwards all
# arguments and the exit status), receipts .grok-lhc-managed, installed-name,
# installed-version, installed-prefix. Never touches %USERPROFILE%\.grok and
# never edits PATH.
[CmdletBinding()]
param(
    [string]$Version = $env:GROK_LHC_VERSION,
    [string]$Name = $env:GROK_LHC_NAME,
    [string]$Prefix = $env:GROK_LHC_PREFIX,
    [string]$InstallRoot = $env:GROK_LHC_INSTALL_ROOT,
    [string]$AssetDir = $env:GROK_LHC_ASSET_DIR,
    [string]$Platform = $env:GROK_LHC_PLATFORM,
    [switch]$Download,
    [switch]$Uninstall
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Fail([string]$Message) { throw "grok-lhc installer: $Message" }
function Absolute([string]$Path) {
    if ([System.IO.Path]::IsPathRooted($Path)) { return [System.IO.Path]::GetFullPath($Path) }
    return [System.IO.Path]::GetFullPath((Join-Path (Get-Location).Path $Path))
}
function ReadReceipt([string]$Path) { return ([System.IO.File]::ReadAllText($Path)).Trim() }
function WriteReceipt([string]$Path, [string]$Value) { [System.IO.File]::WriteAllText($Path, "$Value`n") }
function RemoveJunction([string]$Path) {
    # Removes the link only; never recurses into the target.
    if (Test-Path -LiteralPath $Path) { [System.IO.Directory]::Delete($Path) }
}
# The launcher is BOM-less UTF-8: an ASCII `chcp 65001 >nul` line switches the
# console to UTF-8 before cmd.exe reads the line carrying the executable path, so
# any Unicode install root is preserved (the console stays UTF-8 afterwards).
$launcherEncoding = New-Object System.Text.UTF8Encoding($false)
function ReadLauncher([string]$Path) { return $launcherEncoding.GetString([System.IO.File]::ReadAllBytes($Path)) }

$releaseBase = $env:GROK_LHC_RELEASE_BASE
if ($releaseBase) {
    $downloadBase = "$($releaseBase.TrimEnd('/'))/download"
    $latestUrl = "$($releaseBase.TrimEnd('/'))/latest"
} else {
    $downloadBase = "https://github.com/liminal-ai/grok-build-lhc/releases/download"
    $latestUrl = "https://api.github.com/repos/liminal-ai/grok-build-lhc/releases/latest"
}

if (-not $InstallRoot) { $InstallRoot = Join-Path $env:LOCALAPPDATA "grok-lhc" }
$InstallRoot = Absolute $InstallRoot
if ($InstallRoot -eq [System.IO.Path]::GetPathRoot($InstallRoot) -or ($env:USERPROFILE -and $InstallRoot -eq (Absolute $env:USERPROFILE))) {
    Fail "refusing unsafe install root: $InstallRoot"
}

# Receipts win for an existing store: the recorded command name and prefix are
# reused unless the caller names the same ones explicitly.
$prefixReceipt = Join-Path $InstallRoot "installed-prefix"
if (-not $Prefix -and (Test-Path -LiteralPath $prefixReceipt)) { $Prefix = ReadReceipt $prefixReceipt }
if (-not $Prefix) { $Prefix = Join-Path $env:LOCALAPPDATA "grok-lhc" }
$Prefix = Absolute $Prefix
$BinDir = Join-Path $Prefix "bin"
$marker = Join-Path $InstallRoot ".grok-lhc-managed"
$nameReceipt = Join-Path $InstallRoot "installed-name"
$current = Join-Path $InstallRoot "current"

if ($Uninstall) {
    if (-not (Test-Path -LiteralPath $marker)) { Fail "$InstallRoot is not managed by this installer" }
    if (-not (Test-Path -LiteralPath $nameReceipt)) { Fail "$InstallRoot is missing its installed command receipt" }
    $installedName = ReadReceipt $nameReceipt
    if ($Name -and $Name -ne $installedName) { Fail "installed command is $installedName, not $Name" }
    $Name = $installedName
    if ($Name -notmatch '^[A-Za-z0-9._-]+$') { Fail "invalid installed command receipt" }
    $launcher = Join-Path $BinDir "$Name.cmd"
    if (Test-Path -LiteralPath $launcher) {
        $text = ReadLauncher $launcher
        if ($text -notmatch [regex]::Escape($InstallRoot)) { Fail "$launcher is not managed by this installer" }
        Remove-Item -LiteralPath $launcher -Force
    }
    RemoveJunction $current
    Remove-Item -LiteralPath $InstallRoot -Recurse -Force
    Write-Host "Removed Grok-LHC command and managed packages; user configuration and LHC archives were preserved."
    exit 0
}

if ((Test-Path -LiteralPath $InstallRoot) -and -not (Test-Path -LiteralPath $marker)) {
    Fail "$InstallRoot already exists and is not managed by this installer"
}
if (Test-Path -LiteralPath $nameReceipt) {
    $installedName = ReadReceipt $nameReceipt
    if ($Name -and $Name -ne $installedName) { Fail "managed store is installed as $installedName; use that name" }
    $Name = $installedName
}
if (-not $Name) { $Name = "grok-lhc" }
if ($Name -notmatch '^[A-Za-z0-9._-]+$') { Fail "-Name must be a command name" }
$launcher = Join-Path $BinDir "$Name.cmd"

if ($Download -and $AssetDir) { Fail "-Download and -AssetDir are exclusive" }
if (-not $Download -and -not $AssetDir) { Fail "-AssetDir DIR or -Download is required" }

if ($Download -and -not $Version) {
    $latest = Invoke-RestMethod -UseBasicParsing $latestUrl
    $Version = ([string]$latest.tag_name).Trim()
    if ($Version.StartsWith("v")) { $Version = $Version.Substring(1) }
    if (-not $Version) { Fail "could not resolve the latest release from $latestUrl" }
}
if (-not $Version) { Fail "-Version is required with -AssetDir" }
if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(-lhc\.[0-9]+)?$') { Fail "invalid release: $Version (expected <major>.<minor>.<patch>[-lhc.<n>])" }

if (-not $Platform) {
    $Platform = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "windows-aarch64" } else { "windows-x86_64" }
}
if ($Platform -notin @("windows-x86_64", "windows-aarch64")) { Fail "unsupported platform: $Platform" }
$asset = "grok-$Version-$Platform.exe"

$cleanupDir = $null
try {
    if ($Download) {
        $cleanupDir = Join-Path ([System.IO.Path]::GetTempPath()) "grok-lhc-download-$([guid]::NewGuid())"
        New-Item -ItemType Directory -Path $cleanupDir | Out-Null
        $AssetDir = $cleanupDir
        foreach ($file in @("SHA256SUMS", "release-manifest.json", $asset)) {
            $url = "$downloadBase/v$Version/$file"
            try { Invoke-WebRequest -UseBasicParsing $url -OutFile (Join-Path $AssetDir $file) }
            catch { Fail "download failed: $url" }
        }
    }
    $AssetDir = Absolute $AssetDir
    $assetPath = Join-Path $AssetDir $asset
    $sumsPath = Join-Path $AssetDir "SHA256SUMS"
    $manifestPath = Join-Path $AssetDir "release-manifest.json"
    if (-not (Test-Path -LiteralPath $assetPath)) { Fail "release is missing $asset" }
    if (-not (Test-Path -LiteralPath $sumsPath)) { Fail "release is missing SHA256SUMS" }
    if (-not (Test-Path -LiteralPath $manifestPath)) { Fail "release is missing release-manifest.json" }
    $line = Get-Content -LiteralPath $sumsPath | Where-Object { $_ -match "^\S+\s+\*?$([regex]::Escape($asset))$" } | Select-Object -First 1
    if (-not $line) { Fail "SHA256SUMS does not list $asset" }
    $expected = ($line -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $assetPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { Fail "checksum mismatch for $asset" }
    if (([System.IO.File]::ReadAllText($manifestPath)) -notmatch [regex]::Escape("`"release_version`": `"$Version`"")) {
        Fail "release-manifest.json is not for $Version"
    }

    if (Test-Path -LiteralPath $launcher) {
        $text = ReadLauncher $launcher
        if ($text -notmatch [regex]::Escape($InstallRoot)) { Fail "$launcher already exists; choose another name" }
    }
    $exe = Join-Path (Join-Path $current "bin") "grok.exe"
    $launcherBytes = $launcherEncoding.GetBytes("@echo off`r`nchcp 65001 >nul`r`n`"$($exe.Replace('%', '%%'))`" %*`r`nexit /b %ERRORLEVEL%`r`n")

    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $InstallRoot "versions") -Force | Out-Null
    WriteReceipt $marker "managed by grok-lhc install.ps1"
    $destination = Join-Path (Join-Path $InstallRoot "versions") $Version
    $stage = Join-Path (Join-Path $InstallRoot "versions") ".$Version.tmp.$PID"
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    New-Item -ItemType Directory -Path (Join-Path $stage "bin") -Force | Out-Null
    Copy-Item -LiteralPath $assetPath -Destination (Join-Path (Join-Path $stage "bin") "grok.exe")
    Copy-Item -LiteralPath $manifestPath -Destination (Join-Path $stage "release-manifest.json")
    # Reinstalling the release that is currently running fails here (the image is
    # locked): close grok first. A different release installs beside it.
    if (Test-Path -LiteralPath $destination) { Remove-Item -LiteralPath $destination -Recurse -Force }
    Move-Item -LiteralPath $stage -Destination $destination
    RemoveJunction $current
    New-Item -ItemType Junction -Path $current -Value $destination | Out-Null

    [System.IO.File]::WriteAllBytes($launcher, $launcherBytes)
    WriteReceipt (Join-Path $InstallRoot "installed-version") $Version
    WriteReceipt $nameReceipt $Name
    WriteReceipt $prefixReceipt $Prefix
    Write-Host "Installed Grok-LHC $Version ($Platform) as $launcher"
    Write-Host "Full transcripts are retained separately under GROK_LHC_ROOT (default: ~\.grok-lhc)."
} finally {
    if ($cleanupDir -and (Test-Path -LiteralPath $cleanupDir)) { Remove-Item -LiteralPath $cleanupDir -Recurse -Force }
}
