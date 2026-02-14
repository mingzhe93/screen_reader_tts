param(
  [string]$Token = "dev-token",
  [ValidateSet("auto", "qwen", "mock")]
  [string]$SynthBackend = "auto",
  [string]$QwenDeviceMap = "",
  [string]$QwenDtype = "",
  [string]$QwenAttnImplementation = "",
  [string]$QwenSpeaker = "",
  [int]$WsTimeoutSec = 30,
  [switch]$UseSubprotocolAuth
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
$smokeScript = Join-Path $PSScriptRoot "smoke_test.ps1"
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
  $healthy = $false
  $headers = @{ Authorization = "Bearer $Token" }
  for ($i = 0; $i -lt 50; $i++) {
    Start-Sleep -Milliseconds 200
    if ($proc.HasExited) {
      $proc.WaitForExit()
      $exitCode = if ($null -ne $proc.ExitCode) { $proc.ExitCode } else { "unknown" }
      $stdoutTail = if (Test-Path $stdoutLog) { (Get-Content $stdoutLog -Tail 20) -join [Environment]::NewLine } else { "" }
      $stderrTail = if (Test-Path $stderrLog) { (Get-Content $stderrLog -Tail 20) -join [Environment]::NewLine } else { "" }
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

  if ($UseSubprotocolAuth) {
    & $smokeScript -BaseUrl $baseUrl -Token $Token -WsTimeoutSec $WsTimeoutSec -UseSubprotocolAuth -QuitOnSuccess
  } else {
    & $smokeScript -BaseUrl $baseUrl -Token $Token -WsTimeoutSec $WsTimeoutSec -QuitOnSuccess
  }
} finally {
  if (-not $proc.HasExited) {
    try {
      $null = Invoke-RestMethod -Method POST -Uri "$baseUrl/v1/quit" -Headers @{ Authorization = "Bearer $Token" }
      Start-Sleep -Milliseconds 600
    } catch {
      # no-op; fallback to force stop below
    }
  }

  if (-not $proc.HasExited) {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
  }

  Remove-Item $stdoutLog, $stderrLog -ErrorAction SilentlyContinue
}
