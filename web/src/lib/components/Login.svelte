<script lang="ts">
  import { sendLoginPhone, sendLoginCode, sendLoginPassword, setToken } from '$lib/api';

  let { onSuccess }: { onSuccess: () => void } = $props();

  let step: 'phone' | 'code' | 'password' = $state('phone');
  let phone = $state('');
  let code = $state('');
  let tgPassword = $state('');
  let hint = $state('');
  let busy = $state(false);
  let error = $state('');

  function finish(token: string): void {
    setToken(token);
    onSuccess();
  }

  async function submitPhone(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    if (busy || phone.trim().length === 0) return;
    busy = true;
    error = '';
    try {
      await sendLoginPhone(phone.trim());
      step = 'code';
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      busy = false;
    }
  }

  async function submitCode(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    if (busy || code.trim().length === 0) return;
    busy = true;
    error = '';
    try {
      const res = await sendLoginCode(code.trim());
      if (res.status === 'password_required') {
        hint = res.hint ?? '';
        step = 'password';
      } else {
        finish(res.token);
      }
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      busy = false;
    }
  }

  async function submitPassword(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    if (busy || tgPassword.length === 0) return;
    busy = true;
    error = '';
    try {
      const res = await sendLoginPassword(tgPassword);
      finish(res.token);
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      busy = false;
    }
  }
</script>

<div class="center-screen">
  <form
    class="card login-card"
    onsubmit={step === 'phone' ? submitPhone : step === 'code' ? submitCode : submitPassword}
  >
    <h1 class="brand">ii-drive</h1>
    <p class="muted tagline">Sign in with your Telegram account</p>

    {#if step === 'phone'}
      <label class="lbl" for="phone">Phone number (with country code)</label>
      <input
        id="phone"
        class="field"
        type="tel"
        placeholder="+15551234567"
        autocomplete="tel"
        bind:value={phone}
        disabled={busy}
      />
    {:else if step === 'code'}
      <label class="lbl" for="code">Confirmation code sent to Telegram</label>
      <input
        id="code"
        class="field"
        type="text"
        inputmode="numeric"
        placeholder="12345"
        autocomplete="one-time-code"
        bind:value={code}
        disabled={busy}
      />
    {:else}
      <label class="lbl" for="tgpw">Two-factor password</label>
      <input
        id="tgpw"
        class="field"
        type="password"
        autocomplete="current-password"
        bind:value={tgPassword}
        disabled={busy}
      />
      {#if hint}<p class="muted hint">{hint}</p>{/if}
    {/if}

    {#if error}
      <p class="error-text">{error}</p>
    {/if}

    <button
      class="btn btn-primary submit"
      type="submit"
      disabled={busy ||
        (step === 'phone' && phone.trim().length === 0) ||
        (step === 'code' && code.trim().length === 0) ||
        (step === 'password' && tgPassword.length === 0)}
    >
      {busy
        ? 'Working…'
        : step === 'phone'
          ? 'Send code'
          : step === 'code'
            ? 'Sign in'
            : 'Confirm'}
    </button>
  </form>
</div>

<style>
  .login-card {
    width: min(360px, 100%);
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .brand {
    font-size: 26px;
    letter-spacing: 0.5px;
    text-align: center;
  }

  .tagline {
    text-align: center;
    margin: 0 0 14px;
    font-size: 13.5px;
  }

  .lbl {
    font-size: 13px;
    color: var(--muted);
    margin-bottom: -4px;
  }

  .hint {
    font-size: 12.5px;
    margin: -2px 0 0;
  }

  .submit {
    margin-top: 12px;
  }
</style>
