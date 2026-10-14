# Downloads MediaPipe's face landmarker model (float16, v1) into models\ and
# verifies its SHA-256. Run from the repository root.
$ErrorActionPreference = 'Stop'

$url    = 'https://storage.googleapis.com/mediapipe-models/face_landmarker/face_landmarker/float16/1/face_landmarker.task'
$sha256 = '64184E229B263107BC2B804C6625DB1341FF2BB731874B0BCC2FE6544E0BC9FF'
$dir    = Join-Path $PSScriptRoot '..\models'
$out    = Join-Path $dir 'face_landmarker.task'

New-Item -ItemType Directory -Force $dir | Out-Null
if ((Test-Path $out) -and ((Get-FileHash $out -Algorithm SHA256).Hash -eq $sha256)) {
    Write-Host "already present: $out"; exit 0
}
Invoke-WebRequest -Uri $url -OutFile "$out.tmp" -UseBasicParsing
$got = (Get-FileHash "$out.tmp" -Algorithm SHA256).Hash
if ($got -ne $sha256) { Remove-Item "$out.tmp"; throw "SHA-256 mismatch: expected $sha256, got $got" }
Move-Item -Force "$out.tmp" $out
Write-Host "saved $out"
