#Requires -Version 5.1

[CmdletBinding()]
param(
    [string]$BinDir = "",
    [string]$Version = "",
    [string]$Target = "",
    [switch]$PrintTarget
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$ProgressPreference = "SilentlyContinue"
if ($PSVersionTable.PSVersion.Major -lt 6) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
}

$Repo = "world-in-progress/c-two"

# A Windows PE cannot execute on Unix hosts, so the downloaded binary is only
# run as a smoke check where the installed exe can actually run.
$isUnixHost = $PSVersionTable.ContainsKey("Platform") -and $PSVersionTable["Platform"] -eq "Unix"

function Fail([string]$Message) {
    [Console]::Error.WriteLine("c3 installer: $Message")
    exit 1
}

if (-not $Version) { $Version = if ($env:C3_VERSION) { $env:C3_VERSION } else { "latest" } }
if (-not $Target) { $Target = if ($env:C3_TARGET) { $env:C3_TARGET } else { "" } }
if (-not $BinDir) { $BinDir = if ($env:C3_INSTALL_DIR) { $env:C3_INSTALL_DIR } else { "" } }

function Get-DetectedTarget {
    $os = if ($env:C3_INSTALLER_OS) { $env:C3_INSTALLER_OS.ToLowerInvariant() } else { "windows" }
    $arch = if ($env:C3_INSTALLER_ARCH) { $env:C3_INSTALLER_ARCH.ToLowerInvariant() } else {
        [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString().ToLowerInvariant()
    }
    switch ($arch) {
        "x64" { $arch = "x86_64" }
        "amd64" { $arch = "x86_64" }
        "x86_64" { $arch = "x86_64" }
        default { Fail "unsupported CPU architecture: $arch" }
    }
    switch ($os) {
        "windows" { return "$arch-pc-windows-msvc" }
        default { Fail "unsupported operating system: $os" }
    }
}

if (-not $Target) { $Target = Get-DetectedTarget }
if ($PrintTarget) {
    Write-Output $Target
    exit 0
}

function Get-ReleaseBaseUrl {
    if ($env:C3_RELEASE_BASE_URL) {
        return $env:C3_RELEASE_BASE_URL.TrimEnd("/")
    }
    if ($Version -eq "latest") {
        return "https://github.com/$Repo/releases/latest/download"
    }
    $tag = if ($Version.StartsWith("c3-v")) { $Version } else { "c3-v$Version" }
    return "https://github.com/$Repo/releases/download/$tag"
}

if (-not $BinDir) {
    if (-not $env:LOCALAPPDATA) {
        Fail "LOCALAPPDATA is not set; pass -BinDir"
    }
    $BinDir = Join-Path $env:LOCALAPPDATA "Programs\c3"
}

$baseUrl = Get-ReleaseBaseUrl
$name = "c3-$Target.exe"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("c3-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tempDir | Out-Null

try {
    $assetPath = Join-Path $tempDir $name
    $sidecarPath = Join-Path $tempDir "$name.sha256"
    try {
        Invoke-WebRequest -Uri "$baseUrl/$name" -OutFile $assetPath -UseBasicParsing
    } catch {
        Fail "failed to download $baseUrl/$name : $($_.Exception.Message)"
    }
    try {
        Invoke-WebRequest -Uri "$baseUrl/$name.sha256" -OutFile $sidecarPath -UseBasicParsing
    } catch {
        Fail "failed to download checksum sidecar $baseUrl/$name.sha256 : $($_.Exception.Message)"
    }

    $digestText = Get-Content -LiteralPath $sidecarPath -Raw
    if ("$digestText" -notmatch "(?m)^\s*([0-9a-fA-F]{64})(\s|$)") {
        Fail "checksum sidecar $name.sha256 does not contain a sha256 digest"
    }
    $expected = $Matches[1].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $assetPath).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        Fail "checksum mismatch for ${name}: expected $expected, got $actual"
    }

    if (-not $isUnixHost) {
        try {
            $null = & $assetPath --version
        } catch {
            Fail "downloaded c3 binary failed to run on this host; check -Target"
        }
        if ($LASTEXITCODE -ne 0) {
            Fail "downloaded c3 binary failed to run on this host; check -Target"
        }
    }

    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    $installPath = Join-Path $BinDir "c3.exe"
    Copy-Item -LiteralPath $assetPath -Destination $installPath -Force

    Write-Output "Installed c3 to $installPath"
    if (-not $isUnixHost) {
        & $installPath --version
    }
} finally {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}

<#
.SYNOPSIS
Install the c3 CLI from GitHub Releases (Windows).

.DESCRIPTION
Windows counterpart of cli/install-c3.sh. It resolves the release target,
downloads the c3-<target>.exe asset plus its exact .exe.sha256 sidecar from
the world-in-progress/c-two c3-v<Version> release, verifies the SHA256
digest before placing anything, and only then copies c3.exe into the
requested install directory.

.PARAMETER BinDir
Install c3.exe into this directory. Defaults to the per-user directory
"$env:LOCALAPPDATA\Programs\c3", which does not require elevation.

.PARAMETER Version
Install c3-v<Version> instead of the latest release. A value that already
starts with "c3-v" is used as-is.

.PARAMETER Target
Download a specific release target asset, for example
"x86_64-pc-windows-msvc". Defaults to the detected Windows target.

.PARAMETER PrintTarget
Print the detected release target and exit without installing.

.EXAMPLE
.\install-c3.ps1

Install the latest release into the per-user directory.

.EXAMPLE
.\install-c3.ps1 -Version 0.2.0 -BinDir "$HOME\bin"

Install c3-v0.2.0 into the given directory.

.NOTES
Environment variables (mirroring cli/install-c3.sh):

  C3_VERSION            Default version when -Version is not passed.
  C3_TARGET             Default target when -Target is not passed.
  C3_INSTALL_DIR        Default install directory when -BinDir is not passed.
  C3_RELEASE_BASE_URL   Override release asset base URL for mirrors or tests.
  C3_INSTALLER_OS       Override the detected OS name for target selection.
  C3_INSTALLER_ARCH     Override the detected CPU architecture.
#>
