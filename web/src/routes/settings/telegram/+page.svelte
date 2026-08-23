<svelte:head>
  <title>ii-drive — Telegram settings</title>
</svelte:head>

<script lang="ts">
  import {
    addBot,
    getBots,
    removeBot,
    type BotAddResult,
    type BotEntry,
  } from '$lib/api';
  import BotFatherChat from '$lib/components/BotFatherChat.svelte';
  import Channels from '$lib/components/ChannelPicker.svelte';
  import { collapse, fadeUp, stagger } from '$lib/motion';

  let chat: BotFatherChat | null = $state(null);

  let bots = $state<BotEntry[]>([]);
  let token = $state('');
  let adding = $state(false);
  let botError = $state('');
  let results = $state<BotAddResult[]>([]);
  let removingId = $state<number | null>(null);

  $effect(() => {
    void (async () => {
      bots = await getBots();
    })();
  });

  async function add(): Promise<void> {
    const t = token.trim();
    if (adding || t.length === 0) return;
    adding = true;
    botError = '';
    results = [];
    try {
      const res = await addBot(t);
      bots = await getBots();
      results = res.results;
      token = '';
    } catch (err) {
      botError = err instanceof Error ? err.message : String(err);
    } finally {
      adding = false;
    }
  }

  /** Called by the BotFather wizard with the freshly minted token. */
  async function addFromBotFather(token: string): Promise<void> {
    botError = '';
    try {
      const res = await addBot(token);
      bots = await getBots();
      results = res.results;
    } catch (err) {
      botError = err instanceof Error ? err.message : String(err);
    }
  }

  async function drop(bot: BotEntry): Promise<void> {
    if (removingId !== null) return;
    removingId = bot.id;
    botError = '';
    try {
      await removeBot(bot.id);
      bots = bots.filter((b) => b.id !== bot.id);
    } catch (err) {
      botError = err instanceof Error ? err.message : String(err);
    } finally {
      removingId = null;
    }
  }
</script>

<main class="content">
  <section class="card section" in:fadeUp={{ delay: stagger(0) }}>
    <h2>Storage channels</h2>
    <p class="muted hint">
      Uploaded files are stored as documents inside your storage channels.
    </p>
    <Channels onDone={null} redirectOnSave={false} embedded />
  </section>

  <section class="card section" in:fadeUp={{ delay: stagger(1) }}>
    <h2>Download bots</h2>
    <p class="muted hint">
      Bots download files through their own rate limits, so several bots
      spread the load. Adding a bot invites it into every selected storage
      channel as an admin.
    </p>
    <p class="muted hint">
      No token yet? <button class="linklike" type="button" onclick={() => chat?.openChat()}>
        Create a bot with @BotFather
      </button> right here.
    </p>
    <BotFatherChat bind:this={chat} onCreated={(t) => addFromBotFather(t)} />

    <form
      class="bot-row"
      onsubmit={(e) => {
        e.preventDefault();
        void add();
      }}
    >
      <input
        class="field"
        type="password"
        placeholder="123456:AA… bot token"
        autocomplete="off"
        bind:value={token}
        disabled={adding}
      />
      <button
        class="btn btn-primary busy-btn"
        type="submit"
        disabled={adding || token.trim().length === 0}
      >
        {#if adding}<span class="spinner btn-spin"></span>{/if}
        {adding ? 'Adding…' : 'Add bot'}
      </button>
    </form>

    {#if botError}<p class="error-text">{botError}</p>{/if}

    {#if bots.length > 0}
      <ul class="bot-list">
        {#each bots as b, i (b.id)}
          <li class="bot-item" in:fadeUp={{ delay: stagger(i) }} out:collapse>
            <span>@{b.username}</span>
            <button
              class="btn ghost busy-btn"
              type="button"
              disabled={removingId === b.id}
              onclick={() => drop(b)}
            >
              {#if removingId === b.id}<span class="spinner btn-spin"></span>{/if}
              {removingId === b.id ? 'Removing…' : 'Remove'}
            </button>
          </li>
        {/each}
      </ul>
    {:else}
      <p class="muted">No bots configured — downloads use your own account.</p>
    {/if}

    {#if results.length > 0}
      <div class="results">
        <p class="muted">Channel access:</p>
        <ul>
          {#each results as r, i (i)}
            <li in:fadeUp={{ delay: stagger(i) }}>
              <span>{r.ok ? '✓' : '✗'} @{r.bot} in {r.title}</span>
              {#if !r.ok}<span class="error-text">{r.error}</span>{/if}
            </li>
          {/each}
        </ul>
      </div>
    {/if}
  </section>
</main>

<style>
  .bot-row {
    display: flex;
    gap: 8px;
    margin-bottom: 10px;
  }

  .linklike {
    appearance: none;
    background: none;
    border: none;
    padding: 0;
    font: inherit;
    color: var(--accent, inherit);
    text-decoration: underline;
    cursor: pointer;
  }

  .bot-row .field {
    flex: 1;
    margin: 0;
  }

  .bot-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .bot-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 7px 10px;
  }

  .results ul {
    list-style: none;
    padding: 0;
    margin: 4px 0 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
    font-size: 12.5px;
  }

  .results li {
    display: flex;
    gap: 8px;
    justify-content: space-between;
  }
</style>
