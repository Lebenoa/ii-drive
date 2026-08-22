<script lang="ts">
  import { deleteFile, rawUrl, setFileVisibility, thumbUrl, type DriveFile } from '$lib/api';
  import { humanSize, mimeIcon, relTime } from '../format';
  import Modal from './Modal.svelte';
  import { closeAttrs, openAttrs, openDialog } from '$lib/invoker';

  let { files, onDeleted }: { files: DriveFile[]; onDeleted: () => void } = $props();

  let copiedId = $state('');
  let deletingId = $state('');
  let copyTimer: ReturnType<typeof setTimeout> | undefined;

  let togglingId = $state('');
  let brokenThumbs = $state<Set<string>>(new Set());
  let brokenFull = $state<Set<string>>(new Set());

  const VIEW_KEY = 'ii_view';
  let view = $state<'list' | 'grid'>(localStorage.getItem(VIEW_KEY) === 'grid' ? 'grid' : 'list');

  function setView(next: 'list' | 'grid'): void {
    view = next;
    localStorage.setItem(VIEW_KEY, next);
  }

  async function toggleVisibility(file: DriveFile): Promise<void> {
    if (togglingId) return;
    togglingId = file.id;
    try {
      await setFileVisibility(file.id, !file.public);
      file.public = !file.public;
    } finally {
      togglingId = '';
    }
  }

  const DELETE_DIALOG = 'dlg-delete-file';
  let pendingFile = $state<DriveFile | null>(null);

  async function copyLink(file: DriveFile): Promise<void> {
    try {
      await navigator.clipboard.writeText(rawUrl(file.id));
      copiedId = file.id;
      clearTimeout(copyTimer);
      copyTimer = setTimeout(() => (copiedId = ''), 1400);
    } catch {
      // clipboard unavailable (insecure context) — open the link instead so the user can copy manually
      window.open(rawUrl(file.id), '_blank');
    }
  }

  function askRemove(file: DriveFile): void {
    pendingFile = file;
    if (!('commandFor' in HTMLButtonElement.prototype)) openDialog(DELETE_DIALOG);
  }

  async function remove(file: DriveFile): Promise<void> {
    deletingId = file.id;
    try {
      await deleteFile(file.id);
      onDeleted();
    } finally {
      deletingId = '';
    }
  }
</script>

<div class="view-toggle" role="group" aria-label="Display mode">
  <button
    class="icon-btn"
    class:active={view === 'list'}
    type="button"
    title="List view"
    aria-pressed={view === 'list'}
    onclick={() => setView('list')}
  >
    ☰
  </button>
  <button
    class="icon-btn"
    class:active={view === 'grid'}
    type="button"
    title="Grid view"
    aria-pressed={view === 'grid'}
    onclick={() => setView('grid')}
  >
    ▦
  </button>
</div>

