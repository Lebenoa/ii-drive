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
      copiedId = id;
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
          {#if file.mime.startsWith('image/') && file.has_thumb && !brokenThumbs.has(file.id)}
            <img
              class="thumb"
              src={thumbUrl(file.id, file.public)}
              alt=""
              loading="lazy"
              title={file.mime}
              onerror={() => (brokenThumbs = new Set([...brokenThumbs, file.id]))}
            />
          {:else if file.mime.startsWith('image/') && !brokenFull.has(file.id)}
            <!-- svelte-ignore a11y_use_click_handler -->
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
            {copiedId === file.id ? '✓' : '🔗'}<span class="act-lbl">{copiedId === file.id ? 'copied' : ''}</span>
          </button>
          <a class="icon-btn" href={rawUrl(file.id, true, file.public)} title="Download">⬇</a>
          <button
            class="icon-btn"
            type="button"
            title={file.public ? 'Public — anyone with the link can download' : 'Private — only you can download'}
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
</style>
