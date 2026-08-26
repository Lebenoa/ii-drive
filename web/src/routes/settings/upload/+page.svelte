<svelte:head>
  <title>ii-drive — {t('settings.cat.uploads')}</title>
</svelte:head>

<script lang="ts">
  import { sweepThumbs as apiSweepThumbs } from '$lib/api';
  import {
    getInstance,
    getRules,
    getSettings,
    listFolders,
    saveInstance,
    saveRules,
    saveSettings,
    type Folder,
    type Instance,
    type RouteRule,
  } from '$lib/api';
  import { collapse, fadeUp, stagger } from '$lib/motion';
  import { t } from '$lib/i18n.svelte';
  import { settingsSession } from '../+layout.svelte';

  const session = settingsSession();

  // --- Instance settings (operator-only) ---
  // The cap is stored in bytes but edited in a selectable unit: nobody
  // wants to type 2147483648, and the server accepts either.
  const UNITS = { MB: 1024 * 1024, GB: 1024 ** 3 } as const;
  type CapUnit = keyof typeof UNITS;
  let instance = $state<Instance | undefined>();
  let capValue = $state(0);
  let capUnit = $state<CapUnit>('MB');
  let instanceSaving = $state(false);
  let instanceMsg = $state('');
  let instanceError = $state('');

  async function loadInstance(): Promise<void> {
    if (!session.admin || instance) return;
    try {
      instance = await getInstance();
      capUnit = instance.max_file_size >= UNITS.GB ? 'GB' : 'MB';
      capValue = instance.max_file_size / UNITS[capUnit];
      sweepAt = instance.thumb_sweep_time;
      sweepHours = instance.thumb_sweep_hours;
    } catch (err) {
      // A 404 here means the account lost operator rights between the
      // /api/me probe and this call; the panel simply stays hidden.
      instanceError = err instanceof Error ? err.message : String(err);
    }
  }

  // A cap of 500 MB (decimal, as `max_file_size = "500MB"` once allowed)
  // displays as 477 MB, so re-sending the rounded field would quietly move
  // the stored value. Compare in bytes and send only when the operator
  // actually edited it — the endpoint keeps whatever a request leaves out.
  const capBytes = $derived(
    !!instance ? Math.floor(Math.max(0, Number(capValue) || 0) * UNITS[capUnit]) : 0,
  );
  const capEdited = $derived(!!instance && capBytes !== instance.max_file_size);

  let sweepAt = $state('00:00');
  let sweepHours = $state(24);

  async function saveInstanceSettings(): Promise<void> {
    if (instanceSaving || !instance) return;
    instanceSaving = true;
    instanceMsg = '';
    instanceError = '';
    try {
      instance = await saveInstance({
        ...(capEdited ? { max_file_size: Math.max(1, capBytes) } : {}),
        media_thumbs: instance.media_thumbs,
        thumb_sweep_time: /^\d{1,2}:\d{2}$/.test(sweepAt.trim()) ? sweepAt.trim() : '00:00',
        thumb_sweep_hours: Math.max(0, Math.min(168, Math.floor(Number(sweepHours) || 0))),
        upload_strategy: instance.upload_strategy,
      });
      capUnit = instance.max_file_size >= UNITS.GB ? 'GB' : 'MB';
      capValue = instance.max_file_size / UNITS[capUnit];
      sweepAt = instance.thumb_sweep_time;
      sweepHours = instance.thumb_sweep_hours;
      instanceMsg = t('common.saved');
    } catch (err) {
      instanceError = err instanceof Error ? err.message : String(err);
    } finally {
      instanceSaving = false;
    }
  }

  // --- Orphan thumbnail sweep (operator-only) ---
  let sweeping = $state(false);
  let sweepMsg = $state('');
  let sweepError = $state('');

  async function sweepThumbs(): Promise<void> {
    if (sweeping) return;
    sweeping = true;
    sweepMsg = '';
    sweepError = '';
    try {
      const { removed } = await apiSweepThumbs();
      sweepMsg = t('instance.swept', { count: removed, s: removed === 1 ? '' : 's' });
    } catch (err) {
      sweepError = err instanceof Error ? err.message : String(err);
    } finally {
      sweeping = false;
    }
  }

  // --- Split uploads ---
  let splitMb = $state(0);
  let splitLoaded = $state(false);
  let splitSaving = $state(false);
  let splitMsg = $state('');
  let splitError = $state('');

  const SPLIT_PRESETS = [0, 250, 500, 1024];

  function splitLabel(mb: number): string {
    return mb === 0 ? t('upload.off') : mb >= 1024 ? `${mb / 1024} GB` : `${mb} MB`;
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
      splitMsg = t('common.saved');
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
      rulesMsg = t('common.saved');
    } catch (err) {
      rulesError = err instanceof Error ? err.message : String(err);
    } finally {
      rulesSaving = false;
    }
  }

  // All panels are on screen together, so their loads run concurrently
  // instead of waiting on each other.
  $effect(() => {
    void Promise.all([loadSplit(), loadRouting(), loadInstance()]);
  });
</script>

