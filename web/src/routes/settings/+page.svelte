<script lang="ts">
  import { goto } from '$app/navigation';
  import {
    addBot,
    getBots,
    getRules,
    getSettings,
    getToken,
    listFolders,
    removeBot,
    saveRules,
    saveSettings,
    type BotAddResult,
    type BotEntry,
    type Folder,
    type RouteRule,
  } from '$lib/api';
  import Channels from '$lib/components/ChannelPicker.svelte';
  import { collapse, fadeUp, stagger } from '$lib/motion';

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

  // Auto-upload routing rules (mime prefix -> folder), ordered. `uid` is
  // client-only and never sent: keyed rows need an identity that survives
  // the save round-trip, and index keys would animate the wrong row out.
  type UiRule = RouteRule & { uid: number };
  let ruleSeq = 0;
  let rules = $state<UiRule[]>([]);
  let folders = $state<Folder[]>([]);
  let rulesLoaded = $state(false);
  let rulesSaving = $state(false);
  let rulesMsg = $state('');
  let rulesError = $state('');

  const MIME_PRESETS = ['image/', 'video/', 'audio/', 'application/pdf', 'text/'];

  function addRule(): void {
    rules = [...rules, { uid: ruleSeq++, mime: 'image/', folder: folders[0]?.uid ?? '' }];
    rulesMsg = '';
    rulesError = '';
  }

  function removeRule(i: number): void {
    rules = rules.filter((_, idx) => idx !== i);
  }

  function folderName(uid: string): string {
    return folders.find((f) => f.uid === uid)?.name ?? uid;
  }

  async function saveRouting(): Promise<void> {
    if (rulesSaving) return;
    rulesSaving = true;
    rulesMsg = '';
    rulesError = '';
    try {
      const kept = rules
        .map((r) => ({ ...r, mime: r.mime.trim() }))
        .filter((r) => r.mime !== '' && r.folder !== '');
      await saveRules(kept.map(({ mime, folder }) => ({ mime, folder })));
      rules = kept;
      rulesMsg = 'Saved.';
    } catch (err) {
      rulesError = err instanceof Error ? err.message : String(err);
    } finally {
      rulesSaving = false;
    }
  }

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
        const [loadedRules, loadedFolders] = await Promise.all([getRules(), listFolders()]);
        rules = loadedRules.map((r) => ({ ...r, uid: ruleSeq++ }));
        folders = loadedFolders;
        rulesLoaded = true;
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
    <!-- No exit on the spinner: it fills the viewport, so fading it out
         while the sections mount below would double the page height. -->
    <div class="center-screen">
      <div class="spinner" aria-label="loading"></div>
    </div>
  {:else}
    <main class="content">
      <section class="card section" in:fadeUp={{ delay: stagger(0) }}>
        <h2>Storage channels</h2>
        <p class="muted hint">
          Uploads are spread across the selected channels. You can also create a
          brand-new channel here.
        </p>
        <Channels onDone={null} redirectOnSave={false} embedded />
      </section>

      <section class="card section" in:fadeUp={{ delay: stagger(1) }}>
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
              class="btn btn-primary busy-btn"
              type="button"
              disabled={splitSaving}
              onclick={() => void saveSplit()}
            >
              {#if splitSaving}<span class="spinner btn-spin"></span>{/if}
              {splitSaving ? 'Saving…' : 'Save'}
            </button>
          </div>
          <p class="muted hint">
            0 (Off) uploads every file as a single message. Already-uploaded
            files keep their current layout; the threshold only affects new
            uploads.
          </p>
          {#if splitMsg}<p class="ok-text" transition:fadeUp>{splitMsg}</p>{/if}
          {#if splitError}<p class="error-text">{splitError}</p>{/if}
        {:else}
          <p class="muted">Loading…</p>
        {/if}
      </section>

      <section class="card section" in:fadeUp={{ delay: stagger(2) }}>
        <h2>Auto-upload routing</h2>
        <p class="muted hint">
          Files uploaded to the root folder are sorted automatically: the
          first rule whose type prefix matches claims the file. Explicit
          folder picks in the drive always win over rules.
        </p>
        {#if rulesLoaded}
          {#if rules.length > 0}
            <ul class="rule-list">
              {#each rules as rule, i (rule.uid)}
                <li class="rule-row" in:fadeUp={{ delay: stagger(i) }} out:collapse>
                  <input
                    class="field rule-mime"
                    list="mime-presets"
                    placeholder="image/"
                    bind:value={rule.mime}
                  />
                  <span class="muted">→</span>
                  <select class="field rule-folder" bind:value={rule.folder}>
                    {#each folders as f (f.uid)}
                      <option value={f.uid}>{f.name}</option>
                    {/each}
                  </select>
                  <button class="icon-btn danger" type="button" title="Remove rule" onclick={() => removeRule(i)}>
                    ✕
                  </button>
                </li>
              {/each}
            </ul>
            <datalist id="mime-presets">
              {#each MIME_PRESETS as p (p)}
                <option value={p}></option>
              {/each}
            </datalist>
          {:else}
            <p class="muted">No rules — uploads to the root stay in the root.</p>
          {/if}
          <div class="rule-actions">
            <button class="btn ghost" type="button" onclick={addRule} disabled={folders.length === 0}>
              + Add rule
            </button>
            <button
              class="btn btn-primary busy-btn"
              type="button"
              disabled={rulesSaving || folders.length === 0}
              onclick={() => void saveRouting()}
            >
              {#if rulesSaving}<span class="spinner btn-spin"></span>{/if}
              {rulesSaving ? 'Saving…' : 'Save'}
            </button>
          </div>
          {#if folders.length === 0}
            <p class="muted hint">Create a folder in the drive first.</p>
          {/if}
          {#if rulesMsg}<p class="ok-text" transition:fadeUp>{rulesMsg}</p>{/if}
          {#if rulesError}<p class="error-text">{rulesError}</p>{/if}
        {:else}
          <p class="muted">Loading…</p>
        {/if}
      </section>

      <section class="card section" in:fadeUp={{ delay: stagger(3) }}>
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
    transition: color var(--dur-fast) var(--ease);
  }

  .back:hover {
    color: inherit;
  }

  .title {
    font-weight: 700;
  }

  .content {
    width: min(720px, 100%);
    margin: 16px auto;
    padding: 0 16px;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  /* Cards here carry mostly single controls — tighten the generous
     global card padding instead of shipping empty margins. */
  .section {
    padding: 16px 18px;
  }

  .section h2 {
    margin: 0 0 4px;
    font-size: 16px;
  }

  .hint {
    font-size: 12.5px;
    margin: 0 0 10px;
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
    margin-bottom: 6px;
  }

  .split-input {
    width: 90px;
    margin: 0;
  }

  .preset.active {
    border-color: var(--accent, inherit);
    font-weight: 600;
  }

  .rule-list {
    list-style: none;
    padding: 0;
    margin: 0 0 8px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .rule-row {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .rule-mime {
    flex: 1;
    margin: 0;
  }

  .rule-folder {
    flex: 1;
    margin: 0;
  }

  .rule-actions {
    display: flex;
    gap: 8px;
    margin-top: 4px;
  }

  .ok-text {
    color: var(--ok, #2e9e5b);
    font-size: 12.5px;
    margin: 4px 0 0;
  }

  .busy-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 7px;
  }

  /* Reuses the global .spinner animation at control scale. */
  .btn-spin {
    width: 13px;
    height: 13px;
    border-width: 2px;
    border-color: color-mix(in srgb, currentColor 30%, transparent);
    border-top-color: currentColor;
    flex-shrink: 0;
  }
</style>
