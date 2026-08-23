import { redirect } from '@sveltejs/kit';

// /settings has no page of its own — land on the first category.
export function load(): never {
  redirect(307, '/settings/telegram');
}
