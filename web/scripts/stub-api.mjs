// Dev-only stub backend implementing the documented /api contract for UI smoke testing.
// NOT part of the app build; lives in web/scripts.
import { createServer } from 'node:http';

const PORT = 8080;
// SCENARIO=unconnected|unauthorized controls the /api/me shape (default: fully connected)
const SCENARIO = process.env.SCENARIO || 'ok';
let tokenCounter = 1;
let tgAuthorized = SCENARIO !== 'unauthorized';
const files = [
  { id: 'f3', name: 'holiday.jpg', mime: 'image/jpeg', size: 2_450_000, created_at: Math.floor(Date.now() / 1000) - 7200 },
  { id: 'f2', name: 'backup.tar.gz', mime: 'application/gzip', size: 52_428_800, created_at: Math.floor(Date.now() / 1000) - 86400 * 3 },
  { id: 'f1', name: 'notes.md', mime: 'text/markdown', size: 812, created_at: Math.floor(Date.now() / 1000) - 86400 * 45 },
];

function send(res, status, body) {
  const data = JSON.stringify(body);
  res.writeHead(status, { 'Content-Type': 'application/json' });
  res.end(data);
}

async function readJson(req) {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  try {
    return JSON.parse(Buffer.concat(chunks).toString('utf8') || '{}');
  } catch {
    return {};
  }
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url, `http://127.0.0.1:${PORT}`);
  const p = url.pathname;
  const auth = req.headers.authorization ?? '';

  if (p === '/api/auth/login' && req.method === 'POST') {
    const body = await readJson(req);
    if (body.password === 'hunter2') return send(res, 200, { token: 'stub-token' });
    return send(res, 401, { error: 'invalid password' });
  }
  if (p === '/api/me') {
    if (auth !== 'Bearer stub-token') return send(res, 401, { error: 'unauthorized' });
    if (SCENARIO === 'unconnected')
      return send(res, 200, { connected: false, authorized: false, user: null, error: 'Telegram not configured — set api_id/api_hash' });
    return send(res, 200, { connected: true, authorized: tgAuthorized, user: tgAuthorized ? { name: 'Stub User' } : null, error: null });
  }
  if (p === '/api/tg/signin' && req.method === 'POST') {
    void (async () => { for await (const c of req) void c; })();
    if (!tgAuthorized) {
      tgAuthorized = true;
      return send(res, 200, { status: 'ok' });
    }
    return send(res, 200, { status: 'ok' });
  }
  if (p === '/api/files' && req.method === 'GET') {
    const q = (url.searchParams.get('q') || '').toLowerCase();
    const matched = files.filter((f) => f.name.toLowerCase().includes(q));
    return send(res, 200, { files: matched });
  }
  if (p === '/api/files' && req.method === 'POST') {
    // multipart smoke response; size reported via X-File-Size
    const sizeHeader = req.headers['x-file-size'];
    const id = `u${tokenCounter++}`;
    const f = { id, name: `upload-${id}.bin`, mime: 'application/octet-stream', size: Number(sizeHeader || 0), created_at: Math.floor(Date.now() / 1000) };
    files.unshift(f);
    void Promise.all([req]); // drain body
    for await (const c of req) void c;
    return send(res, 200, { file: f });
  }
  const del = p.match(/^\/api\/files\/([^/]+)$/);
  if (del && req.method === 'DELETE') {
    const i = files.findIndex((f) => f.id === decodeURIComponent(del[1]));
    if (i >= 0) files.splice(i, 1);
    return send(res, 200, { ok: true });
  }
  const raw = p.match(/^\/api\/files\/([^/]+)\/raw$/);
  if (raw) {
    res.writeHead(200, { 'Content-Type': 'text/plain', 'Content-Disposition': url.searchParams.get('dl') === '1' ? 'attachment' : 'inline' });
    return res.end(`raw content of ${decodeURIComponent(raw[1])}`);
  }
  if (p.startsWith('/api/tg/')) return send(res, 200, { status: 'ok' });
  send(res, 404, { error: 'not found' });
});

server.listen(PORT, '127.0.0.1', () => console.log(`stub api on ${PORT}`));
