param(
    [ValidateSet("windows")]
    [string]$TargetPlatform = "windows",
    [string]$FlutterRoot = $env:FLUTTER_ROOT
)

$ErrorActionPreference = "Stop"
$ClientDir = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$RepoRoot = Split-Path -Parent (Split-Path -Parent $ClientDir)

function Test-NeedsShortBuildPath([string]$Path) {
    $probe = Join-Path $Path "build\windows\x64\plugins\media_kit_libs_windows_video\x64\Release\media_kit_libs_windows_video_ANGLE_EXTRACT\media_ki.00000000.tlog\media_kit_libs_windows_video_ANGLE_EXTRACT.lastbuildstate"
    return $probe.Length -ge 250
}

function Get-FreeSubstDrive {
    foreach ($letter in @("S", "R", "Q", "T", "U", "V", "W", "Y", "Z")) {
        if ((Get-PSDrive -Name $letter -ErrorAction SilentlyContinue) -or (Test-Path "$letter`:\")) {
            continue
        }
        return "$letter`:"
    }
    throw "no free drive letter is available for the short Windows build path"
}

if (-not $env:STREAMSERVER_DESKTOP_SHORT_BUILD_ACTIVE -and (Test-NeedsShortBuildPath $ClientDir)) {
    $drive = Get-FreeSubstDrive
    subst.exe $drive $RepoRoot
    if ($LASTEXITCODE -ne 0) {
        throw "subst failed with exit code $LASTEXITCODE"
    }
    $env:STREAMSERVER_DESKTOP_SHORT_BUILD_ACTIVE = "1"
    try {
        $shortScript = Join-Path "$drive\" "clients\streamserver-desktop\scripts\package_offline.ps1"
        $args = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $shortScript, "-TargetPlatform", $TargetPlatform)
        if (-not [string]::IsNullOrWhiteSpace($FlutterRoot)) {
            $args += @("-FlutterRoot", $FlutterRoot)
        }
        powershell.exe @args
        $code = $LASTEXITCODE
    } finally {
        Remove-Item Env:\STREAMSERVER_DESKTOP_SHORT_BUILD_ACTIVE -ErrorAction SilentlyContinue
        subst.exe $drive /D | Out-Null
    }
    exit $code
}

Set-Location $ClientDir

function Remove-DirectoryIfExists([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) {
        return
    }
    $resolved = (Resolve-Path -LiteralPath $Path).Path
    if (-not $resolved.StartsWith($ClientDir, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "refusing to remove directory outside client dir: $resolved"
    }
    [System.GC]::Collect()
    [System.GC]::WaitForPendingFinalizers()
    [System.IO.Directory]::Delete("\\?\" + $resolved, $true)
}

if ([string]::IsNullOrWhiteSpace($FlutterRoot)) {
    $candidate = Join-Path $env:USERPROFILE ".codex\toolchains\flutter-3.44.1\flutter"
    if (Test-Path -LiteralPath (Join-Path $candidate "bin\flutter.bat") -PathType Leaf) {
        $FlutterRoot = $candidate
    }
}
if (-not [string]::IsNullOrWhiteSpace($FlutterRoot)) {
    $flutterBinDir = Join-Path $FlutterRoot "bin"
    if (-not (Test-Path -LiteralPath (Join-Path $flutterBinDir "flutter.bat") -PathType Leaf)) {
        throw "invalid FlutterRoot: $FlutterRoot"
    }
    $env:PATH = "$flutterBinDir;$env:PATH"
}

$flutter = Get-Command flutter -ErrorAction SilentlyContinue
if (-not $flutter) {
    throw "flutter was not found on PATH; pass -FlutterRoot or set FLUTTER_ROOT"
}

& (Join-Path $ClientDir "scripts\build_native.ps1")
& $flutter.Source pub get
if ($LASTEXITCODE -ne 0) {
    throw "flutter pub get failed with exit code $LASTEXITCODE"
}
Remove-DirectoryIfExists (Join-Path $ClientDir "build\windows")
& $flutter.Source build windows --release
if ($LASTEXITCODE -ne 0) {
    throw "flutter build windows failed with exit code $LASTEXITCODE"
}

$releaseDir = Join-Path $ClientDir "build\windows\x64\runner\Release"
if (-not (Test-Path -LiteralPath $releaseDir -PathType Container)) {
    throw "Windows release directory was not produced: $releaseDir"
}
Remove-Item -Path (Join-Path $releaseDir "streamserver_desktop*.dll") -Force -ErrorAction SilentlyContinue
Copy-Item -LiteralPath (Join-Path $ClientDir "build\native\streamserver_desktop.dll") -Destination $releaseDir -Force

$distDir = Join-Path $ClientDir "dist"
New-Item -ItemType Directory -Force -Path $distDir | Out-Null
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$artifact = "streamserver-desktop-windows-x64-$stamp.zip"
$artifactPath = Join-Path $distDir $artifact
Remove-Item -LiteralPath $artifactPath -Force -ErrorAction SilentlyContinue

Compress-Archive -Path (Join-Path $releaseDir "*") -DestinationPath $artifactPath -Force
Get-FileHash -Algorithm SHA256 -LiteralPath $artifactPath |
    ForEach-Object { "$($_.Hash.ToLowerInvariant())  $artifact" } |
    Set-Content -LiteralPath (Join-Path $distDir "SHA256SUMS") -Encoding ASCII

Write-Host "offline package written to $artifactPath"
