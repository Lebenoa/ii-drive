<script lang="ts">
  import {
    botfatherCancel,
    botfatherDraft,
    botfatherSend,
    type BotEntry,
    type DraftMsg,
  } from '$lib/api';
  import { closeDialog, openDialog } from '$lib/invoker';
  import { fadeUp, stagger } from '$lib/motion';
  import { t } from '$lib/i18n.svelte';
  import Modal from './Modal.svelte';

  let {
    onCreated,
    ondraft,
  }: {
    onCreated: (token: string, bot: BotEntry) => void | Promise<void>;
    /** Fires whenever the pending-draft state changes, so the page can
        label its entry point "resume" instead of "create". */
    ondraft?: (active: boolean) => void;
  } = $props();

  const DIALOG = 'dlg-botfather';

  let busy = $state(false);
  let input = $state('');
  // Chat transcript; `me` entries are what we sent, `bf` BotFather's replies.
  let log = $state<DraftMsg[]>([]);
  // Set once the transcript contains a bot token — enables one-click add.
  let foundToken = $state('');
  // What BotFather is waiting for, so a resumed chat says so out loud.
  let stage = $state<'name' | 'username' | 'token' | ''>('');
  // True when the transcript was restored rather than started fresh.
  let resumed = $state(false);
  let cancelling = $state(false);

  /**
   * Opens the wizard. A previous run that was closed mid-question left
   * BotFather waiting on that question, so resume the saved conversation;
   * sending a second /newbot would be answering it with the command text.
   */
  export async function openChat(): Promise<void> {
    openDialog(DIALOG);
    if (log.length > 0) return;
    busy = true;
    try {
      const d = await botfatherDraft();
      if (d.active) {
        log = d.log;
        foundToken = d.token;
        stage = d.stage;
        resumed = true;
        ondraft?.(true);
        return;
      }
    } catch {
      // No draft readable — fall through to a fresh conversation.
    } finally {
      busy = false;
    }
    await sendRaw('/newbot');
  }

  export function closeChat(): void {
    closeDialog(DIALOG);
  }

  /**
   * The transcript is deliberately NOT cleared here: the conversation is
   * still open on BotFather's side, and the server keeps the draft. Use
   * "Cancel with BotFather" to actually end it.
   */
  function onDialogClose(): void {
    busy = false;
  }

  async function sendRaw(text: string): Promise<void> {
    if (busy || text.trim().length === 0) return;
    busy = true;
    input = '';
    log = [...log, { who: 'me', text }];
    try {
      const { reply, draft } = await botfatherSend(text);
      log = [...log, { who: 'bf', text: reply }];
      if (draft.active) {
        foundToken = draft.token;
        stage = draft.stage;
        ondraft?.(true);
      }
    } catch (err) {
      log = [
        ...log,
        { who: 'bf', text: err instanceof Error ? err.message : String(err) },
      ];
    } finally {
      busy = false;
    }
  }

  /** Ends the conversation at BotFather's end and drops the saved draft. */
  async function cancelDraft(): Promise<void> {
    if (cancelling) return;
    cancelling = true;
    try {
      await botfatherCancel();
      reset();
      ondraft?.(false);
      closeChat();
    } catch (err) {
      log = [
        ...log,
        { who: 'bf', text: err instanceof Error ? err.message : String(err) },
      ];
    } finally {
      cancelling = false;
    }
  }

  function reset(): void {
    log = [];
    foundToken = '';
    stage = '';
    resumed = false;
  }

  async function addCreated(): Promise<void> {
    if (!foundToken) return;
    // Derive a username for display from the token's bot id prefix.
    await onCreated(foundToken, {
      id: Number(foundToken.split(':')[0]),
      username: foundToken.split(':')[0],
    });
    // The server drops the draft once the bot is in the pool.
    reset();
    ondraft?.(false);
    closeChat();
  }
</script>

<Modal id={DIALOG} title={t('botfather.title')} onclose={onDialogClose}>
  {#if resumed}
    <p class="resumed" transition:fadeUp>
      {t('botfather.resumed')}
    </p>
  {/if}

  <div class="chat">
    {#each log as m, i (i)}
      <p class="bubble {m.who}" in:fadeUp={{ delay: stagger(i) }}>{m.text}</p>
    {/each}
    {#if busy}<span class="spinner"></span>{/if}
  </div>

  {#if stage}
    <p class="muted stage-hint">{t(`botfather.stage.${stage}`)}</p>
  {/if}

  <!-- Not a <form>: Modal already wraps children in a method="dialog"
       form and nested forms are invalid HTML. -->
  <div class="composer">
    <input
      class="field"
      placeholder={t('botfather.placeholder')}
      bind:value={input}
      disabled={busy}
      onkeydown={(e) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          void sendRaw(input);
        }
      }}
    />
    <button
      class="btn btn-primary"
      type="button"
      disabled={busy || input.trim().length === 0}
      onclick={() => void sendRaw(input)}
    >
      {t('botfather.send')}
    </button>
  </div>

  <div class="actions">
    {#if foundToken}
      <button class="btn btn-primary" type="button" onclick={() => void addCreated()}>
        {t('botfather.addToPool', { bot: foundToken.split(':')[0] })}
      </button>
    {/if}
    {#if log.length > 0 && !foundToken}
      <button
        class="btn ghost busy-btn"
        type="button"
        disabled={cancelling}
        onclick={() => void cancelDraft()}
        title={t('botfather.cancelHint')}
      >
        {#if cancelling}<span class="spinner btn-spin"></span>{/if}
        {cancelling ? t('botfather.cancelling') : t('botfather.cancel')}
      </button>
    {/if}
  </div>
</Modal>

<style>
  .chat {
    display: flex;
    flex-direction: column;
    gap: 6px;
    max-height: 300px;
    overflow-y: auto;
    margin-bottom: 10px;
  }

  .bubble {
    margin: 0;
    padding: 7px 11px;
    border-radius: 12px;
    font-size: 13px;
    white-space: pre-wrap;
    word-break: break-word;
    max-width: 85%;
  }

  .bubble.me {
    align-self: flex-end;
    background: var(--accent, #4a7dff);
    color: #fff;
    border-bottom-right-radius: 4px;
  }

  .bubble.bf {
    align-self: flex-start;
    background: var(--panel);
    border: 1px solid var(--border);
    border-bottom-left-radius: 4px;
  }

  .composer {
    display: flex;
    gap: 8px;
  }

  .composer .field {
    flex: 1;
    margin: 0;
  }

  .hint {
    font-size: 12px;
    margin: 8px 0 0;
  }

  /* Both footer actions share one row; either may be absent. */
  .actions {
    display: flex;
    justify-content: center;
    gap: 8px;
    margin-top: 8px;
  }

  .actions:empty {
    margin-top: 0;
  }

  .resumed {
    margin: 0 0 8px;
    font-size: 12.5px;
    padding: 6px 9px;
    border-radius: 8px;
    border: 1px solid color-mix(in srgb, var(--accent) 35%, var(--border));
    background: color-mix(in srgb, var(--accent) 10%, transparent);
  }

  .stage-hint {
    font-size: 12.5px;
    margin: 6px 0 0;
  }

  /* Matches the global control spinner at inline scale. */
  .btn-spin {
    width: 13px;
    height: 13px;
    border-width: 2px;
    border-color: color-mix(in srgb, currentColor 28%, transparent);
    border-top-color: currentColor;
    flex-shrink: 0;
  }

  .busy-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 7px;
  }
</style>
