<svelte:head>
  <title>ii-drive — {t('setup.title')}</title>
</svelte:head>

<script lang="ts">
  import { getSetupState, submitSetup } from '$lib/api';
  import { fadeUp } from '$lib/motion';
  import { t } from '$lib/i18n.svelte';

  /**
   * `absent` is the case where someone typed /setup at an already-configured
   * server: the endpoint only exists while the wizard is running, so a failed
   * probe is the answer rather than an error worth showing.
   */
  let phase = $state<'checking' | 'form' | 'done' | 'absent'>('checking');
  let configPath = $state('');

  let apiId = $state('');
  let apiHash = $state('');
  let phones = $state('');
  let busy = $state(false);
  let error = $state('');

  const idValid = $derived(/^\d+$/.test(apiId.trim()) && Number(apiId.trim()) > 0);
  const ready = $derived(idValid && apiHash.trim().length > 0 && phones.trim().length > 0);

  $effect(() => {
    void (async () => {
      try {
        configPath = (await getSetupState()).config_path;
        phase = 'form';
      } catch {
        phase = 'absent';
      }
    })();
  });

  async function submit(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    if (busy || !ready) return;
    busy = true;
    error = '';
    try {
      // The server writes the file and then exits, so this is the last
      // request the page will make.
      configPath = (await submitSetup({
        api_id: Number(apiId.trim()),
        api_hash: apiHash.trim(),
        phones,
      })).config_path;
      phase = 'done';
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      busy = false;
    }
  }
</script>

<div class="center-screen">
  {#if phase === 'checking'}
    <div class="spinner" aria-label={t('common.loading')}></div>
  {:else if phase === 'absent'}
    <div class="card setup-card" in:fadeUp>
      <h1 class="brand">ii-drive</h1>
      <p class="muted">{t('setup.alreadyConfigured')}</p>
      <a class="btn btn-primary submit" href="/">{t('setup.toDrive')}</a>
    </div>
  {:else if phase === 'done'}
    <div class="card setup-card" in:fadeUp>
      <h1 class="brand">✓ {t('setup.doneTitle')}</h1>
      <p class="muted">{t('setup.wrote')}</p>
      <code class="path">{configPath}</code>
      <p class="muted hint">{t('setup.restart')}</p>
    </div>
  {:else}
    <form class="card setup-card" onsubmit={submit} in:fadeUp>
      <h1 class="brand">ii-drive</h1>
      <p class="muted tagline">{t('setup.tagline')}</p>
      <a class="tg-link" href="https://my.telegram.org/apps" target="_blank" rel="noreferrer">
        {t('setup.openTelegram')}
      </a>
      <label class="lbl" for="api-id">{t('setup.apiId')}</label>
      <input
        id="api-id"
        class="field"
        type="text"
        inputmode="numeric"
        placeholder="1234567"
        bind:value={apiId}
        disabled={busy}
      />

      <label class="lbl" for="api-hash">{t('setup.apiHash')}</label>
      <input
        id="api-hash"
        class="field"
        type="text"
        spellcheck="false"
        autocomplete="off"
        placeholder="0123456789abcdef0123456789abcdef"
        bind:value={apiHash}
        disabled={busy}
      />

      <label class="lbl" for="phones">{t('setup.phones')}</label>
      <textarea
        id="phones"
        class="field phones"
        rows="3"
        placeholder="+1 555 123 4567"
        bind:value={phones}
        disabled={busy}
      ></textarea>
      <p class="muted hint">{t('setup.phonesHint')}</p>

      {#if error}<p class="error-text">{error}</p>{/if}

      <button class="btn btn-primary submit busy-btn" type="submit" disabled={busy || !ready}>
        {#if busy}<span class="spinner btn-spin"></span>{/if}
        {busy ? t('setup.writing') : t('setup.write')}
      </button>
      <p class="muted hint">
        {t('setup.credentialsHint')}
        <br />
        {t('setup.willWrite')} <code>{configPath}</code>
      </p>
    </form>
  {/if}
</div>

<style>
  .setup-card {
    width: min(420px, 92vw);
    display: flex;
    flex-direction: column;
    text-align: center;
  }

  .brand {
    margin: 0 0 4px;
  }

  .tagline {
    margin: 0 0 6px;
  }

  .tg-link {
    font-size: 13px;
    margin-bottom: 18px;
  }

  /* Labels read as a column with the fields, so they align left while the
     card's heading stays centred. */
  .lbl {
    text-align: left;
    margin-bottom: 4px;
  }

  .phones {
    resize: vertical;
    font: inherit;
  }

  .hint {
    text-align: left;
    margin: 4px 0 12px;
  }

  .path {
    display: block;
    margin: 6px 0 14px;
    word-break: break-all;
  }

  .submit {
    margin-top: 4px;
  }

  a.submit {
    text-decoration: none;
  }
</style>
