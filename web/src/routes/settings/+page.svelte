<script lang="ts">
  import { goto } from '$app/navigation';
  import {
    addBot,
    getBots,
    getSettings,
    getToken,
    removeBot,
    saveSettings,
    type BotAddResult,
    type BotEntry,
  } from '$lib/api';
  import Channels from '$lib/components/ChannelPicker.svelte';

  let checking = $state(true);
  let bots = $state<BotEntry[]>([]);
  let token = $state('');
  let adding = $state(false);
  let botError = $state('');
  let results = $state<BotAddResult[]>([]);
  let removingId = $state<number | null>(null);

  let splitMb = $state(0);
  let splitLoaded = $state(false);
  let splitSaving = $state(false);
  let splitMsg = $state('');
  let splitError = $state('');

  const SPLIT_PRESETS = [0, 250, 500, 1024];

  function splitLabel(mb: number): string {
    return mb === 0 ? 'Off' : mb >= 1024 ? `${mb / 1024} GB` : `${mb} MB`;
  }

  $effect(() => {
    void (async () => {
      if (!getToken()) {
        goto('/login');
        return;
      }
      try {
        bots = await getBots();
        const s = await getSettings();
        splitMb = s.split_mb;
        splitLoaded = true;
      } catch (err) {
        goto('/login');
        return;
      }
      checking = false;
    })();
  });

  async function saveSplit(): Promise<void> {
    if (splitSaving) return;
    splitSaving = true;
    splitMsg = '';
    splitError = '';
    try {
      // A threshold at/above the upload cap can never trigger, so the
      // server rejects it; clamp client-side to keep the error friendly.
      const mb = Math.max(0, Math.min(2047, Math.floor(Number(splitMb) || 0)));
      await saveSettings({ split_mb: mb });
      splitMb = mb;
      splitMsg = 'Saved.';
    } catch (err) {
      splitError = err instanceof Error ? err.message : String(err);
    } finally {
      splitSaving = false;
    }
  }

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

<div class="settings-shell">
  <header class="topbar">
    <a class="back" href="/">← Back to files</a>
    <span class="title">Settings</span>
    <span></span>
  </header>

  {#if checking}
    <div class="center-screen">
      <div class="spinner" aria-label="loading"></div>
    </div>
  {:else}
    <main class="content">
      <section class="card section">
        <h2>Storage channels</h2>
        <p class="muted hint">
          Uploads are spread across the selected channels. You can also create a
          brand-new channel here.
        </p>
        <Channels onDone={null} redirectOnSave={false} />
      </section>

      <section class="card section">
        <h2>Split uploads</h2>
        <p class="muted hint">
          Files larger than this threshold are split into parts that upload
          <strong>in parallel</strong> instead of one slow stream — usually
          much faster for big files. With several download bots configured,
          each part can also be fetched by a different bot under its own rate
          limit, so downloads speed up too. Parts are stitched back together
          automatically when the file is streamed or downloaded.
        </p>
        {#if splitLoaded}
          <div class="split-row">
            <input
              class="field split-input"
              type="number"
              min="0"
              max="2048"
              step="1"
              bind:value={splitMb}
              disabled={splitSaving}
              aria-label="Split threshold in megabytes"
            />
            <span class="muted">MB</span>
            {#each SPLIT_PRESETS as p (p)}
              <button
                class="btn ghost preset"
                class:active={splitMb === p}
                type="button"
                disabled={splitSaving}
                onclick={() => (splitMb = p)}
              >
                {splitLabel(p)}
              </button>
            {/each}
            <button
              class="btn btn-primary"
              type="button"
              disabled={splitSaving}
              onclick={() => void saveSplit()}
            >
              {splitSaving ? 'Saving…' : 'Save'}
            </button>
          </div>
          <p class="muted hint">
            0 (Off) uploads every file as a single message. Already-uploaded
            files keep their current layout; the threshold only affects new
            uploads.
          </p>
          {#if splitMsg}<p class="ok-text">{splitMsg}</p>{/if}
          {#if splitError}<p class="error-text">{splitError}</p>{/if}
        {:else}
          <p class="muted">Loading…</p>
        {/if}
      </section>

      <section class="card section">
        <h2>Download bots</h2>
        <p class="muted hint">
          Bots download files through their own rate limits, so several bots
          spread the load. Adding a bot invites it into every selected storage
          channel as an admin. Create tokens with
          <a href="https://t.me/BotFather" target="_blank" rel="noreferrer">@BotFather</a>.
        </p>

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
          <button class="btn btn-primary" type="submit" disabled={adding || token.trim().length === 0}>
            {adding ? 'Adding…' : 'Add bot'}
          </button>
        </form>

        {#if botError}<p class="error-text">{botError}</p>{/if}

        {#if bots.length > 0}
          <ul class="bot-list">
            {#each bots as b (b.id)}
              <li class="bot-item">
                <span>@{b.username}</span>
                <button
                  class="btn ghost"
                  type="button"
                  disabled={removingId === b.id}
                  onclick={() => drop(b)}
                >
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
                <li>
                  <span>{r.ok ? '✓' : '✗'} @{r.bot} in {r.title}</span>
                  {#if !r.ok}<span class="error-text">{r.error}</span>{/if}
                </li>
              {/each}
            </ul>
          </div>
        {/if}
      </section>
    </main>
  {/if}
</div>

<style>
  .settings-shell {
    min-height: 100vh;
  }

  .topbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 22px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
    position: sticky;
    top: 0;
    z-index: 5;
  }

  .back {
    color: var(--muted);
    text-decoration: none;
    font-size: 13.5px;
  }

  .back:hover {
    color: inherit;
  }

  .title {
    font-weight: 700;
  }

  .content {
    width: min(720px, 100%);
    margin: 24px auto;
    padding: 0 20px;
    display: flex;
    flex-direction: column;
    gap: 20px;
  }

  .section h2 {
    margin: 0 0 6px;
    font-size: 17px;
  }

  .hint {
    font-size: 12.5px;
    margin: 0 0 14px;
  }

  .bot-row {
    display: flex;
    gap: 8px;
    margin-bottom: 12px;
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

  .split-row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px;
    margin-bottom: 10px;
  }

  .split-input {
    width: 90px;
    margin: 0;
  }

  .preset.active {
    border-color: var(--accent, inherit);
    font-weight: 600;
  }

  .ok-text {
    color: var(--ok, #2e9e5b);
    font-size: 12.5px;
    margin: 4px 0 0;
  }
</style>
