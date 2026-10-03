// Constants and helpers used by both the main window and the floating toolbar window.

export const TOOLBAR_ACTION_EVENT = "voicereader:toolbar-action";
export const TOOLBAR_SHOW_EVENT = "voicereader:toolbar-show";
export const TOOLBAR_HIDE_EVENT = "voicereader:toolbar-hide";
export const TOOLBAR_PAUSED_EVENT = "voicereader:toolbar-paused";
export const TOOLBAR_SKIP_BACK_NOOP_EVENT = "voicereader:toolbar-skip-back-noop";
export const RATE_UPDATED_EVENT = "voicereader:rate-updated";

const MIN_RATE = 0.25;
const MAX_RATE = 4;

export function clampRate(rate: number): number {
  return Math.min(MAX_RATE, Math.max(MIN_RATE, rate));
}

export function formatRateNumber(rate: number): string {
  return rate.toFixed(2).replace(/\.?0+$/, "");
}

export function rateFromPayload(payload: Record<string, unknown>): number | null {
  const parsed = Number(payload.rate ?? NaN);
  return Number.isFinite(parsed) ? parsed : null;
}
