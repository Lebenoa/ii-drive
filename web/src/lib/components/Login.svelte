<script lang="ts">
  import { sendLoginPhone, sendLoginCode, sendLoginPassword, setToken } from '$lib/api';
  import { fadeOnly, slideX } from '$lib/motion';

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
      if (res.status === 'ok') finish(res.token);
      else error = res.hint ?? 'Password required.';
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

    <!-- Keyed so each step is its own block: the outgoing one fades while
         the incoming slides in over it. Both share one grid cell, so the
         crossfade never pushes the button around. -->
    <div class="steps">
      {#key step}
        <div class="step" in:slideX={{ x: 14 }} out:fadeOnly>
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
        </div>
      {/key}
    </div>

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
      {#if busy}<span class="spinner btn-spin"></span>{/if}
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

  /* One grid cell holds every step, so the outgoing and incoming blocks
     stack instead of stretching the card mid-transition. */
  .steps {
    display: grid;
  }

  .step {
    grid-area: 1 / 1;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .submit {
    margin-top: 12px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 7px;
  }

  /* Reuses the global .spinner animation at control scale. */
  .btn-spin {
    width: 13px;
    height: 13px;
    border-width: 2px;
    border-color: color-mix(in srgb, currentColor 30%, transparent);
    border-top-color: currentColor;
    flex-shrink: 0;
  }
</style>
