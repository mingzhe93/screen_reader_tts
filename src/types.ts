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

export type QueuedPlayback = {
  buffers: AudioBuffer[];
  bufferedSeconds: number;
  started: boolean;
  terminal: boolean;
  // How many times this job ran out of audio mid-playback and had to refill.
  rebufferCount: number;
};

export type ThemeMode = "dark" | "light";
