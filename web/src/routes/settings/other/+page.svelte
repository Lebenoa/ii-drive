<svelte:head>
  <title>ii-drive — {t('settings.cat.other')}</title>
</svelte:head>

<script lang="ts">
  import { fadeUp, stagger } from '$lib/motion';
  import { settingsSession } from '../+layout.svelte';
  import {
    currentLocale,
    listLocales,
    setLocale,
    t,
    type LocaleMeta,
  } from '$lib/i18n.svelte';

  const session = settingsSession();
  const admin = $derived(session.admin);

  const DEV_KEY = 'ii_dev_mode';
  let devMode = $state(localStorage.getItem(DEV_KEY) === '1');

  // bind:checked flips the flag itself; persisting reactively avoids a
  // second flip in an onchange handler (which made the box appear dead).
  $effect(() => {
    localStorage.setItem(DEV_KEY, devMode ? '1' : '0');
  });

  // Languages the server currently offers; picked one downloads on change.
  let languages = $state<LocaleMeta[]>([]);
  $effect(() => {
    void listLocales().then((ls) => (languages = ls));
  });

  let switching = $state(false);
  async function onPick(e: Event): Promise<void> {
    const code = (e.currentTarget as HTMLSelectElement).value;
    switching = true;
    try {
      await setLocale(code);
    } finally {
      switching = false;
    }
  }
</script>

<main class="content">
  <section class="card section" in:fadeUp={{ delay: stagger(0) }}>
    <h2>{t('other.language')}</h2>
    <p class="muted hint">{t('other.languageHint')}</p>
    <label class="switch-row">
      <select
        class="field lang-select"
        value={currentLocale()}
        disabled={switching}
        onchange={onPick}
      >
        {#if !languages.some((l) => l.code === currentLocale())}
          <option value={currentLocale()}>{currentLocale()}</option>
        {/if}
        {#each languages as l (l.code)}
          <option value={l.code}>{l.name}</option>
        {/each}
      </select>
      {#if switching}<span class="spinner btn-spin"></span>{/if}
    </label>
  </section>

  <section class="card section" in:fadeUp={{ delay: stagger(1) }}>
    {#if admin}
      <h2>{t('other.devMode')}</h2>
      <p class="muted hint">{t('other.devModeHint')}</p>
      <label class="switch-row">
        <input type="checkbox" bind:checked={devMode} />
        <span>{devMode ? t('other.enabled') : t('other.disabled')}</span>
      </label>

      {#if devMode}
        <a
          class="card dev-link"
          href="/internal-db"
          in:fadeUp={{ delay: stagger(2) }}
        >
          <span>🗄 {t('other.internalDb')}</span>
          <span class="muted">{t('other.internalDbHint')}</span>
        </a>
      {/if}
    {:else}
      <!-- Developer mode only ever unlocked the internal-DB browser, whose
           endpoints are operator-only and answer 404 for everyone else, so
           the toggle here would just reveal a dead link. -->
      <h2>{t('other.internalDbTitle')}</h2>
      <p class="muted hint">{t('other.internalDbReserved')}</p>
    {/if}
  </section>
</main>

<style>
  .lang-select {
    width: min(240px, 100%);
  }

  .btn-spin {
    width: 14px;
    height: 14px;
    border-width: 2px;
    border-color: color-mix(in srgb, currentColor 30%, transparent);
    border-top-color: currentColor;
  }

  .switch-row {
    display: flex;
    align-items: center;
    gap: 10px;
    cursor: pointer;
    font-size: 14px;
  }

  .switch-row input {
    width: 18px;
    height: 18px;
    accent-color: var(--accent, currentColor);
    cursor: pointer;
  }

  .dev-link {
    display: flex;
    flex-direction: column;
    gap: 2px;
    margin-top: 12px;
    padding: 12px 14px;
    border: 1px solid var(--border);
    border-radius: 8px;
    color: inherit;
    text-decoration: none;
    transition:
      border-color var(--dur-fast) var(--ease),
      background var(--dur-fast) var(--ease);
  }

  .dev-link:hover {
    border-color: var(--accent, currentColor);
  }
</style>
