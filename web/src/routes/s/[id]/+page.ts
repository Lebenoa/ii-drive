import { error } from '@sveltejs/kit';
import { shareMeta, type ShareMeta } from '$lib/api';

/** Client-side only (pure SPA): pull the public metadata or 404. */
export async function load({ params }: { params: { id: string } }): Promise<{
  meta: ShareMeta;
  id: string;
}> {
  try {
    return { meta: await shareMeta(params.id), id: params.id };
  } catch {
    error(404, 'File not found');
  }
}
