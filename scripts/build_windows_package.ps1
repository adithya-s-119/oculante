[CmdletBinding()]
param(
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$versionLine = Get-Content Cargo.toml |
    Where-Object { $_ -match '^version\s*=\s*"([^"]+)"' } |
    Select-Object -First 1
if (-not $versionLine) {
    throw "Could not read the package version from Cargo.toml"
}
$version = [regex]::Match($versionLine, '"([^"]+)"').Groups[1].Value

$exe = Join-Path $root "target\release\pictor.exe"
if (-not $SkipBuild) {
    if (Get-Process pictor -ErrorAction SilentlyContinue) {
        throw "Close Pictor before building the package."
    }
    $env:PATH = "$root\tools\nasm-2.16.03;$env:PATH"
    & cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed"
    }
}
if (-not (Test-Path $exe)) {
    throw "Missing $exe. Run cargo build --release first."
}

$gcc = (Get-Command gcc -ErrorAction Stop).Source
$mingwBin = Split-Path -Parent $gcc
$dlls = @(
    "libstdc++-6.dll",
    "libgcc_s_seh-1.dll",
    "libwinpthread-1.dll"
)
foreach ($dll in $dlls) {
    $path = Join-Path $mingwBin $dll
    if (-not (Test-Path $path)) {
        throw "Missing MinGW runtime DLL: $path"
    }
}

$dist = Join-Path $root "dist"
$stage = Join-Path $dist "Pictor-$version-portable"
$zip = Join-Path $dist "Pictor-$version-portable.zip"
$setup = Join-Path $dist "Pictor-Setup-$version.exe"

if (Test-Path $stage) {
    Remove-Item $stage -Recurse -Force
}
New-Item -ItemType Directory -Path $stage -Force | Out-Null

Copy-Item $exe $stage
foreach ($dll in $dlls) {
    Copy-Item (Join-Path $mingwBin $dll) $stage
}
Copy-Item (Join-Path $root "README.md") $stage
Copy-Item (Join-Path $root "LICENSE") $stage
Copy-Item (Join-Path $root "installer\THIRD-PARTY-NOTICES.txt") $stage

if (Test-Path $zip) {
    Remove-Item $zip -Force
}
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip -CompressionLevel Optimal

$makensis = "C:\Program Files (x86)\NSIS\makensis.exe"
if (-not (Test-Path $makensis)) {
    $command = Get-Command makensis -ErrorAction SilentlyContinue
    if ($command) {
        $makensis = $command.Source
    } else {
        throw "NSIS not found. Install it with: winget install NSIS.NSIS"
    }
}

$config = @"
!define VERSION "$version"
!define SOURCE "$stage"
!define OUTFILE "$setup"
!define ICON "$root\icon.ico"
!define LICENSE "$root\LICENSE"
"@
$configPath = Join-Path $root "installer\build-config.nsh"
[System.IO.File]::WriteAllText($configPath, $config, [System.Text.UTF8Encoding]::new($false))
try {
    & $makensis /V2 (Join-Path $root "installer\pictor.nsi")
    if ($LASTEXITCODE -ne 0) {
        throw "makensis failed"
    }
} finally {
    Remove-Item $configPath -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "Portable ZIP: $zip"
Write-Host "Installer:    $setup"
