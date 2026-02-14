param(
  [string]$BaseUrl = "http://127.0.0.1:8765",
  [string]$Token = "dev-token",
  [string]$VoiceId = "0",
  [int]$WsTimeoutSec = 120,
  [int]$ChunkMaxChars = 160,
  [switch]$UseSubprotocolAuth,
  [switch]$QuitOnDone,
  [string]$SaveWavPath = "",
  [string]$Text = "This is a standalone streaming playback test. It uses multiple sentences to validate chunking behavior. You should hear several short audio chunks played in sequence. If this works, the engine streaming path and local playback loop are both healthy."
)

$ErrorActionPreference = "Stop"

if ($ChunkMaxChars -lt 100) {
  Write-Warning "ChunkMaxChars=$ChunkMaxChars is below API minimum 100. Using 100."
  $ChunkMaxChars = 100
}

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

function Convert-PcmToWavBytes {
  param(
    [byte[]]$PcmBytes,
    [int]$SampleRate,
    [int]$Channels,
    [int]$BitsPerSample = 16
  )

  $blockAlign = [int](($Channels * $BitsPerSample) / 8)
  $byteRate = [int]($SampleRate * $blockAlign)
  $dataSize = [int]$PcmBytes.Length
  $riffSize = [int](36 + $dataSize)

  $stream = New-Object System.IO.MemoryStream
  $writer = New-Object System.IO.BinaryWriter($stream)

  # RIFF header
  $writer.Write([System.Text.Encoding]::ASCII.GetBytes("RIFF"))
  $writer.Write($riffSize)
  $writer.Write([System.Text.Encoding]::ASCII.GetBytes("WAVE"))

  # fmt chunk
  $writer.Write([System.Text.Encoding]::ASCII.GetBytes("fmt "))
  $writer.Write([int]16)   # PCM fmt chunk size
  $writer.Write([int16]1)  # PCM format
  $writer.Write([int16]$Channels)
  $writer.Write([int]$SampleRate)
  $writer.Write([int]$byteRate)
  $writer.Write([int16]$blockAlign)
  $writer.Write([int16]$BitsPerSample)

  # data chunk
  $writer.Write([System.Text.Encoding]::ASCII.GetBytes("data"))
  $writer.Write([int]$dataSize)
  $writer.Write($PcmBytes)

  $writer.Flush()
  $wavBytes = $stream.ToArray()
  $writer.Dispose()
  $stream.Dispose()
  return $wavBytes
}

function Play-WavBytesSync {
  param([byte[]]$WavBytes)
  $memory = New-Object System.IO.MemoryStream(,$WavBytes)
  $player = New-Object System.Media.SoundPlayer($memory)
  try {
    $player.Load()
    $player.PlaySync()
  } finally {
    $player.Dispose()
    $memory.Dispose()
  }
}

$headers = @{ Authorization = "Bearer $Token" }

Write-Host "[1/5] Health check..."
$health = Invoke-EngineJson -Method "GET" -Url "$BaseUrl/v1/health" -Headers $headers
Write-Host "  model=$($health.active_model_id) device=$($health.device)"
if ($health.runtime) {
  Write-Host "  runtime.backend=$($health.runtime.backend) fallback_active=$($health.runtime.fallback_active)"
  if ($health.runtime.detail) {
    Write-Host "  runtime.detail=$($health.runtime.detail)"
  }
  if ($health.runtime.backend -eq "mock") {
    Write-Warning "Mock backend active: you will hear placeholder tones, not real speech."
  }
}

Write-Host "[2/5] Voice list check..."
$voices = Invoke-EngineJson -Method "GET" -Url "$BaseUrl/v1/voices" -Headers $headers
$selected = $voices.voices | Where-Object { $_.voice_id -eq $VoiceId } | Select-Object -First 1
if (-not $selected) {
  throw "Requested voice_id '$VoiceId' not found."
}
Write-Host "  using voice_id=$VoiceId display_name=$($selected.display_name)"

Write-Host "[3/5] Speak request..."
$speakBody = @{
  voice_id = $VoiceId
  text = $Text
  settings = @{
    chunking = @{
      max_chars = $ChunkMaxChars
    }
  }
} | ConvertTo-Json -Compress -Depth 5

