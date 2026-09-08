# Windows installer lifecycle fixture (runs on the Windows build runner):
# local asset-dir mode, download mode against a loopback server, receipts
# (name/prefix), junction + launcher, argument/exit forwarding, refusals.
# The fixture binary is where.exe; the real candidate is exercised by
# candidate_check.ps1.
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$installer = Join-Path $PSScriptRoot "install.ps1"
$tempBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
# The whole fixture lives under a Unicode path beyond any single OEM code page
# (u-umlaut U+00FC and U+6F22) so the launcher, receipts, junction, and download
# server all carry it; the code points keep this file ASCII.
$root = Join-Path $tempBase "grok-lhc-install-test-$([char]0x00FC)$([char]0x6F22)-$([guid]::NewGuid())"
$platform = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "windows-aarch64" } else { "windows-x86_64" }
$fixture = "$env:SystemRoot\System32\where.exe"
$server = $null

function Check([bool]$Condition, [string]$Message) { if (-not $Condition) { throw "FAIL: $Message" }; Write-Host "ok   $Message" }
function Expect([scriptblock]$Block, [string]$Message) {
    $failed = $false
    try { & $Block | Out-Null } catch { $failed = $true }
    Check $failed $Message
}
function MakeRelease([string]$Version) {
    # One release directory: asset (where.exe re-tagged), SHA256SUMS, release-manifest.json.
    $rel = Join-Path $root "release-$Version"
    New-Item -ItemType Directory -Path $rel -Force | Out-Null
    $asset = "grok-$Version-$platform.exe"
    Copy-Item $fixture (Join-Path $rel $asset)
    $digest = (Get-FileHash (Join-Path $rel $asset) -Algorithm SHA256).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText((Join-Path $rel "SHA256SUMS"), "$digest  $asset`n")
    [System.IO.File]::WriteAllText((Join-Path $rel "release-manifest.json"), "{`n  `"product`": `"grok-lhc`",`n  `"release_version`": `"$Version`"`n}`n")
    return $rel
}
function Receipt([string]$Store, [string]$Name) { return ([System.IO.File]::ReadAllText((Join-Path $Store $Name))).Trim() }
# Named splatting: an array splat would bind positionally (-Version would land in $Version as text).
function Install([hashtable]$Arguments) { & $installer @Arguments | Out-Null }

