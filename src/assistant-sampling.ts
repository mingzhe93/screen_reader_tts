// Sampling settings for the Assistant: stored per model in localStorage
// (`voicereader.assistant.sampling.<model id>`, only the values that differ from the model's
// recommended `sampling_defaults`), sent with every chat request, and the panel section that
// edits them.

import { button, h, readStored, removeStored, writeStored } from "./assistant-dom";
import type { AssistantModelInfo, AssistantSampling } from "./types";

const KEY_PREFIX = "voicereader.assistant.sampling.";

type Key = keyof AssistantSampling;
type Range = { min: number; max: number; step: number; integer: boolean };

const RANGES: Record<Key, Range> = {
  temperature: { min: 0, max: 2, step: 0.05, integer: false },
  top_p: { min: 0, max: 1, step: 0.01, integer: false },
  top_k: { min: 0, max: 200, step: 1, integer: true },
  min_p: { min: 0, max: 1, step: 0.01, integer: false },
  repeat_penalty: { min: 1, max: 2, step: 0.01, integer: false },
  max_tokens: { min: 16, max: 32768, step: 1, integer: true },
};
const KEYS = Object.keys(RANGES) as Key[];

/** Used only if a status reply lacks `sampling_defaults`; llama.cpp's own defaults. */
const FALLBACK: AssistantModelInfo["sampling_defaults"] = {
  temperature: 0.8,
  top_p: 0.95,
  top_k: 40,
  min_p: 0.05,
  repeat_penalty: 1,
};

/** Values kept for this session, so changes last until the app closes even if storage is blocked. */
const session = new Map<string, Partial<AssistantSampling>>();

/** A value clamped into its range, or null when it is not a usable number. */
function clean(value: unknown, range: Range): number | null {
  if (typeof value !== "number" || !Number.isFinite(value)) return null;
  const clamped = Math.min(range.max, Math.max(range.min, value));
  return range.integer ? Math.round(clamped) : Math.round(clamped * 1000) / 1000;
}

export function recommendedSampling(model: AssistantModelInfo): AssistantSampling {
  return { ...FALLBACK, ...(model.sampling_defaults ?? {}), max_tokens: null };
}

function storedOverrides(modelId: string): Partial<AssistantSampling> {
  const cached = session.get(modelId);
  if (cached) return cached;
  const raw = readStored(KEY_PREFIX + modelId);
  if (!raw) return {};
  try {
    const parsed: unknown = JSON.parse(raw);
    return parsed && typeof parsed === "object" ? (parsed as Partial<AssistantSampling>) : {};
  } catch {
    return {};
  }
}

/** The sampling settings for a model: its recommended values with the user's changes on top. */
export function samplingFor(model: AssistantModelInfo): AssistantSampling {
  const result = recommendedSampling(model);
  const overrides = storedOverrides(model.id) as Record<string, unknown>;
  for (const key of KEYS) {
    if (!(key in overrides)) continue;
    if (key === "max_tokens" && overrides[key] === null) {
      result.max_tokens = null;
      continue;
    }
    const value = clean(overrides[key], RANGES[key]);
    if (value !== null) result[key] = value;
  }
  return result;
}

function saveSampling(model: AssistantModelInfo, values: AssistantSampling): void {
  const defaults = recommendedSampling(model);
  const overrides: Partial<AssistantSampling> = {};
  for (const key of KEYS) if (values[key] !== defaults[key]) (overrides as Record<string, unknown>)[key] = values[key];
  session.set(model.id, overrides);
  if (Object.keys(overrides).length === 0) removeStored(KEY_PREFIX + model.id);
  else writeStored(KEY_PREFIX + model.id, JSON.stringify(overrides));
}

function resetSampling(model: AssistantModelInfo): void {
  session.set(model.id, {});
  removeStored(KEY_PREFIX + model.id);
}

function display(value: number | null): string {
  return value === null ? "" : String(Math.round(value * 1000) / 1000);
}

type FieldSpec = { key: Key; label: string; hint: string };

const ADVANCED: FieldSpec[] = [
  { key: "top_p", label: "Top-p", hint: "Keeps the smallest set of likely words whose chances add up to this (0–1)." },
  { key: "top_k", label: "Top-k", hint: "Keeps only this many of the most likely words (0–200, 0 = off)." },
  { key: "min_p", label: "Min-p", hint: "Drops words less likely than this share of the top word (0–1)." },
  { key: "repeat_penalty", label: "Repeat penalty", hint: "Above 1 discourages repeating words (1.0–2.0)." },
  { key: "max_tokens", label: "Max response length", hint: "Stops an answer after this many tokens (16–32768). Empty means no limit." },
];

