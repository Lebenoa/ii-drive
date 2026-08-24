<script lang="ts">
  import { flip } from 'svelte/animate';
  import {
    createChannel,
    fetchChannels,
    saveChannels,
    type BotWireFailure,
    type ChannelInfo,
  } from '$lib/api';
  import { fadeUp, flipDur, pop, stagger } from '$lib/motion';
  import { t } from '$lib/i18n.svelte';

  let {
    onDone = null,
    redirectOnSave = true,
    embedded = false,
  }: { onDone?: (() => void) | null; redirectOnSave?: boolean; embedded?: boolean } = $props();

  let available = $state<ChannelInfo[]>([]);
  let selectedChats = $state<string[]>([]);
  let loading = $state(true);
  let saving = $state(false);
  let error = $state('');
  let newTitle = $state('');
  let creating = $state(false);
  let saved = $state(false);
  let wireFailures = $state<BotWireFailure[]>([]);

  function toggle(chat: string): void {
    if (selectedChats.includes(chat)) {
      selectedChats = selectedChats.filter((c) => c !== chat);
    } else {
      selectedChats = [...selectedChats, chat];
    }
  }

  async function create(): Promise<void> {
    const title = newTitle.trim();
    if (creating || title.length === 0) return;
    creating = true;
    error = '';
    try {
      const ch = await createChannel(title);
      available = [...available.filter((a) => a.chat !== ch.chat), ch];
      selectedChats = [...selectedChats, ch.chat];
      newTitle = '';
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      creating = false;
    }
  }

  async function load(): Promise<void> {
    loading = true;
    try {
      const res = await fetchChannels();
      available = res.available;
      selectedChats = res.selected.map((s) => s.chat);
      error = '';
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    void load();
  });

  async function save(): Promise<void> {
    if (saving || selectedChats.length === 0) return;
    saving = true;
    error = '';
    wireFailures = [];
    const chosen = selectedChats
      .map((chat) => available.find((a) => a.chat === chat))
      .filter((a): a is ChannelInfo => a !== undefined);
    try {
      // The save succeeds even if wiring bots into a channel fails;
      // surface those failures without blocking the selection.
      wireFailures = await saveChannels(chosen);
      saved = true;
      if (redirectOnSave && onDone && wireFailures.length === 0) onDone();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      saving = false;
    }
  }
</script>

<div class={embedded ? 'embed' : 'center-screen'}>
  <div class="card picker-card" class:bare={embedded}>
    {#if !embedded}
      <h1 class="brand">ii-drive</h1>
      <p class="muted tagline">{t('channels.tagline')}</p>
      <p class="muted hint">{t('channels.hint')}</p>
    {/if}

    {#if loading}
      <p class="muted loading-row">
        <span class="spinner btn-spin"></span>
        {t('channels.loading')}
      </p>
    {:else}
      <div class="list" role="listbox" aria-label={t('telegram.storageChannels')}>
        {#each available as c, i (c.chat)}
          <button
            type="button"
            class="item"
            class:picked={selectedChats.includes(c.chat)}
            onclick={() => toggle(c.chat)}
            in:fadeUp={{ delay: stagger(i) }}
            animate:flip={{ duration: flipDur() }}
          >
            <span class="check">
              {#if selectedChats.includes(c.chat)}<span in:pop>✓</span>{/if}
            </span>
            {c.title}
            <span class="muted key">{c.chat}</span>
          </button>
        {:else}
          <p class="muted" in:fadeUp>{t('channels.noneFound')}</p>
        {/each}
      </div>

      <form
        class="create-row"
        in:fadeUp={{ delay: stagger(available.length) }}
        onsubmit={(e) => {
          e.preventDefault();
          void create();
        }}
      >
        <input
          class="field"
          type="text"
          placeholder={t('channels.createPlaceholder')}
          bind:value={newTitle}
          maxlength={128}
          disabled={creating}
        />
        <button
          class="btn ghost busy-btn"
          type="submit"
          disabled={creating || newTitle.trim().length === 0}
        >
          {#if creating}<span class="spinner btn-spin"></span>{/if}
          {creating ? t('channels.creating') : t('channels.create')}
        </button>
      </form>

      {#if error}<p class="error-text">{error}</p>{/if}
      {#if wireFailures.length > 0}
        <div class="wire-warn">
          <p class="muted">{t('channels.wireWarn')}</p>
          <ul>
            {#each wireFailures as f (f.bot + f.chat)}
              <li><span class="error-text">{f.bot} → {f.title}: {f.error}</span></li>
            {/each}
          </ul>
          <button class="btn ghost" type="button" onclick={() => void save()}>
            {t('common.retry')}
          </button>
        </div>
      {:else if saved && !redirectOnSave}<p class="muted saved-note" transition:fadeUp>{t('common.saved')}</p>{/if}

      <button
        class="btn btn-primary busy-btn"
        type="button"
        disabled={saving || selectedChats.length === 0}
        onclick={save}
      >
        {#if saving}<span class="spinner btn-spin"></span>{/if}
        {saving ? t('common.saving') : t('channels.useSelected', { n: selectedChats.length })}
      </button>
    {/if}
  </div>
</div>

<style>
  .picker-card {
    width: min(480px, 100%);
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  /* Embedded in settings: the section card already provides chrome —
     drop ours and fill the width. */
  .picker-card.bare {
    width: 100%;
    border: none;
    background: none;
    padding: 0;
    gap: 8px;
  }

  .picker-card.bare .list {
    max-height: 240px;
  }

  .brand {
    font-size: 26px;
    text-align: center;
  }

  .tagline {
    text-align: center;
    margin: 0;
    font-size: 13.5px;
  }

  .hint {
    font-size: 12.5px;
    margin: -4px 0 4px;
    text-align: center;
  }

  .list {
    max-height: 320px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }


  .create-row {
    display: flex;
    gap: 8px;
    margin-top: 4px;
  }

  .create-row .field {
    flex: 1;
    margin: 0;
  }
  .item {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 10px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: transparent;
    color: inherit;
    text-align: left;
    cursor: pointer;
    font-size: 14px;
    transition:
      border-color var(--dur-fast) var(--ease),
      background var(--dur) var(--ease),
      transform var(--dur-fast) var(--ease);
  }

  .item:hover {
    border-color: #39435c;
  }

  .item:active {
    transform: scale(0.99);
  }

  .item.picked {
    border-color: var(--primary, #4f8cff);
    background: color-mix(in srgb, var(--primary, #4f8cff) 12%, transparent);
  }

  .check {
    width: 16px;
    flex-shrink: 0;
  }

  /* The tick pops in on select; inline boxes ignore transform, so it
     needs its own block. */
  .check span {
    display: inline-block;
  }

  .key {
    margin-left: auto;
    font-size: 11.5px;
  }

  .saved-note {
    text-align: center;
    margin: 0;
  }

  .loading-row {
    display: flex;
    align-items: center;
    gap: 9px;
  }

  /* Reuses the global .spinner animation; only the scale changes so it
     sits inside a control instead of holding a whole screen. */
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

  .wire-warn {
    margin-top: 8px;
  }

  .wire-warn ul {
    list-style: none;
    padding: 0;
    margin: 4px 0 8px;
    font-size: 12.5px;
  }
</style>

