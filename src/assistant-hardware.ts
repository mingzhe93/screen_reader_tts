// The detected hardware (`assistant_hardware`), shared by the Assistant page and the Models
// page. GPU devices are only known once the backend is installed (they come from
// `llama-server --list-devices`), so the first status that shows the backend installed while
// the devices are still unknown triggers one refreshed query.

import { invoke } from "@tauri-apps/api/tauri";
import type { AssistantHardware } from "./types";

type Listener = (hardware: AssistantHardware) => void;

let current: AssistantHardware | null = null;
let asked = 0;
let inFlight = false;
let backendInstalled = false;
let refreshedAfterInstall = false;
const listeners: Listener[] = [];

/** Calls `listener` with every hardware result, and now if one is already known. */
export function onHardware(listener: Listener): void {
  listeners.push(listener);
  if (current) listener(current);
}

/** Queries the hardware. The newest query wins if replies arrive out of order. */
export function requestHardware(refresh = false, log?: (message: string, level?: "info" | "error") => void): void {
  const ticket = ++asked;
  inFlight = true;
  invoke<AssistantHardware>("assistant_hardware", refresh ? { refresh: true } : {}).then(
    (hardware) => {
      if (ticket !== asked) return;
      inFlight = false;
      current = hardware;
      for (const listener of listeners) listener(hardware);
      maybeRefresh();
    },
    (error) => {
      if (ticket !== asked) return;
      inFlight = false;
      log?.(`Assistant hardware check failed: ${String(error)}`, "error");
    },
  );
}

/** Queries the hardware unless it is known or a query is running (a failed query is retried). */
export function ensureHardware(log?: (message: string, level?: "info" | "error") => void): void {
  if (!current && !inFlight) requestHardware(false, log);
}

/** Pages call this with each status, so devices are re-read once after the backend is installed. */
export function noteBackendInstalled(installed: boolean): void {
  backendInstalled = installed;
  maybeRefresh();
}

function maybeRefresh(): void {
  if (!backendInstalled || refreshedAfterInstall || !current || current.devices_known) return;
  refreshedAfterInstall = true;
  requestHardware(true);
}