export type SamplingSection = {
  element: HTMLElement;
  /** Shows the settings of this model. Re-reading the same model keeps what is on screen. */
  setModel: (model: AssistantModelInfo | null) => void;
};

/** The body of the panel's Sampling section. */
export function createSamplingSection(): SamplingSection {
  let model: AssistantModelInfo | null = null;
  let values: AssistantSampling | null = null;

  const empty = h("p", "caption", "Download a model on the Models page to change its sampling.");
  const tempRange = h("input", "assistant-range");
  tempRange.type = "range";
  tempRange.id = "assistant-temperature";
  tempRange.min = String(RANGES.temperature.min);
  tempRange.max = String(RANGES.temperature.max);
  tempRange.step = String(RANGES.temperature.step);
  const tempNumber = numberInput("temperature");
  tempNumber.setAttribute("aria-label", "Temperature value");
  const tempLabel = h("label", "assistant-field-label", "Temperature");
  tempLabel.htmlFor = tempRange.id;
  const tempField = h(
    "div",
    "assistant-field",
    h("div", "assistant-field-head", tempLabel, tempNumber),
    tempRange,
    h("p", "caption", "Higher is more varied, lower is more focused (0–2)."),
  );

  const numberInputs = new Map<Key, HTMLInputElement>([["temperature", tempNumber]]);
  const advancedFields = ADVANCED.map((spec) => {
    const input = numberInput(spec.key);
    input.id = `assistant-sampling-${spec.key}`;
    if (spec.key === "max_tokens") input.placeholder = "No limit";
    numberInputs.set(spec.key, input);
    const label = h("label", "assistant-field-label", spec.label);
    label.htmlFor = input.id;
    return h("div", "assistant-field", h("div", "assistant-field-head", label, input), h("p", "caption", spec.hint));
  });
  const advanced = h("details", "assistant-advanced", h("summary", "", "Advanced"), ...advancedFields);
  const resetBtn = button("Reset to recommended", "btn btn-sm", () => {
    if (!model) return;
    resetSampling(model);
    values = samplingFor(model);
    paint();
  });
  const recommendedNote = h("p", "caption");
  const controls = h("div", "assistant-sampling", tempField, advanced, h("div", "action-row", resetBtn), recommendedNote);
  const element = h("div", "assistant-section-body", empty, controls);

  function numberInput(key: Key): HTMLInputElement {
    const range = RANGES[key];
    const input = h("input", "assistant-num");
    input.type = "number";
    input.min = String(range.min);
    input.max = String(range.max);
    input.step = String(range.step);
    input.inputMode = range.integer ? "numeric" : "decimal";
    input.addEventListener("change", () => commit(key, input.value));
    return input;
  }

  function commit(key: Key, raw: string): void {
    if (!model || !values) return;
    const text = raw.trim();
    if (key === "max_tokens" && text === "") {
      values.max_tokens = null;
    } else {
      const value = clean(text === "" ? NaN : Number(text), RANGES[key]);
      // An unusable entry restores the previous value.
      if (value !== null) values[key] = value;
    }
    saveSampling(model, values);
    paint();
  }

  tempRange.addEventListener("input", () => {
    if (!model || !values) return;
    const value = clean(Number(tempRange.value), RANGES.temperature);
    if (value === null) return;
    values.temperature = value;
    tempNumber.value = display(value);
    paintRangeFill();
    saveSampling(model, values);
    paintReset();
  });

  /** The shared range style colours the track up to `--fill`. */
  function paintRangeFill(): void {
    const { min, max } = RANGES.temperature;
    const percent = values ? ((values.temperature - min) / (max - min)) * 100 : 0;
    tempRange.style.setProperty("--fill", `${percent.toFixed(1)}%`);
  }

  function paintReset(): void {
    if (!model || !values) return;
    const recommended = recommendedSampling(model);
    resetBtn.disabled = KEYS.every((key) => values![key] === recommended[key]);
  }

  function paint(): void {
    const show = !!model && !!values;
    empty.classList.toggle("is-hidden", show);
    controls.classList.toggle("is-hidden", !show);
    if (!model || !values) return;
    tempRange.value = String(values.temperature);
    paintRangeFill();
    for (const [key, input] of numberInputs) input.value = display(values[key]);
    const r = recommendedSampling(model);
    recommendedNote.textContent = `Recommended for ${model.label}: temperature ${display(r.temperature)}, top-p ${display(r.top_p)}, top-k ${r.top_k}, min-p ${display(r.min_p)}, repeat penalty ${display(r.repeat_penalty)}.`;
    paintReset();
  }

  function setModel(next: AssistantModelInfo | null): void {
    if (next?.id === model?.id && values) {
      model = next;
      return;
    }
    model = next;
    values = next ? samplingFor(next) : null;
    paint();
  }

  paint();
  return { element, setModel };
}
