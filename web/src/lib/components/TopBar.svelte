<script lang="ts">
  import type { Snippet } from 'svelte';
  import type { TgUser } from '$lib/api';

  let {
    user,
    onLogout,
    children,
  }: { user: TgUser | null; onLogout: () => void; children: Snippet } = $props();
</script>

<div class="shell">
  <header class="topbar">
    <span class="brand">ii-drive</span>
    <span class="right">
      <a class="settings-link" href="/settings">Settings</a>
      {#if user}
        <span class="user muted" title="Telegram account">{user.name}</span>
      {/if}
      <button class="btn ghost" type="button" onclick={onLogout}>Log out</button>
    </span>
  </header>

  <main class="content">
    {@render children()}
  </main>
</div>

<style>
  .shell {
    height: 100vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .topbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 22px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
    position: sticky;
    top: 0;
    z-index: 5;
  }

  .brand {
    font-size: 17px;
    font-weight: 700;
    letter-spacing: 0.4px;
  }

  .right {
    display: flex;
    align-items: center;
    gap: 14px;
  }

  .user {
    font-size: 13.5px;
    max-width: 220px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .settings-link {
    font-size: 13px;
    color: var(--muted);
    text-decoration: none;
  }

  .settings-link:hover {
    color: inherit;
  }

  .btn.ghost {
    padding: 6px 12px;
    font-size: 13px;
  }

  .content {
    width: min(1200px, 100%);
    margin: 0 auto;
    padding: 20px 20px 24px;
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
</style>
