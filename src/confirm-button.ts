// A two-step button for deleting downloaded models: the first click arms it for a few
// seconds and changes its label; only a second click runs the action. Nothing is deleted
// by a single stray click.

const ARMED_MS = 4000;
/** A confirming click must come at least this long after arming, so a double-click
 *  (two clicks in quick succession) can never delete. */
const MIN_CONFIRM_DELAY_MS = 500;

export type ConfirmButtonOptions = {
  label: string;
  confirmLabel: string;
  busyLabel: string;
  onConfirm: () => Promise<void>;
};

export function makeConfirmButton(button: HTMLButtonElement, options: ConfirmButtonOptions): void {
  let armed = false;
  let armedAt = 0;
  let timer = 0;
  const reset = () => {
    armed = false;
    window.clearTimeout(timer);
    button.textContent = options.label;
    button.classList.remove("is-armed");
  };
  reset();
  button.addEventListener("click", async (event) => {
    if (button.disabled) return;
    if (!armed) {
      armed = true;
      armedAt = performance.now();
      button.textContent = options.confirmLabel;
      button.classList.add("is-armed");
      timer = window.setTimeout(reset, ARMED_MS);
      return;
    }
    // The second click of a double-click (detail 2) or any click right after arming is
    // ignored; the button stays armed for a deliberate confirmation.
    if (event.detail > 1 || performance.now() - armedAt < MIN_CONFIRM_DELAY_MS) return;
    window.clearTimeout(timer);
    button.disabled = true;
    button.textContent = options.busyLabel;
    try {
      await options.onConfirm();
    } finally {
      button.disabled = false;
      reset();
    }
  });
}
