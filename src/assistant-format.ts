// Text shared by the Assistant page and the Models page: sizes, context labels, the
// memory estimate shown in a tooltip and the memory-fit rule (plan §5.2–5.3).

import type { AssistantHardware, AssistantMemoryModel, AssistantModelInfo } from "./types";

export function gigabytes(bytes: number): string {
  return (bytes / (1024 * 1024 * 1024)).toFixed(1);
}

export function contextLabel(tokens: number): string {
  return `${Math.round(tokens / 1024)}K`;
}

function gibFromMib(mib: number): string {
  return (mib / 1024).toFixed(1);
}

/** Tooltip for a model's estimated memory at the context the estimate was made for. */
export function memoryTooltip(model: AssistantModelInfo): string {
  const estimate = model.memory_estimate;
  const total = estimate.gpu_mib + estimate.host_mib;
  const source = estimate.measured
    ? "Measured with llama.cpp on Apple Silicon."
    : "Calculated from the model's layout; not measured yet.";
  return [
    `Estimated memory with ${contextLabel(estimate.ctx_size)} context: about ${gibFromMib(total)} GB.`,
    `GPU memory (VRAM): about ${gibFromMib(estimate.gpu_mib)} GB. System RAM: about ${gibFromMib(estimate.host_mib)} GB.`,
    "On Apple Silicon both come from the same unified memory. A longer context needs more.",
    source,
    `Longest context this model supports: ${contextLabel(model.max_context)} tokens.`,
  ].join("\n");
}

/** A compact "Memory" badge; the estimate is in its tooltip so the row stays uncluttered. */
export function memoryBadge(model: AssistantModelInfo): HTMLSpanElement {
  const badge = document.createElement("span");
  badge.className = "badge badge-hint";
  badge.textContent = model.memory_estimate.measured ? "Memory" : "Memory (calc.)";
  badge.title = memoryTooltip(model);
  badge.setAttribute("aria-label", badge.title);
  badge.tabIndex = 0;
  return badge;
}

export async function copyToClipboard(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    // Fall back to the legacy path when the WebView refuses the async clipboard.
  }
  const area = document.createElement("textarea");
  area.className = "sr-only";
  area.value = text;
  document.body.appendChild(area);
  area.select();
  const copied = document.execCommand("copy");
  area.remove();
  if (!copied) throw new Error("Copy was blocked by the system.");
}

// ---------------------------------------------------------------------------
// Memory fit (plan §5.3). Pure: tested by scripts/test-assistant-fit.mjs.
// ---------------------------------------------------------------------------

export type FitLevel = "fits" | "tight" | "over";

export type MemoryFit = {
  level: FitLevel;
  /** ✅, ⚠️ or ⛔. */
  icon: string;
  /** One short line, for example "Fits in memory". */
  label: string;
  /** The figures behind the verdict, for a caption or tooltip. */
  detail: string;
  gpu_mib: number;
  host_mib: number;
};

const FIT_ICONS: Record<FitLevel, string> = { fits: "\u2705", tight: "\u26a0\ufe0f", over: "\u26d4" };
/** Kept free on a discrete GPU, matching llama.cpp's `--fit` default margin. */
const GPU_MARGIN_MIB = 1024;

/** Estimated GPU and host memory in MiB for a context length, rounded up to whole 1K blocks. */
export function memoryAt(memory: AssistantMemoryModel, ctx: number): { gpu: number; host: number } {
  const blocks = Math.ceil(Math.max(0, ctx) / 1024);
  return {
    gpu: memory.gpu_fixed_mib + memory.gpu_mib_per_1k_ctx * blocks,
    host: memory.host_fixed_mib + memory.host_mib_per_1k_ctx * blocks,
  };
}

/**
 * Whether a model at a context length fits this computer.
 * - Unified memory, no discrete GPU, or devices not known yet: GPU + host against system RAM:
 *   fits up to 50%, tight up to 75%, over beyond.
 * - Discrete GPU (the largest non-integrated device): fits if the GPU pool fits in its memory
 *   minus 1 GB, otherwise tight (the rest spills to the CPU). Over in either case if the host
 *   pool plus any spill exceeds 50% of system RAM.
 */
export function memoryFit(memory: AssistantMemoryModel, ctx: number, hardware: AssistantHardware): MemoryFit {
  const { gpu, host } = memoryAt(memory, ctx);
  const ram = hardware.total_ram_mib;
  const result = (level: FitLevel, label: string, detail: string[]): MemoryFit => ({
    level,
    icon: FIT_ICONS[level],
    label,
    detail: detail.filter(Boolean).join(" "),
    gpu_mib: gpu,
    host_mib: host,
  });
  if (!(ram > 0)) {
    return result("tight", "Memory could not be checked", ["The amount of system memory is unknown."]);
  }
  const discrete = hardware.devices_known
    ? hardware.devices.filter((device) => !device.integrated).sort((a, b) => b.total_mib - a.total_mib)[0]
    : undefined;

  if (hardware.unified_memory || !discrete) {
    const total = gpu + host;
    const level: FitLevel = total <= ram * 0.5 ? "fits" : total <= ram * 0.75 ? "tight" : "over";
    const label = level === "fits" ? "Fits in memory" : level === "tight" ? "Tight, may slow the computer" : "Likely to run out of memory";
    return result(level, label, [
      `Needs about ${gibFromMib(total)} GB of ${gibFromMib(ram)} GB system memory${hardware.unified_memory ? ", shared with the GPU" : ""}.`,
      `Up to 50% (${gibFromMib(ram * 0.5)} GB) fits comfortably; up to 75% is tight.`,
      hardware.devices_known || hardware.unified_memory ? "" : "GPU memory unknown until the backend is installed.",
    ]);
  }

  const budget = discrete.total_mib - GPU_MARGIN_MIB;
  const spill = Math.max(0, gpu - budget);
  const hostTotal = host + spill;
  const hostOver = hostTotal > ram * 0.5;
  const level: FitLevel = hostOver ? "over" : spill > 0 ? "tight" : "fits";
  const label = hostOver
    ? "Likely to run out of memory"
    : spill > 0
      ? "Won't fit in GPU memory; parts run on the CPU and it will be slower"
      : "Fits in GPU memory";
  return result(level, label, [
    `GPU memory: about ${gibFromMib(gpu)} GB of ${gibFromMib(discrete.total_mib)} GB on ${discrete.name} (1 GB kept free).`,
    spill > 0 && hostOver ? "It won't fit in GPU memory, so parts run on the CPU." : "",
    `System RAM: about ${gibFromMib(hostTotal)} GB${spill > 0 ? `, including ${gibFromMib(spill)} GB moved off the GPU` : ""}, of ${gibFromMib(ram)} GB (up to 50% is safe).`,
  ]);
}

/** A small coloured dot for a fit level, with the explanation as its tooltip. */
export function fitDot(fit: MemoryFit, prefix: string): HTMLSpanElement {
  const dot = document.createElement("span");
  dot.className = `assistant-fit-dot fit-${fit.level}`;
  dot.title = `${prefix}: ${fit.icon} ${fit.label}. ${fit.detail}`;
  dot.setAttribute("role", "img");
  dot.setAttribute("aria-label", dot.title);
  dot.tabIndex = 0;
  return dot;
}
