# On-runner qualification of the exact Windows candidate executable (release
# lane, no network beyond loopback): architecture, identity, isolated
# installer lifecycle with the real binary, launcher argument/exit forwarding,
# native `grok update` against local release assets, uninstall with data
# preserved, and stock paths untouched. Reusing the candidate bytes under a
# later fixture tag proves transfer and activation only: `--lhc-version`
# stays the embedded release throughout (asserted). Same/later-version
# decisions are covered by the update crate's lhc_decisions unit tests.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$Version
)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$installer = Join-Path $PSScriptRoot "install.ps1"
$Exe = [System.IO.Path]::GetFullPath($Exe)
$tempBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
$root = Join-Path $tempBase "grok-lhc-candidate-$([guid]::NewGuid())"
$platform = "windows-x86_64"
$upstream = $Version -replace '-lhc\.\d+$', ''
if ($Version -match '-lhc\.(\d+)$') { $next = "$upstream-lhc.$([int]$Matches[1] + 1)" } else { $next = "$upstream-lhc.1" }
$server = $null
$failures = 0

function Check([bool]$Condition, [string]$Message) {
    if ($Condition) { Write-Host "ok   $Message" } else { Write-Host "FAIL $Message"; $script:failures++ }
}
function Native([string]$Command, [string[]]$Arguments) {
    # Run a native command, capturing both streams and the exit status.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $output = (& $Command @Arguments 2>&1 | Out-String)
        return @{ Code = $LASTEXITCODE; Out = $output }
    } finally { $ErrorActionPreference = $previous }
}
function Receipt([string]$Store, [string]$Name) { return ([System.IO.File]::ReadAllText((Join-Path $Store $Name))).Trim() }
function Sha([string]$Path) { return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function MakeRelease([string]$Tag) {
    # Release directory under the local site: candidate bytes re-tagged, this
    # tree's install.ps1, SHA256SUMS over both, release-manifest.json.
    $rel = Join-Path $site "download\v$Tag"
    New-Item -ItemType Directory -Path $rel -Force | Out-Null
    $asset = "grok-$Tag-$platform.exe"
    Copy-Item -LiteralPath $Exe -Destination (Join-Path $rel $asset)
    Copy-Item -LiteralPath $installer -Destination (Join-Path $rel "install.ps1")
    $sums = "$(Sha (Join-Path $rel $asset))  $asset`n$(Sha (Join-Path $rel 'install.ps1'))  install.ps1`n"
    [System.IO.File]::WriteAllText((Join-Path $rel "SHA256SUMS"), $sums)
    [System.IO.File]::WriteAllText((Join-Path $rel "release-manifest.json"), "{`n  `"product`": `"grok-lhc`",`n  `"release_version`": `"$Tag`"`n}`n")
    return $rel
}

try {
    New-Item -ItemType Directory -Path $root | Out-Null
    $candidateSha = Sha $Exe
    Write-Host "candidate $Exe sha256=$candidateSha version=$Version next-fixture=$next"

    # 1. PE machine type is x64 (0x8664).
    $bytes = [System.IO.File]::ReadAllBytes($Exe)
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
    $machine = [BitConverter]::ToUInt16($bytes, $peOffset + 4)
    Check ($machine -eq 0x8664) ("PE machine type 0x{0:X4} is x86_64" -f $machine)

    # 2. Identity from the bare executable.
    $r = Native $Exe @("--version")
    Check ($r.Code -eq 0 -and $r.Out -match [regex]::Escape("grok $upstream")) "--version reports upstream base grok $upstream"
    $r = Native $Exe @("--lhc-version")
    Check ($r.Code -eq 0 -and $r.Out.Trim() -eq $Version) "--lhc-version = $Version"
    $r = Native $Exe @("--help")
    Check ($r.Code -eq 0) "--help exits 0"

    # 3. Isolated home, LHC root, and stock-side state that must never change.
    $isolatedHome = Join-Path $root "home"
    $grokHome = Join-Path $isolatedHome ".grok"
    $lhcRoot = Join-Path $root "lhc-root"
    New-Item -ItemType Directory -Path $grokHome, $lhcRoot | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $grokHome "config.toml"), "[cli]`ninstaller = `"gh-release`"`nauto_update = false`n")
    [System.IO.File]::WriteAllText((Join-Path $lhcRoot "keep"), "archive")
    $configSha = Sha (Join-Path $grokHome "config.toml")
    $env:HOME = $isolatedHome
    $env:USERPROFILE = $isolatedHome
    $env:GROK_HOME = $grokHome
    $env:GROK_LHC_ROOT = $lhcRoot
    $env:GROK_DISABLE_TELEMETRY = "1"

    # 4. Local release site: the candidate tag and one later fixture tag (same bytes).
    $site = Join-Path $root "site"
    $release = MakeRelease $Version
    $null = MakeRelease $next
    [System.IO.File]::WriteAllText((Join-Path $site "latest"), "{`"tag_name`": `"v$next`"}")
    $port = Get-Random -Minimum 20000 -Maximum 40000
    $server = Start-Process python -ArgumentList @("-m", "http.server", "$port", "--bind", "127.0.0.1", "--directory", $site) -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 2
    $env:GROK_LHC_RELEASE_BASE = "http://127.0.0.1:$port"

    # 5. Install the candidate from local assets with a custom name and prefix.
    $prefix = Join-Path $root "prefix"
    $store = Join-Path $root "store"
    & $installer -Version $Version -Name grok-lhc-check -Prefix $prefix -InstallRoot $store -AssetDir $release | Out-Null
    $launcher = Join-Path $prefix "bin\grok-lhc-check.cmd"
    Check (Test-Path $launcher) "launcher installed"
    Check ((Receipt $store "installed-name") -eq "grok-lhc-check" -and (Receipt $store "installed-version") -eq $Version -and (Receipt $store "installed-prefix") -eq $prefix) "receipts name/version/prefix"
    Check ((Get-Item (Join-Path $store "current")).LinkType -eq "Junction") "current is a junction"
    Check ((Sha (Join-Path $store "current\bin\grok.exe")) -eq $candidateSha) "installed bytes equal the candidate"

    # 6. Launcher forwards arguments and the exit status; managed classification.
    $r = Native $launcher @("--lhc-version")
    Check ($r.Code -eq 0 -and $r.Out.Trim() -eq $Version) "launcher --lhc-version = $Version (arguments forwarded)"
    $r = Native $launcher @("--version")
    Check ($r.Code -eq 0 -and $r.Out -match [regex]::Escape("grok $upstream")) "launcher --version = grok $upstream"
    $r = Native $launcher @("--definitely-not-a-grok-flag")
    Check ($r.Code -ne 0) "launcher forwards a non-zero exit status ($($r.Code))"
    $r = Native $launcher @("update", "--check", "--json")
    Check ($r.Code -eq 0 -and $r.Out -match '"installer"\s*:\s*"lhc-managed"') "update --check: installer = lhc-managed"
    Check ($r.Out -match [regex]::Escape("`"latestVersion`": `"$next`"") -or $r.Out -match [regex]::Escape("`"latestVersion`":`"$next`"")) "update --check sees local latest $next"
    Check ($r.Out -match '"updateAvailable"\s*:\s*true') "update --check: updateAvailable = true"

    # 7. Native update: the binary fetches the tag's install.ps1 + SHA256SUMS from the
    #    local site, verifies, runs PowerShell, activates the fixture tag in this store.
    $r = Native $launcher @("update")
    Write-Host $r.Out
    Check ($r.Code -eq 0) "grok update exit 0"
    Check ((Receipt $store "installed-version") -eq $next) "installed-version = $next after update"
    Check ((Receipt $store "installed-name") -eq "grok-lhc-check" -and (Receipt $store "installed-prefix") -eq $prefix) "name and prefix kept"
    Check ((Get-Item (Join-Path $store "current")).LinkType -eq "Junction" -and ((Get-Content (Join-Path $store "current\release-manifest.json") -Raw) -match [regex]::Escape($next))) "current junction repointed at versions\$next"
    Check (Test-Path (Join-Path $store "versions\$Version\bin\grok.exe")) "previous version kept"
    Check (Test-Path (Join-Path $store "version.json")) "update cache lives in the store"
    Check (-not (Test-Path (Join-Path $grokHome "version.json")) -and -not (Test-Path (Join-Path $grokHome "bin")) -and -not (Test-Path (Join-Path $grokHome "downloads"))) "no ~/.grok/version.json, bin, or downloads"
    $r = Native $launcher @("--lhc-version")
    Check ($r.Code -eq 0 -and $r.Out.Trim() -eq $Version) "fixture caveat: --lhc-version still $Version (same bytes re-tagged; activation proven, not a new revision)"
    $r = Native $launcher @("update", "--check", "--json")
    Write-Host $r.Out
    Check ($r.Code -eq 0) "update --check after update exits 0 (decision semantics: lhc_decisions unit tests)"

    # 8. Uninstall through the receipts; data and stock paths preserved.
    & $installer -InstallRoot $store -Uninstall | Out-Null
    Check (-not (Test-Path $launcher)) "launcher removed"
    Check (-not (Test-Path $store)) "store removed"
    Check ((Get-Content (Join-Path $lhcRoot "keep") -Raw) -eq "archive") "LHC data preserved"
    Check ((Sha (Join-Path $grokHome "config.toml")) -eq $configSha) "shared config.toml byte-identical (installer key untouched)"
    if ($failures -eq 0) { Write-Host "Windows candidate check: PASS" } else { Write-Host "Windows candidate check: $failures FAILED" }
} finally {
    if ($server) { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item Env:GROK_LHC_RELEASE_BASE -ErrorAction SilentlyContinue
    if (Test-Path $root) { Remove-Item $root -Recurse -Force -ErrorAction SilentlyContinue }
}
exit $failures
