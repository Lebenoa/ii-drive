<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { clearToken, fetchAvatar, getMe, getToken, type TgUser } from '$lib/api';

  // The navbar is owned by the layout, so it fetches its own user info.
  let user = $state<TgUser | null>(null);

  // Profile photo for the account button; null = keep the initial-letter
  // fallback (no photo on the account, or the fetch failed).
  let avatarUrl = $state<string | null>(null);
  let avatarLoaded = $state(false);
  // Bumps on every refetch/logout so a slow stale response can never revoke
  // or overwrite the blob URL of a newer one.
  let avatarGen = 0;

  function dropAvatar(): void {
    avatarGen++;
    if (avatarUrl) URL.revokeObjectURL(avatarUrl);
    avatarUrl = null;
    avatarLoaded = false;
  }

  // Depends on the route: logging in does not remount the layout, so the
  // token check must re-run on navigation for the avatar to appear.
  $effect(() => {
    void page.url.pathname;
    if (!getToken()) {
      user = null;
      dropAvatar();
      return;
    }
    void getMe()
      .then((m) => {
        if (m.authorized) {
          user = m.user;
          const gen = ++avatarGen;
          void fetchAvatar()
            .then((url) => {
              if (gen !== avatarGen) {
                if (url) URL.revokeObjectURL(url);
                return;
              }
              if (avatarUrl) URL.revokeObjectURL(avatarUrl);
              avatarUrl = url;
              avatarLoaded = false;
            })
            .catch(() => {});
        }
      })
  });

  	// Navigation is brand + account menu only: Files is the root, Channels is
	// reached via the boot redirect, Settings lives in the account menu.

  // Popover API fallback: without it the unknown `popover` attribute is
  // ignored and the menu would render permanently visible, so display is
  // controlled here in CSS and toggled by class instead.
  const popoverSupported =
    typeof HTMLElement !== 'undefined' && 'showPopover' in HTMLElement.prototype;
  let menuOpen = $state(false);
  let menuEl = $state<HTMLElement | null>(null);

  function logout(): void {
    clearToken();
    goto('/login');
  }

  const ACCOUNT_POPOVER = 'account-menu';

  function toggleMenu(): void {
    if (popoverSupported) return; // popovertarget handles it
    menuOpen = !menuOpen;
  }

  function closeMenu(): void {
    if (popoverSupported) {
      document.getElementById(ACCOUNT_POPOVER)?.hidePopover();
    } else {
      menuOpen = false;
    }
  }
</script>

<!-- Fallback-only light dismiss: native popovers dismiss themselves. -->
<svelte:window
  onclick={(e) => {
    if (menuOpen && !e.composedPath().includes(menuEl as EventTarget)) menuOpen = false;
  }}
/>

