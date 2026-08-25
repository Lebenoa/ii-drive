/**
 * Runtime i18n. Only the English dictionary ships with the app; every
 * other language is downloaded from this repository (raw.githubusercontent)
 * when the user selects it. The choice persists in localStorage; English is
 * the fallback for missing keys.
 *
 * Dictionary files are nested JSON; keys are flattened to dot paths
 * (`login.title`). A top-level `_meta` object carries file metadata and is
 * ignored here.
 */

export type LocaleMeta = { code: string; name: string };

/** Where non-English dictionaries live: this repository's locales/ folder.
 *  Two mirrors are tried in order — raw.githubusercontent serves
 *  `access-control-allow-origin: *`, and jsDelivr mirrors the same files
 *  for networks where GitHub's CDN is slow or blocked. */
const REPO_BASES = [
	'https://raw.githubusercontent.com/Lebenoa/ii-drive/master/locales',
	'https://cdn.jsdelivr.net/gh/Lebenoa/ii-drive@master/locales',
];

const STORAGE_KEY = 'ii-drive.locale';
const FALLBACK = 'en';

let locale = $state(FALLBACK);
/** Flattened key -> translated text for the active language. */
let dict = $state<Record<string, string>>({});
/** English dictionary, loaded once as the fallback net. */
let fallbackDict = $state<Record<string, string>>({});
/** True once the initial locale has been resolved and fetched. */
let ready = $state(false);

function flatten(node: unknown, prefix = '', out: Record<string, string> = {}) {
	if (node === null || typeof node !== 'object') return out;
	for (const [key, value] of Object.entries(node as Record<string, unknown>)) {
		if (key === '_meta') continue;
		const path = prefix ? `${prefix}.${key}` : key;
		if (value !== null && typeof value === 'object') flatten(value, path, out);
		else if (typeof value === 'string') out[path] = value;
	}
	return out;
}

async function fetchDict(code: string): Promise<Record<string, string>> {
	// English ships beside the app; other languages come from the repo,
	// with the local server as a last-resort override point.
	//
	// Server paths are absolute on purpose: the server mounts them at
	// `/locales/`, and a relative URL resolves against the current route, so
	// a hard load of /settings/upload would ask for /settings/locales/en.json
	// — which the SPA fallback answers with index.html, leaving the whole UI
	// showing raw keys.
	if (code === FALLBACK) {
		return flatten(await (await fetch(`/locales/${code}.json`)).json());
	}
	for (const base of REPO_BASES) {
		try {
			const res = await fetch(`${base}/${encodeURIComponent(code)}.json`);
			if (res.ok) return flatten(await res.json());
		} catch {
			// try the next mirror
		}
	}
	const res = await fetch(`/locales/${encodeURIComponent(code)}.json`);
	if (!res.ok) throw new Error(`locale "${code}" not available`);
	return flatten(await res.json());
}

/**
 * Translate `key` in the active language, falling back to English and then
 * to the raw key itself (so a missing translation reads as its name rather
 * than an empty spot). `{name}` placeholders are replaced from `params`.
 */
export function t(key: string, params?: Record<string, string | number>): string {
	let s = dict[key] ?? fallbackDict[key] ?? key;
	if (params) {
		for (const [k, v] of Object.entries(params)) s = s.replaceAll(`{${k}}`, String(v));
	}
	return s;
}

/** Reactive accessor for the active language code. */
export function currentLocale(): string {
	return locale;
}

/** True once the startup locale has been loaded (gates first render). */
export function i18nReady(): boolean {
	return ready;
}

/**
 * Languages the switcher offers. The catalog lives in the repository next
 * to the dictionaries (`locales/manifest.json`), so a new language shows up
 * without touching the app. Falls back to whatever the local server ships,
 * then to bare English.
 */
export async function listLocales(): Promise<LocaleMeta[]> {
	const parse = (body: unknown): LocaleMeta[] => {
		const langs = (body as { languages?: unknown })?.languages;
		return Array.isArray(langs) ? langs : [];
	};
	// Catalog from the repo mirrors, then whatever the local server ships,
	// then bare English.
	for (const base of REPO_BASES) {
		try {
			const res = await fetch(`${base}/manifest.json`);
			if (res.ok) return parse(await res.json());
		} catch {
			// try the next mirror
		}
	}
	try {
		const res = await fetch('/locales/manifest.json');
		if (res.ok) return parse(await res.json());
	} catch {
		// Server without a locales folder — degrade to English only.
	}
	return [{ code: FALLBACK, name: FALLBACK }];
}

/** Switches language, downloads its dictionary if needed, remembers it. */
export async function setLocale(code: string): Promise<void> {
	const next = await fetchDict(code);
	dict = code === FALLBACK ? {} : next;
	if (code === FALLBACK) fallbackDict = next;
	locale = code;
	localStorage.setItem(STORAGE_KEY, code);
	document.documentElement.lang = code;
}

/**
 * Startup: restore the saved choice or fall back to English. English loads
 * either way — it doubles as the key-fallback dictionary. A total failure
 * (server without a locales folder) still resolves: t() shows raw keys.
 */
export async function initI18n(): Promise<void> {
	try {
		fallbackDict = await fetchDict(FALLBACK);
	} catch {
		// No en.json on the server — keep going with raw keys.
	}
	const saved = localStorage.getItem(STORAGE_KEY);
	if (saved && saved !== FALLBACK) {
		try {
			await setLocale(saved);
			ready = true;
			return;
		} catch {
			// Saved language vanished from the server — stay on English.
		}
	}
	document.documentElement.lang = FALLBACK;
	locale = FALLBACK;
	ready = true;
}
