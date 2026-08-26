<script lang="ts">
  /**
   * Floating upload-progress panel, mounted in the root layout so it
   * stays on screen across navigation. Reads the global queue store;
   * all state changes come from `uploads.svelte.ts`.
   */
  import { flip } from 'svelte/animate';
  import { collapse, fadeUp, pop, stagger, flipDur } from '$lib/motion';
  import { t } from '$lib/i18n.svelte';
  import {
    cancelUpload,
    clearFinished,
    fmtSpeed,
    panelCollapsed,
    queue,
  } from '$lib/uploads.svelte';

  const activeCount = $derived(
    queue.filter((i) => i.state === 'uploading' || i.state === 'pending').length,
  );
</script>

{#if queue.length > 0}
  <div class="uploads-panel" transition:fadeUp={{ y: 10 }}>
    <button
      class="up-head"
      type="button"
      onclick={() => (panelCollapsed.value = !panelCollapsed.value)}
      aria-expanded={!panelCollapsed.value}
    >
      <span class="up-title">
        {#if activeCount > 0}
          {t('drive.uploadingN', { n: activeCount, total: queue.length })}
        {:else}
          {t('drive.uploads', { n: queue.length })}
        {/if}
      </span>
      {#if queue.every((i) => i.state === 'done' || i.state === 'error')}
        <span
          class="up-clear"
          role="button"
          tabindex="-1"
          title={t('drive.clearFinished')}
          onclick={(e) => {
            e.stopPropagation();
            clearFinished();
          }}
          onkeydown={(e) => e.stopPropagation()}
        >
          ✕
        </span>
      {:else}
        <span class="up-chevron">{panelCollapsed.value ? '▲' : '▼'}</span>
      {/if}
    </button>
    {#if !panelCollapsed.value}
      <ul class="queue" transition:collapse>
        {#each queue as item, i (item.key)}
          <li
            class="q-item"
            class:error={item.state === 'error'}
            animate:flip={{ duration: flipDur() }}
            transition:fadeUp={{ delay: stagger(i, 12, 120) }}
          >
            <div class="q-top">
              <span class="q-name" title={item.name}>{item.name}</span>
              <span class="q-state" class:ok={item.state === 'done'}>
                {#if item.state === 'pending'}
                  {t('drive.queued')}
                {:else if item.state === 'uploading'}
                  {item.progress}%
                  {#if item.speed > 0}
                    <span class="q-speed">{fmtSpeed(item.speed)}</span>
                  {/if}
                {:else if item.state === 'cancelled'}
                  {t('drive.cancelled')}
                {:else if item.state === 'done'}
                  <span class="q-tick" in:pop>✓</span>
                {:else}
                  ✗
                {/if}
              </span>
              {#if item.state === 'uploading'}
                <button
                  class="q-cancel"
                  type="button"
                  title={t('drive.cancelUpload')}
                  aria-label={t('drive.cancelUpload')}
                  onclick={() => cancelUpload(item.key)}
                >✕</button>
              {/if}
            </div>
            <div class="bar">
              <div
                class="fill"
                class:err={item.state === 'error'}
                class:done={item.state === 'done'}
                style={`width:${item.progress}%`}
              ></div>
            </div>
            {#if item.state === 'error'}
              <p class="error-text">{item.error}</p>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </div>
{/if}

<style>
  .uploads-panel {
    position: fixed;
    right: 20px;
    bottom: 20px;
    width: min(340px, calc(100vw - 40px));
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--panel);
    box-shadow: 0 12px 36px rgba(0, 0, 0, 0.45);
    z-index: 30;
    overflow: hidden;
  }
  .up-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    width: 100%;
    border: none;
    background: var(--panel-2);
    color: var(--text);
    font: inherit;
    font-size: 13.5px;
    text-align: left;
    padding: 10px 12px;
    cursor: pointer;
  }
  .up-title {
    font-weight: 600;
  }
  .up-clear,
  .up-chevron {
    color: var(--muted);
    transition: color var(--dur-fast) var(--ease);
  }
  .up-clear:hover {
    color: var(--danger);
  }
  .queue {
    list-style: none;
    margin: 0;
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    max-height: 260px;
    overflow-y: auto;
  }
  .q-item {
    background: var(--panel-2);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 8px 12px;
  }
  .q-item.error {
    border-color: #5b2730;
  }
  .q-top {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    font-size: 13px;
    margin-bottom: 6px;
  }
  .q-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .q-state {
    color: var(--muted);
    flex-shrink: 0;
    transition: color var(--dur) var(--ease);
  }
  .q-state.ok {
    color: var(--ok);
  }
  /* `pop` scales, which a bare inline box would ignore. */
  .q-tick {
    display: inline-block;
  }
  .q-speed {
    margin-left: 6px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }
  .q-cancel {
    flex-shrink: 0;
    border: 0;
    background: transparent;
    color: var(--muted);
    cursor: pointer;
    padding: 2px 6px;
    border-radius: 4px;
    line-height: 1;
  }
  .q-cancel:hover {
    color: var(--danger, #e5484d);
    background: rgba(229, 72, 77, 0.12);
  }
  .bar {
    height: 4px;
    border-radius: 2px;
    background: #10141c;
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--accent);
    border-radius: 2px;
    transition:
      width var(--dur) var(--ease-out),
      background var(--dur) var(--ease);
  }
  .fill.done {
    background: var(--ok);
  }
  .fill.err {
    background: var(--danger);
  }
  @media (max-width: 720px) {
    .uploads-panel {
      bottom: 86px;
    }
  }
</style>
