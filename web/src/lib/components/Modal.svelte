<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    id,
    title,
    children,
    actions,
    onclose,
  }: {
    id: string;
    title: string;
    children: Snippet;
    /** Footer buttons; submit buttons close the dialog with their `value`
     * as returnValue (form method="dialog"), Cancel buttons typically use
     * commandfor/command="close". */
    actions?: Snippet;
    /** Receives the dialog's returnValue on close ('' for cancel). */
    onclose?: (returnValue: string) => void;
  } = $props();

  let dlg = $state<HTMLDialogElement | null>(null);
</script>

<!-- Native <dialog>: Esc and focus trapping come for free. A click that
     hits the dialog element itself landed on the ::backdrop, so close. -->
<dialog
  bind:this={dlg}
  {id}
  class="modal"
  onclose={() => onclose?.(dlg?.returnValue ?? '')}
  onclick={(e: MouseEvent) => {
    if (e.target === dlg) dlg?.close();
  }}
>
  <form method="dialog" class="card">
    <h3 class="m-title">{title}</h3>
    <div class="m-body">
      {@render children()}
    </div>
    <div class="m-actions">
      {#if actions}
        {@render actions()}
      {:else}
        <button class="btn" type="submit">Close</button>
      {/if}
    </div>
  </form>
</dialog>

<style>
  dialog.modal {
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--panel);
    color: var(--text);
    padding: 0;
    width: min(430px, calc(100vw - 40px));
    box-shadow: 0 18px 50px rgba(0, 0, 0, 0.45);
  }

  dialog.modal::backdrop {
    background: rgba(6, 9, 15, 0.6);
    backdrop-filter: blur(3px);
  }

  dialog.modal[open] {
    animation: modal-in 0.16s ease;
  }

  @keyframes modal-in {
    from {
      opacity: 0;
      transform: translateY(8px) scale(0.98);
    }
    to {
      opacity: 1;
      transform: none;
    }
  }

  .card {
    padding: 18px 20px 16px;
  }

  .m-title {
    margin: 0 0 10px;
    font-size: 15.5px;
  }

  .m-body {
    font-size: 13.5px;
    color: var(--text);
    margin-bottom: 16px;
  }

  .m-body :global(.field) {
    width: 100%;
  }

  .m-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
</style>
