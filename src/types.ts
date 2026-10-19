export type JsonValue = Record<string, unknown>;

export type SpeakerPreset = {
  id: string;
  description: string;
  native_language: string;
};

export type ModelOption = {
  id: string;
  label: string;
  status: string;
  notes: string;
};

export type BootstrapPayload = {
  hotkey: string;
  selected_voice_id: string;
  selected_model: string;
  selected_speaker: string;
  startup_error?: string | null;
  build_variant: string;
  qwen_enabled: boolean;
  audio8_supported?: boolean;
  audio8_downloaded?: boolean;
  models: ModelOption[];
  preset_speakers: SpeakerPreset[];
  health: JsonValue;
  voices: JsonValue;
};

export type RuntimeStatusPayload = {
  running: boolean;
  pid: number | null;
  base_url: string;
  selected_voice_id: string;
  selected_model: string;
  selected_speaker: string;
  model_label?: string;
  device_label?: string;
};

export type ComputeDevicePayload = {
  preference: string;
  gpu_supported: boolean;
  model_loaded: boolean;
  active_device: string | null;
  note: string | null;
};

export type ModelUpdatePayload = {
  selected_model: string;
  selected_speaker: string;
  preset_speakers: SpeakerPreset[];
  applied: boolean;
  message: string;
  health: JsonValue;
};

export type JobCancelRequestedPayload = {
  job_id: string;
};

export type JobStartedPayload = {
  job_id: string;
  ws_url: string;
  source: string;
  source_window?: string;
  rate?: number;
  /** True when the audio arrives at normal speed and the player applies `rate`. */
  rate_applied_by_player?: boolean;
  /** "assistant" for an Assistant voice answer: streamed phrase by phrase, no toolbar. */
  kind?: "read" | "assistant";
};

export type ToolbarActionPayload = {
  action: "pause-toggle" | "skip-back" | "skip-forward" | "stop";
};

export type ToolbarShowPayload = {
  job_id: string;
  source_window: string;
  rate: number;
};

export type ToolbarPausePayload = {
  paused: boolean;
};

export type CloneVoiceResult = {
  ok: boolean;
  message: string;
  voice_id: string;
};

export type HotkeyResult = {
  ok: boolean;
  message: string;
  hotkey: string;
};

export type StoredVoice = {
  voice_id: string;
  display_name: string;
  language_hint?: string | null;
  description?: string | null;
};

export type UnifiedVoiceOption = {
  value: string;
  label: string;
  kind: "preset" | "stored";
  id: string;
};

export type EngineStoragePathsPayload = {
  data_dir: string;
  models_dir: string;
  hf_cache_dir: string;
};

export type PrefetchModelsResult = {
  ok: boolean;
  message: string;
  mode: string;
  downloaded: string[];
  data_dir: string;
  models_dir: string;
  hf_cache_dir: string;
};

export type Audio8ModelStatus = {
  supported: boolean;
  downloaded: boolean;
  loaded: boolean;
  model_dir: string;
  repo: string;
  download_size_bytes: number;
};

export type Audio8DownloadResult = {
  ok: boolean;
  message: string;
};

export type ModelDownloadProgressPayload = {
  model: string;
  state: "progress" | "done" | "error";
  file: string;
  file_index: number;
  file_count: number;
  downloaded_bytes: number;
  total_bytes: number;
  message: string;
};

export type AsrModelStatus = {
  supported: boolean;
  downloaded: boolean;
  model_dir: string;
  repo: string;
  diarizer_repo: string;
  download_size_bytes: number;
  default_max_speakers: number;
  max_speakers_limit: number;
};

export type TranscribeStartPayload = {
  job_id: string;
};

export type TranscriptTurn = {
  id: number;
  /** Zero-based, in order of first appearance. */
  speaker: number;
  /** True for speech from people beyond the speaker limit, who share one label. */
  unknown: boolean;
  start_secs: number;
  end_secs: number;
  text: string;
};

export type TranscriptEventPayload = {
  job_id: string;
  kind: "loading" | "started" | "turn" | "progress" | "level" | "done" | "cancelled" | "error";
  turn: TranscriptTurn | null;
  processed_secs: number;
  total_secs: number | null;
  /** Input loudness from 0 to 1, in "level" events. */
  level: number;
  message: string;
};

export type AudioInput = {
  name: string;
  is_default: boolean;
};

export type ThemeMode = "dark" | "light";

// Assistant (local LLM chat). Payload fields are snake_case, as sent by the Rust backend.

export type AssistantModelTier = "small" | "mid" | "mid-high" | "large";

export type AssistantModelInfo = {
  id: string;
  label: string;
  tier: AssistantModelTier;
  download_bytes: number;
  installed: boolean;
  /** True only after the recorded tool evaluation passes. */
  tools?: boolean;
  /** Longest context the model was trained for; larger settings are refused. */
  max_context: number;
  memory_estimate: { ctx_size: number; gpu_mib: number; host_mib: number; measured: boolean };
  /** The publisher's recommended sampling settings: the panel's defaults for this model. */
  sampling_defaults: AssistantSamplingDefaults;
  /** Memory in MiB, linear in context: `fixed + per_1k_ctx * ceil(ctx / 1024)`, per pool. */
  memory: AssistantMemoryModel;
};

export type AssistantSamplingDefaults = {
  temperature: number;
  top_p: number;
  /** 0 turns top-k off. */
  top_k: number;
  min_p: number;
  repeat_penalty: number;
};

/** Sent with `assistant_chat_start`. `max_tokens: null` means no limit. */
export type AssistantSampling = AssistantSamplingDefaults & { max_tokens: number | null };

