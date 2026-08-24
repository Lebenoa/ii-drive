<script lang="ts" module>
  import { getContext, setContext } from 'svelte';

  const SESSION = Symbol('settings-session');

  /** The signed-in account, as far as the settings pages care about it. */
  export interface SettingsSession {
    /** Operator-only endpoints answer 404 for everyone else, so the controls
     *  that call them must not render at all. */
    readonly admin: boolean;
  }

  /**
   * Read the session from a page rendered inside this layout, so the whole
   * settings section shares one /api/me round-trip.
   */
  export function settingsSession(): SettingsSession {
    return getContext<SettingsSession>(SESSION);
  }
</script>

<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { getMe, getToken } from '$lib/api';
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
  let admin = $state(false);

  setContext<SettingsSession>(SESSION, {
    get admin() {
      return admin;
    },
  });

  $effect(() => {
    void (async () => {
      if (!getToken()) {
        goto('/login');
        return;
      }
      try {
        admin = (await getMe()).admin;
      } catch {
        // Fail closed: a probe that did not answer must not unlock operator
        // controls. The pages below still render their non-admin content.
        admin = false;
      }
      checking = false;
    })();
  });
</script>

<div class="settings-shell">
  <header class="topbar">
    <a class="back" href="/">← Back to files</a>
    <span class="title">Settings</span>
    <!-- Always present so the sticky bar keeps three flex slots and the
         title does not jump when the operator button is absent. -->
    <div class="tools">
      {#if admin}
        <ReloadConfigButton />
      {/if}
    </div>
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
