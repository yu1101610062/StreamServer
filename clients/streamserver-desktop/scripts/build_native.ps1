$ErrorActionPreference = "Stop"

$ClientDir = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$Root = Split-Path -Parent (Split-Path -Parent $ClientDir)
$OutDir = Join-Path $ClientDir "build\native"

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

cargo build -p streamserver-desktop-native --release
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}

$source = Join-Path $Root "target\release\streamserver_desktop_native.dll"
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
    throw "native DLL was not produced: $source"
}
Copy-Item -LiteralPath $source -Destination (Join-Path $OutDir "streamserver_desktop_native.dll") -Force

Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $OutDir "streamserver_desktop_native.dll") |
    ForEach-Object { "$($_.Hash.ToLowerInvariant())  streamserver_desktop_native.dll" } |
    Set-Content -LiteralPath (Join-Path $OutDir "SHA256SUMS") -Encoding ASCII

Write-Host "native library written to $OutDir"