<header class="topbar">
  <a class="brand" href="/">ii-drive</a>

  <span class="spacer"></span>

  {#if user}
    <button
      class="account-btn"
      type="button"
      popovertarget={popoverSupported ? ACCOUNT_POPOVER : undefined}
      popovertargetaction="toggle"
      aria-expanded={popoverSupported ? undefined : menuOpen}
      aria-label="Account menu"
      onclick={toggleMenu}
      title={user.name}
    >
      {#if avatarUrl}
        <img
          class="avatar-img"
          class:is-loaded={avatarLoaded}
          src={avatarUrl}
          alt=""
          onload={() => (avatarLoaded = true)}
          onerror={() => {
            URL.revokeObjectURL(avatarUrl!);
            avatarUrl = null;
          }}
        />
      {:else}
        {user.name.slice(0, 1).toUpperCase()}
      {/if}
    </button>

    <!-- Popover API: light-dismiss, top layer, Esc — all native. -->
    <div
      id={ACCOUNT_POPOVER}
      popover={popoverSupported ? 'auto' : undefined}
      class:native={popoverSupported}
      class="account-menu"
      class:open={menuOpen}
      bind:this={menuEl}
    >
      <p class="who" title={user.name}>{user.name}</p>
      <a class="menu-item" href="/settings" onclick={closeMenu}>⚙ Settings</a>
      <button class="menu-item danger" type="button" onclick={logout}>⎋ Log out</button>
    </div>
  {/if}
</header>

<style>
  .topbar {
    display: flex;
    align-items: center;
    gap: 18px;
    padding: 10px 18px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
    position: sticky;
    top: 0;
    z-index: 5;
    /* This bar is shared across routes, so it stays put while the body
       crossfades. The name must be unique per document — keeping it in
       TopBar's scoped CSS pins it to this header and nothing else. */
    view-transition-name: topbar;
  }

  .brand {
    font-size: 16px;
    font-weight: 700;
    letter-spacing: 0.4px;
    color: var(--text);
    text-decoration: none;
  }

  .spacer {
    flex: 1;
  }

  .account-btn {
    width: 32px;
    height: 32px;
    border-radius: 50%;
    border: 1px solid var(--border);
    background: var(--panel-2);
    color: var(--text);
    font-size: 14px;
    font-weight: 600;
    /* The popover anchors to this element; unsupported browsers ignore it. */
    anchor-name: --account-btn;
    cursor: pointer;
    padding: 0;
    transition:
      border-color var(--dur-fast) var(--ease),
      background var(--dur-fast) var(--ease),
      box-shadow var(--dur-fast) var(--ease),
      transform var(--dur-fast) var(--ease);
  }

  /* Profile photo fills the round button; fades in once decoded so a
     half-loaded image never flashes. */
  .avatar-img {
    width: 100%;
    height: 100%;
    border-radius: inherit;
    object-fit: cover;
    opacity: 0;
    transition: opacity var(--dur-fast) var(--ease);
  }

  .avatar-img.is-loaded {
    opacity: 1;
  }

  .account-btn:hover {
    border-color: var(--accent);
  }

  .account-btn:active {
    transform: scale(0.92);
  }

  .account-btn:focus-visible {
    outline: none;
    box-shadow: 0 0 0 3px rgba(91, 157, 255, 0.35);
  }

  /* Hidden by default: the UA popover rule only exists where the API
     does, and the .open class covers the fallback path. */
  .account-menu {
    position: fixed;
    /* Fallback placement for browsers without anchor positioning. */
    top: 52px;
    right: 14px;
    inset: auto;
    margin: 0;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--panel);
    box-shadow: 0 12px 36px rgba(0, 0, 0, 0.45);
    padding: 6px;
    min-width: 180px;
    flex-direction: column;
    gap: 2px;
    /* The menu belongs to the avatar it hangs off, so it scales out of it. */
    transform-origin: top right;
  }

  /* CSS anchor positioning: the menu hangs off the avatar button instead of
     a hardcoded navbar offset, so it stays glued to it at any size. */
  @supports (anchor-name: --a) {
    .account-menu {
      position-anchor: --account-btn;
      top: calc(anchor(bottom) + 6px);
      right: anchor(right);
    }
  }

  /* Native path. Opening/closing stays entirely with popovertarget; this is
     only paint. `display`/`overlay` need allow-discrete or the close would
     yank the element out of the top layer before the exit could run. */
  .account-menu.native {
    display: none;
    opacity: 0;
    transform: scale(0.94) translateY(-4px);
    transition:
      opacity var(--dur-fast) var(--ease),
      transform var(--dur-fast) var(--ease),
      overlay var(--dur-fast) var(--ease) allow-discrete,
      display var(--dur-fast) var(--ease) allow-discrete;
  }

  .account-menu.native:popover-open {
    display: flex;
    opacity: 1;
    transform: none;
    transition:
      opacity var(--dur) var(--ease-out),
      transform var(--dur) var(--ease-out),
      overlay var(--dur) var(--ease-out) allow-discrete,
      display var(--dur) var(--ease-out) allow-discrete;

    @starting-style {
      opacity: 0;
      transform: scale(0.94) translateY(-4px);
    }
  }

  /* Fallback path. A browser without the popover API has no
     @starting-style either, so `visibility` carries the hidden state —
     it holds `visible` for the whole exit, then flips discretely. */
  .account-menu:not(.native) {
    display: flex;
    visibility: hidden;
    opacity: 0;
    transform: scale(0.94) translateY(-4px);
    transition:
      opacity var(--dur-fast) var(--ease),
      transform var(--dur-fast) var(--ease),
      visibility var(--dur-fast) var(--ease);
  }

  .account-menu:not(.native).open {
    visibility: visible;
    opacity: 1;
    transform: none;
    transition:
      opacity var(--dur) var(--ease-out),
      transform var(--dur) var(--ease-out),
      visibility var(--dur) var(--ease-out);
  }

  .who {
    margin: 2px 8px 8px;
    font-size: 13px;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    border-bottom: 1px solid var(--border);
    padding-bottom: 8px;
  }

  .menu-item {
    display: block;
    width: 100%;
    border: none;
    background: none;
    color: var(--text);
    font: inherit;
    font-size: 13.5px;
    text-align: left;
    padding: 7px 8px;
    border-radius: 6px;
    cursor: pointer;
    text-decoration: none;
  }

  .menu-item {
    transition:
      background var(--dur-fast) var(--ease),
      color var(--dur-fast) var(--ease);
  }

  .menu-item:hover {
    background: rgba(255, 255, 255, 0.05);
  }

  .menu-item.danger:hover {
    color: var(--danger);
  }

</style>
