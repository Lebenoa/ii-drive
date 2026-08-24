<svelte:head>
  <title>ii-drive — {t('settings.cat.telegram')}</title>
</svelte:head>

<script lang="ts">
  import {
    addBot,
    botfatherBots,
    botfatherDraft,
    botfatherToken,
    getBots,
    removeBot,
    type BotAddResult,
    type BotEntry,
  } from '$lib/api';
  import BotFatherChat from '$lib/components/BotFatherChat.svelte';
  import Channels from '$lib/components/ChannelPicker.svelte';
  import { collapse, fadeUp, stagger } from '$lib/motion';
  import { t } from '$lib/i18n.svelte';

  let chat: BotFatherChat | null = $state(null);

  let bots = $state<BotEntry[]>([]);
  let token = $state('');
  let adding = $state(false);
  let botError = $state('');
  let results = $state<BotAddResult[]>([]);
  let removingId = $state<number | null>(null);
  let owned = $state<string[]>([]);
  let loadingOwned = $state(false);
  let ownedError = $state('');
  let importing = $state('');
  // An unfinished @BotFather /newbot conversation the user can pick back up.
  let draftPending = $state(false);

  async function loadOwned(): Promise<void> {
    if (loadingOwned) return;
    loadingOwned = true;
    ownedError = '';
    try {
      owned = (await botfatherBots()).bots;
    } catch (err) {
      ownedError = err instanceof Error ? err.message : String(err);
    } finally {
      loadingOwned = false;
    }
  }

  /** Fetch the token via BotFather menus, then run the normal add flow. */
  async function importBot(name: string): Promise<void> {
    if (importing !== '') return;
    importing = name;
    botError = '';
    results = [];
    try {
      // `tok`, not `t`: the translator is imported as t().
      const { token: tok } = await botfatherToken(name);
      const res = await addBot(tok);
      bots = await getBots();
      results = res.results;
    } catch (err) {
      botError = err instanceof Error ? err.message : String(err);
    } finally {
      importing = '';
    }
  }

  $effect(() => {
    void (async () => {
      bots = await getBots();
    })();
  });

  // Surface a half-finished wizard without making the user open it first.
  $effect(() => {
    void (async () => {
      try {
        draftPending = (await botfatherDraft()).active;
      } catch {
        draftPending = false;
      }
    })();
  });

  async function add(): Promise<void> {
    const tok = token.trim();
    if (adding || tok.length === 0) return;
    adding = true;
    botError = '';
    results = [];
    try {
      const res = await addBot(tok);
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
  async function addFromBotFather(t: string): Promise<void> {
    botError = '';
    try {
      const res = await addBot(t);
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
    <h2>{t('telegram.storageChannels')}</h2>
    <p class="muted hint">{t('telegram.storageHint')}</p>
    <Channels onDone={null} redirectOnSave={false} embedded />
  </section>

  <section class="card section" in:fadeUp={{ delay: stagger(1) }}>
    <div class="head-row">
      <h2>{t('telegram.downloadBots')}</h2>
      {#if bots.length > 0}<span class="pill">{bots.length}</span>{/if}
    </div>
    <p class="muted hint">{t('telegram.botsHint')}</p>
    <BotFatherChat
      bind:this={chat}
      onCreated={(t) => addFromBotFather(t)}
      ondraft={(active) => (draftPending = active)}
    />
    <div class="sub">
      <div class="sub-head">
        <div class="sub-title">
          <strong>{t('telegram.importExisting')}</strong>
          <span class="muted sub-note">{t('telegram.importNote')}</span>
        </div>
        <button
          class="btn ghost busy-btn"
          type="button"
          disabled={loadingOwned}
          onclick={() => void loadOwned()}
        >
          {#if loadingOwned}<span class="spinner btn-spin"></span>{/if}
          {loadingOwned ? t('telegram.asking') : t('telegram.listMyBots')}
        </button>
      </div>
      {#if ownedError}<p class="error-text">{ownedError}</p>{/if}
      {#if owned.length > 0}
        <ul class="row-list">
          {#each owned as name, i (name)}
            <li class="row-item" in:fadeUp={{ delay: stagger(i) }}>
              <span class="who">
                <span class="avatar">{name.replace(/^@/, '').slice(0, 1)}</span>
                <span>{name}</span>
              </span>
              <button
                class="btn ghost busy-btn"
                type="button"
                disabled={importing !== ''}
                onclick={() => void importBot(name)}
              >
                {#if importing === name}<span class="spinner btn-spin"></span>{/if}
                {importing === name ? t('telegram.importing') : t('telegram.import')}
              </button>
            </li>
          {/each}
        </ul>
      {:else if !loadingOwned && !ownedError}
        <p class="muted hint">{t('telegram.nothingListed', { button: t('telegram.listMyBots') })}</p>
      {/if}
    </div>

    <div class="sub">
      <div class="sub-head">
        <div class="sub-title">
          <strong>{t('telegram.newBot')}</strong>
          <span class="muted sub-note">
            {draftPending ? t('telegram.draftOpen') : t('telegram.newBotNote')}
          </span>
        </div>
        <button
          class="btn ghost"
          class:accent-outline={draftPending}
          type="button"
          onclick={() => void chat?.openChat()}
        >
          {draftPending ? t('telegram.resumeSetup') : t('telegram.openBotfather')}
        </button>
      </div>
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
          {adding ? t('telegram.adding') : t('telegram.addBot')}
        </button>
      </form>

    </div>

    {#if botError}<p class="error-text">{botError}</p>{/if}

    {#if bots.length > 0}
      <ul class="row-list">
        {#each bots as b, i (b.id)}
          <li class="row-item" in:fadeUp={{ delay: stagger(i) }} out:collapse>
            <span class="who">
              <span class="avatar">{b.username.replace(/^@/, '').slice(0, 1)}</span>
              <span>@{b.username}</span>
            </span>
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
      <p class="muted">{t('telegram.noBots')}</p>
    {/if}

    {#if results.length > 0}
      <div class="results">
        <p class="muted">{t('telegram.channelAccess')}</p>
        <ul>
          {#each results as r, i (i)}
            <li in:fadeUp={{ delay: stagger(i) }}>
              <span class={r.ok ? 'ok-text' : 'error-text'}>{r.ok ? '✓' : '✗'} @{r.bot} in {r.title}</span>
              {#if !r.ok}<span class="error-text">{r.error}</span>{/if}
            </li>
          {/each}
        </ul>
      </div>
    {/if}
  </section>
</main>

<style>
  /* Marks the entry point when a /newbot conversation is still open. */
  .accent-outline {
    border-color: var(--accent);
    color: inherit;
  }

  .bot-row {
    display: flex;
    gap: 8px;
    margin-bottom: 10px;
  }

  .bot-row .field {
    flex: 1;
    margin: 0;
  }

  .row-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .row-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 7px 10px;
  }

  .who {
    display: flex;
    align-items: center;
    gap: 9px;
    min-width: 0;
  }

  .avatar {
    width: 26px;
    height: 26px;
    border-radius: 50%;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: color-mix(in srgb, var(--accent) 20%, transparent);
    color: var(--accent);
    font-size: 12px;
    font-weight: 700;
    text-transform: uppercase;
    flex-shrink: 0;
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

  .head-row {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .head-row h2 {
    margin: 0;
  }

  .pill {
    background: color-mix(in srgb, var(--accent) 16%, transparent);
    color: var(--accent);
    border-radius: 999px;
    padding: 1px 9px;
    font-size: 12px;
    font-weight: 600;
  }

  .sub {
    border-top: 1px solid var(--border);
    margin-top: 12px;
    padding-top: 12px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .sub-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }

  .sub-title {
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .sub-title strong {
    font-size: 13.5px;
  }

  .sub-note {
    font-size: 12px;
  }

  .results .ok-text {
    margin: 0;
  }
</style>
