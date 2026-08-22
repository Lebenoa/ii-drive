/**
 * Command Invoker API (button `commandfor`/`command`) helpers.
 *
 * The attributes are always rendered — browsers without support simply
 * ignore them — so the markup stays declarative everywhere. Only the
 * fallback opener below needs a support check.
 */

export const invokerSupported: boolean =
  typeof window !== 'undefined' && 'commandFor' in HTMLButtonElement.prototype;

/** Spread onto a button that should open `id` as a modal, natively. */
export function openAttrs(id: string): Record<string, string> {
  return { commandfor: id, command: 'showModal' };
}

/**
 * Spread onto a Cancel-style button inside the dialog: it should be
 * `type="submit"` in the `method="dialog"` form, so browsers without the
 * Command Invoker API still close it natively; where the API works, the
 * explicit commandfor/command="close" closes it too (a second close on an
 * already-closed dialog is a no-op). Empty `commandfor` was tried and does
 * nothing in current browsers, so the id stays explicit.
 */
export function closeAttrs(id: string): Record<string, string> {
  return { commandfor: id, command: 'close' };
}

/** Fallback opener for browsers without the Command Invoker API. */
export function openDialog(id: string): void {
  (document.getElementById(id) as HTMLDialogElement | null)?.showModal();
}
