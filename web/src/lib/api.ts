import { goto } from '$app/navigation';

const TOKEN_KEY = 'ii_token';

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}

export function setToken(token: string): void {
  localStorage.setItem(TOKEN_KEY, token);
}

export function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY);
}

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

/**
 * Reacts to session-invalid errors (HTTP 401 from the API): drops the local
 * token and bounces to the login page. Returns true when it fired so callers
 * can skip their own error handling.
 */
export function handleSessionInvalid(err: unknown): boolean {
  if (!(err instanceof ApiError) || err.status !== 401) return false;
  clearToken();
  if (!location.pathname.startsWith('/login')) void goto('/login');
  return true;
}


export interface TgUser {
  name: string;
}

/** GET /api/me */
export interface Me {
  connected: boolean;
  authorized: boolean;
  user: TgUser | null;
  error: string | null;
  /** true when Telegram demands a fresh sign-in */
  relogin?: boolean;
  /** true once the user picked storage channels */
  channel_selected?: boolean;
}

export interface DriveFile {
  id: string;
  name: string;
  mime: string;
  size: number;
  /** unix seconds */
  created_at: number;
  /** false = raw link needs a session token */
  public: boolean;
  /** true when a tiny server-side thumbnail exists */
  has_thumb: boolean;
}

interface ErrBody {
  error?: unknown;
}

function isErrBody(v: unknown): v is ErrBody {
  return typeof v === 'object' && v !== null && 'error' in v;
}

async function parseBody(res: Response): Promise<unknown> {
  try {
    return await res.json();
  } catch {
    return null;
  }
}

function errMessage(body: unknown, fallback: string): string {
  if (isErrBody(body)) {
    const e = body.error;
    if (typeof e === 'string' && e.length > 0) return e;
  }
  return fallback;
}