try {
    New-Item -ItemType Directory -Path $root | Out-Null
    $first = MakeRelease "1.0.16"
    $second = MakeRelease "1.0.16-lhc.1"
    $prefix = Join-Path $root "prefix"
    $store = Join-Path $root "packages"
    $data = Join-Path $root "lhc-data"
    New-Item -ItemType Directory -Path $data | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $data "keep"), "archive")

    # Fresh install: default name grok-lhc, receipts, junction, launcher forwards args and exit status.
    Install @{ Version = "1.0.16"; AssetDir = $first; Prefix = $prefix; InstallRoot = $store }
    $launcher = Join-Path $prefix "bin\grok-lhc.cmd"
    Check (Test-Path $launcher) "launcher installed at $launcher"
    Check ((Receipt $store "installed-name") -eq "grok-lhc") "installed-name = grok-lhc"
    Check ((Receipt $store "installed-version") -eq "1.0.16") "installed-version = 1.0.16"
    Check ((Receipt $store "installed-prefix") -eq $prefix) "installed-prefix recorded"
    $current = Get-Item (Join-Path $store "current")
    Check ($current.LinkType -eq "Junction") "current is a directory junction"
    $viaCurrent = (Get-FileHash (Join-Path $store "current\bin\grok.exe") -Algorithm SHA256).Hash
    $viaVersion = (Get-FileHash (Join-Path $store "versions\1.0.16\bin\grok.exe") -Algorithm SHA256).Hash
    Check ($viaCurrent -eq $viaVersion) "current\bin\grok.exe resolves to versions\1.0.16"
    $launcherBytes = [System.IO.File]::ReadAllBytes($launcher)
    Check ($launcherBytes[0] -eq 0x40) "launcher has no BOM (starts with @echo off)"
    $launcherText = (New-Object System.Text.UTF8Encoding($false, $true)).GetString($launcherBytes)
    Check ($launcherText.Contains("`r`nchcp 65001 >nul`r`n`"$store\current\bin\grok.exe`" %*`r`n")) "launcher switches to UTF-8 and carries the Unicode store path intact"
    & $launcher /Q cmd.exe | Out-Null
    Check ($LASTEXITCODE -eq 0) "launcher forwards arguments (where /Q cmd.exe -> 0)"
    & $launcher /Q definitely-not-a-program-$([guid]::NewGuid()) | Out-Null
    Check ($LASTEXITCODE -eq 1) "launcher forwards the exit status (where -> 1)"

    # Collision with an unmanaged command of the requested name refuses.
    [System.IO.File]::WriteAllText((Join-Path $prefix "bin\other.cmd"), "@echo off`r`nexit /b 0`r`n")
    Expect { Install @{ Version = "1.0.16"; AssetDir = $first; Prefix = $prefix; InstallRoot = (Join-Path $root "p2"); Name = "other" } } "refuses to replace an unmanaged other.cmd"
    Check (-not (Test-Path (Join-Path $root "p2"))) "refused install leaves no store"

    # Update names only the store: name and prefix come from receipts; old versions are kept.
    Install @{ Version = "1.0.16-lhc.1"; AssetDir = $second; InstallRoot = $store }
    Check ((Receipt $store "installed-version") -eq "1.0.16-lhc.1") "update recorded 1.0.16-lhc.1"
    Check ((Receipt $store "installed-name") -eq "grok-lhc") "update kept the name"
    Check ((Receipt $store "installed-prefix") -eq $prefix) "update kept the prefix"
    Check (Test-Path (Join-Path $store "versions\1.0.16\bin\grok.exe")) "old version kept"
    $viaCurrent = (Get-FileHash (Join-Path $store "current\bin\grok.exe") -Algorithm SHA256).Hash
    Check ((Get-Item (Join-Path $store "current")).LinkType -eq "Junction") "current still a junction after update"
    Check ((Get-Content (Join-Path $store "current\release-manifest.json") -Raw) -match "1\.0\.16-lhc\.1") "current points at the updated release"
    & $launcher /Q cmd.exe | Out-Null
    Check ($LASTEXITCODE -eq 0) "launcher still runs through current"

    # Name change for a managed store refuses; invalid release strings never reach the store.
    Expect { Install @{ Version = "1.0.16"; AssetDir = $first; InstallRoot = $store; Name = "grok-other" } } "refuses a command name change"
    Check (-not (Test-Path (Join-Path $prefix "bin\grok-other.cmd"))) "no grok-other launcher"
    Expect { Install @{ Version = "1.0.16-alpha.1"; AssetDir = $first; InstallRoot = (Join-Path $root "bad") } } "refuses an invalid release string"

    # Uninstall takes name and prefix from receipts and preserves user data.
    Install @{ InstallRoot = $store; Uninstall = $true }
    Check (-not (Test-Path $launcher)) "launcher removed"
    Check (-not (Test-Path $store)) "store removed"
    Check (Test-Path (Join-Path $data "keep")) "LHC data preserved"
    Check (Test-Path (Join-Path $prefix "bin\other.cmd")) "unrelated other.cmd untouched"

    # Checksum mismatch and a manifest for another release refuse before anything is written.
    $tampered = MakeRelease "1.0.17"
    [System.IO.File]::WriteAllText((Join-Path $tampered "grok-1.0.17-$platform.exe"), "tampered")
    Expect { Install @{ Version = "1.0.17"; AssetDir = $tampered; Prefix = $prefix; InstallRoot = (Join-Path $root "t") } } "refuses a checksum mismatch"
    Check (-not (Test-Path (Join-Path $root "t"))) "mismatch leaves no store"
    $wrong = MakeRelease "1.0.18"
    [System.IO.File]::WriteAllText((Join-Path $wrong "release-manifest.json"), "{`"release_version`": `"0.0.0`"}`n")
    Expect { Install @{ Version = "1.0.18"; AssetDir = $wrong; Prefix = $prefix; InstallRoot = (Join-Path $root "w") } } "refuses a manifest for another release"

    # An existing unmanaged directory is never taken over.
    $owned = Join-Path $root "owned"
    New-Item -ItemType Directory -Path $owned | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $owned "keep"), "user-owned")
    Expect { Install @{ Version = "1.0.16"; AssetDir = $first; Prefix = $prefix; InstallRoot = $owned } } "refuses an unowned install root"
    Check (([System.IO.File]::ReadAllText((Join-Path $owned "keep"))) -eq "user-owned") "unowned root untouched"

    # Download mode against a loopback server: latest resolution, platform asset, receipts.
    $site = Join-Path $root "site"
    New-Item -ItemType Directory -Path (Join-Path $site "download\v1.0.16-lhc.2") -Force | Out-Null
    $third = MakeRelease "1.0.16-lhc.2"
    Copy-Item (Join-Path $third "*") (Join-Path $site "download\v1.0.16-lhc.2")
    [System.IO.File]::WriteAllText((Join-Path $site "latest"), "{`"tag_name`": `"v1.0.16-lhc.2`"}")
    $port = Get-Random -Minimum 20000 -Maximum 40000
    $server = Start-Process python -ArgumentList @("-m", "http.server", "$port", "--bind", "127.0.0.1", "--directory", $site) -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 2
    $env:GROK_LHC_RELEASE_BASE = "http://127.0.0.1:$port"
    $dlStore = Join-Path $root "dl-store"
    Install @{ Download = $true; Prefix = $prefix; InstallRoot = $dlStore }
    Check ((Receipt $dlStore "installed-version") -eq "1.0.16-lhc.2") "download mode resolved latest = 1.0.16-lhc.2"
    Check (Test-Path (Join-Path $prefix "bin\grok-lhc.cmd")) "download mode installed the default command"
    Expect { Install @{ Download = $true; Version = "9.9.9"; InstallRoot = (Join-Path $root "m") } } "missing release refuses"
    Check (-not (Test-Path (Join-Path $root "m"))) "missing release leaves no store"
    Install @{ InstallRoot = $dlStore; Uninstall = $true }
    Check (-not (Test-Path $dlStore)) "download-mode store removed"
    Write-Host "Windows installer fixture: PASS"
} finally {
    if ($server) { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item Env:GROK_LHC_RELEASE_BASE -ErrorAction SilentlyContinue
    if (Test-Path $root) { Remove-Item $root -Recurse -Force }
}
