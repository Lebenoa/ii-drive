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
  /**
   * True when this account may reach operator-only endpoints (config reload,
   * the internal-DB browser). Those answer 404 for everyone else, so the UI
   * must not offer them at all.
   */
  admin: boolean;
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
  opts: { method?: string; body?: unknown; auth?: boolean; signal?: AbortSignal } = {},
): Promise<T> {
  const { method = 'GET', body, auth = true, signal } = opts;
  const headers: Record<string, string> = {};
  if (auth && getToken()) headers['Authorization'] = `Bearer ${getToken()}`;
  if (body !== undefined) headers['Content-Type'] = 'application/json';

  const res = await fetch(path, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
    signal,
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

/** One line of a saved @BotFather transcript. */
export interface DraftMsg {
  who: 'me' | 'bf';
  text: string;
}

/**
 * A pending /newbot conversation. BotFather holds the question it asked, so
 * the wizard resumes this instead of starting a second one.
 * `stage` says what BotFather is waiting for.
 */
export type BotDraft =
  | { active: false }
  | {
      active: true;
      stage: 'name' | 'username' | 'token';
      token: string;
      updated_at: number;
      log: DraftMsg[];
    };

/** POST /api/botfather — relay one message to @BotFather, get its reply. */
export function botfatherSend(text: string): Promise<{ reply: string; draft: BotDraft }> {
  return request('/api/botfather', { method: 'POST', body: { text } });
}

/** GET /api/botfather/draft — the unfinished /newbot conversation, if any. */
export function botfatherDraft(): Promise<BotDraft> {
  return request('/api/botfather/draft');
}

/** DELETE /api/botfather/draft — /cancel at BotFather, then forget the draft. */
export function botfatherCancel(): Promise<{ ok: true; cancelled: boolean }> {
  return request('/api/botfather/draft', { method: 'DELETE' });
}

/** GET /api/botfather/bots — owned bot names parsed from BotFather's /mybots menu */
export function botfatherBots(): Promise<{ bots: string[] }> {
  return request('/api/botfather/bots');
}

/** POST /api/botfather/token {bot} — walk the menus to fetch one bot's API token */
export function botfatherToken(bot: string): Promise<{ token: string }> {
  return request('/api/botfather/token', { method: 'POST', body: { bot } });
}

/** Instance-wide upload settings. Operator-only: every other caller gets 404. */
export interface Instance {
  max_file_size: number;
  media_thumbs: boolean;
  upload_strategy: 'stream' | 'spill';
}

/** GET /api/instance — the instance-wide upload settings. */
export function getInstance(): Promise<Instance> {
  return request('/api/instance');
}

/**
 * PUT /api/instance — change instance-wide settings, effective immediately.
 *
 * Partial by design: fields left out keep their stored value, so two
 * operators editing different settings cannot clobber each other.
 */
export function saveInstance(patch: Partial<Instance>): Promise<Instance> {
  return request('/api/instance', { method: 'PUT', body: patch });
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
export interface BotWireFailure {
  chat: string;
  title: string;
  bot: string;
  error: string;
}

/** POST /api/channels — persist selection; also wires bots into new channels. */
export async function saveChannels(channels: ChannelInfo[]): Promise<BotWireFailure[]> {
  const res = await request<{ ok: true; results: BotWireFailure[] }>('/api/channels', {
    method: 'POST',
    body: { channels },
  });
  return res.results ?? [];
}

export interface RouteRule {
  /** mime prefix, e.g. "image/" or "application/pdf" */
  mime: string;
  /** target folder uid */
  folder: string;
}

/** GET /api/internal-db/tables — table names in the embedded store. */
export function dbTables(): Promise<{ tables: string[] }> {
  return request('/api/internal-db/tables');
}

export interface DbQueryResult {
  ok: boolean;
  result?: unknown[];
  error?: string;
}

/** POST /api/internal-db/query — run raw SurrealQL (internal admin tool). */
export function dbQuery(sql: string): Promise<{ results: DbQueryResult[] }> {
  return request('/api/internal-db/query', { method: 'POST', body: { sql } });
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

/**
 * POST /api/auth/phone -> {login_id}; 401 when the phone is not allowlisted.
 * The id identifies this attempt so concurrent sign-ins never share state.
 */
export async function sendLoginPhone(phone: string): Promise<string> {
  const res = await request<{ login_id: string }>('/api/auth/phone', {
    method: 'POST',
    body: { phone },
    auth: false,
  });
  if (typeof res.login_id !== 'string' || res.login_id.length === 0) {
    throw new ApiError(500, 'Malformed login response');
  }
  return res.login_id;
}

export type LoginResult =
  | { status: 'ok'; token: string }
  | { status: 'password_required'; hint?: string };

/** POST /api/auth/code — returns a token unless 2FA is enabled */
export async function sendLoginCode(loginId: string, code: string): Promise<LoginResult> {
  const res = await request<AuthOk | { status: 'password_required'; hint?: string }>(
    '/api/auth/code',
    { method: 'POST', body: { login_id: loginId, code }, auth: false },
  );
  if (res.status === 'ok') return { status: 'ok', token: tokenFrom(res) };
  return res;
}

/** POST /api/auth/password — 2FA step; returns the token on success */
export async function sendLoginPassword(loginId: string, password: string): Promise<LoginResult> {
  const res = await request<AuthOk>('/api/auth/password', {
    method: 'POST',
    body: { login_id: loginId, password },
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

/** GET /api/limits — server upload cap. Fetched per upload rather than
 * cached: the cap is a runtime-reloadable config field, and a stale value
 * here would make the UI reject uploads the server now allows. The request
 * is tiny next to any upload. */
export async function maxFileSize(): Promise<number> {
  const res = await request<{ max_file_size: number }>('/api/limits', { auth: false });
  return typeof res.max_file_size === 'number' ? res.max_file_size : Number.POSITIVE_INFINITY;
}

const UPLOAD_CHUNK = 8 * 1024 * 1024;

/** Thrown when an upload is cancelled through its AbortSignal. */
export class UploadCancelled extends Error {
  constructor() {
    super('Upload cancelled');
    this.name = 'UploadCancelled';
  }
}

export interface UploadOpts {
  signal?: AbortSignal;
}

/**
 * Chunked resumable upload: init -> PUT chunks at X-Offset -> complete.
 * Chunks retry with backoff; the server-acknowledged offset survives page
 * reloads in localStorage so a dropped connection resumes instead of
 * restarting a multi-gigabyte transfer. `onProgress` reports percent plus
 * an EMA-smoothed bytes/sec rate. Aborting `signal` stops the transfer,
 * tells the server to drop the spill, and rejects with UploadCancelled.
 */
export async function uploadFile(
  file: File,
  onProgress: (pct: number, speed: number) => void,
  folder = '',
  opts: UploadOpts = {},
): Promise<DriveFile> {
  // Abort checks live below, once the session id exists — cancelling must
  // also drop the server-side spill, which needs that id.
  // EMA-smoothed rate over chunk progress events: raw per-event deltas are
  // too spiky to read, but a 0.3 factor tracks direction changes within a
  // couple of updates.
  let ema = 0;
  let lastBytes = 0;
  let lastTime = performance.now();
  const tick = (bytes: number) => {
    const now = performance.now();
    const dt = (now - lastTime) / 1000;
    if (dt <= 0.05) return;
    const inst = (bytes - lastBytes) / dt;
    ema = ema === 0 ? inst : ema * 0.7 + inst * 0.3;
    lastBytes = bytes;
    lastTime = now;
  };

  const max = await maxFileSize();
  if (file.size > max) {
    throw new ApiError(413, `File is larger than the server limit (${max} bytes)`);
  }

  const resumeKey = `${folder}|${file.name}|${file.size}|${file.lastModified}`;
  let resumeMap: Record<string, { id: string }> = {};
  try {
    resumeMap = JSON.parse(localStorage.getItem('ii_uploads') ?? '{}');
  } catch {
    resumeMap = {};
  }

  let id: string | null = null;
  let received = 0;
  const remembered = resumeMap[resumeKey]?.id;
  if (remembered) {
    try {
      const st = await request<{ received: number }>(`/api/files/upload/${remembered}`);
      id = remembered;
      received = Math.min(st.received, file.size);
    } catch {
      // Session expired or purged — start over.
    }
  }
  // Abort always means "user cancelled". The server spill is dropped
  // explicitly on the cancel throw paths below — a signal listener cannot
  // own this job, because per-chunk XHR teardown would strip it after the
  // first successful chunk.
  const abandonSession = () => {
    if (!id) return false;
    void fetch(`/api/files/upload/${id}`, {
      method: 'DELETE',
      headers: { Authorization: `Bearer ${getToken() ?? ''}` },
    }).catch(() => {});
    delete resumeMap[resumeKey];
    localStorage.setItem('ii_uploads', JSON.stringify(resumeMap));
    id = null;
    return true;
  };
  const checkAbort = () => {
    if (opts.signal?.aborted) {
      abandonSession();
      throw new UploadCancelled();
    }
  };
  if (!id) {
    const r = await request<{ id: string }>('/api/files/upload/init', {
      method: 'POST',
      body: { size: file.size, name: file.name, mime: file.type, folder },
    });
    id = r.id;
  }
  resumeMap[resumeKey] = { id };
  localStorage.setItem('ii_uploads', JSON.stringify(resumeMap));

  const rawPut = (blob: Blob, offset: number) => {
    const { promise, resolve } = Promise.withResolvers<{
      status: number;
      body: unknown;
      loaded: number;
    }>();
    const xhr = new XMLHttpRequest();
    xhr.open('PUT', `/api/files/upload/${id}`);
    xhr.setRequestHeader('Authorization', `Bearer ${getToken() ?? ''}`);
    xhr.setRequestHeader('X-Offset', String(offset));
    let loaded = 0;
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) {
        loaded = e.loaded;
        tick(offset + loaded);
        onProgress(Math.min(99, Math.round(((offset + loaded) / file.size) * 100)), ema);
      }
    };
    xhr.onabort = () => {

      resolve({ status: -1, body: null, loaded });
    };
    xhr.onerror = () => {

      resolve({ status: 0, body: null, loaded });
    };
    xhr.onload = () => {

      let body: unknown = null;
      try {
        body = JSON.parse(xhr.responseText);
      } catch {
        /* non-JSON error body */
      }
      resolve({ status: xhr.status, body, loaded });
    };
    xhr.send(blob);
    return promise;
  };

  outer: while (received < file.size) {
    for (let attempt = 1; ; attempt++) {
      checkAbort();
      const end = Math.min(received + UPLOAD_CHUNK, file.size);
      const res = await rawPut(file.slice(received, end), received);
      if (res.status === -1) {
        abandonSession();
        throw new UploadCancelled();
      }
      if (res.status >= 200 && res.status < 300) {
        received = end;
        continue outer;
      }
      // Another tab advanced the session: re-sync rather than fail.
      if (res.status === 409) {
        const st = await request<{ received: number }>(`/api/files/upload/${id}`);
        received = Math.min(st.received, file.size);
        continue outer;
      }
      // Transient failures get bounded backoff; anything else is fatal.
      if ((res.status === 0 || res.status >= 500 || res.status === 429) && attempt <= 5) {
        await new Promise((r) => setTimeout(r, 400 * attempt));
        continue;
      }
      delete resumeMap[resumeKey];
      localStorage.setItem('ii_uploads', JSON.stringify(resumeMap));
      throw new ApiError(res.status || 0, errMessage(res.body, 'Connection lost during upload — check the file size and try again'));
    }
  }
  checkAbort();
  onProgress(99, ema);

  try {
    const done = await request<{ file: DriveFile }>(`/api/files/upload/${id}/complete`, {
      method: 'POST',
      signal: opts.signal,
    });
    delete resumeMap[resumeKey];
    localStorage.setItem('ii_uploads', JSON.stringify(resumeMap));
    onProgress(100, ema);
    return done.file;
  } catch (e) {
    // Keep the session record: complete is retryable while the spill
    // lives server-side.
    if (e instanceof ApiError && e.status === 0 && opts.signal?.aborted) {
      abandonSession();
      throw new UploadCancelled();
    }
    throw e;
  }
}
