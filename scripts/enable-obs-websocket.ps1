# Enables OBS WebSocket (port 4455) with authentication and stores the password
# for AutoCam in %APPDATA%\AutoCam\config.toml. Reuses an existing OBS WebSocket
# password if one is already configured. Never prints the password.
#
# OBS must be closed: it rewrites its WebSocket settings on exit.
$ErrorActionPreference = 'Stop'

if (Get-Process obs64 -ErrorAction SilentlyContinue) {
    throw 'OBS is running. Close it (File > Exit) and run this script again.'
}

$wsDir  = Join-Path $env:APPDATA 'obs-studio\plugin_config\obs-websocket'
$wsFile = Join-Path $wsDir 'config.json'

$password = $null
if (Test-Path $wsFile) {
    $existing = Get-Content $wsFile -Raw | ConvertFrom-Json
    if ($existing.auth_required -and $existing.server_password) {
        $password = $existing.server_password
    }
}
if (-not $password) {
    $bytes = New-Object byte[] 24
    [System.Security.Cryptography.RandomNumberGenerator]::Fill($bytes)
    $password = [Convert]::ToBase64String($bytes).Replace('+', 'A').Replace('/', 'B').Replace('=', '')
}

New-Item -ItemType Directory -Force $wsDir | Out-Null
[ordered]@{
    alerts_enabled  = $false
    auth_required   = $true
    first_load      = $false
    server_enabled  = $true
    server_password = $password
    server_port     = 4455
} | ConvertTo-Json | Set-Content -Encoding utf8NoBOM $wsFile

# Skip OBS's first-run auto-configuration wizard if OBS has never run.
$obsDir = Join-Path $env:APPDATA 'obs-studio'
foreach ($ini in 'global.ini', 'user.ini') {
    $p = Join-Path $obsDir $ini
    if (-not (Test-Path $p)) { Set-Content -Encoding utf8NoBOM $p "[General]`r`nFirstRun=true`r`n" }
}

# Store the password in AutoCam's config, preserving everything else.
$acDir  = Join-Path $env:APPDATA 'AutoCam'
$acFile = Join-Path $acDir 'config.toml'
New-Item -ItemType Directory -Force $acDir | Out-Null
$line = "password = `"$password`""
if (Test-Path $acFile) {
    $text = Get-Content $acFile -Raw
    if ($text -match '(?m)^\[obs\]\s*$') {
        if ($text -match '(?ms)^\[obs\]\s*$(.*?)(^\[|\z)' -and $Matches[1] -match '(?m)^password\s*=') {
            $text = [regex]::Replace($text, '(?m)^password\s*=.*$', $line, 1)
        } else {
            $text = [regex]::Replace($text, '(?m)^\[obs\]\s*$', "[obs]`r`n$line", 1)
        }
    } else {
        $text = "[obs]`r`n$line`r`n`r`n" + $text
    }
    Set-Content -Encoding utf8NoBOM -NoNewline $acFile $text
} else {
    Set-Content -Encoding utf8NoBOM $acFile "[obs]`r`n$line`r`n"
}

Write-Host "OBS WebSocket: enabled on port 4455 with authentication."
Write-Host "Password stored in $acFile (not shown)."
Write-Host "Next: start OBS, then run: .\target\release\autocam.exe obs-setup --left-device <name> --right-device <name>"
