param(
  [string]$BaseUrl = "http://127.0.0.1:8765",
  [string]$Token = "dev-token",
  [int]$WsTimeoutSec = 30,
  [switch]$UseSubprotocolAuth,
  [switch]$QuitOnSuccess
)

$ErrorActionPreference = "Stop"

function Invoke-EngineJson {
  param(
    [string]$Method,
    [string]$Url,
    [hashtable]$Headers,
    [string]$BodyJson
  )

  if ($BodyJson) {
    return Invoke-RestMethod -Method $Method -Uri $Url -Headers $Headers -ContentType "application/json" -Body $BodyJson
  }

  return Invoke-RestMethod -Method $Method -Uri $Url -Headers $Headers
}

Write-Host "[1/4] Health check..."
$headers = @{ Authorization = "Bearer $Token" }
$health = Invoke-EngineJson -Method "GET" -Url "$BaseUrl/v1/health" -Headers $headers
Write-Host "  engine_version=$($health.engine_version) model=$($health.active_model_id) device=$($health.device)"
if ($health.runtime) {
  Write-Host "  runtime.backend=$($health.runtime.backend) fallback_active=$($health.runtime.fallback_active)"
  if ($health.runtime.backend -eq "mock") {
    Write-Warning "Engine is running on mock backend. This validates API/streaming but not real model inference."
  }
}

Write-Host "[2/4] Voice list check..."
$voices = Invoke-EngineJson -Method "GET" -Url "$BaseUrl/v1/voices" -Headers $headers
$defaultVoice = $voices.voices | Where-Object { $_.voice_id -eq "0" } | Select-Object -First 1
if (-not $defaultVoice) {
  throw "Default voice_id '0' not found in /v1/voices response."
}
Write-Host "  default voice found: $($defaultVoice.display_name)"

Write-Host "[3/4] Speak request with default voice_id=0..."
$speakBody = @{
  voice_id = "0"
  text = "VoiceReader standalone engine smoke test using the default built-in voice."
} | ConvertTo-Json -Compress
$speak = Invoke-EngineJson -Method "POST" -Url "$BaseUrl/v1/speak" -Headers $headers -BodyJson $speakBody
Write-Host "  job_id=$($speak.job_id)"
Write-Host "  ws_url=$($speak.ws_url)"

Write-Host "[4/4] WebSocket stream check..."
$ws = [System.Net.WebSockets.ClientWebSocket]::new()
if ($UseSubprotocolAuth) {
  $ws.Options.AddSubProtocol("auth.bearer.v1")
  $ws.Options.AddSubProtocol($Token)
  Write-Host "  auth mode: Sec-WebSocket-Protocol fallback"
} else {
  $ws.Options.SetRequestHeader("Authorization", "Bearer $Token")
  Write-Host "  auth mode: Authorization header"
}

$uri = [Uri]$speak.ws_url
$cts = [System.Threading.CancellationTokenSource]::new()
$cts.CancelAfter([TimeSpan]::FromSeconds($WsTimeoutSec))
try {
  $null = $ws.ConnectAsync($uri, $cts.Token).GetAwaiter().GetResult()
} catch {
  throw "WebSocket connect failed. Ensure the engine environment includes a WS runtime (`websockets` or `wsproto`) and that token auth settings match."
}

$buffer = New-Object byte[] 131072
$sb = [System.Text.StringBuilder]::new()
$terminalTypes = @("JOB_DONE", "JOB_CANCELED", "JOB_ERROR")
$sawAudioChunk = $false
$sawTerminal = $false

while ($ws.State -eq [System.Net.WebSockets.WebSocketState]::Open) {
  $segment = [ArraySegment[byte]]::new($buffer)
  $recv = $ws.ReceiveAsync($segment, $cts.Token).GetAwaiter().GetResult()

  if ($recv.MessageType -eq [System.Net.WebSockets.WebSocketMessageType]::Close) {
    break
  }

  $chunk = [System.Text.Encoding]::UTF8.GetString($buffer, 0, $recv.Count)
  [void]$sb.Append($chunk)
  if (-not $recv.EndOfMessage) {
    continue
  }

  $message = $sb.ToString()
  $null = $sb.Clear()
  $event = $message | ConvertFrom-Json
  Write-Host ("  ws_event={0}" -f $event.type)

  if ($event.type -eq "AUDIO_CHUNK") {
    $sawAudioChunk = $true
  }
  if ($terminalTypes -contains $event.type) {
    $sawTerminal = $true
    break
  }
}

if ($ws.State -eq [System.Net.WebSockets.WebSocketState]::Open) {
  $null = $ws.CloseAsync(
    [System.Net.WebSockets.WebSocketCloseStatus]::NormalClosure,
    "done",
    [System.Threading.CancellationToken]::None
  ).GetAwaiter().GetResult()
}
$ws.Dispose()
$cts.Dispose()

if (-not $sawAudioChunk) {
  throw "No AUDIO_CHUNK event received."
}
if (-not $sawTerminal) {
  throw "No terminal WS event (JOB_DONE/JOB_CANCELED/JOB_ERROR) received."
}

if ($QuitOnSuccess) {
  Write-Host "[5/5] Sending /v1/quit..."
  $quit = Invoke-EngineJson -Method "POST" -Url "$BaseUrl/v1/quit" -Headers $headers
  if (-not $quit.quitting) {
    throw "Engine did not acknowledge quit request."
  }
  Write-Host "  quit acknowledged"
}

Write-Host "SMOKE_TEST_OK"
