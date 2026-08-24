<svelte:head>
  <title>ii-drive — {t('nav.signIn')}</title>
</svelte:head>

<script lang="ts">
  import { goto } from '$app/navigation';
  import { getMe, getToken } from '$lib/api';
  import Login from '$lib/components/Login.svelte';
  import { fadeUp } from '$lib/motion';
  import { t } from '$lib/i18n.svelte';

  let checking = $state(true);

  $effect(() => {
    void (async () => {
      // Already signed in (and set up)? Skip the form.
      if (getToken()) {
        try {
          const me = await getMe();
          if (me.authorized && !me.relogin) {
            goto(me.channel_selected ? '/' : '/channels');
            return;
          }
        } catch {
          // stale/invalid token — fall through to the form
        }
      }
      checking = false;
    })();
  });

  function onSuccess(): void {
    // Root decides whether channels still need picking.
    goto('/');
  }
</script>

{#if checking}
  <!-- No exit on the spinner: it fills the viewport, so fading it out
       while the form mounts below would double the page height. -->
  <div class="center-screen">
    <div class="spinner" aria-label="loading"></div>
  </div>
{:else}
  <div in:fadeUp>
    <Login onSuccess={onSuccess} />
  </div>
{/if}
