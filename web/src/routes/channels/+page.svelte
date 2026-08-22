<script lang="ts">
  import { goto } from '$app/navigation';
  import { getMe, getToken } from '$lib/api';
  import Channels from '$lib/components/ChannelPicker.svelte';

  let checking = $state(true);

  $effect(() => {
    void (async () => {
      if (!getToken()) {
        goto('/login');
        return;
      }
      try {
        const me = await getMe();
        if (me.relogin || !me.authorized) {
          goto('/login');
          return;
        }
        if (me.channel_selected) {
          goto('/');
          return;
        }
      } catch {
        goto('/login');
        return;
      }
      checking = false;
    })();
  });

  function onDone(): void {
    goto('/');
  }
</script>

{#if checking}
  <div class="center-screen">
    <div class="spinner" aria-label="loading"></div>
  </div>
{:else}
  <Channels onDone={onDone} />
{/if}
