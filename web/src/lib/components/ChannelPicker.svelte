<script lang="ts">
  import { createChannel, fetchChannels, saveChannels, type ChannelInfo } from '$lib/api';

  let {
    onDone = null,
    redirectOnSave = true,
  }: { onDone?: (() => void) | null; redirectOnSave?: boolean } = $props();

  let available = $state<ChannelInfo[]>([]);
  let selectedChats = $state<string[]>([]);
  let loading = $state(true);
  let saving = $state(false);
  let error = $state('');
  let newTitle = $state('');
  let creating = $state(false);
  let saved = $state(false);

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
    const chosen = selectedChats
      .map((chat) => available.find((a) => a.chat === chat))
      .filter((a): a is ChannelInfo => a !== undefined);
    try {
      await saveChannels(chosen);
      saved = true;
      if (redirectOnSave && onDone) onDone();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      saving = false;
    }
  }
</script>

<div class="center-screen">
  <div class="card picker-card">
    <h1 class="brand">ii-drive</h1>
    <p class="muted tagline">Choose where your files are stored</p>
    <p class="muted hint">Pick one or more channels. Uploads are spread across them.</p>

    {#if loading}
      <p class="muted">Loading your chats…</p>
    {:else}
      <div class="list" role="listbox" aria-label="Storage channels">
        {#each available as c (c.chat)}
          <button
            type="button"
            class="item"
            class:picked={selectedChats.includes(c.chat)}
            onclick={() => toggle(c.chat)}
          >
            <span class="check">{selectedChats.includes(c.chat) ? '✓' : ''}</span>
            {c.title}
            <span class="muted key">{c.chat}</span>
          </button>
        {:else}
          <p class="muted">No channels found — add this account to a channel first.</p>
        {/each}
      </div>

      <form
        class="create-row"
        onsubmit={(e) => {
          e.preventDefault();
          void create();
        }}
      >
        <input
          class="field"
          type="text"
          placeholder="Or create a new channel…"
          bind:value={newTitle}
          maxlength={128}
          disabled={creating}
        />
        <button class="btn" type="submit" disabled={creating || newTitle.trim().length === 0}>
          {creating ? 'Creating…' : 'Create'}
        </button>
      </form>

      {#if error}<p class="error-text">{error}</p>{/if}
      {#if saved && !redirectOnSave}<p class="muted saved-note">Saved.</p>{/if}

      <button
        class="btn btn-primary"
        type="button"
        disabled={saving || selectedChats.length === 0}
        onclick={save}
      >
        {saving ? 'Saving…' : `Use ${selectedChats.length || ''} selected`}
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
  }

  .item.picked {
    border-color: var(--primary, #4f8cff);
    background: color-mix(in srgb, var(--primary, #4f8cff) 12%, transparent);
  }

  .check {
    width: 16px;
    flex-shrink: 0;
  }

  .key {
    margin-left: auto;
    font-size: 11.5px;
  }

  .saved-note {
    text-align: center;
    margin: 0;
  }
</style>
