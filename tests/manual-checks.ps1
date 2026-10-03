#!/usr/bin/env pwsh
# Smoke checks against the running stack. Invoked by scripts/with-stack.ps1,
# which exports $env:DDL_ORIGIN and $env:DDL_SERVER.
# Windows PowerShell 5.1 compatible.

$ErrorActionPreference = "Stop"
$origin = $env:DDL_ORIGIN
$server = $env:DDL_SERVER

function Assert($condition, $message) {
  if (-not $condition) { throw "FAIL: $message" }
  "  ok  $message"
}

function Get-Range($target, [long]$from, [long]$to) {
  # HttpWebRequest restricts the Range header, so use its own API.
  $req = [System.Net.HttpWebRequest]::Create($target)
  $req.UserAgent = "ddl-player-manual-check"
  if ($to -lt 0) { $req.AddRange($from) } elseif ($to -eq 0) { $req.AddRange($from) } else { $req.AddRange($from, $to) }
  $resp = $req.GetResponse()
  $buf = New-Object byte[] 65536
  $total = 0
  $stream = $resp.GetResponseStream()
  while (($n = $stream.Read($buf, 0, $buf.Length)) -gt 0) { $total += $n }
  $stream.Close()
  $contentRange = $resp.Headers["Content-Range"]
  $status = $resp.StatusCode.value__
  $resp.Close()
  return @{ Total = $total; ContentRange = $contentRange; Status = $status }
}

"health"
$health = Invoke-RestMethod "$server/api/health"
Assert ($health.status -eq "ok") "server reports ok (uptime $($health.uptime_s)s)"

"static assets"
$index = Invoke-WebRequest "$server/" -UseBasicParsing
Assert ($index.StatusCode -eq 200) "index.html served by the Rust server"
Assert ($index.Content -match 'id="video"') "the video element is in the shell"
$js = Invoke-WebRequest "$server/assets/app.js" -UseBasicParsing
"      app.js = $($js.RawContentLength) bytes uncompressed"

"probe (single round trip)"
$probe = Invoke-RestMethod "$server/api/probe" -Method Post -ContentType "application/json" `
  -Body (@{ url = "$origin/media/360p.mp4" } | ConvertTo-Json -Compress)
Assert ($probe.streamable -eq $true) "360p fixture is streamable"
Assert ($probe.content_type -eq "video/mp4") "content-type is video/mp4"
Assert ($probe.range_supported -eq $true) "range support detected"
Assert ($probe.container -eq "mp4") "container identified as mp4"
Assert ($probe.content_length -gt 900000) "length is $($probe.content_length) bytes"
"      ttfb = $([math]::Round($probe.ttfb_ms, 2)) ms, probe = $([math]::Round($probe.probe_ms, 2)) ms"

"full stream"
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$full = Invoke-WebRequest "$server/api/stream?url=$([uri]::EscapeDataString("$origin/media/360p.mp4"))" -UseBasicParsing
$sw.Stop()
Assert ($full.StatusCode -eq 200) "full stream returns 200"
Assert ($full.RawContentLength -eq $probe.content_length) "received exactly $($probe.content_length) bytes in $([math]::Round($sw.Elapsed.TotalMilliseconds, 1)) ms"
"      throughput = $([math]::Round(($full.RawContentLength / 1MB) / $sw.Elapsed.TotalSeconds, 2)) MB/s"

"byte ranges (720p fixture)"
$url = [uri]::EscapeDataString("$origin/media/720p.mp4")
$r = Get-Range "$server/api/stream?url=$url" 0 1023
Assert ($r.Status -eq 206) "bytes=0-1023 -> 206"
Assert ($r.Total -eq 1024) "bytes=0-1023 -> $($r.Total) bytes, $($r.ContentRange)"

$r = Get-Range "$server/api/stream?url=$url" 1048576 0
Assert ($r.Status -eq 206) "bytes=1048576- -> 206"
Assert ($r.Total -eq (4999379 - 1048576)) "bytes=1048576- -> $($r.Total) bytes, $($r.ContentRange)"

$r = Get-Range "$server/api/stream?url=$url" -2048 0
Assert ($r.Status -eq 206) "bytes=-2048 -> 206"
Assert ($r.Total -eq 2048) "bytes=-2048 -> $($r.Total) bytes, $($r.ContentRange)"

"unsatisfiable range"
try {
  $req = [System.Net.HttpWebRequest]::Create("$server/api/stream?url=$url")
  $req.AddRange(99999999)
  $resp = $req.GetResponse()
  $resp.Close()
  throw "expected a 416"
} catch [System.Net.WebException] {
  $ex = $_.Exception.Response
  Assert ($ex.StatusCode.value__ -eq 416) "past-the-end range -> 416"
  Assert ($ex.Headers["Content-Range"] -eq "bytes */4999379") "416 carries Content-Range: bytes */4999379"
}

"faults"
foreach ($fault in @("500", "503", "html", "no-length", "range-less", "redirect-loop", "disconnect")) {
  # The probe takes the raw URL; only the stream endpoint needs it escaped.
  $raw = "$origin/media/360p.mp4?fault=$fault"
  try {
    $r = Invoke-WebRequest "$server/api/probe" -Method Post -ContentType "application/json" `
      -Body (@{ url = $raw } | ConvertTo-Json -Compress) -UseBasicParsing
    $b = $r.Content | ConvertFrom-Json
    "  --  fault=$fault -> streamable=$($b.streamable) reason=$(if ($b.reason) { $b.reason } else { '-' })"
  } catch {
    $status = $_.Exception.Response.StatusCode.value__
    $body = $_.ErrorDetails.Message
    if (-not $body) {
      $reader = New-Object System.IO.StreamReader($_.Exception.Response.GetResponseStream())
      $body = $reader.ReadToEnd()
    }
    $b = $body | ConvertFrom-Json
    "  --  fault=$fault -> HTTP $status code=$($b.code) retryable=$($b.retryable)"
    Assert ([bool]$b.code -and [bool]$b.message) "fault=$fault produced a structured error"
  }
}

