<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { getToken } from '$lib/api';
  import { fadeUp, stagger } from '$lib/motion';
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
