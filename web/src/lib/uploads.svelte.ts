/**
 * Global upload queue. Lives outside any screen so the progress panel
 * survives client-side navigation — start an upload in the drive, walk
 * into settings, the transfers keep running and stay visible.
 */
import { uploadFile, UploadCancelled } from "$lib/api";
import { t } from "$lib/i18n.svelte";

export type QueueItem = {
    key: number;
    name: string;
    progress: number;
    state: "pending" | "uploading" | "done" | "error" | "cancelled";
    error: string;
    target: string;
    speed: number;
};

export const queue = $state<QueueItem[]>([]);
/** Bumped whenever an item lands, so file listings can refresh themselves. */
export const reloadTick = $state({ n: 0 });
export const panelCollapsed = $state({ value: false });

// Non-reactive plumbing: the File objects behind queue items and the
// abort controllers keyed per in-flight upload.
const filesByKey = new Map<number, File>();
const aborts = new Map<number, AbortController>();
let nextKey = 1;
let activeUploads = 0;

// Parallel uploads to the server: each item runs its own chunk pipeline,
// so a stalled transfer never starves the others.
const MAX_PARALLEL_UPLOADS = 3;

/** Queues files to upload into `target` (folder uid, "" = root). */
export function enqueue(list: FileList | File[], target: string): void {
    // Uploads target the folder open at drop time — navigating mid-queue
    // must not redirect the rest.
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
            speed: 0,
        });
    }
    pump();
}

export function cancelUpload(key: number): void {
    aborts.get(key)?.abort();
}

export function clearFinished(): void {
    for (const item of queue) filesByKey.delete(item.key);
    for (let i = queue.length - 1; i >= 0; i--) {
        if (queue[i].state !== "uploading" && queue[i].state !== "pending") {
            queue.splice(i, 1);
        }
    }
}

export function fmtSpeed(bps: number): string {
    if (bps <= 0) return "";
    if (bps >= 1024 * 1024) return `${(bps / 1024 / 1024).toFixed(1)} MB/s`;
    return `${Math.max(1, Math.round(bps / 1024))} KB/s`;
}

function pump(): void {
    while (activeUploads < MAX_PARALLEL_UPLOADS) {
        const item = queue.find((i) => i.state === "pending");
        if (!item) return;
        void startUpload(item);
    }
}

async function startUpload(item: QueueItem): Promise<void> {
    activeUploads++;
    item.state = "uploading";
    const ctrl = new AbortController();
    aborts.set(item.key, ctrl);
    const file = filesByKey.get(item.key);
    try {
        if (!file) {
            item.state = "error";
            item.error = "file handle lost";
            return;
        }
        await uploadFile(
            file,
            (pct, speed) => {
                item.progress = pct;
                item.speed = speed;
            },
            item.target,
            { signal: ctrl.signal },
        );
        item.state = "done";
        item.progress = 100;
        reloadTick.n++;
    } catch (err) {
        if (err instanceof UploadCancelled || ctrl.signal.aborted) {
            item.state = "cancelled";
            filesByKey.delete(item.key);
        } else {
            item.state = "error";
            item.error = err instanceof Error ? err.message : String(err);
        }
    } finally {
        aborts.delete(item.key);
        activeUploads--;
        pump();
    }
}