export type AssistantMemoryModel = {
  gpu_fixed_mib: number;
  gpu_mib_per_1k_ctx: number;
  host_fixed_mib: number;
  host_mib_per_1k_ctx: number;
  measured: boolean;
};

/** `assistant_get_prompts` and `assistant_set_prompts`. */
export type AssistantPrompts = {
  chat: string;
  voice: string;
  chat_default: string;
  voice_default: string;
};

export type AssistantHardwareDevice = {
  name: string;
  total_mib: number;
  free_mib: number;
  integrated: boolean;
};

/** `assistant_hardware`. `devices` is only reliable when `devices_known` (the backend lists them). */
export type AssistantHardware = {
  total_ram_mib: number;
  unified_memory: boolean;
  devices: AssistantHardwareDevice[];
  devices_known: boolean;
  recommended_tier: AssistantModelTier;
  reason: string;
};

export type AssistantServerState = "stopped" | "starting" | "ready" | "in_use_elsewhere" | "error";

export type AssistantStatus = {
  platform_supported: boolean;
  backend_installed: boolean;
  backend_version: string;
  backend_download_bytes: number;
  models: AssistantModelInfo[];
  download: null | { kind: "backend" | "model"; id: string };
  server: {
    state: AssistantServerState;
    model_id: string | null;
    ctx_size: number | null;
    device_label: string | null;
    error: string | null;
  };
};

export type AssistantDownloadProgressPayload = {
  kind: "backend" | "model";
  id: string;
  file: string;
  downloaded: number;
  total: number;
};

/** One earlier turn sent to the model. `images` are data URLs. */
export type AssistantChatMessage = {
  role: "user" | "assistant";
  text: string;
  images: string[];
};

export type AssistantChatStartResult = {
  request_id: string;
  prompt_tokens: number;
  dropped_messages: number;
};

export type AssistantChatDeltaPayload = {
  round?: number;
  request_id: string;
  content: string | null;
  reasoning: string | null;
};

export type AssistantChatDonePayload = {
  final_text?: string;
  final_round?: number;
  history_note?: string;
  sources?: AssistantToolSource[];
  timings?: { predicted_per_second?: number };
  thinking_budget_tokens?: number | null;
  completion_tokens?: number;
  request_id: string;
  finish_reason: string | null;
  stopped: boolean;
};

export type AssistantChatErrorPayload = {
  request_id: string;
  message: string;
};

/** `assistant_diagnostics`: never contains the API key. */
export type AssistantDiagnostics = {
  backend_version: string;
  platform_asset: string | null;
  assistant_root: string;
  server_binary: string | null;
  server: AssistantStatus["server"];
  pid: number | null;
  port: number | null;
  uptime_secs: number | null;
  log: string;
};

// Assistant voice input (Step 2) and voice answers (Step 3). See docs/assistant/conversation.md.

/** `assistant_mic_open`. */
export type AssistantMicOpenResult = {
  ok: boolean;
  message: string;
};

export type AssistantMicLevelPayload = { rms: number };

export type AssistantMicTextPayload = { turn_id: number; text: string };

export type AssistantMicErrorPayload = { message: string };

export type AssistantMicClosedReason = "transcription" | "error" | "closed" | "exit";

export type AssistantMicClosedPayload = { reason: AssistantMicClosedReason };

/**
 * Auto mode only: Rust started or ended a turn by itself (VAD + Smart Turn). A turn open when
 * detection is paused (`assistant_mic_set_auto(false)`) gets "ended" and no final.
 */
export type AssistantMicTurnPayload = { turn_id: number; state: "started" | "ended" };

/** Automatic sending failed and was switched off; the microphone stays open. */
export type AssistantMicAutoFailedPayload = { message: string };

/** `assistant_voice_respond`. */
export type AssistantVoiceRespondResult = AssistantChatStartResult & { job_id: string };

export type AssistantVoicePhrasePayload = {
  kind?: "answer" | "filler";
  round?: number;
  request_id: string;
  job_id: string;
  seq: number;
  text: string;
};

export type AssistantVoiceEndedPayload = {
  request_id: string;
  job_id: string;
  /** "done": every phrase was handed to the player (audio may still be playing). */
  reason: "done" | "cancelled" | "preempted" | "error";
  message: string;
};

/** Sent by main.ts from the player while an Assistant voice answer plays. */
export type AssistantPlaybackPayload = {
  job_id: string;
  buffered_secs: number;
  played_seq: number | null;
  drained: boolean;
};


export type AssistantThinkingBudget = { mode: "auto" | "unlimited" } | { mode: "tokens"; tokens: number };
export type AssistantToolLimits = {
  timeouts?: Record<string, number>;
  max_rounds: number;
  search_timeout_secs: number;
  fetch_timeout_secs: number;
  weather_timeout_secs: number;
  code_timeout_secs: number;
};
export type AssistantToolsSettings = { provider: "duckduckgo" | "brave"; brave_key_set: boolean; limits: AssistantToolLimits };
export type AssistantSandboxStatus = {
  installed: boolean;
  state: "not_installed" | "installing" | "ready" | "failed";
  version: string;
  download_size_bytes: number;
  installed_size_bytes: number;
  error?: string | null;
};
export type AssistantToolSource = { n: number; title: string; url: string };
export type AssistantToolCallPayload = {
  request_id: string; round: number; call_index: number; call_id?: string; name?: string;
  delta: string; arguments?: string; phase: "streaming" | "queued" | "running" | "done"; done: boolean;
};
export type AssistantToolResultPayload = {
  request_id: string; round: number; call_id: string; name: string;
  status: "ok" | "error" | "timeout" | "cancelled" | "skipped";
  summary: string; sources: AssistantToolSource[]; output?: string;
};
export type AssistantTurnPhasePayload = { request_id: string; round: number; phase: "thinking" | "tool" | "answering" };
