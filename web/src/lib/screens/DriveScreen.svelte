<script lang="ts">
    import {
        createFolder,
        deleteFolder,
        listFiles,
        listFolders,
        moveFile,
        uploadFile,
        type DriveFile,
        type Folder,
    } from "$lib/api";
    import FileTable from "../components/FileTable.svelte";
    import Modal from "../components/Modal.svelte";
    import { closeAttrs, openDialog } from "$lib/invoker";
    import {
        collapse,
        fadeOnly,
        fadeUp,
        pop,
        stagger,
        flipDur,
    } from "$lib/motion";
    import { flip } from "svelte/animate";

    let folders = $state<Folder[]>([]);
    let current = $state(""); // folder uid, '' = root
    let q = $state("");
    let debouncedQ = $state("");
    let files = $state<DriveFile[]>([]);
    let loading = $state(false);
    let errorMsg = $state("");
    let sidebarError = $state("");
    let creating = $state(false);
    let deletingFolder = $state("");
    let reloadTick = $state(0);
    let seq = 0;

    // New-folder modal state.
    const NEW_FOLDER_DIALOG = "dlg-new-folder";
    let newName = $state("");
    let newFolderInput = $state<HTMLInputElement | null>(null);

    // Delete-folder modal state.
    const DEL_FOLDER_DIALOG = "dlg-del-folder";
    let pendingFolder = $state<Folder | null>(null);

    // Upload queue (lives here now that the file display is the drop zone).
    type QueueItem = {
        key: number;
        name: string;
        progress: number;
        state: "pending" | "uploading" | "done" | "error";
        error: string;
        target: string;
    };
    let queue = $state<QueueItem[]>([]);
  let panelCollapsed = $state(false);

  // Cut/paste: ids staged for a move into whichever folder gets the paste.
  let cutIds = $state<Set<string>>(new Set());
  let pasting = $state(false);
  let pasteError = $state('');

  // null, not '': '' is the root folder's own uid, so an empty-string rest
  // value made the "All files" row read as a permanent drop target.
  let dropTarget = $state<string | null>(null);
  let sidebarOpen = $state(false);
  let dropping = $state(false);

  async function dropOnFolder(e: DragEvent, folder: string): Promise<void> {
    e.preventDefault();
    dropping = false;
    dropTarget = null;
    const raw = e.dataTransfer?.getData('application/x-ii-files');
    if (!raw) return; // external file drop — handled by the file-area zone
    let ids: unknown;
    try {
      ids = JSON.parse(raw);
    } catch {
      return;
    }
    if (!Array.isArray(ids) || !ids.every((id) => typeof id === 'string' && id !== '')) return;
    let failed = 0;
    for (const id of ids) {
      try {
        await moveFile(id, folder);
      } catch {
        failed++;
      }
    }
    if (failed === 0) reloadTick++;
    else sidebarError = `${failed} file(s) failed to move`;
  }

  function folderDragOver(e: DragEvent, folder: string): void {
    if (!e.dataTransfer?.types.includes('application/x-ii-files')) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
    dropTarget = folder;
  }

  function cutFiles(ids: string[]): void {
    cutIds = new Set(ids);
    pasteError = '';
  }

  async function pasteHere(): Promise<void> {
    if (pasting || cutIds.size === 0) return;
    pasting = true;
    pasteError = '';
    let failed = 0;
    for (const id of cutIds) {
      try {
        await moveFile(id, current);
      } catch {
        failed++;
      }
    }
    pasting = false;
    if (failed === 0) {
      cutIds = new Set();
      reloadTick++;
    } else {
      pasteError = `${failed} file(s) failed to move`;
    }
  }
    let dragging = $state(false);
    let input = $state<HTMLInputElement | null>(null);
    const filesByKey = new Map<number, File>();
    let nextKey = 1;
    let pumping = false;

    // Folder tree, flattened depth-first with a depth per entry for indentation.
    let tree = $derived.by(() => {
        const byParent = new Map<string, Folder[]>();
        for (const f of folders) {
            const list = byParent.get(f.parent) ?? [];
            list.push(f);
            byParent.set(f.parent, list);
        }
        const out: { node: Folder; depth: number }[] = [];
        const walk = (parent: string, depth: number): void => {
            for (const f of byParent.get(parent) ?? []) {
                out.push({ node: f, depth });
                walk(f.uid, depth + 1);
            }
        };
        walk("", 0);
        return out;
    });

    function folderName(uid: string): string {
        return uid === ""
            ? "All files"
            : (folders.find((f) => f.uid === uid)?.name ?? "");
    }

    // debounce the search box (300ms) into the value actually queried
    $effect(() => {
        const value = q;
        const t = setTimeout(() => (debouncedQ = value), 300);
        return () => clearTimeout(t);
    });

    $effect(() => {
        const query = debouncedQ;
        const folder = current;
        void reloadTick;
        const mySeq = ++seq;
        loading = true;
        listFiles(query, folder)
            .then((res) => {
                if (mySeq !== seq) return;
                files = res.files;
                errorMsg = "";
            })
            .catch((err: unknown) => {
                if (mySeq !== seq) return;
                errorMsg = err instanceof Error ? err.message : String(err);
            })
            .finally(() => {
                if (mySeq === seq) loading = false;
            });
    });

    async function refreshFolders(): Promise<void> {
        try {
            folders = await listFolders();
            sidebarError = "";
        } catch (err) {
            sidebarError = err instanceof Error ? err.message : String(err);
        }
    }

    $effect(() => {
        void refreshFolders();
    });
    /** Opens the new-folder dialog with the name input focused. */
    function openNewFolder(): void {
        newName = "";
        openDialog(NEW_FOLDER_DIALOG);
        // Native autofocus is flagged by a11y linting; focus
        // programmatically once the modal is up.
        queueMicrotask(() => newFolderInput?.focus());
    }

    async function addFolder(name: string): Promise<void> {
        if (creating) return;
        const trimmed = name.trim();
        if (!trimmed) return;
        creating = true;
        try {
            await createFolder(trimmed, current);
            await refreshFolders();
        } catch (err) {
            sidebarError = err instanceof Error ? err.message : String(err);
        } finally {
            creating = false;
        }
    }

    async function dropFolder(uid: string): Promise<void> {
        if (deletingFolder) return;
        deletingFolder = uid;
        try {
            await deleteFolder(uid);
            if (current === uid) current = "";
            await refreshFolders();
        } catch (err) {
            sidebarError = err instanceof Error ? err.message : String(err);
        } finally {
            deletingFolder = "";
        }
    }

    function enqueue(list: FileList | File[]): void {
        // Uploads target the folder open at drop time — navigating mid-queue
        // must not redirect the rest.
        const target = current;
        for (const file of Array.from(list)) {
            const key = nextKey++;
            filesByKey.set(key, file);
            queue.push({
                key,
                name: file.name,
                progress: 0,
                state: "pending",
                error: "",
                target,
            });
        }
        void pump();
    }

    async function pump(): Promise<void> {
        if (pumping) return;
        pumping = true;
        try {
            while (true) {
                const item = queue.find((i) => i.state === "pending");
                if (!item) break;
                const file = filesByKey.get(item.key);
                item.state = "uploading";
                if (!file) {
                    item.state = "error";
                    item.error = "File handle lost";
                    continue;
                }
                try {
                    await uploadFile(
                        file,
                        (pct) => {
                            item.progress = pct;
                        },
                        item.target,
                    );
                    item.state = "done";
                    item.progress = 100;
                    reloadTick++;
                } catch (err) {
                    item.state = "error";
                    item.error =
                        err instanceof Error ? err.message : String(err);
                }
            }
        } finally {
            pumping = false;
        }
    }

    function clearFinished(): void {
        for (const item of queue) filesByKey.delete(item.key);
        queue = queue.filter(
            (i) => i.state === "uploading" || i.state === "pending",
        );
    }

    function onDrop(e: DragEvent): void {
        e.preventDefault();
        dragging = false;
        if (e.dataTransfer && e.dataTransfer.files.length > 0)
            enqueue(e.dataTransfer.files);
    }

    function onDragOver(e: DragEvent): void {
        e.preventDefault();
        if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
        dragging = true;
    }

    function pick(e: Event): void {
        const el = e.currentTarget as HTMLInputElement;
        if (el.files && el.files.length > 0) enqueue(el.files);
        el.value = "";
    }