{#if view === 'grid'}
  <div class="grid">
    {#each files as file (file.id)}
      <div class="card-f">
        <div class="g-thumb">
          {#if file.has_thumb && !brokenThumbs.has(file.id)}
            <img
              class="g-img"
              src={thumbUrl(file.id, file.public)}
              alt=""
              loading="lazy"
              onerror={() => (brokenThumbs = new Set([...brokenThumbs, file.id]))}
            />
          {:else if file.mime.startsWith('image/') && !brokenFull.has(file.id)}
            <img
              class="g-img"
              src={rawUrl(file.id, false, file.public)}
              alt=""
              loading="lazy"
              onerror={() => (brokenFull = new Set([...brokenFull, file.id]))}
            />
          {:else}
            <span class="g-ico" title={file.mime}>{mimeIcon(file.mime, file.name)}</span>
          {/if}
        </div>
        <div class="g-meta">
          <span class="g-name" title={file.name}>{file.name}</span>
          <span class="g-sub muted">{humanSize(file.size)} · {relTime(file.created_at)}</span>
        </div>
        <div class="g-actions">
          <button
            class="icon-btn act-copy"
            type="button"
            title={file.public ? 'Copy public link' : 'Private — make public to share'}
            disabled={!file.public}
            onclick={() => void copyLink(file)}
          >
            {copiedId === file.id ? '✓' : '🔗'}
          </button>
          <a class="icon-btn" href={rawUrl(file.id, true, file.public)} title="Download">⬇</a>
          <button
            class="icon-btn"
            type="button"
            title={file.public
              ? 'Public — anyone with the link can download'
              : 'Private — only you can download'}
            disabled={togglingId === file.id}
            onclick={() => void toggleVisibility(file)}
          >
            {file.public ? '🌐' : '🔒'}
          </button>
          <button
            class="icon-btn danger"
            type="button"
            title="Delete"
            disabled={deletingId === file.id}
            {...openAttrs(DELETE_DIALOG)}
            onclick={() => askRemove(file)}
          >
            🗑
          </button>
        </div>
      </div>
    {:else}
      <p class="empty muted">No files found — upload something to get started.</p>
    {/each}
  </div>
{:else}
  <table class="tbl">
    <thead>
      <tr>
        <th class="c-icon"></th>
        <th>Name</th>
        <th class="c-size">Size</th>
        <th class="c-date">Uploaded</th>
        <th class="c-actions">Actions</th>
      </tr>
    </thead>
    <tbody>
      {#each files as file (file.id)}
        <tr>
          <td class="c-icon">
            {#if file.has_thumb && !brokenThumbs.has(file.id)}
              <img
                class="thumb"
                src={thumbUrl(file.id, file.public)}
                alt=""
                loading="lazy"
                title={file.mime}
                onerror={() => (brokenThumbs = new Set([...brokenThumbs, file.id]))}
              />
            {:else if file.mime.startsWith('image/') && !brokenFull.has(file.id)}
              <img
                class="thumb"
                src={rawUrl(file.id, false, file.public)}
                alt=""
                loading="lazy"
                title={file.mime}
                onerror={() => (brokenFull = new Set([...brokenFull, file.id]))}
              />
            {:else}
              <span title={file.mime}>{mimeIcon(file.mime, file.name)}</span>
            {/if}
          </td>
          <td class="c-name" title={file.name}>{file.name}</td>
          <td class="c-size">{humanSize(file.size)}</td>
          <td class="c-date muted" title={new Date(file.created_at * 1000).toLocaleString()}>
            {relTime(file.created_at)}
          </td>
          <td class="c-actions">
            <button
              class="icon-btn act-copy"
              type="button"
              title={file.public ? 'Copy public link' : 'Private — make public to share'}
              disabled={!file.public}
              onclick={() => void copyLink(file)}
            >
              {copiedId === file.id ? '✓' : '🔗'}<span class="act-lbl"
                >{copiedId === file.id ? 'copied' : ''}</span
              >
            </button>
            <a class="icon-btn" href={rawUrl(file.id, true, file.public)} title="Download">⬇</a>
            <button
              class="icon-btn"
              type="button"
              title={file.public
                ? 'Public — anyone with the link can download'
                : 'Private — only you can download'}
              disabled={togglingId === file.id}
              onclick={() => void toggleVisibility(file)}
            >
              {file.public ? '🌐' : '🔒'}
            </button>
            <button
              class="icon-btn danger"
              type="button"
              title="Delete"
              disabled={deletingId === file.id}
              {...openAttrs(DELETE_DIALOG)}
              onclick={() => askRemove(file)}
            >
              🗑
            </button>
          </td>
        </tr>
      {:else}
        <tr>
          <td colspan="5" class="empty muted">No files found — upload something to get started.</td>
        </tr>
      {/each}
    </tbody>
  </table>
{/if}

<Modal
  id={DELETE_DIALOG}
  title="Delete file"
  onclose={(rv) => {
    if (rv === 'delete' && pendingFile) void remove(pendingFile);
    pendingFile = null;
  }}
>
  {#if pendingFile}
    <p>
      Delete <strong>{pendingFile.name}</strong> ({humanSize(pendingFile.size)})? This removes it
      from Telegram storage as well — there is no undo.
    </p>
  {/if}
  {#snippet actions()}
    <button class="btn" type="submit" {...closeAttrs(DELETE_DIALOG)}>Cancel</button>
    <button
      class="btn btn-danger"
      type="submit"
      value="delete"
      disabled={pendingFile !== null && deletingId === pendingFile.id}
    >
      {pendingFile !== null && deletingId === pendingFile.id ? 'Deleting…' : 'Delete'}
    </button>
  {/snippet}
</Modal>

<style>
  .view-toggle {
    display: flex;
    gap: 4px;
    margin-bottom: 10px;
  }

  .view-toggle .icon-btn.active {
    color: var(--accent);
    background: rgba(91, 157, 255, 0.12);
    border-radius: 6px;
  }

  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
    gap: 12px;
  }

  .card-f {
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--panel-2);
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }

  .card-f:hover {
    border-color: var(--accent);
  }

  .g-thumb {
    aspect-ratio: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--panel);
  }

  .g-img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }

  .g-ico {
    font-size: 42px;
    opacity: 0.75;
  }

  .g-meta {
    padding: 8px 10px 4px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }

  .g-name {
    font-size: 13px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .g-sub {
    font-size: 11.5px;
  }

  .g-actions {
    display: flex;
    gap: 2px;
    padding: 4px 6px 6px;
    margin-top: auto;
  }

  .g-actions a {
    text-decoration: none;
  }

  .tbl {
    width: 100%;
    border-collapse: collapse;
    font-size: 14px;
  }

  th {
    text-align: left;
    font-size: 12px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.6px;
    color: var(--muted);
    padding: 8px 10px;
    border-bottom: 1px solid var(--border);
  }

  td {
    padding: 9px 10px;
    border-bottom: 1px solid #202634;
    vertical-align: middle;
  }

  tbody tr:hover {
    background: rgba(255, 255, 255, 0.025);
  }

  .c-icon {
    width: 34px;
    text-align: center;
  }

  .c-name {
    max-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .c-size {
    width: 90px;
    white-space: nowrap;
    color: var(--muted);
  }

  .c-date {
    width: 110px;
    white-space: nowrap;
  }

  .c-actions {
    width: 165px;
    white-space: nowrap;
    text-align: right;
  }

  .c-actions a {
    text-decoration: none;
  }

  .thumb {
    width: 30px;
    height: 30px;
    object-fit: cover;
    border-radius: 5px;
    display: block;
    margin: 0 auto;
    background: var(--panel-2);
  }

  .act-copy {
    white-space: nowrap;
  }

  .icon-btn.danger:hover:not(:disabled) {
    color: var(--danger);
  }

  .act-lbl {
    font-size: 11.5px;
    color: var(--ok);
    margin-left: 3px;
  }

  .empty {
    text-align: center;
    padding: 36px 10px;
  }

  .grid .empty {
    grid-column: 1 / -1;
  }
</style>
