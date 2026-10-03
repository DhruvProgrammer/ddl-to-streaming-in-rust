#!/usr/bin/env pwsh
# Load and latency run against the release build, one level per process start so
# the resident-memory sample per level is honest.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\with-load.ps1 -Levels 1,10,50,100,200

param(
  [string]$Levels = "1,10,50,100,200",
  [int]$Seconds = 8,
  [string]$File = "720p.mp4",
  [int]$OriginPort = 9000,
  [int]$ServerPort = 8787
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "backend\target\release\ddl-player.exe"
if (-not (Test-Path -LiteralPath $exe)) { throw "build first: cargo build --release" }
$env:DDL_ORIGIN = "http://127.0.0.1:$OriginPort"
$env:DDL_SERVER = "http://127.0.0.1:$ServerPort"

$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$ram = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 1)
"machine: $($cpu.Name), $($cpu.NumberOfCores) cores, $ram GB RAM, Windows"
"node:    $(node --version)"
"origin:  node fixture-origin on 127.0.0.1:$OriginPort (single-threaded, in page cache)"
""

$originLog = Join-Path $env:TEMP "ddl-origin.log"
$serverLog = Join-Path $env:TEMP "ddl-server.log"

$origin = Start-Process -FilePath "node" `
  -ArgumentList @("tests/support/fixture-origin.mjs", "--port", "$OriginPort") `
  -WorkingDirectory $root -PassThru -NoNewWindow `
  -RedirectStandardOutput $originLog -RedirectStandardError "$originLog.err"

$table = @()

try {
  $levelNo = 0
  foreach ($level in $Levels.Split(",")) {
    $levelNo += 1
    # A fresh port per level: reusing one races with the previous process's
    # socket teardown and silently leaves us measuring the wrong server.
    $levelPort = $ServerPort + $levelNo
    $env:DDL_SERVER = "http://127.0.0.1:$levelPort"

    Remove-Item -Force -ErrorAction SilentlyContinue $serverLog, "$serverLog.err"
    $env:DDL_ALLOW_PRIVATE_HOSTS = "1"
    $env:DDL_BIND = "127.0.0.1:$levelPort"
    $env:DDL_STATIC_DIR = Join-Path $root "frontend\dist"
    $env:DDL_LOG = "warn"

    $server = Start-Process -FilePath $exe -WorkingDirectory $root -PassThru -NoNewWindow `
      -RedirectStandardOutput $serverLog -RedirectStandardError "$serverLog.err"
    try {
      $ready = $false
      foreach ($i in 1..80) {
        if ($server.HasExited) {
          throw "server exited during startup: $(Get-Content $serverLog -Raw -ErrorAction SilentlyContinue) $(Get-Content "$serverLog.err" -Raw -ErrorAction SilentlyContinue)"
        }
        try {
          $null = Invoke-WebRequest "$($env:DDL_SERVER)/api/health" -UseBasicParsing -TimeoutSec 2
          $ready = $true; break
        } catch { Start-Sleep -Milliseconds 250 }
      }
      if (-not $ready) { throw "server did not become healthy" }

      # Settle, then take the baseline.
      Start-Sleep -Seconds 1
      $proc = Get-Process -Id $server.Id
      $rss0 = $proc.WorkingSet64
      $cpu0 = $proc.TotalProcessorTime.TotalMilliseconds

      Push-Location $root
      try {
        $out = node tests/load/harness.mjs --levels $level --seconds $Seconds --file $File --json-out 2>&1 | Out-String
      } finally {
        Pop-Location
      }
      Write-Host $out -NoNewline

      $proc2 = Get-Process -Id $server.Id
      $rss1 = $proc2.WorkingSet64
      $cpu1 = $proc2.TotalProcessorTime.TotalMilliseconds
      $wallS = $Seconds + 3
      $cpuPct = [math]::Round((($cpu1 - $cpu0) / 1000) / $wallS * 100 / $cpu.NumberOfLogicalProcessors, 1)

      # The harness writes machine-readable JSON; parsing the table is fragile.
      $results = Get-Content (Join-Path $root "docs\load-results.json") -Raw | ConvertFrom-Json
      $row = @($results)[-1]
      $table += [pscustomobject]@{
        streams = [int]$row.streams
        sessions = [int]$row.sessions
        mb_per_s = [math]::Round($row.throughput / 1MB, 1)
        err_pct = [math]::Round($row.errorRate * 100, 2)
        probe_p50_ms = [math]::Round($row.probe.p50, 1)
        probe_p95_ms = [math]::Round($row.probe.p95, 1)
        probe_p99_ms = [math]::Round($row.probe.p99, 1)
        ttfb_p50_ms = [math]::Round($row.ttfb.p50, 1)
        ttfb_p95_ms = [math]::Round($row.ttfb.p95, 1)
        ttfb_p99_ms = [math]::Round($row.ttfb.p99, 1)
        active_after = [int]$row.active
        rss_delta_mb = [math]::Round(($rss1 - $rss0) / 1MB, 1)
        rss_peak_mb = [math]::Round($rss1 / 1MB, 1)
        cpu_pct = $cpuPct
        prefetch_overhead_pct = [math]::Round($row.originOverhead * 100, 1)
      }
      Write-Host $out -NoNewline
    } finally {      if ($server -and -not $server.HasExited) {
        Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
        # Wait for the process to actually leave the table.
        foreach ($i in 1..40) {
          if (-not (Get-Process -Id $server.Id -ErrorAction SilentlyContinue)) { break }
          Start-Sleep -Milliseconds 100
        }
      }
    }
    Start-Sleep -Milliseconds 500
  }

  ""
  "================ SUMMARY ================"
  $table | Format-Table -AutoSize
  $table | ConvertTo-Json -Depth 4 | Set-Content -Path (Join-Path $root "docs\load-summary.json") -Encoding UTF8
  "wrote docs/load-summary.json"
} finally {
  if ($origin -and -not $origin.HasExited) { Stop-Process -Id $origin.Id -Force -ErrorAction SilentlyContinue }
}
