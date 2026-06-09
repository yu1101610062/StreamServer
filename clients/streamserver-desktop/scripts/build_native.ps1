$ErrorActionPreference = "Stop"

$ClientDir = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$Root = Split-Path -Parent (Split-Path -Parent $ClientDir)
$OutDir = Join-Path $ClientDir "build\native"

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Remove-Item -Path (Join-Path $OutDir "SHA256SUMS") -Force -ErrorAction SilentlyContinue
Remove-Item -Path (Join-Path $OutDir "streamserver_desktop*.dll") -Force -ErrorAction SilentlyContinue

cargo build -p streamserver-desktop --release
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}

$source = Join-Path $Root "target\release\streamserver_desktop.dll"
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
    throw "native DLL was not produced: $source"
}
Copy-Item -LiteralPath $source -Destination (Join-Path $OutDir "streamserver_desktop.dll") -Force

Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $OutDir "streamserver_desktop.dll") |
    ForEach-Object { "$($_.Hash.ToLowerInvariant())  streamserver_desktop.dll" } |
    Set-Content -LiteralPath (Join-Path $OutDir "SHA256SUMS") -Encoding ASCII

Write-Host "native library written to $OutDir"
