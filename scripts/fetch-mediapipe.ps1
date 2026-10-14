# Fetches libmediapipe.dll for the `mediapipe` crate (MediaPipe 0.10.35) from the
# official PyPI wheel, verifies the SHA-256 pinned by the crate, and installs it
# where the crate looks: %LOCALAPPDATA%\mediapipe-rs\0.10.35\libmediapipe.dll.
#
# Normally the crate downloads this itself on first run. Use this script when
# that fails (e.g. corporate TLS inspection: the crate uses rustls, which does
# not trust the Windows certificate store; PowerShell does).
$ErrorActionPreference = 'Stop'

$url    = 'https://files.pythonhosted.org/packages/5b/f6/763477e9aeed98accc984ed6ee3f11a21a0c5fd1d1c6586b8d07067748ff/mediapipe-0.10.35-py3-none-win_amd64.whl'
$sha256 = 'B08F001CF3C3CD0D88D9ED68F3368DC8A4913F568281A93117F083115AA672BA'
$dest   = Join-Path $env:LOCALAPPDATA 'mediapipe-rs\0.10.35'
$dll    = Join-Path $dest 'libmediapipe.dll'

if (Test-Path $dll) { Write-Host "already installed: $dll"; exit 0 }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("mediapipe-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    $whl = Join-Path $tmp 'mediapipe.whl'
    Write-Host "downloading $url"
    Invoke-WebRequest -Uri $url -OutFile $whl -UseBasicParsing
    $got = (Get-FileHash $whl -Algorithm SHA256).Hash
    if ($got -ne $sha256) { throw "SHA-256 mismatch: expected $sha256, got $got" }

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($whl)
    try {
        $entry = $zip.Entries | Where-Object { $_.FullName -like '*libmediapipe.dll' } | Select-Object -First 1
        if (-not $entry) { throw 'wheel contains no libmediapipe.dll' }
        New-Item -ItemType Directory -Force $dest | Out-Null
        [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, "$dll.tmp", $true)
        Move-Item -Force "$dll.tmp" $dll
    } finally { $zip.Dispose() }
    Write-Host "installed $dll"
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
