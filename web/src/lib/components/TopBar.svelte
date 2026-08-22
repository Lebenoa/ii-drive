<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { clearToken, getMe, getToken, type TgUser } from '$lib/api';

  // The navbar is owned by the layout, so it fetches its own user info.
  let user = $state<TgUser | null>(null);

  // Depends on the route: logging in does not remount the layout, so the
  // token check must re-run on navigation for the avatar to appear.
  $effect(() => {
    void page.url.pathname;
    if (!getToken()) {
      user = null;
      return;
    }
    void getMe()
      .then((m) => {
        if (m.authorized) user = m.user;
      })
      .catch(() => {
        // Boot pages handle auth errors themselves; the navbar stays quiet.
      });
  });

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

  <nav class="nav-links" aria-label="Main">
    <a href="/" class="nav-link">Files</a>
    <a href="/channels" class="nav-link">Channels</a>
    <a href="/settings" class="nav-link">Settings</a>
  </nav>

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
      {user.name.slice(0, 1).toUpperCase()}
    </button>

    <!-- Popover API: light-dismiss, top layer, Esc — all native. -->
    <div
      id={ACCOUNT_POPOVER}
      popover={popoverSupported ? 'auto' : undefined}
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
  }

  .brand {
    font-size: 16px;
    font-weight: 700;
    letter-spacing: 0.4px;
    color: var(--text);
    text-decoration: none;
  }

  .nav-links {
    display: flex;
    gap: 4px;
  }

  .nav-link {
    font-size: 13.5px;
    color: var(--muted);
    text-decoration: none;
    padding: 5px 10px;
    border-radius: 6px;
  }

  .nav-link:hover {
    color: var(--text);
    background: rgba(255, 255, 255, 0.05);
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
    cursor: pointer;
    padding: 0;
  }

  .account-btn:hover {
    border-color: var(--accent);
  }

  /* Hidden by default: the UA popover rule only exists where the API
     does, and the .open class covers the fallback path. */
  .account-menu {
    position: fixed;
    /* Anchored to the navbar's fixed height rather than the button:
       CSS anchor positioning is not portable enough yet. */
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
    display: none;
    flex-direction: column;
    gap: 2px;
  }

  .account-menu:popover-open,
  .account-menu.open {
    display: flex;
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

  .menu-item:hover {
    background: rgba(255, 255, 255, 0.05);
  }

  .menu-item.danger:hover {
    color: var(--danger);
  }

  @media (max-width: 560px) {
    .nav-links {
      display: none;
    }
  }
</style>
