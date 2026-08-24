<script lang="ts">
  import { deleteFile, rawUrl, setFileVisibility, thumbUrl, type DriveFile } from '$lib/api';
  import { humanSize, mimeIcon, relTime } from '../format';
  import Modal from './Modal.svelte';
  import { closeAttrs, closeDialog, openDialog } from '$lib/invoker';
  import { collapse, fadeOnly, fadeUp, flipDur, pop, stagger } from '$lib/motion';
  import { flip } from 'svelte/animate';
  import { t } from '$lib/i18n.svelte';

  let {
    files,
    onDeleted,
    cutIds,
    onCut,
  }: { files: DriveFile[]; onDeleted: () => void; cutIds: Set<string>; onCut: (ids: string[]) => void } =
    $props();

  let copiedId = $state('');
  let deletingId = $state('');
  let copyTimer: ReturnType<typeof setTimeout> | undefined;

  let togglingId = $state('');
  let brokenThumbs = $state<Set<string>>(new Set());
  let brokenFull = $state<Set<string>>(new Set());
  /** Thumbnails fade in once they decode, so a late image cannot pop into
   *  the middle of an already laid-out row. Keyed by id, so re-filtering
   *  does not re-fade an image the browser already holds. */
  let loadedThumbs = $state<Set<string>>(new Set());

  function markLoaded(id: string): void {
    if (!loadedThumbs.has(id)) loadedThumbs = new Set([...loadedThumbs, id]);
  }

  /** Signed media URLs keyed by `${id}:${kind}:${dl}`. Private files must
   *  never carry the session token in a URL, so each raw/thumb link is
   *  minted against the short-lived media token instead. */
  let mediaUrls = $state<Record<string, string>>({});

  function mediaKey(id: string, kind: 'raw' | 'thumb', dl = false): string {
    return `${id}:${kind}:${dl ? 1 : 0}`;
  }

  $effect(() => {
    const variants = [
      ['thumb', false],
      ['raw', false],
      ['raw', true],
    ] as const;
    for (const f of files) {
      for (const [kind, dl] of variants) {
        const key = mediaKey(f.id, kind, dl);
        if (key in mediaUrls) continue;
        (kind === 'thumb' ? thumbUrl(f.id) : rawUrl(f.id, dl)).then((url) => {
          mediaUrls[key] = url;
        });
      }
    }
  });

  /** Resolved media URL; undefined while minting so the src attr is omitted. */
  function murl(file: DriveFile, kind: 'raw' | 'thumb', dl = false): string | undefined {
    return mediaUrls[mediaKey(file.id, kind, dl)];
  }

  let selected = $state<Set<string>>(new Set());
  let bulkBusy = $state(false);
  let bulkError = $state('');

  function dragIds(file: DriveFile): string[] {
    return selected.has(file.id) ? [...selected] : [file.id];
  }

  function onDragStart(e: DragEvent, file: DriveFile): void {
    const ids = dragIds(file);
    e.dataTransfer?.setData('application/x-ii-files', JSON.stringify(ids));
    if (e.dataTransfer) e.dataTransfer.effectAllowed = 'move';
  }

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

  // Prune selection when the list refilters or reloads — bulk actions must
  // never touch rows that are no longer visible.
  $effect(() => {
    const visible = new Set(files.map((f) => f.id));
    if ([...selected].some((id) => !visible.has(id))) {
      selected = new Set([...selected].filter((id) => visible.has(id)));
    }
  });

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
    if (failed > 0) bulkError = t('files.deleteFailed', { n: failed });
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
    else bulkError = t('files.updateFailed', { n: failed });
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

  const PREVIEW_DIALOG = 'dlg-preview';
  let previewFile = $state<DriveFile | null>(null);

  function previewable(file: DriveFile): boolean {
    return (
      file.mime.startsWith('image/') ||
      file.mime.startsWith('video/') ||
      file.mime.startsWith('audio/')
    );
  }

  function openPreview(file: DriveFile): void {
    if (!previewable(file)) return;
    previewFile = file;
    openDialog(PREVIEW_DIALOG);
  }

  /** Only reachable for public files — the button is not rendered otherwise. */
  async function copyLink(file: DriveFile): Promise<void> {
    const url = await rawUrl(file.id);
    try {
      await navigator.clipboard.writeText(url);
      copiedId = file.id;
      clearTimeout(copyTimer);
      copyTimer = setTimeout(() => (copiedId = ''), 1400);
    } catch {
      // clipboard unavailable (insecure context) — open the link instead so the user can copy manually
      window.open(url, '_blank');
    }
  }

  function askRemove(file: DriveFile): void {
    pendingFile = file;
    openDialog(DELETE_DIALOG);
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
  <div class="bulk-bar" role="toolbar" aria-label={t('files.selectionActions')} transition:collapse>
    <span class="bulk-count">{t('files.nSelected', { n: selected.size })}</span>
    <button class="btn ghost" type="button" onclick={selectAll}>
      {allSelected ? t('files.clearAll') : t('files.selectAll')}
    </button>
    <button
      class="btn ghost"
      type="button"
      disabled={bulkBusy}
      onclick={() => void bulkVisibility(true)}
    >
      {t('files.makePublic')}
    </button>
    <button
      class="btn ghost"
      type="button"
      disabled={bulkBusy}
      onclick={() => void bulkVisibility(false)}
    >
      {t('files.makePrivate')}
    </button>
    <button
      class="btn ghost"
      type="button"
      disabled={bulkBusy}
      onclick={() => {
        onCut([...selected]);
        selected = new Set();
      }}
    >
      ✂ {t('common.cut')}
    </button>
    <button
      class="btn btn-danger"
      type="button"
      disabled={bulkBusy}
      onclick={() => openDialog(BULK_DIALOG)}
    >
      {t('common.delete')}
    </button>
    <button class="btn ghost" type="button" onclick={() => (selected = new Set())}>
      ✕
    </button>
    {#if bulkError}<span class="error-text">{bulkError}</span>{/if}
  </div>
{:else}
<!-- Both bars share one slot, so both collapse: otherwise whichever leaves
     drops its height instantly and the table jumps up under the cursor. -->
<div class="view-toggle" role="group" aria-label={t('files.displayMode')} transition:collapse>
  <button
    class="icon-btn"
    class:active={view === 'list'}
    type="button"
    title={t('files.listView')}
    aria-pressed={view === 'list'}
    onclick={() => setView('list')}
  >
    ☰
  </button>
  <button
    class="icon-btn"
    class:active={view === 'grid'}
    type="button"
    title={t('files.gridView')}
    aria-pressed={view === 'grid'}
    onclick={() => setView('grid')}
  >
    ▦
  </button>
</div>
{/if}

{#if view === 'grid'}
  <div class="grid" in:fadeOnly>
    {#each files as file, i (file.id)}
      <div
          class="card-f"
          class:selected={selected.has(file.id)}
          class:cut={cutIds.has(file.id)}
          class:busy={deletingId === file.id || togglingId === file.id}
          role="button"
          tabindex="0"
          aria-label={t('files.openPreview', { name: file.name })}
          draggable="true"
          ondragstart={(e) => onDragStart(e, file)}
          in:fadeUp={{ delay: stagger(i) }}
          out:pop
          animate:flip={{ duration: flipDur() }}
        >
        <label class="g-check">
          <input
            type="checkbox"
            aria-label={t('files.select', { name: file.name })}
            checked={selected.has(file.id)}
            onchange={() => toggleSelect(file.id)}
          />
        </label>
        <div
          class="g-thumb"
          class:previewable={previewable(file)}
          role="button"
          tabindex="0"
          onclick={() => openPreview(file)}
          onkeydown={(e) => {
            // Space would scroll the page under the modal without this.
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              openPreview(file);
            }
          }}
        >
          {#if file.has_thumb && !brokenThumbs.has(file.id)}
            <img
              class="g-img"
              class:is-loaded={loadedThumbs.has(file.id)}
              src={murl(file, "thumb")}
              alt=""
              loading="lazy"
              onload={() => markLoaded(file.id)}
              onerror={() => (brokenThumbs = new Set([...brokenThumbs, file.id]))}
            />
          {:else if file.mime.startsWith('image/') && !brokenFull.has(file.id)}
            <img
              class="g-img"
              class:is-loaded={loadedThumbs.has(file.id)}
              src={murl(file, "raw")}
              alt=""
              loading="lazy"
              onload={() => markLoaded(file.id)}
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
          {#if file.public}
            <button
              class="icon-btn act-copy"
              type="button"
              title={t('files.copyLink')}
              onclick={() => void copyLink(file)}
            >
              <span class="act-ico">
                {#if copiedId === file.id}
                  <span class="act-chk" in:pop>✓</span>
                {:else}
                  🔗
                {/if}
              </span>
            </button>
          {/if}
          <a class="icon-btn" href={murl(file, "raw", true)} title={t('common.download')}>⬇</a>
          <button
            class="icon-btn"
            type="button"
            title={file.public
              ? t('files.publicTitle')
              : t('files.privateTitle')}
            disabled={togglingId === file.id}
            onclick={() => void toggleVisibility(file)}
          >
            {file.public ? '🌐' : '🔒'}
          </button>
          <button
            class="icon-btn danger"
            type="button"
            title={t('common.delete')}
            disabled={deletingId === file.id}
            onclick={() => askRemove(file)}
          >
            🗑
          </button>
        </div>
      </div>
    {:else}
      <p class="empty muted">{t('files.empty')}</p>
    {/each}
  </div>
{:else}
  <table class="tbl" in:fadeOnly>
    <thead>
      <tr>
        <th class="c-sel">
          <input
            type="checkbox"
            aria-label={t('files.selectAll')}
            checked={allSelected}
            indeterminate={!allSelected && selected.size > 0}
            onchange={selectAll}
          />
        </th>
        <th class="c-icon"></th>
        <th>{t('files.name')}</th>
        <th class="c-size">{t('files.size')}</th>
        <th class="c-date">{t('files.uploaded')}</th>
        <th class="c-actions">{t('files.actions')}</th>
      </tr>
    </thead>
    <tbody>
      {#each files as file, i (file.id)}
        <tr
          class:selected={selected.has(file.id)}
          class:cut={cutIds.has(file.id)}
          class:busy={deletingId === file.id || togglingId === file.id}
          draggable="true"
          ondragstart={(e) => onDragStart(e, file)}
          in:fadeUp={{ delay: stagger(i) }}
          out:fadeOnly
          animate:flip={{ duration: flipDur() }}
        >
          <td class="c-sel">
            <input
              type="checkbox"
              aria-label={t('files.select', { name: file.name })}
              checked={selected.has(file.id)}
              onchange={() => toggleSelect(file.id)}
            />
          </td>
          <td class="c-icon">
            {#if file.has_thumb && !brokenThumbs.has(file.id)}
              <img
                class="thumb"
                class:is-loaded={loadedThumbs.has(file.id)}
                src={murl(file, "thumb")}
                alt=""
                loading="lazy"
                title={file.mime}
                onload={() => markLoaded(file.id)}
                onerror={() => (brokenThumbs = new Set([...brokenThumbs, file.id]))}
              />
            {:else if file.mime.startsWith('image/') && !brokenFull.has(file.id)}
              <img
                class="thumb"
                class:is-loaded={loadedThumbs.has(file.id)}
                src={murl(file, "raw")}
                alt=""
                loading="lazy"
                title={file.mime}
                onload={() => markLoaded(file.id)}
                onerror={() => (brokenFull = new Set([...brokenFull, file.id]))}
              />
            {:else}
              <span title={file.mime}>{mimeIcon(file.mime, file.name)}</span>
            {/if}
          </td>
          <td class="c-name" title={file.name}>
              <button
                class="name-btn"
                class:previewable={previewable(file)}
                type="button"
                onclick={() => openPreview(file)}
              >
                {file.name}
              </button>
            </td>
          <td class="c-size">{humanSize(file.size)}</td>
          <td class="c-date muted" title={new Date(file.created_at * 1000).toLocaleString()}>
            {relTime(file.created_at)}
          </td>
          <td class="c-actions">
            {#if file.public}
              <button
                class="icon-btn act-copy"
                type="button"
                title={t('files.copyLink')}
                onclick={() => void copyLink(file)}
              >
                <span class="act-ico">
                  {#if copiedId === file.id}
                    <span class="act-chk" in:pop>✓</span>
                  {:else}
                    🔗
                  {/if}
                </span><span class="act-lbl">{copiedId === file.id ? t('files.copied') : ''}</span>
              </button>
            {/if}
            <a class="icon-btn" href={murl(file, "raw", true)} title={t('common.download')}>⬇</a>
            <button
              class="icon-btn"
              type="button"
              title={file.public
                ? t('files.publicTitle')
                : t('files.privateTitle')}
              disabled={togglingId === file.id}
              onclick={() => void toggleVisibility(file)}
            >
              {file.public ? '🌐' : '🔒'}
            </button>
            <button
              class="icon-btn danger"
              type="button"
              title={t('common.delete')}
              disabled={deletingId === file.id}
              onclick={() => askRemove(file)}
            >
              🗑
            </button>
          </td>
        </tr>
      {:else}
        <tr>
          <td colspan="5" class="empty muted">{t('files.empty')}</td>
        </tr>
      {/each}
    </tbody>
  </table>
{/if}

<!-- previewFile is cleared on close so the media element unmounts: a
     <video>/<audio> left mounted in a closed dialog keeps playing audio. -->
<dialog
  id={PREVIEW_DIALOG}
  class="preview"
  onclose={() => (previewFile = null)}
  onclick={(e: MouseEvent) => {
    const dlg = e.currentTarget as HTMLDialogElement | null;
    const tgt = e.target as HTMLElement | null;
    // Light dismiss: the dialog itself (::backdrop) or the empty area of
    // .pv-media around the media element — but not the media/controls.
    if (dlg && (tgt === dlg || tgt?.classList.contains('pv-media'))) dlg.close();
  }}
>
  {#if previewFile}
    <div class="pv-media">
      {#if previewFile.mime.startsWith('video/')}
        <!-- User-uploaded media has no caption tracks; the a11y rule
             targets published video content. -->
        <!-- svelte-ignore a11y_media_has_caption -->
        <video controls autoplay src={murl(previewFile, "raw")}></video>
      {:else if previewFile.mime.startsWith('audio/')}
        <div class="pv-audio">
          {#if previewFile.has_thumb && !brokenThumbs.has(previewFile.id)}
            <img src={murl(previewFile, "thumb")} alt="" />
          {/if}
          <span class="pv-audio-name" title={previewFile.name}>{previewFile.name}</span>
          <audio controls autoplay src={murl(previewFile, "raw")}></audio>
        </div>
      {:else}
        <img src={murl(previewFile, "raw")} alt={previewFile.name} />
      {/if}
    </div>
    <div class="pv-bar">
      <span class="pv-name" title={previewFile.name}>{previewFile.name}</span>
      <span class="pv-meta muted">{humanSize(previewFile.size)}</span>
      <a class="icon-btn" href={murl(previewFile, "raw", true)} title={t('common.download')}>⬇</a>
      <button
        class="icon-btn"
        type="button"
        title={t('common.close')}
        onclick={() => closeDialog(PREVIEW_DIALOG)}
      >
        ✕
      </button>
    </div>
  {/if}
</dialog>

<Modal
  id={DELETE_DIALOG}
  title={t('files.deleteTitle')}
  onclose={(rv) => {
    if (rv === 'delete' && pendingFile) void remove(pendingFile);
    pendingFile = null;
  }}
>
  {#if pendingFile}
    <p>{t('files.deleteBody', { name: pendingFile.name, size: humanSize(pendingFile.size) })}</p>
  {/if}
  {#snippet actions()}
    <button class="btn" type="submit" {...closeAttrs(DELETE_DIALOG)}>{t('common.cancel')}</button>
    <button
      class="btn btn-danger"
      type="submit"
      value="delete"
      disabled={pendingFile !== null && deletingId === pendingFile.id}
    >
      {pendingFile !== null && deletingId === pendingFile.id ? t('common.deleting') : t('common.delete')}
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

  /* The dialog fades itself rather than delaying close(): `allow-discrete`
     keeps display/overlay alive for the exit, so Escape, a backdrop click
     and ✕ all still close on the tick they fire. The closed state is
     pointer-transparent, so a preview on its way out can never swallow a
     click, and the exit is deliberately shorter than the entry because
     `onclose` unmounts the media immediately. */
  dialog.preview {
    border: none;
    background: transparent;
    max-width: 100vw;
    max-height: 100dvh;
    width: 100%;
    height: 100%;
    margin: 0;
    padding: 0;
    overflow: hidden;
    opacity: 0;
    pointer-events: none;
    transition:
      opacity var(--dur-fast) var(--ease),
      overlay var(--dur-fast) var(--ease) allow-discrete,
      display var(--dur-fast) var(--ease) allow-discrete;
  }

  dialog.preview[open] {
    opacity: 1;
    pointer-events: auto;
    transition-duration: var(--dur);
    transition-timing-function: var(--ease-out);

    @starting-style {
      opacity: 0;
    }
  }

  dialog.preview::backdrop {
    background: rgba(4, 6, 10, 0.88);
    opacity: 0;
    transition:
      opacity var(--dur-fast) var(--ease),
      overlay var(--dur-fast) var(--ease) allow-discrete,
      display var(--dur-fast) var(--ease) allow-discrete;
  }

  dialog.preview[open]::backdrop {
    opacity: 1;
    transition-duration: var(--dur);

    @starting-style {
      opacity: 0;
    }
  }

  /* Contents mount with previewFile, i.e. on the tick the dialog opens, so
     a plain entrance animation is enough — no transition to coordinate. */
  dialog.preview[open] .pv-media {
    animation: pop var(--dur-slow) var(--ease-out) both;
  }

  dialog.preview[open] .pv-bar {
    animation: rise var(--dur) var(--ease-out) both;
  }

  .pv-media {
    display: flex;
    align-items: center;
    justify-content: center;
    height: calc(100dvh - 56px);
    padding: 12px;
  }

  .pv-media img,
  .pv-media video {
    max-width: 100%;
    max-height: 100%;
    object-fit: contain;
    border-radius: 6px;
  }

  .pv-audio {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 16px;
    max-width: min(480px, 90vw);
  }

  .pv-audio img {
    width: min(280px, 50dvh);
    height: min(280px, 50dvh);
    object-fit: cover;
    border-radius: 10px;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.5);
  }

  .pv-audio-name {
    font-size: 14px;
    font-weight: 600;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .pv-audio audio {
    width: 100%;
  }

  .pv-bar {
    display: flex;
    align-items: center;
    gap: 12px;
    height: 44px;
    padding: 0 16px;
    background: var(--panel);
    border-top: 1px solid var(--border);
  }

  .pv-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 13.5px;
  }

  .pv-meta {
    font-size: 12.5px;
    flex-shrink: 0;
  }

  .name-btn {
    border: none;
    background: none;
    color: inherit;
    font: inherit;
    padding: 0;
    text-align: left;
    cursor: default;
  }

  .name-btn.previewable {
    cursor: pointer;
  }

  .name-btn.previewable:hover {
    color: var(--accent);
    text-decoration: underline;
    text-underline-offset: 3px;
  }

  .g-thumb.previewable {
    cursor: zoom-in;
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
    position: relative;
    transition:
      border-color var(--dur-fast) var(--ease),
      box-shadow var(--dur-fast) var(--ease),
      opacity var(--dur) var(--ease),
      transform var(--dur-fast) var(--ease-out);
  }

  /* Hover lifts the card a hair; the press cancels the lift so clicking
     reads as pushing it back down instead of stacking two effects. */
  .card-f:hover {
    border-color: var(--accent);
    transform: translateY(-2px);
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.35);
  }

  .card-f:active {
    transform: none;
    box-shadow: none;
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
    transition: opacity var(--dur-fast) var(--ease);
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

  /* Thumbnails start transparent and fade in on load, so an image that
     decodes late does not flash into a row that has already settled. The
     fallback glyph is not animated: it renders on every search keystroke. */
  .g-img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
    opacity: 0;
    transition: opacity var(--dur) var(--ease-out);
  }

  .g-img.is-loaded,
  .thumb.is-loaded {
    opacity: 1;
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

  .tbl tr.cut td {
    opacity: 0.45;
  }

  .card-f.cut {
    opacity: 0.45;
    border-style: dashed;
  }

  /* A row mid-delete or mid-visibility-flip dims instead of just freezing.
     Declared after .cut so a cut row that is also busy reads as busy. */
  .card-f.busy,
  .tbl tr.busy td {
    opacity: 0.5;
    cursor: progress;
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
    transition:
      opacity var(--dur) var(--ease),
      background-color var(--dur-fast) var(--ease);
  }

  tbody tr {
    transition: background-color var(--dur-fast) var(--ease);
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
    opacity: 0;
    transition: opacity var(--dur) var(--ease-out);
  }

  .act-copy {
    white-space: nowrap;
  }

  .icon-btn.danger:hover:not(:disabled) {
    color: var(--danger);
  }

  /* Both the glyph slot and the label reserve their space unconditionally,
     so the 🔗 → ✓ swap pops in place instead of reflowing the row. */
  .act-ico {
    display: inline-block;
    width: 1.3em;
    text-align: center;
  }

  .act-chk {
    display: inline-block;
    color: var(--ok);
  }

  .act-lbl {
    display: inline-block;
    width: 44px;
    text-align: left;
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
  title={t('files.deleteBulkTitle')}
  onclose={(rv) => {
    if (rv === 'delete') void bulkDelete();
  }}
>
  <p>{t('files.deleteBulkBody', { n: selected.size })}</p>
  {#snippet actions()}
    <button class="btn" type="submit" {...closeAttrs(BULK_DIALOG)}>{t('common.cancel')}</button>
    <button class="btn btn-danger" type="submit" value="delete" disabled={bulkBusy}>
      {bulkBusy ? t('common.deleting') : t('common.delete')}
    </button>
  {/snippet}
</Modal>