$speak = Invoke-EngineJson -Method "POST" -Url "$BaseUrl/v1/speak" -Headers $headers -BodyJson $speakBody
Write-Host "  job_id=$($speak.job_id)"
Write-Host "  ws_url=$($speak.ws_url)"

Write-Host "[4/5] Stream + playback..."
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
$null = $ws.ConnectAsync($uri, $cts.Token).GetAwaiter().GetResult()

$buffer = New-Object byte[] 262144
$builder = [System.Text.StringBuilder]::new()
$terminalTypes = @("JOB_DONE", "JOB_CANCELED", "JOB_ERROR")
$audioChunkCount = 0
$terminalEvent = ""
$pcmCollector = New-Object System.Collections.Generic.List[byte]
$timer = [System.Diagnostics.Stopwatch]::StartNew()
$lastChunkRecvMs = 0

while ($ws.State -eq [System.Net.WebSockets.WebSocketState]::Open) {
  $segment = [ArraySegment[byte]]::new($buffer)
  $recv = $ws.ReceiveAsync($segment, $cts.Token).GetAwaiter().GetResult()

  if ($recv.MessageType -eq [System.Net.WebSockets.WebSocketMessageType]::Close) {
    break
  }

  $jsonPart = [System.Text.Encoding]::UTF8.GetString($buffer, 0, $recv.Count)
  [void]$builder.Append($jsonPart)
  if (-not $recv.EndOfMessage) {
    continue
  }

  $json = $builder.ToString()
  $null = $builder.Clear()
  $event = $json | ConvertFrom-Json
  Write-Host ("  ws_event={0}" -f $event.type)

  if ($event.type -eq "AUDIO_CHUNK") {
    $recvMs = [int]$timer.ElapsedMilliseconds
    $sampleRate = [int]$event.audio.sample_rate
    $channels = [int]$event.audio.channels
    $pcmBytes = [Convert]::FromBase64String([string]$event.audio.data_base64)
    $wavBytes = Convert-PcmToWavBytes -PcmBytes $pcmBytes -SampleRate $sampleRate -Channels $channels

    foreach ($b in $pcmBytes) {
      $pcmCollector.Add($b)
    }

    $audioChunkCount += 1
    $gapMs = $recvMs - $lastChunkRecvMs
    $lastChunkRecvMs = $recvMs
    Write-Host ("    chunk={0} recv_t={1}ms gap_since_prev={2}ms bytes={3}" -f $audioChunkCount, $recvMs, $gapMs, $pcmBytes.Length)
    $playStartMs = [int]$timer.ElapsedMilliseconds
    Play-WavBytesSync -WavBytes $wavBytes
    $playDurMs = [int]$timer.ElapsedMilliseconds - $playStartMs
    Write-Host ("    chunk={0} playback_dur={1}ms" -f $audioChunkCount, $playDurMs)
  }

  if ($terminalTypes -contains $event.type) {
    $terminalEvent = [string]$event.type
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

if ($audioChunkCount -lt 1) {
  throw "No AUDIO_CHUNK event received."
}
if (-not $terminalEvent) {
  throw "No terminal event received."
}

Write-Host "  received $audioChunkCount audio chunk(s); terminal=$terminalEvent"

if ($SaveWavPath) {
  # Save combined PCM as a single wav file for verification.
  $combinedPcm = $pcmCollector.ToArray()
  $fullWav = Convert-PcmToWavBytes -PcmBytes $combinedPcm -SampleRate 24000 -Channels 1
  [System.IO.File]::WriteAllBytes($SaveWavPath, $fullWav)
  Write-Host "  saved combined wav to: $SaveWavPath"
}

if ($QuitOnDone) {
  Write-Host "[5/5] Sending /v1/quit..."
  $quit = Invoke-EngineJson -Method "POST" -Url "$BaseUrl/v1/quit" -Headers $headers
  if (-not $quit.quitting) {
    throw "Engine did not acknowledge quit request."
  }
  Write-Host "  quit acknowledged"
} else {
  Write-Host "[5/5] Engine left running (no quit requested)."
}

Write-Host "STREAM_PLAY_TEST_OK"
