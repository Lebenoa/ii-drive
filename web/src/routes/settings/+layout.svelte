<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { fadeOnly, fadeUp, stagger } from '$lib/motion';
  import { fade } from 'svelte/transition';
  import { getToken, reloadConfig } from '$lib/api';
  import './settings.css';

  let { children } = $props();

  let checking = $state(true);

  $effect(() => {
    void (async () => {
      if (!getToken()) goto('/login');
      else checking = false;
    })();
  });

  const CATEGORIES = [
    { path: '/settings/telegram', label: 'Telegram' },
    { path: '/settings/upload', label: 'Uploads' },
  ];

  function isActive(path: string): boolean {
    return page.url.pathname === path;
  }

  // Server-side config.toml re-read. Runtime fields (upload cap, phone
  // allowlist, thumbnails) apply immediately; paths/credentials need a
  // restart, which the tooltip mentions.
  let reloading = $state(false);
  let reloadOk = $state(false);
  let reloadMsg = $state('');
  let reloadTimer: ReturnType<typeof setTimeout> | undefined;

  // The label is derived, so the {#key} block below only replays its fade
  // when the text genuinely changes — no redundant remounts, no blinking.
  let reloadLabel = $derived(
    reloading ? 'Reloading…' : (reloadMsg || '↻ Reload config')
  );

  async function doReload(): Promise<void> {
    if (reloading) return;
    clearTimeout(reloadTimer);
    reloading = true;
    try {
      await reloadConfig();
      reloadOk = true;
      reloadMsg = '✓ Config reloaded.';
    } catch (err) {
      reloadOk = false;
      reloadMsg = `⚠ ${err instanceof Error ? err.message : String(err)}`;
    } finally {
      reloading = false;
      // Success stays green and disabled for a beat; then everything
      // resets together so no stale state lingers.
      reloadTimer = setTimeout(() => {
        reloadMsg = '';
        reloadOk = false;
      }, 2500);
    }
  }
</script>

<div class="settings-shell">
  <header class="topbar">
    <a class="back" href="/">← Back to files</a>
    <span class="title">Settings</span>
    <button
      class="btn ghost busy-btn reload-btn"
      class:success={reloadOk && !reloading}
      type="button"
      title="Re-read config.toml — paths and credentials need a restart"
      disabled={reloading || reloadOk}
      onclick={() => void doReload()}
    >
      {#if reloading}<span class="spinner btn-spin"></span>{/if}
      {#key reloadLabel}
        <span in:fade>{reloadLabel}</span>
      {/key}
    </button>
  </header>

  {#if checking}
    <!-- No exit on the spinner: it fills the viewport, so fading it out
         while the sections mount below would double the page height. -->
    <div class="center-screen">
      <div class="spinner" aria-label="loading"></div>
    </div>
  {:else}
    <nav class="cats" aria-label="Settings categories">
      {#each CATEGORIES as cat, i (cat.path)}
        <a
          class="cat"
          class:active={isActive(cat.path)}
          href={cat.path}
          in:fadeUp={{ delay: stagger(i) }}
        >
          {cat.label}

        </a>
      {/each}
    </nav>
    {@render children()}
  {/if}
</div>

<style>
  /* Green success state while the "✓ Config reloaded." label is showing;
     text and border ease in and back out with the state. */
  .reload-btn {
      color: var(--dur, 180ms) var(--ease, ease-out),
      border-color: var(--dur, 180ms) var(--ease, ease-out);
  }

  .reload-btn.success {
    color: var(--ok, #2e9e5b);
    border-color: color-mix(in srgb, var(--ok, #2e9e5b) 55%, transparent);
  }
</style>