<main class="content">
  {#if session.admin}
    <section class="card section" in:fadeUp={{ delay: stagger(0) }}>
      <h2>{t('instance.title')}</h2>
      <p class="muted hint">{t('instance.hint')}</p>
      {#if instance}
        <div class="split-row">
          <label class="cap-label" for="cap">{t('instance.cap')}</label>
          <input
            id="cap"
            class="field split-input"
            type="number"
            min="1"
            step="any"
            bind:value={capValue}
          />
          <select class="field split-input" bind:value={capUnit} aria-label="Unit">
            {#each Object.keys(UNITS) as u (u)}
              <option value={u}>{u}</option>
            {/each}
          </select>
        </div>
        <label class="switch-row">
          <input type="checkbox" bind:checked={instance.media_thumbs} />
          <span>{t('instance.thumbs')}</span>
        </label>
        <div class="split-row">
          <label class="cap-label" for="sweeptime">{t('instance.sweepAt')}</label>
          <input
            id="sweeptime"
            class="field split-input"
            type="time"
            bind:value={sweepAt}
          />
          <label class="cap-label" for="sweehours">{t('instance.sweepEvery')}</label>
          <input
            id="sweehours"
            class="field split-input"
            type="number"
            min="0"
            max="168"
            bind:value={sweepHours}
          />
          <span class="muted">{t('instance.sweepUnit')}</span>
        </div>
        <div class="split-row">
          <button
            class="btn ghost busy-btn"
            type="button"
            disabled={sweeping}
            onclick={() => void sweepThumbs()}
          >
            {#if sweeping}<span class="spinner btn-spin"></span>{/if}
            {t('instance.sweepThumbs')}
          </button>
          {#if sweepMsg}<span class="ok-text">{sweepMsg}</span>{/if}
        </div>
        {#if sweepError}<p class="error-text">{sweepError}</p>{/if}
        <div class="split-row">
          <label class="cap-label" for="strategy">{t('instance.strategy')}</label>
          <select id="strategy" class="field" bind:value={instance.upload_strategy}>
            <option value="stream">{t('instance.strategyStream')}</option>
            <option value="spill">{t('instance.strategySpill')}</option>
          </select>
          <button
            class="btn btn-primary busy-btn"
            type="button"
            disabled={instanceSaving}
            onclick={() => void saveInstanceSettings()}
          >
            {#if instanceSaving}<span class="spinner btn-spin"></span>{/if}
            {instanceSaving ? t('common.saving') : t('common.save')}
          </button>
        </div>
        {#if instanceMsg}<p class="ok-text" transition:fadeUp>{instanceMsg}</p>{/if}
      {:else if !instanceError}
        <p class="muted">{t('common.loading')}</p>
      {/if}
      {#if instanceError}<p class="error-text">{instanceError}</p>{/if}
    </section>
  {/if}

  <section class="card section" in:fadeUp={{ delay: stagger(session.admin ? 1 : 0) }}>
    <h2>{t('upload.splitTitle')}</h2>
    <p class="muted hint">{t('upload.splitHint')}</p>
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
          {splitSaving ? t('common.saving') : t('common.save')}
        </button>
      </div>
      <p class="muted hint">{t('upload.currentThreshold', { size: splitLabel(splitMb) })}</p>
      {#if splitMsg}<p class="ok-text" transition:fadeUp>{splitMsg}</p>{/if}
      {#if splitError}<p class="error-text">{splitError}</p>{/if}
    {:else}
      <p class="muted">{t('common.loading')}</p>
    {/if}
  </section>

  <section class="card section" in:fadeUp={{ delay: stagger(session.admin ? 2 : 1) }}>
    <h2>{t('upload.routingTitle')}</h2>
    <p class="muted hint">{t('upload.routingHint')}</p>
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
              <button class="icon-btn danger" type="button" title={t('upload.removeRule')} onclick={() => removeRule(i)}>
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
        <p class="muted">{t('upload.noRules')}</p>
      {/if}
      <div class="rule-actions">
        <button class="btn ghost" type="button" onclick={addRule} disabled={folders.length === 0}>
          + {t('upload.addRule')}
        </button>
        <button
          class="btn btn-primary busy-btn"
          type="button"
          disabled={rulesSaving || folders.length === 0}
          onclick={() => void saveRouting()}
        >
          {#if rulesSaving}<span class="spinner btn-spin"></span>{/if}
          {rulesSaving ? t('common.saving') : t('common.save')}
        </button>
      </div>
      {#if folders.length === 0}
        <p class="muted hint">{t('upload.createFolderFirst')}</p>
      {/if}
      {#if rulesMsg}<p class="ok-text" transition:fadeUp>{rulesMsg}</p>{/if}
      {#if rulesError}<p class="error-text">{rulesError}</p>{/if}
    {:else}
      <p class="muted">{t('common.loading')}</p>
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

  .cap-label {
    font-size: 14px;
    min-width: 5.5rem;
  }

  /* Mirrors the toggle row on the Other page so the two operator panels
     line up rather than each inventing a spacing. */
  .switch-row {
    display: flex;
    align-items: center;
    gap: 10px;
    cursor: pointer;
    font-size: 14px;
    margin-bottom: 6px;
  }

  .switch-row input {
    width: 16px;
    height: 16px;
    accent-color: var(--accent, inherit);
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