</script>

<div class="shell-body">
    <div class="layout">
        {#if sidebarOpen}
            <div
                class="sidebar-backdrop"
                aria-hidden="true"
                onclick={() => (sidebarOpen = false)}
                transition:fadeOnly
            ></div>
        {/if}
        <aside class="sidebar" class:open={sidebarOpen}>
            <div class="side-head">
                <span class="side-title">Folders</span>
                <button
                    class="icon-btn"
                    type="button"
                    title="New folder in current location"
                    disabled={creating}
                    onclick={openNewFolder}
                    >
                    <span aria-hidden="true">+</span>
                </button>
            </div>
            <nav class="folder-list">
                <button
                    class="folder-item"
                    class:active={current === ""}
                    class:drop={dropTarget === ""}
                    type="button"
                    onclick={() => {
                            current = "";
                            sidebarOpen = false;
                        }}
                    ondragover={(e) => folderDragOver(e, "")}
                    ondragleave={() => (dropTarget = null)}
                    ondrop={(e) => void dropOnFolder(e, "")}
                >
                    <span class="f-ico">📁</span><span class="f-name"
                        >All files</span
                    >
                </button>
                {#each tree as { node, depth }, i (node.uid)}
                    <button
                        class="folder-item"
                        class:active={current === node.uid}
                        class:drop={dropTarget === node.uid}
                        style={`padding-left:${12 + depth * 16}px`}
                        type="button"
                        onclick={() => {
                            current = node.uid;
                            sidebarOpen = false;
                        }}
                        ondragover={(e) => folderDragOver(e, node.uid)}
                        ondragleave={() => (dropTarget = null)}
                        ondrop={(e) => void dropOnFolder(e, node.uid)}
                        animate:flip={{ duration: flipDur() }}
                        transition:fadeUp={{ delay: stagger(i) }}
                    >
                        <span class="f-ico">📁</span><span
                            class="f-name"
                            title={node.name}>{node.name}</span
                        >
                        <span
                            class="f-del"
                            class:busy={deletingFolder === node.uid}
                            role="button"
                            tabindex="-1"
                            title={deletingFolder === node.uid
                                ? "Deleting…"
                                : "Delete folder"}
                            onclick={(e) => {
                                e.stopPropagation();
                                pendingFolder = node;
                                openDialog(DEL_FOLDER_DIALOG);
                            }}
                            onkeydown={(e) => e.stopPropagation()}
                        >
                            {deletingFolder === node.uid ? "…" : "✕"}
                        </span>
                    </button>
                {/each}
            </nav>
            {#if sidebarError}<p class="error-text side-error">
                    {sidebarError}
                </p>{/if}
        </aside>

        <section
            class="file-area"
            class:dragging
            aria-label="Files — drop here to upload into the current folder"
            ondragover={onDragOver}
            ondragleave={() => (dragging = false)}
            ondrop={onDrop}
        >
            <div class="area-head">
                <button
                    class="icon-btn sidebar-toggle"
                    type="button"
                    aria-label="Folders"
                    aria-expanded={sidebarOpen}
                    onclick={() => (sidebarOpen = !sidebarOpen)}
                >
                    ☰
                </button>
                {#key current}
                    <h2 class="area-title" in:fadeUp={{ y: 4 }}>
                        {folderName(current)}
                    </h2>
                {/key}
                <div class="search-row">
                    <input
                        class="field search"
                        type="search"
                        placeholder="Search in this folder…"
                        bind:value={q}
                        aria-label="Search files"
                    />
                    {#if loading}<div
                            class="spinner small"
                            aria-hidden="true"
                            transition:fadeOnly
                        ></div>{/if}
                    <button
                        class="btn"
                        type="button"
                        onclick={() => input?.click()}>Upload</button
                    >
                </div>
            </div>
            {#if cutIds.size > 0}
                <div
                    class="paste-bar"
                    role="toolbar"
                    aria-label="Paste"
                    transition:collapse
                >
                    <span class="bulk-count">{cutIds.size} cut</span>
                    <button
                        class="btn btn-primary"
                        type="button"
                        disabled={pasting}
                        onclick={() => void pasteHere()}
                    >
                        {pasting ? 'Moving…' : `Paste into "${folderName(current)}"`}
                    </button>
                    <button class="btn ghost" type="button" onclick={() => (cutIds = new Set())}>
                        Cancel
                    </button>
                    {#if pasteError}<span class="error-text">{pasteError}</span>{/if}
                </div>
            {/if}
            <input
                bind:this={input}
                class="hidden-input"
                type="file"
                multiple
                onchange={pick}
            />


            {#if errorMsg}
                <p class="error-text">{errorMsg}</p>
            {:else}
                <FileTable
                {files}
                onDeleted={() => reloadTick++}
                {cutIds}
                onCut={cutFiles}
            />
            {/if}

            <div class="drop-hint muted" aria-hidden="true">
                Drop files to upload here
            </div>
        </section>

        {#if queue.length > 0}
            <div class="uploads-panel" transition:fadeUp={{ y: 10 }}>
                <button
                    class="up-head"
                    type="button"
                    onclick={() => (panelCollapsed = !panelCollapsed)}
                    aria-expanded={!panelCollapsed}
                >
                    <span class="up-title">
                        {#if queue.filter((i) => i.state === "uploading" || i.state === "pending").length > 0}
                            Uploading {queue.filter((i) => i.state === "uploading" || i.state === "pending").length} of {queue.length}
                        {:else}
                            Uploads ({queue.length})
                        {/if}
                    </span>
                    {#if queue.every((i) => i.state === "done" || i.state === "error")}
                        <span
                            class="up-clear"
                            role="button"
                            tabindex="-1"
                            title="Clear finished"
                            onclick={(e) => {
                                e.stopPropagation();
                                clearFinished();
                            }}
                            onkeydown={(e) => e.stopPropagation()}
                        >
                            ✕
                        </span>
                    {:else}
                        <span class="up-chevron">{panelCollapsed ? "▲" : "▼"}</span>
                    {/if}
                </button>
                {#if !panelCollapsed}
                    <ul class="queue" transition:collapse>
                        {#each queue as item, i (item.key)}
                            <li
                                class="q-item"
                                class:error={item.state === "error"}
                                animate:flip={{ duration: flipDur() }}
                                transition:fadeUp={{ delay: stagger(i, 12, 120) }}
                            >
                                <div class="q-top">
                                    <span class="q-name" title={item.name}>{item.name}</span>
                                    <span
                                        class="q-state"
                                        class:ok={item.state === "done"}
                                    >
                                        {#if item.state === "pending"}
                                            queued
                                        {:else if item.state === "uploading"}
                                            {item.progress}%
                                        {:else if item.state === "done"}
                                            <span class="q-tick" in:pop>✓</span>
                                        {:else}
                                            ✗
                                        {/if}
                                    </span>
                                </div>
                                <div class="bar">
                                    <div
                                        class="fill"
                                        class:err={item.state === "error"}
                                        class:done={item.state === "done"}
                                        style={`width:${item.progress}%`}
                                    ></div>
                                </div>
                                {#if item.state === "error"}
                                    <p class="error-text">{item.error}</p>
                                {/if}
                            </li>
                        {/each}
                    </ul>
                {/if}
            </div>
        {/if}

        <!-- Phones have no drag &amp; drop: dedicated upload button. -->
        <button
            class="upload-fab"
            type="button"
            aria-label="Upload files"
            title="Upload to {folderName(current)}"
            onclick={() => input?.click()}
        >
            +
        </button>
    </div>

    <Modal
        id={NEW_FOLDER_DIALOG}
        title="New folder"
        onclose={(rv) => {
            if (rv === "create") void addFolder(newName);
        }}
    >
        <p class="muted modal-hint">
            Create a folder inside "{folderName(current)}".
        </p>
        <input
            class="field"
            type="text"
            placeholder="Folder name"
            maxlength="128"
            bind:value={newName}
            bind:this={newFolderInput}
        />
        {#snippet actions()}
            <button class="btn" type="submit" {...closeAttrs(NEW_FOLDER_DIALOG)}>
                Cancel
            </button>
            <button
                class="btn btn-primary"
                type="submit"
                value="create"
                disabled={!newName.trim()}
            >
                Create
            </button>
        {/snippet}
    </Modal>

    <Modal
        id={DEL_FOLDER_DIALOG}
        title="Delete folder"
        onclose={(rv) => {
            if (rv === "delete" && pendingFolder)
                void dropFolder(pendingFolder.uid);
            pendingFolder = null;
        }}
    >
        {#if pendingFolder}
            <p>
                Delete <strong>{pendingFolder.name}</strong>? The folder must be
                empty — files and subfolders inside it are kept and must be
                removed first.
            </p>
        {/if}
        {#snippet actions()}
            <button class="btn" type="submit" {...closeAttrs(DEL_FOLDER_DIALOG)}>
                Cancel
            </button>
            <button class="btn btn-danger" type="submit" value="delete"
                >Delete</button
            >
        {/snippet}
    </Modal>
</div>

<style>
    /* Replaces the shell the old TopBar provided: full remaining viewport
       column (navbar is 53px) and the old content wrapper's width/padding. */
    .shell-body {
        height: calc(100dvh - 53px);
        display: flex;
        flex-direction: column;
        width: min(1200px, 100%);
        margin: 0 auto;
        padding: 20px 20px 24px;
        box-sizing: border-box;
    }

    .layout {
        display: flex;
        gap: 20px;
        align-items: stretch;
        flex: 1;
        min-height: 0;
    }

    .sidebar {
        width: 230px;
        flex-shrink: 0;
        border: 1px solid var(--border);
        border-radius: var(--radius);
        background: var(--panel);
        padding: 10px 8px;
        overflow-y: auto;
        min-height: 0;
    }

    .side-head {
        display: flex;
        align-items: center;
        justify-content: space-between;
        padding: 2px 6px 8px;
    }

    .side-title {
        font-size: 12px;
        font-weight: 600;
        text-transform: uppercase;
        letter-spacing: 0.6px;
        color: var(--muted);
    }

    .folder-list {
        display: flex;
        flex-direction: column;
        gap: 2px;
    }

    .folder-item {
        display: flex;
        align-items: center;
        gap: 8px;
        width: 100%;
        border: none;
        background: none;
        color: var(--text);
        font: inherit;
        font-size: 13.5px;
        text-align: left;
        padding: 6px 8px;
        border-radius: 6px;
        cursor: pointer;
        /* Outline never participates in layout, so the drag affordance can
           appear without nudging the row the way a border would. */
        outline: 1px dashed transparent;
        outline-offset: -1px;
        transition:
            background var(--dur-fast) var(--ease),
            color var(--dur-fast) var(--ease),
            box-shadow var(--dur-fast) var(--ease),
            outline-color var(--dur-fast) var(--ease);
    }

    /* The transparent outline above would otherwise swallow the UA focus
       ring, so borrow the app-wide ring instead. */
    .folder-item:focus-visible {
        outline-color: transparent;
        box-shadow: 0 0 0 3px rgba(91, 157, 255, 0.35);
    }

    .folder-item:hover {
        background: rgba(255, 255, 255, 0.05);
    }

    .folder-item.drop {
        outline-color: var(--accent);
        background: rgba(91, 157, 255, 0.12);
    }

    .folder-item.active {
        background: rgba(91, 157, 255, 0.14);
        font-weight: 600;
    }

    .f-ico {
        flex-shrink: 0;
    }

    .f-name {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
        flex: 1;
    }

    .f-del {
        color: var(--muted);
        padding: 0 4px;
        border-radius: 4px;
        flex-shrink: 0;
        transition:
            color var(--dur-fast) var(--ease),
            background var(--dur-fast) var(--ease);
    }

    .f-del:hover {
        color: var(--danger);
        background: rgba(255, 255, 255, 0.06);
    }

    /* Mid-delete the glyph swaps to "…" — accent it and pop once so the
       change registers, rather than looping a second spinner. */
    .f-del.busy {
        color: var(--accent);
        animation: pop var(--dur-fast) var(--ease-spring);
    }

    .side-error {
        font-size: 12px;
        margin: 8px 6px 0;
    }

    .file-area {
        flex: 1;
        min-width: 0;
        min-height: 0;
        display: flex;
        flex-direction: column;
        border: 2px dashed transparent;
        border-radius: var(--radius);
        padding: 4px;
        transition:
            border-color var(--dur) var(--ease),
            background var(--dur) var(--ease),
            box-shadow var(--dur) var(--ease);
        overflow-y: auto;
    }

    .file-area.dragging {
        border-color: var(--accent);
        background: rgba(91, 157, 255, 0.06);
        box-shadow: inset 0 0 0 1px rgba(91, 157, 255, 0.22);
    }

    .area-head {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 12px;
        flex-wrap: wrap;
        margin-bottom: 12px;
    }

    .area-title {
        margin: 0;
        font-size: 17px;
    }

    .paste-bar {
        display: flex;
        align-items: center;
        flex-wrap: wrap;
        gap: 8px;
        margin: 0 0 12px;
        padding: 8px 10px;
        border: 1px dashed var(--accent);
        border-radius: var(--radius);
        background: rgba(91, 157, 255, 0.08);
    }

    .paste-bar .bulk-count {
        font-weight: 600;
        font-size: 13.5px;
        margin-right: 6px;
    }

    .search-row {
        display: flex;
        align-items: center;
        gap: 10px;
    }

    .search {
        width: min(300px, 40vw);
    }

    .spinner.small {
        width: 16px;
        height: 16px;
        border-width: 2px;
    }

    .hidden-input {
        display: none;
    }

    .queue {
        list-style: none;
        margin: 0;
        padding: 10px 12px;
        display: flex;
        flex-direction: column;
        gap: 8px;
        max-height: 260px;
        overflow-y: auto;
    }

    .q-item {
        background: var(--panel-2);
        border: 1px solid var(--border);
        border-radius: 8px;
        padding: 8px 12px;
    }

    .q-item.error {
        border-color: #5b2730;
    }

    .q-top {
        display: flex;
        justify-content: space-between;
        gap: 12px;
        font-size: 13px;
        margin-bottom: 6px;
    }

    .q-name {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
    }

    .q-state {
        color: var(--muted);
        flex-shrink: 0;
        transition: color var(--dur) var(--ease);
    }

    .q-state.ok {
        color: var(--ok);
    }

    /* `pop` scales, which a bare inline box would ignore. */
    .q-tick {
        display: inline-block;
    }

    .bar {
        height: 4px;
        border-radius: 2px;
        background: #10141c;
        overflow: hidden;
    }

    .fill {
        height: 100%;
        background: var(--accent);
        border-radius: 2px;
        transition:
            width var(--dur) var(--ease-out),
            background var(--dur) var(--ease);
    }

    .fill.done {
        background: var(--ok);
    }

    .fill.err {
        background: var(--danger);
    }


    .drop-hint {
        margin-top: auto;
        text-align: center;
        font-size: 12px;
        padding: 10px 0 2px;
        opacity: 0.7;
        transition:
            opacity var(--dur) var(--ease),
            color var(--dur) var(--ease);
    }

    .file-area.dragging .drop-hint {
        opacity: 1;
        color: var(--accent);
    }

    .upload-fab {
        display: none;
    }

    .uploads-panel {
        position: fixed;
        right: 20px;
        bottom: 20px;
        width: min(340px, calc(100vw - 40px));
        border: 1px solid var(--border);
        border-radius: var(--radius);
        background: var(--panel);
        box-shadow: 0 12px 36px rgba(0, 0, 0, 0.45);
        z-index: 30;
        overflow: hidden;
    }

    .up-head {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 10px;
        width: 100%;
        border: none;
        background: var(--panel-2);
        color: var(--text);
        font: inherit;
        font-size: 13.5px;
        text-align: left;
        padding: 10px 12px;
        cursor: pointer;
    }

    .up-title {
        font-weight: 600;
    }

    .up-clear,
    .up-chevron {
        color: var(--muted);
        transition: color var(--dur-fast) var(--ease);
    }

    .up-clear:hover {
        color: var(--danger);
    }

    .sidebar-toggle {
        display: none;
        font-size: 18px;
    }

    .sidebar-backdrop {
        display: none;
    }

    @media (max-width: 720px) {
        .layout {
            flex-direction: column;
        }

        .sidebar-toggle {
            display: inline-block;
        }

        .sidebar {
            position: fixed;
            top: 0;
            left: 0;
            bottom: 0;
            width: min(280px, 82vw);
            z-index: 40;
            border-radius: 0 var(--radius) var(--radius) 0;
            transform: translateX(-100%);
            transition: transform var(--dur) var(--ease-out);
            box-shadow: 0 0 40px rgba(0, 0, 0, 0.5);
        }

        .sidebar.open {
            transform: none;
        }

        .sidebar-backdrop {
            display: block;
            position: fixed;
            inset: 0;
            background: rgba(6, 9, 15, 0.55);
            z-index: 39;
        }

        .uploads-panel {
            bottom: 86px;
        }

        .upload-fab {
            display: flex;
            align-items: center;
            justify-content: center;
            position: fixed;
            right: 18px;
            bottom: 18px;
            width: 56px;
            height: 56px;
            border-radius: 50%;
            border: none;
            background: var(--accent-strong);
            color: #fff;
            font-size: 30px;
            line-height: 1;
            cursor: pointer;
            box-shadow: 0 8px 24px rgba(0, 0, 0, 0.4);
            z-index: 20;
            transition:
                transform var(--dur-fast) var(--ease),
                background var(--dur-fast) var(--ease);
        }

        .upload-fab:active {
            background: var(--accent);
            transform: scale(0.92);
        }
    }

    .modal-hint {
        margin: 0 0 10px;
        font-size: 12.5px;
    }
</style>
