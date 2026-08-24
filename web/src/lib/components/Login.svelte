<script lang="ts">
  import { sendLoginPhone, sendLoginCode, sendLoginPassword, setToken } from '$lib/api';
  import CountrySelect from '$lib/components/CountrySelect.svelte';
  import { guessCountry, splitNumber, toE164, type Country } from '$lib/countries';
  import { fadeOnly, slideX } from '$lib/motion';

  let { onSuccess }: { onSuccess: () => void } = $props();

  let step: 'phone' | 'code' | 'password' = $state('phone');
  /**
   * The field is kept verbatim — never rewritten under the user's caret, or
   * a typed "+" would vanish before the next keystroke lands. Everything
   * else is derived from it.
   */
  let phoneRaw = $state('');
  /** The combobox choice; overridden whenever the field names its own country. */
  let chosen = $state<Country>(guessCountry());
  /** identifies the in-flight attempt on the server; issued by the phone step */
  let loginId = $state('');
  let code = $state('');
  let tgPassword = $state('');
  let hint = $state('');
  let busy = $state(false);
  let error = $state('');

  const parsed = $derived(splitNumber(phoneRaw, chosen));
  const country = $derived(parsed.country);
  const e164 = $derived(toE164(country, parsed.national));
  /** Dial code alone is not a number; guards the submit button. */
  const phoneReady = $derived(parsed.national.length > 0);

  function finish(token: string): void {
    setToken(token);
    onSuccess();
  }

  async function submitPhone(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    if (busy || !phoneReady) return;
    busy = true;
    error = '';
    try {
      loginId = await sendLoginPhone(e164);
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
    // No id means the phone step never completed (or the page reloaded mid-flow);
    // the code can only be checked against the attempt that requested it.
    if (loginId.length === 0) {
      code = '';
      step = 'phone';
      error = 'That sign-in attempt is no longer valid. Request a new code.';
      return;
    }
    busy = true;
    error = '';
    try {
      const res = await sendLoginCode(loginId, code.trim());
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
      const res = await sendLoginPassword(loginId, tgPassword);
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
            <label class="lbl" for="phone">Phone number</label>
            <div class="phone-row">
              <CountrySelect
                {country}
                disabled={busy}
                onpick={(c) => {
                  chosen = c;
                  // An international number in the field would keep winning
                  // over the pick, so an explicit choice clears it instead.
                  const digits = phoneRaw.replace(/\D/g, '');
                  if (phoneRaw.trim().startsWith('+') || digits.startsWith('00')) {
                    phoneRaw = '';
                  }
                }}
              />
              <input
                id="phone"
                class="field"
                type="tel"
                placeholder="988 962 019"
                autocomplete="tel-national"
                inputmode="tel"
                bind:value={phoneRaw}
                disabled={busy}
              />
            </div>
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
        (step === 'phone' && !phoneReady) ||
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

  /* Country picker and the number share one control, visually. */
  .phone-row {
    display: flex;
    gap: 6px;
    align-items: stretch;
  }

  .phone-row .field {
    flex: 1;
    min-width: 0;
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
