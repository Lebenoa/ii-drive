<script lang="ts">
  import { goto } from '$app/navigation';
  import { getMe, getToken } from '$lib/api';
  import Login from '$lib/components/Login.svelte';

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
  <div class="center-screen">
    <div class="spinner" aria-label="loading"></div>
  </div>
{:else}
  <Login onSuccess={onSuccess} />
{/if}
