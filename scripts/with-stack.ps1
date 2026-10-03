#!/usr/bin/env pwsh
# Boot the fixture origin and the real server, run a script against them, then
# shut everything down. Windows PowerShell 5.1 compatible.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\with-stack.ps1 `
#     -Script tests\manual-checks.ps1

param(
  [Parameter(Mandatory = $true)][string]$Script,
  [int]$OriginPort = 9000,
  [int]$ServerPort = 8787
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "backend\target\release\ddl-player.exe"

if (-not (Test-Path -LiteralPath $exe)) {
  throw "release binary not found at $exe - run: cargo build --release"
}

$env:DDL_ORIGIN = "http://127.0.0.1:$OriginPort"
$env:DDL_SERVER = "http://127.0.0.1:$ServerPort"

$originLog = Join-Path $env:TEMP "ddl-origin.log"
$serverLog = Join-Path $env:TEMP "ddl-server.log"
Remove-Item -Force -ErrorAction SilentlyContinue $originLog, "$originLog.err", $serverLog, "$serverLog.err"

$origin = Start-Process -FilePath "node" `
  -ArgumentList @("tests/support/fixture-origin.mjs", "--port", "$OriginPort") `
  -WorkingDirectory $root -PassThru -NoNewWindow `
  -RedirectStandardOutput $originLog -RedirectStandardError "$originLog.err"

$env:DDL_ALLOW_PRIVATE_HOSTS = "1"
$env:DDL_BIND = "127.0.0.1:$ServerPort"
$env:DDL_STATIC_DIR = Join-Path $root "frontend\dist"
$env:DDL_LOG = "info"

$server = Start-Process -FilePath $exe -WorkingDirectory $root -PassThru -NoNewWindow `
  -RedirectStandardOutput $serverLog -RedirectStandardError "$serverLog.err"

try {
  $ready = $false
  foreach ($i in 1..80) {
    try {
      $null = Invoke-WebRequest "$($env:DDL_SERVER)/api/health" -UseBasicParsing -TimeoutSec 2
      $ready = $true
      break
    } catch {
      Start-Sleep -Milliseconds 250
    }
  }
  if (-not $ready) { throw "server did not become healthy" }
  & $Script
} finally {
  foreach ($p in @($server, $origin)) {
    if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
  }
  if ($env:DDL_SHOW_LOGS -eq "1") {
    "--- server log ---"
    Get-Content $serverLog -ErrorAction SilentlyContinue
    "--- origin log ---"
    Get-Content $originLog -ErrorAction SilentlyContinue
  }
}
