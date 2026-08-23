<svelte:head>
  <title>ii-drive — Upload settings</title>
</svelte:head>

<script lang="ts">
  import {
    getRules,
    getSettings,
    listFolders,
    saveRules,
    saveSettings,
    type Folder,
    type RouteRule,
  } from '$lib/api';
  import { collapse, fadeUp, stagger } from '$lib/motion';

  // --- Split uploads ---
  let splitMb = $state(0);
  let splitLoaded = $state(false);
  let splitSaving = $state(false);
  let splitMsg = $state('');
  let splitError = $state('');

  const SPLIT_PRESETS = [0, 250, 500, 1024];

  function splitLabel(mb: number): string {
    return mb === 0 ? 'Off' : mb >= 1024 ? `${mb / 1024} GB` : `${mb} MB`;
  }

  async function loadSplit(): Promise<void> {
    if (splitLoaded) return;
    const s = await getSettings();
    splitMb = s.split_mb;
    splitLoaded = true;
  }

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

  // --- Auto-upload routing ---
  // Rules (mime prefix -> folder), ordered. `uid` is client-only and never
  // sent: keyed rows need an identity that survives the save round-trip,
  // and index keys would animate the wrong row out.
  type UiRule = RouteRule & { uid: number };
  let ruleSeq = 0;
  let rules = $state<UiRule[]>([]);
  let folders = $state<Folder[]>([]);
  let rulesLoaded = $state(false);
  let rulesSaving = $state(false);
  let rulesMsg = $state('');
  let rulesError = $state('');

  const MIME_PRESETS = ['image/', 'video/', 'audio/', 'application/pdf', 'text/'];

  async function loadRouting(): Promise<void> {
    if (rulesLoaded) return;
    const [loadedRules, loadedFolders] = await Promise.all([getRules(), listFolders()]);
    rules = loadedRules.map((r) => ({ ...r, uid: ruleSeq++ }));
    folders = loadedFolders;
    rulesLoaded = true;
  }

  function addRule(): void {
    rules = [...rules, { uid: ruleSeq++, mime: 'image/', folder: folders[0]?.uid ?? '' }];
    rulesMsg = '';
    rulesError = '';
  }

  function removeRule(i: number): void {
    rules = rules.filter((_, idx) => idx !== i);
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

  // Both panels are on screen together, so their loads run concurrently
  // instead of waiting on each other.
  $effect(() => {
    void Promise.all([loadSplit(), loadRouting()]);
  });
</script>

<main class="content">
  <section class="card section" in:fadeUp={{ delay: stagger(0) }}>
    <h2>Split uploads</h2>
    <p class="muted hint">
      Files larger than this threshold are cut into parts of that size and
      uploaded in parallel — each part on a different bot's connection when
      you have a pool, so big files finish much faster. 0 disables
      splitting.
    </p>
    {#if splitLoaded}
      <div class="split-row">
        {#each SPLIT_PRESETS as p (p)}
          <button
            class="btn ghost preset"
            class:active={splitMb === p}
            type="button"
            onclick={() => (splitMb = p)}
          >
            {splitLabel(p)}
          </button>
        {/each}
        <input
          class="field split-input"
          type="number"
          min="0"
          bind:value={splitMb}
        />
        <span class="muted">MB</span>
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
        Current threshold: {splitLabel(splitMb)} — parts upload in parallel,
        so more bots means faster large uploads.
      </p>
      {#if splitMsg}<p class="ok-text" transition:fadeUp>{splitMsg}</p>{/if}
      {#if splitError}<p class="error-text">{splitError}</p>{/if}
    {:else}
      <p class="muted">Loading…</p>
    {/if}
  </section>

  <section class="card section" in:fadeUp={{ delay: stagger(1) }}>
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
</main>

<style>
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
</style>
