param(
    [ValidatePattern('^\d+\.\d+\.\d+([-.][0-9A-Za-z.-]+)?$')]
    [string]$Version,

    [string]$ProjectRoot,

    [string]$OutputRoot,

    [ValidateSet('x64', 'arm64')]
    [string]$Architecture = 'x64'
)

$ErrorActionPreference = 'Stop'

function Get-Sha256Hex([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    try {
        $sha256 = [Security.Cryptography.SHA256]::Create()
        try {
            return ([BitConverter]::ToString($sha256.ComputeHash($stream))).Replace('-', '')
        }
        finally {
            $sha256.Dispose()
        }
    }
    finally {
        $stream.Dispose()
    }
}

if ([string]::IsNullOrWhiteSpace($ProjectRoot)) {
    $ProjectRoot = Split-Path -Parent $PSScriptRoot
}
if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $OutputRoot = Join-Path $ProjectRoot 'release'
}

$project = [IO.Path]::GetFullPath($ProjectRoot)
$output = [IO.Path]::GetFullPath($OutputRoot)
$packageJson = Join-Path $project 'package.json'
if ([string]::IsNullOrWhiteSpace($Version)) {
    if (-not (Test-Path -LiteralPath $packageJson -PathType Leaf)) {
        throw "package.json not found: $packageJson"
    }
    $Version = (Get-Content -LiteralPath $packageJson -Raw | ConvertFrom-Json).version
    if ($Version -notmatch '^\d+\.\d+\.\d+([-.][0-9A-Za-z.-]+)?$') {
        throw "package.json contains an invalid version: $Version"
    }
}
$targetTriple = if ($Architecture -eq 'arm64') {
    'aarch64-pc-windows-msvc'
}
else {
    $null
}
$releaseDirectory = if ($targetTriple) {
    Join-Path $project "src-tauri\target\$targetTriple\release"
}
else {
    Join-Path $project 'src-tauri\target\release'
}
$releaseExe = Join-Path $releaseDirectory 'BnetSwitchLite.exe'
$artifact = Join-Path $output "BnetSwitchLite-$Version-windows-$Architecture.exe"

if (-not (Test-Path -LiteralPath $releaseExe -PathType Leaf)) {
    throw "Release EXE not found for ${Architecture}: $releaseExe"
}

$releaseDlls = @(Get-ChildItem -LiteralPath $releaseDirectory -File -Filter '*.dll')
if ($releaseDlls.Count -gt 0) {
    throw "Unexpected DLLs next to the Release EXE: $($releaseDlls.Name -join ', ')"
}

if (Test-Path -LiteralPath $artifact) {
    Remove-Item -LiteralPath $artifact -Force
}

New-Item -ItemType Directory -Path $output -Force | Out-Null
Copy-Item -LiteralPath $releaseExe -Destination $artifact

$sourceHash = Get-Sha256Hex $releaseExe
$artifactHash = Get-Sha256Hex $artifact
if ($sourceHash -ne $artifactHash) {
    throw 'Packaged EXE does not match the Release EXE.'
}

$artifactInfo = Get-Item -LiteralPath $artifact
Write-Output ([pscustomobject]@{
    Path = $artifactInfo.FullName
    Size = $artifactInfo.Length
    SHA256 = $artifactHash
})
