<script lang="ts">
  import { botfatherSend, type BotEntry } from '$lib/api';
  import { closeDialog, openDialog } from '$lib/invoker';
  import { fadeUp, stagger } from '$lib/motion';
  import Modal from './Modal.svelte';

  let {
    onCreated,
  }: { onCreated: (token: string, bot: BotEntry) => void | Promise<void> } = $props();

  const DIALOG = 'dlg-botfather';
  const TOKEN_RE = /(\d{6,12}:[A-Za-z0-9_-]{30,})/;

  let busy = $state(false);
  let input = $state('');
  // Chat transcript; `me` entries are what we sent, `bf` BotFather's replies.
  let log = $state<Array<{ who: 'me' | 'bf'; text: string }>>([]);
  // Set once the transcript contains a bot token — enables one-click add.
  let foundToken = $state('');

  export function openChat(): void {
    openDialog(DIALOG);
    if (log.length === 0) void sendRaw('/newbot');
  }

  export function closeChat(): void {
    closeDialog(DIALOG);
  }

  function onDialogClose(): void {
    // Reset the transcript so a reopened chat starts a fresh /newbot.
    log = [];
    foundToken = '';
    busy = false;
  }

  async function sendRaw(text: string): Promise<void> {
    if (busy || text.trim().length === 0) return;
    busy = true;
    input = '';
    log = [...log, { who: 'me', text }];
    try {
      const { reply } = await botfatherSend(text);
      log = [...log, { who: 'bf', text: reply }];
      const m = reply.match(TOKEN_RE);
      if (m) foundToken = m[1];
    } catch (err) {
      log = [
        ...log,
        { who: 'bf', text: err instanceof Error ? err.message : String(err) },
      ];
    } finally {
      busy = false;
    }
  }

  async function addCreated(): Promise<void> {
    if (!foundToken) return;
    // Derive a username for display from the token's bot id prefix.
    await onCreated(foundToken, {
      id: Number(foundToken.split(':')[0]),
      username: foundToken.split(':')[0],
    });
    closeChat();
  }
</script>

<Modal id={DIALOG} title="Create a bot with @BotFather" onclose={onDialogClose}>
  <div class="chat">
    {#each log as m, i (i)}
      <p class="bubble {m.who}" in:fadeUp={{ delay: stagger(i) }}>{m.text}</p>
    {/each}
    {#if busy}<span class="spinner"></span>{/if}
  </div>

  <!-- Not a <form>: Modal already wraps children in a method="dialog"
       form and nested forms are invalid HTML. -->
  <div class="composer">
    <input
      class="field"
      placeholder="Message @BotFather…"
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
      Send
    </button>
  </div>

  {#if foundToken}
    <div class="created">
      <button class="btn btn-primary" type="button" onclick={() => void addCreated()}>
        Add @{foundToken.split(':')[0]} to the pool
      </button>
    </div>
  {/if}
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

  .created {
    margin-top: 8px;
    text-align: center;
  }
</style>
