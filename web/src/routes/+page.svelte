<svelte:head>
  <title>ii-drive — {t('nav.files')}</title>
</svelte:head>

<script lang="ts">
  import { goto } from '$app/navigation';
  import { ApiError, clearToken, getMe, getToken, type Me } from '$lib/api';
  import Drive from '$lib/screens/DriveScreen.svelte';
  import { fadeOnly, fadeUp } from '$lib/motion';
  import { t } from '$lib/i18n.svelte';

  let me = $state<Me | null>(null);
  let booting = $state(true);
  let bootError = $state('');
  let retryTick = $state(0);

  // The web token is stateless and stays valid across server restarts.
  // It is only dropped when the server rejects it (401) or Telegram says
  // the session itself must be re-authorized — never for a transient
  // "still connecting" state, which used to force a log-in on every boot.
  $effect(() => {
    void retryTick;
    void (async () => {
      if (!getToken()) {
        goto('/login');
        return;
      }
      booting = true;
      bootError = '';
      try {
        const m = await getMe();
        if (m.relogin) {
          clearToken();
          goto('/login');
          return;
        }
        if (!m.authorized) {
          // Telegram not connected yet (e.g. right after a server restart)
          // — keep the token and let the user retry.
          bootError = m.error ?? t('boot.notConnected');
          return;
        }
        if (!m.channel_selected) {
          goto('/channels');
          return;
        }
        me = m;
      } catch (err) {
        if (err instanceof ApiError && err.status === 401) {
          clearToken();
          goto('/login');
          return;
        }
        bootError = err instanceof Error ? err.message : String(err);
      } finally {
        booting = false;
      }
    })();
  });

</script>

{#if booting}
  <!-- Delayed fade: a boot that resolves in <250ms never flashes a spinner. -->
  <div class="center-screen" in:fadeOnly={{ delay: 250 }}>
    <div class="spinner" aria-label={t('common.loading')}></div>
  </div>
{:else if bootError}
  <div class="center-screen boot-retry" in:fadeUp>
    <p class="error-text">{bootError}</p>
    <button class="btn btn-primary" type="button" onclick={() => retryTick++}>{t('common.retry')}</button>
  </div>
{:else if me}
  <!-- Opacity only: a transform here would become the containing block for
       the drive's position:fixed panels (uploads, mobile drawer) mid-fade. -->
  <div in:fadeOnly={{ duration: 220 }}>
    <Drive />
  </div>
{/if}

<style>
  .boot-retry {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
    margin: 40px auto;
    max-width: 420px;
    padding: 0 16px;
  }
</style>
