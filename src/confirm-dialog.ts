import { confirm } from "@tauri-apps/api/dialog";

/**
 * A native OK/Cancel dialog. Never call `window.confirm`: Tauri v1's injected script replaces
 * it with a function that returns a Promise, which is always truthy, so `!window.confirm(...)`
 * silently accepts. Any failure to show the dialog counts as Cancel.
 */
export async function confirmAction(message: string, okLabel = "OK"): Promise<boolean> {
  try {
    return (await confirm(message, { title: "VoiceReader", type: "warning", okLabel, cancelLabel: "Cancel" })) === true;
  } catch {
    return false;
  }
}
