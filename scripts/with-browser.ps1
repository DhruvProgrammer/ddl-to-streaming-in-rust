#!/usr/bin/env pwsh
# Run the Playwright suite against the real website.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\with-browser.ps1 -Project chromium

param(
  [string]$Project = "chromium",
  [int]$OriginPort = 9000,
  [int]$ServerPort = 8787
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$env:DDL_ORIGIN = "http://127.0.0.1:$OriginPort"
$env:DDL_SERVER = "http://127.0.0.1:$ServerPort"
$exe = Join-Path $root "backend\target\release\ddl-player.exe"
if (-not (Test-Path -LiteralPath $exe)) { throw "build the server first: cargo build --release" }

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
$env:DDL_LOG = "warn"

$server = Start-Process -FilePath $exe -WorkingDirectory $root -PassThru -NoNewWindow `
  -RedirectStandardOutput $serverLog -RedirectStandardError "$serverLog.err"

try {
  $ready = $false
  foreach ($i in 1..80) {
    try {
      $null = Invoke-WebRequest "$($env:DDL_SERVER)/api/health" -UseBasicParsing -TimeoutSec 2
      $ready = $true; break
    } catch { Start-Sleep -Milliseconds 250 }
  }
  if (-not $ready) { throw "server did not become healthy" }

  Push-Location $root
  try {
    npx playwright test --project=$Project
    $code = $LASTEXITCODE
  } finally {
    Pop-Location
  }
} finally {
  foreach ($p in @($server, $origin)) {
    if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
  }
  if ($code -ne 0) {
    "--- server log (tail) ---"
    Get-Content $serverLog -Tail 40 -ErrorAction SilentlyContinue
  }
}
exit $code
