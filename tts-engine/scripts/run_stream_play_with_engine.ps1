param(
  [string]$Token = "dev-token",
  [string]$VoiceId = "0",
  [ValidateSet("auto", "qwen", "mock")]
  [string]$SynthBackend = "auto",
  [string]$QwenDeviceMap = "",
  [string]$QwenDtype = "",
  [string]$QwenAttnImplementation = "",
  [string]$QwenSpeaker = "",
  [int]$WsTimeoutSec = 120,
  [int]$ChunkMaxChars = 160,
  [double]$Rate = 1.0,
  [double]$Pitch = 1.0,
  [double]$Volume = 1.0,
  [int]$PrefetchQueueSize = 5,
  [int]$StartPlaybackAfter = 2,
  [switch]$SkipWarmup,
  [switch]$ForceWarmup,
  [switch]$UseSubprotocolAuth,
  [string]$SaveWavPath = "",
  [string]$Text = "This is a standalone streaming playback test. It uses multiple sentences to validate chunking behavior. You should hear several short audio chunks played in sequence. If this works, the engine streaming path and local playback loop are both healthy."
)

$ErrorActionPreference = "Stop"

function Get-FreePort {
  $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
  $listener.Start()
  try {
    return $listener.LocalEndpoint.Port
  } finally {
    $listener.Stop()
  }
}

$engineRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$playScript = Join-Path $PSScriptRoot "stream_play_queue_test.py"
$port = Get-FreePort
$baseUrl = "http://127.0.0.1:$port"
$stdoutLog = Join-Path $env:TEMP ("tts_engine_stdout_{0}.log" -f [guid]::NewGuid())
$stderrLog = Join-Path $env:TEMP ("tts_engine_stderr_{0}.log" -f [guid]::NewGuid())
$venvPython = Join-Path $engineRoot ".venv\\Scripts\\python.exe"
$pythonExe = if (Test-Path $venvPython) { $venvPython } else { "python" }

$env:SPEAK_SELECTION_ENGINE_TOKEN = $Token
$env:PYTHONPATH = (Join-Path $engineRoot "src")
$env:VOICEREADER_SYNTH_BACKEND = $SynthBackend
if ($QwenDeviceMap) {
  $env:VOICEREADER_QWEN_DEVICE_MAP = $QwenDeviceMap
}
if ($QwenDtype) {
  $env:VOICEREADER_QWEN_DTYPE = $QwenDtype
}
if ($QwenAttnImplementation) {
  $env:VOICEREADER_QWEN_ATTN_IMPLEMENTATION = $QwenAttnImplementation
}
if ($QwenSpeaker) {
  $env:VOICEREADER_QWEN_SPEAKER = $QwenSpeaker
}

Write-Host "Starting engine on $baseUrl ..."
$proc = Start-Process `
  -FilePath $pythonExe `
  -ArgumentList @("-m", "tts_engine", "--server", "--port", "$port") `
  -WorkingDirectory $engineRoot `
  -RedirectStandardOutput $stdoutLog `
  -RedirectStandardError $stderrLog `
  -PassThru

try {
  if ($ChunkMaxChars -lt 100) {
    Write-Warning "ChunkMaxChars=$ChunkMaxChars is below API minimum 100. Using 100."
    $ChunkMaxChars = 100
  }
  if ($PrefetchQueueSize -lt 2) {
    throw "PrefetchQueueSize must be >= 2."
  }
  if ($StartPlaybackAfter -lt 1) {
    throw "StartPlaybackAfter must be >= 1."
  }
  if ($StartPlaybackAfter -gt $PrefetchQueueSize) {
    throw "StartPlaybackAfter cannot be greater than PrefetchQueueSize."
  }
  if ($Rate -lt 0.5 -or $Rate -gt 2.0) {
    throw "Rate must be in [0.5, 2.0]."
  }
  if ($Pitch -lt 0.5 -or $Pitch -gt 2.0) {
    throw "Pitch must be in [0.5, 2.0]."
  }
  if ($Volume -lt 0.0 -or $Volume -gt 2.0) {
    throw "Volume must be in [0.0, 2.0]."
  }

  $healthy = $false
  $headers = @{ Authorization = "Bearer $Token" }
  for ($i = 0; $i -lt 60; $i++) {
    Start-Sleep -Milliseconds 200
    if ($proc.HasExited) {
      $proc.WaitForExit()
      $exitCode = if ($null -ne $proc.ExitCode) { $proc.ExitCode } else { "unknown" }
      $stdoutTail = if (Test-Path $stdoutLog) { (Get-Content $stdoutLog -Tail 30) -join [Environment]::NewLine } else { "" }
      $stderrTail = if (Test-Path $stderrLog) { (Get-Content $stderrLog -Tail 30) -join [Environment]::NewLine } else { "" }
      throw "Engine exited early with code $exitCode.`nSTDOUT:`n$stdoutTail`nSTDERR:`n$stderrTail"
    }
    try {
      $null = Invoke-RestMethod -Method GET -Uri "$baseUrl/v1/health" -Headers $headers
      $healthy = $true
      break
    } catch {
      continue
    }
  }

  if (-not $healthy) {
    throw "Engine did not become healthy in time."
  }

  $playArgs = @(
    $playScript,
    "--base-url", $baseUrl,
    "--token", $Token,
    "--voice-id", $VoiceId,
    "--ws-timeout-sec", "$WsTimeoutSec",
    "--chunk-max-chars", "$ChunkMaxChars",
    "--rate", "$Rate",
    "--pitch", "$Pitch",
    "--volume", "$Volume",
    "--prefetch-queue-size", "$PrefetchQueueSize",
    "--start-playback-after", "$StartPlaybackAfter",
    "--text", $Text,
    "--quit-on-done"
  )
  if (-not $SkipWarmup) {
    $playArgs += "--warmup-wait"
  }
  if ($ForceWarmup) {
    $playArgs += "--warmup-force"
  }
  if ($UseSubprotocolAuth) {
    $playArgs += "--use-subprotocol-auth"
  }
  if ($SaveWavPath) {
    $playArgs += @("--save-wav-path", $SaveWavPath)
  }

  & $pythonExe @playArgs
  if ($LASTEXITCODE -ne 0) {
    throw "Queue playback script failed with exit code $LASTEXITCODE."
  }
} finally {
  if (-not $proc.HasExited) {
    try {
      $null = Invoke-RestMethod -Method POST -Uri "$baseUrl/v1/quit" -Headers @{ Authorization = "Bearer $Token" }
      Start-Sleep -Milliseconds 700
    } catch {
      # no-op; fallback to force-stop
    }
  }

  if (-not $proc.HasExited) {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
  }

  Remove-Item $stdoutLog, $stderrLog -ErrorAction SilentlyContinue
}
