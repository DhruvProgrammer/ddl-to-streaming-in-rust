#!/usr/bin/env pwsh
# Hold the stack open so an external tool (k6, curl, a browser) can drive it.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\hold-stack.ps1 -Seconds 300

param(
  [int]$Seconds = 300,
  [int]$OriginPort = 9000,
  [int]$ServerPort = 8787
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "backend\target\release\ddl-player.exe"
if (-not (Test-Path -LiteralPath $exe)) { throw "build first: cargo build --release" }

$origin = Start-Process -FilePath "node" `
  -ArgumentList @("tests/support/fixture-origin.mjs", "--port", "$OriginPort") `
  -WorkingDirectory $root -PassThru -NoNewWindow `
  -RedirectStandardOutput "$env:TEMP\ddl-origin.log" -RedirectStandardError "$env:TEMP\ddl-origin.err"

$env:DDL_ALLOW_PRIVATE_HOSTS = "1"
$env:DDL_BIND = "127.0.0.1:$ServerPort"
$env:DDL_STATIC_DIR = Join-Path $root "frontend\dist"
$env:DDL_LOG = "info"

$server = Start-Process -FilePath $exe -WorkingDirectory $root -PassThru -NoNewWindow `
  -RedirectStandardOutput "$env:TEMP\ddl-server.log" -RedirectStandardError "$env:TEMP\ddl-server.err"

Write-Host "website:  http://127.0.0.1:$ServerPort"
Write-Host "origin:   http://127.0.0.1:$OriginPort/media/720p.mp4"
Write-Host "holding for $Seconds seconds (ctrl-c to stop)"
try {
  Start-Sleep -Seconds $Seconds
} finally {
  foreach ($p in @($server, $origin)) {
    if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
  }
}