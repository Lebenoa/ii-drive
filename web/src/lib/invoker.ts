/**
 * Dialog open/close helpers.
 *
 * Opening is done imperatively: every opener in this app also has to set
 * component state (which file/folder the dialog is about), so JS runs
 * regardless, and several openers are not `<button>` elements at all
 * (grid thumbnails, the folder-row delete affordance) where the Command
 * Invoker API does not apply. One imperative path keeps all of them
 * working on every browser.
 *
 * Closing has two shapes. Cancel buttons inside Modal's
 * `<form method="dialog">` are `type="submit"`, so `closeAttrs` is pure
 * enhancement there — they close natively without Invoker support. A close
 * button that is NOT inside such a form (the preview bar's ✕) would be a
 * dead control on pre-Invoker browsers, so it calls `closeDialog` instead.
 */

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

/** Show `id` as a modal. Idempotent: showModal() on an open dialog throws. */
export function openDialog(id: string): void {
  const dlg = document.getElementById(id) as HTMLDialogElement | null;
  if (dlg !== null && !dlg.open) dlg.showModal();
}

/** Close `id`. For close buttons outside a `method="dialog"` form. */
export function closeDialog(id: string): void {
  (document.getElementById(id) as HTMLDialogElement | null)?.close();
}