/** fetch wrapper; attaches bearer token unless `auth:false`. Rejects ApiError on !ok. */
export async function request<T>(
  path: string,
  opts: { method?: string; body?: unknown; auth?: boolean } = {},
): Promise<T> {
  const { method = 'GET', body, auth = true } = opts;
  const headers: Record<string, string> = {};
  if (auth && getToken()) headers['Authorization'] = `Bearer ${getToken()}`;
  if (body !== undefined) headers['Content-Type'] = 'application/json';

  const res = await fetch(path, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const data = await parseBody(res);
  if (!res.ok) {
    const msg = errMessage(data, `Request failed (${res.status})`);
    const apiErr = new ApiError(res.status, msg);
    handleSessionInvalid(apiErr);
    throw apiErr;
  }
  // Trusted boundary: backend serves the documented contract.
  return data as T;
}
/**
 * GET /api/avatar — the signed-in user's profile photo as a blob object URL.
 * Returns null when the account has no photo (404) — callers fall back to
 * the initial-letter avatar.
 */
export async function fetchAvatar(): Promise<string | null> {
  const headers: Record<string, string> = {};
  if (getToken()) headers['Authorization'] = `Bearer ${getToken()}`;
  const res = await fetch('/api/avatar', { headers });
  if (!res.ok) return null;
  return URL.createObjectURL(await res.blob());
}

interface AuthOk {
  status: 'ok';
  token?: unknown;
  channel_selected?: boolean;
}

/** GET /api/me */
export function getMe(): Promise<Me> {
  return request<Me>('/api/me');
}

/** GET /api/channels — candidate dialogs + current selection */
export interface ChannelInfo {
  chat: string;
  title: string;
}

export async function fetchChannels(): Promise<{
  available: ChannelInfo[];
  selected: ChannelInfo[];
}> {
  return await request('/api/channels');
}

/** GET /api/bot — configured download bots */
export interface BotEntry {
  id: number;
  username: string;
}

export async function getBots(): Promise<BotEntry[]> {
  const res = await request<{ bots: BotEntry[] }>('/api/bot');
  return res.bots;
}

export interface BotAddResult {
  chat: string;
  title: string;
  bot: string;
  ok: boolean;
  error: string | null;
}

/** POST /api/bot — sign a bot in, persist it, and wire it into storage channels */
export async function addBot(token: string): Promise<{
  bot: BotEntry;
  pool_size: number;
  results: BotAddResult[];
}> {
  return await request('/api/bot', { method: 'POST', body: { token } });
}

/** DELETE /api/bot/{id} — drop a bot from the pool */
export async function removeBot(id: number): Promise<void> {
  await request(`/api/bot/${id}`, { method: 'DELETE' });
}

/** POST /api/botfather — relay one message to @BotFather, get its reply. */
export function botfatherSend(text: string): Promise<{ reply: string }> {
  return request('/api/botfather', { method: 'POST', body: { text } });
}

/** POST /api/channels/create — creates a broadcast channel on Telegram */
export async function createChannel(title: string, about = ''): Promise<ChannelInfo> {
  const res = await request<{ channel: ChannelInfo }>('/api/channels/create', {
    method: 'POST',
    body: { title, about },
  });
  return res.channel;
}

/** POST /api/channels — persist the storage-channel selection */
export async function saveChannels(channels: ChannelInfo[]): Promise<void> {
  await request('/api/channels', { method: 'POST', body: { channels } });
}

export interface RouteRule {
  /** mime prefix, e.g. "image/" or "application/pdf" */
  mime: string;
  /** target folder uid */
  folder: string;
}

/** GET /api/rules — this user's auto-upload routing rules (ordered) */
export async function getRules(): Promise<RouteRule[]> {
  const res = await request<{ rules: RouteRule[] }>('/api/rules');
  return res.rules;
}

/** PUT /api/rules */
export async function saveRules(rules: RouteRule[]): Promise<void> {
  await request<{ ok: true }>('/api/rules', { method: 'PUT', body: { rules } });
}

export interface Settings {
  /** files larger than this many MiB upload as parallel parts; 0 = off */
  split_mb: number;
}

/** GET /api/settings */
export async function getSettings(): Promise<Settings> {
  return await request('/api/settings');
}

/** PUT /api/settings */
export async function saveSettings(settings: Settings): Promise<void> {
  await request('/api/settings', { method: 'PUT', body: settings });
}


function tokenFrom(res: AuthOk): string {
  if (typeof res.token === 'string') return res.token;
  throw new ApiError(500, 'Malformed login response');
}

/** POST /api/auth/phone -> {ok:true}; 401 when the phone is not allowlisted */
export async function sendLoginPhone(phone: string): Promise<void> {
  await request<{ ok: true }>('/api/auth/phone', { method: 'POST', body: { phone }, auth: false });
}

export type LoginResult =
  | { status: 'ok'; token: string }
  | { status: 'password_required'; hint?: string };

/** POST /api/auth/code — returns a token unless 2FA is enabled */
export async function sendLoginCode(code: string): Promise<LoginResult> {
  const res = await request<AuthOk | { status: 'password_required'; hint?: string }>(
    '/api/auth/code',
    { method: 'POST', body: { code }, auth: false },
  );
  if (res.status === 'ok') return { status: 'ok', token: tokenFrom(res) };
  return res;
}

/** POST /api/auth/password — 2FA step; returns the token on success */
export async function sendLoginPassword(password: string): Promise<LoginResult> {
  const res = await request<AuthOk>('/api/auth/password', {
    method: 'POST',
    body: { password },
    auth: false,
  });
  return { status: 'ok', token: tokenFrom(res) };
}

/** GET /api/files?q=&folder=&limit=&offset= — folder '' is the root */
export async function listFiles(
  q = '',
  folder = '',
  limit = 100,
  offset = 0,
): Promise<{ files: DriveFile[] }> {
  const params = new URLSearchParams({ q, folder, limit: String(limit), offset: String(offset) });
  return await request(`/api/files?${params}`);
}

export interface Folder {
  uid: string;
  name: string;
  /** parent folder uid, '' = root */
  parent: string;
}

/** GET /api/folders */
export async function listFolders(): Promise<Folder[]> {
  const res = await request<{ folders: Folder[] }>('/api/folders');
  return res.folders;
}

/** POST /api/folders */
export async function createFolder(name: string, parent = ''): Promise<Folder> {
  const res = await request<{ folder: Folder }>('/api/folders', {
    method: 'POST',
    body: { name, parent },
  });
  return res.folder;
}

/** DELETE /api/folders/{id} — server refuses when the folder is not empty */
export async function deleteFolder(id: string): Promise<void> {
  await request<{ ok: true }>(`/api/folders/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

/** DELETE /api/files/{id} */
export async function deleteFile(id: string): Promise<void> {
  await request<{ ok: true }>(`/api/files/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

/**
 * GET /api/files/{id}/link — time-limited single-file share URL for a
 * private file (does NOT carry the session token).
 */
export async function fileShareLink(id: string): Promise<string> {
  const res = await request<{ url: string }>(`/api/files/${encodeURIComponent(id)}/link`);
  return `${location.origin}${res.url}`;
}

/** PATCH /api/files/{id}/move — cut/paste target (folder '' = root) */
export async function moveFile(id: string, folder: string): Promise<void> {
  await request<{ ok: true }>(`/api/files/${encodeURIComponent(id)}/move`, {
    method: 'PATCH',
    body: { folder },
  });
}

/** PATCH /api/files/{id}/visibility — private by default, public opt-in */
export async function setFileVisibility(id: string, isPublic: boolean): Promise<void> {
  await request<{ ok: true }>(`/api/files/${encodeURIComponent(id)}/visibility`, {
    method: 'PATCH',
    body: { public: isPublic },
  });
}

/** Short-lived media token cache (backend TTL is 1h). */
let mediaToken: string | null = null;

async function ensureMediaToken(): Promise<string | null> {
  if (!getToken()) return null;
  if (!mediaToken) {
    try {
      const res = await request<{ token: string }>('/api/media-token');
      mediaToken = res.token;
    } catch {
      return null;
    }
  }
  return mediaToken;
}

function fileUrl(
  id: string,
  kind: 'raw' | 'thumb',
  extra: Record<string, string>,
  mt: string | null,
): string {
  const params = new URLSearchParams(extra);
  if (mt) params.set('mt', mt);
  const qs = params.toString();
  return `${location.origin}/api/files/${encodeURIComponent(id)}/${kind}${qs ? `?${qs}` : ''}`;
}

/**
 * URL for a file's raw stream. Public files work for anyone; private ones
 * carry a short-lived signed media token — never the session token, which
 * would leak via server logs, browser history and Referer headers.
 */
export async function rawUrl(id: string, download = false): Promise<string> {
  const mt = await ensureMediaToken();
  return fileUrl(id, 'raw', download ? { dl: '1' } : {}, mt);
}

/** GET /api/files/{id}/thumb — tiny cached JPEG, same auth rules as raw */
export async function thumbUrl(id: string): Promise<string> {
  const mt = await ensureMediaToken();
  return fileUrl(id, 'thumb', {}, mt);
}

let cachedMax: number | null = null;

/** GET /api/limits — server upload cap, fetched once per page load */
export async function maxFileSize(): Promise<number> {
  if (cachedMax === null) {
    const res = await request<{ max_file_size: number }>('/api/limits', { auth: false });
    cachedMax = typeof res.max_file_size === 'number' ? res.max_file_size : Number.POSITIVE_INFINITY;
  }
  return cachedMax;
}

/**
 * Upload via XHR so we get progress events.
 * Requires X-File-Size header + multipart field "file".
 * `folder` is the target folder id ('' = root), sent as X-Folder.
 */
export async function uploadFile(
  file: File,
  onProgress: (pct: number) => void,
  folder = '',
): Promise<DriveFile> {
  const max = await maxFileSize();
  if (file.size > max) {
    throw new ApiError(413, `File is larger than the server limit (${max} bytes)`);
  }

  const { promise, resolve, reject } = Promise.withResolvers<DriveFile>();

  const xhr = new XMLHttpRequest();
  xhr.open('POST', '/api/files');
  xhr.setRequestHeader('Authorization', `Bearer ${getToken() ?? ''}`);
  xhr.setRequestHeader('X-File-Size', String(file.size));
  if (folder) xhr.setRequestHeader('X-Folder', folder);
  xhr.responseType = 'json';

  xhr.upload.onprogress = (e) => {
    if (e.lengthComputable) onProgress(Math.min(100, Math.round((e.loaded / e.total) * 100)));
  };
  xhr.onerror = () =>
    reject(new ApiError(0, 'Connection lost during upload — check the file size and try again'));
  xhr.onabort = () => reject(new ApiError(0, 'Upload aborted'));
  xhr.onload = () => {
    const body: unknown = xhr.response;
    if (xhr.status < 200 || xhr.status >= 300) {
      const msg = errMessage(body, `Upload failed (${xhr.status})`);
      const apiErr = new ApiError(xhr.status, msg);
      handleSessionInvalid(apiErr);
      reject(apiErr);
      return;
    }
    resolve(
      body !== null && typeof body === 'object' && 'file' in body
        ? // Backend contract: {file: DriveFile}; no client-side validator worth its cost here.
          (body.file as DriveFile)
        : {
            id: '',
            name: file.name,
            mime: file.type || 'application/octet-stream',
            size: file.size,
            created_at: Math.floor(Date.now() / 1000),
          },
    );
  };

  const form = new FormData();
  form.append('file', file);
  xhr.send(form);
  return promise;
}