"ssrf (this stack runs with DDL_ALLOW_PRIVATE_HOSTS=1, so only syntactic and
 name-based refusals apply here; address-class refusals are covered by
 tests/integration/proxy.rs::ssrf_targets_are_blocked_by_default)"
foreach ($bad in @("http://metadata.google.internal/x", "file:///etc/passwd", "javascript:alert(1)", "not a url", "http://10.0.0.5/x`r`nX-Injected: 1")) {
  try {
    $r = Invoke-WebRequest "$server/api/probe" -Method Post -ContentType "application/json" `
      -Body (@{ url = $bad } | ConvertTo-Json -Compress) -UseBasicParsing
    $b = $r.Content | ConvertFrom-Json
    "  --  $bad -> HTTP 200 code=$($b.code)"
  } catch {
    $status = $_.Exception.Response.StatusCode.value__
    $body = $_.ErrorDetails.Message
    if (-not $body) {
      $reader = New-Object System.IO.StreamReader($_.Exception.Response.GetResponseStream())
      $body = $reader.ReadToEnd()
    }
    $b = $body | ConvertFrom-Json
    "  --  $bad -> HTTP $status code=$($b.code)"
    Assert ($b.code -eq "INVALID_URL" -or $b.code -eq "UNSUPPORTED_PROTOCOL") "refused: $bad"
  }
}

"stats"
$stats = Invoke-RestMethod "$server/api/stats"
"      active_streams=$($stats.active_streams) streams_total=$($stats.streams_total)"
"      ttfb p50=$($stats.ttfb.p50_us)us p95=$($stats.ttfb.p95_us)us p99=$($stats.ttfb.p99_us)us"
"      http p50=$($stats.http_latency.p50_us)us p95=$($stats.http_latency.p95_us)us p99=$($stats.http_latency.p99_us)us"
"      bytes_from_origin=$($stats.bytes_from_origin) bytes_to_client=$($stats.bytes_to_client)"
"      cache hit_rate=$($stats.cache.hit_rate)"
"      errors=$(($stats.errors | ConvertTo-Json -Compress))"
Assert ($stats.active_streams -eq 0) "no stream leaked after every check"
Assert ($stats.registry.sessions -eq 0) "no session leaked"

""
"ALL CHECKS PASSED"