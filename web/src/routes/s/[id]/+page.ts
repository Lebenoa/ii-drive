import type { ShareMeta } from '$lib/api';

/** Data handed to the page. `meta` is null until the client fetch lands. */
export interface Data {
	id: string;
	meta: ShareMeta | null;
}

/**
 * Pure SPA: don't block `load` on the metadata fetch — that leaves a blank
 * screen during the round-trip. Return the id now, let the page render a
 * loading skeleton and fetch/404 client-side.
 */
export function load({ params }: { params: { id: string } }): Data {
	return { id: params.id, meta: null };
}