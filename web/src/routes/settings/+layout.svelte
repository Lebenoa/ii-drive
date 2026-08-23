<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { getToken } from '$lib/api';
  import ReloadConfigButton from '$lib/components/ReloadConfigButton.svelte';
  import { fadeUp, stagger } from '$lib/motion';
  import './settings.css';

  let { children } = $props();

  const CATEGORIES = [
    { path: '/settings/telegram', label: 'Telegram' },
    { path: '/settings/upload', label: 'Uploads' },
    { path: '/settings/other', label: 'Other' },
  ];

  let checking = $state(true);

  $effect(() => {
    void (async () => {
      if (!getToken()) goto('/login');
      else checking = false;
    })();
  });
</script>

<div class="settings-shell">
  <header class="topbar">
    <a class="back" href="/">← Back to files</a>
    <span class="title">Settings</span>
    <ReloadConfigButton />
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
          class:active={page.url.pathname === cat.path}
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
