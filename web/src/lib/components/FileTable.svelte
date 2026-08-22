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

  let selected = $state<Set<string>>(new Set());
  let bulkBusy = $state(false);
  let bulkError = $state('');

  function toggleSelect(id: string): void {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    selected = next;
  }

  function selectAll(): void {
    selected = new Set(files.every((f) => selected.has(f.id)) ? [] : files.map((f) => f.id));
  }

  let allSelected = $derived(files.length > 0 && files.every((f) => selected.has(f.id)));

  async function bulkDelete(): Promise<void> {
    if (bulkBusy) return;
    bulkBusy = true;
    bulkError = '';
    const ids = [...selected];
    let failed = 0;
    for (const id of ids) {
      try {
        await deleteFile(id);
      } catch {
        failed++;
      }
    }
    bulkBusy = false;
    selected = new Set();
    onDeleted();
    if (failed > 0) bulkError = `${failed} file(s) failed to delete`;
  }

  async function bulkVisibility(isPublic: boolean): Promise<void> {
    if (bulkBusy) return;
    bulkBusy = true;
    bulkError = '';
    const ids = [...selected];
    let failed = 0;
    for (const id of ids) {
      try {
        await setFileVisibility(id, isPublic);
        const f = files.find((x) => x.id === id);
        if (f) f.public = isPublic;
      } catch {
        failed++;
      }
    }
    bulkBusy = false;
    if (failed === 0) selected = new Set();
    else bulkError = `${failed} file(s) failed to update`;
  }

  const BULK_DIALOG = 'dlg-bulk-delete';

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

{#if selected.size > 0}
  <div class="bulk-bar" role="toolbar" aria-label="Selection actions">
    <span class="bulk-count">{selected.size} selected</span>
    <button class="btn ghost" type="button" onclick={selectAll}>
      {allSelected ? 'Clear all' : 'Select all'}
    </button>
    <button
      class="btn ghost"
      type="button"
      disabled={bulkBusy}
      onclick={() => void bulkVisibility(true)}
    >
      Make public
    </button>
    <button
      class="btn ghost"
      type="button"
      disabled={bulkBusy}
      onclick={() => void bulkVisibility(false)}
    >
      Make private
    </button>
    <button
      class="btn btn-danger"
      type="button"
      disabled={bulkBusy}
      {...openAttrs(BULK_DIALOG)}
      onclick={() => {
        if (!('commandFor' in HTMLButtonElement.prototype)) openDialog(BULK_DIALOG);
      }}
    >
      Delete
    </button>
    <button class="btn ghost" type="button" onclick={() => (selected = new Set())}>
      ✕
    </button>
    {#if bulkError}<span class="error-text">{bulkError}</span>{/if}
  </div>
{:else}
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
{/if}

{#if view === 'grid'}
  <div class="grid">
    {#each files as file (file.id)}
      <div class="card-f" class:selected={selected.has(file.id)}>
        <label class="g-check">
          <input
            type="checkbox"
            aria-label="Select {file.name}"
            checked={selected.has(file.id)}
            onchange={() => toggleSelect(file.id)}
          />
        </label>
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
        <th class="c-sel">
          <input
            type="checkbox"
            aria-label="Select all"
            checked={allSelected}
            onchange={selectAll}
          />
        </th>
        <th class="c-icon"></th>
        <th>Name</th>
        <th class="c-size">Size</th>
        <th class="c-date">Uploaded</th>
        <th class="c-actions">Actions</th>
      </tr>
    </thead>
    <tbody>
      {#each files as file (file.id)}
        <tr class:selected={selected.has(file.id)}>
          <td class="c-sel">
            <input
              type="checkbox"
              aria-label="Select {file.name}"
              checked={selected.has(file.id)}
              onchange={() => toggleSelect(file.id)}
            />
          </td>
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
  .bulk-bar {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px;
    margin-bottom: 10px;
    padding: 8px 10px;
    border: 1px solid var(--accent);
    border-radius: var(--radius);
    background: rgba(91, 157, 255, 0.08);
  }

  .bulk-count {
    font-weight: 600;
    font-size: 13.5px;
    margin-right: 6px;
  }

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

  .card-f {
    position: relative;
  }

  .card-f.selected {
    border-color: var(--accent);
    background: rgba(91, 157, 255, 0.08);
  }

  .g-check {
    position: absolute;
    top: 6px;
    left: 6px;
    z-index: 2;
    background: var(--panel);
    border-radius: 4px;
    padding: 2px;
    opacity: 0;
    transition: opacity 0.12s ease;
  }

  .card-f:hover .g-check,
  .card-f.selected .g-check {
    opacity: 1;
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

  .c-sel {
    width: 30px;
    text-align: center;
  }

  .tbl tr.selected td {
    background: rgba(91, 157, 255, 0.1);
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

<Modal
  id={BULK_DIALOG}
  title="Delete files"
  onclose={(rv) => {
    if (rv === 'delete') void bulkDelete();
  }}
>
  <p>Delete <strong>{selected.size}</strong> file(s)? This removes them from Telegram storage as
    well — there is no undo.</p>
  {#snippet actions()}
    <button class="btn" type="submit" {...closeAttrs(BULK_DIALOG)}>Cancel</button>
    <button class="btn btn-danger" type="submit" value="delete" disabled={bulkBusy}>
      {bulkBusy ? 'Deleting…' : 'Delete'}
    </button>
  {/snippet}
</Modal>
