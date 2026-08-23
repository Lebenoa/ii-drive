<svelte:head>
  <title>ii-drive — Other settings</title>
</svelte:head>

<script lang="ts">
  import { fadeUp, stagger } from '$lib/motion';

  const DEV_KEY = 'ii_dev_mode';
  let devMode = $state(localStorage.getItem(DEV_KEY) === '1');

  // bind:checked flips the flag itself; persisting reactively avoids a
  // second flip in an onchange handler (which made the box appear dead).
  $effect(() => {
    localStorage.setItem(DEV_KEY, devMode ? '1' : '0');
  });
</script>

<main class="content">
  <section class="card section" in:fadeUp={{ delay: stagger(0) }}>
    <h2>Developer mode</h2>
    <p class="muted hint">
      Exposes internal tooling — including a direct view of the embedded
      database. Only enable this if you know what you are doing.
    </p>
    <label class="switch-row">
      <input type="checkbox" bind:checked={devMode} />
      <span>{devMode ? 'Enabled' : 'Disabled'}</span>
    </label>

    {#if devMode}
      <a
        class="card dev-link"
        href="/internal-db"
        in:fadeUp={{ delay: stagger(1) }}
      >
        <span>🗄 Internal DB</span>
        <span class="muted">browse tables & run SurrealQL →</span>
      </a>
    {/if}
  </section>
</main>

<style>
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
