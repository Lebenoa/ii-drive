<script lang="ts">
  import { fade } from 'svelte/transition';
  import { reloadConfig } from '$lib/api';

  let reloading = $state(false);
  let reloadOk = $state(false);
  let reloadMsg = $state('');
  let reloadTimer: ReturnType<typeof setTimeout> | undefined;

  // The label is derived, so the {#key} block below only replays its fade
  // when the text genuinely changes — no redundant remounts, no blinking.
  const label = $derived(reloading ? 'Reloading…' : (reloadMsg || '↻ Reload config'));

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

<button
  class="btn ghost busy-btn"
  class:success={reloadOk && !reloading}
  type="button"
  title="Re-read config.toml — paths and credentials need a restart"
  disabled={reloading || reloadOk}
  onclick={() => void doReload()}
>
  {#if reloading}<span class="spinner btn-spin"></span>{/if}
  {#key label}
    <span in:fade>{label}</span>
  {/key}
</button>

<style>
  button {
    transition:
      color var(--dur) var(--ease),
      border-color var(--dur) var(--ease);
  }

  /* Green success state while "✓ Config reloaded." is showing. */
  button.success {
    color: var(--ok, #2e9e5b);
    border-color: color-mix(in srgb, var(--ok, #2e9e5b) 55%, transparent);
  }
</style>
