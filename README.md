# ii-drive

A personal file drive backed by Telegram, in the spirit of
[teldrive](https://github.com/teldrive/teldrive). Files are uploaded as
documents to Telegram **storage channels** you pick after signing in and
streamed back on demand — Telegram is the storage backend, this server is
the interface.
- **Backend:** Rust, [axum](https://github.com/tokio-rs/axum) +
  [grammers](https://github.com/Lonami/grammers) (MTProto), embedded
  [SurrealDB](https://surrealdb.com) (SurrealKv) for metadata.
- **Frontend:** [SvelteKit](https://kit.svelte.dev) (Svelte 5) with
  `adapter-static` — a pure-SPA build served straight from the Rust binary.
- **Max file size:** 2 GiB (Telegram bot-free account upload limit).

## Requirements

- Rust 1.85+ (2021 edition works; built with 1.98)
- Node.js 18+ for the web UI — any package manager/runner works
  (npm, pnpm, bun, yarn, nub, …); examples below use `nub`
- A Telegram **api_id / api_hash** from <https://my.telegram.org/apps>
- A Telegram account (user account, not a bot) for uploads

## Setup

1. **Configure** — copy the example and edit:

   ```sh
   cp config.example.toml config.toml
   ```

   At minimum set `api_id`, `api_hash`, `allowed_phones`, and a long random
   `secret` (the secret signs session tokens; logging in happens through
   Telegram).

   | Option | Default | Meaning |
   |---|---|---|
   | `host` / `port` | `127.0.0.1:8080` | HTTP bind address |
   | `api_id` / `api_hash` | — | Telegram app credentials (required) |
   | `secret` | — | HMAC key for session tokens (required) |
   | `allowed_phones` | `[]` | Phone numbers that may log in via Telegram |
   | `token_ttl_secs` | 30 days | Web session lifetime |
   | `db_path` | `data/drive.surrealkv` | Embedded metadata store |
   | `session_path` | `data/session.db` | MTProto session (grammers SQLite) |
   | `max_file_size` | `2GiB` | Upload cap; `2GiB`, `500MiB`, `2GB` (=2·10⁹), plain bytes |
   | `web_dist` | `web/dist` | Built SPA folder; API-only if missing |
   | `media_thumbs` | `true` | Generate image/video thumbnails (ffmpeg on PATH for videos) |

2. **Build the web UI:**

   ```sh
   cd web && nub install && nub run build && cd ..   # or: npm/pnpm/bun install && npm/pnpm/bun run build
   ```

3. **Build and run the server:**

   ```sh
   cargo build --release
   ./target/release/ii-drive
   ```

4. **Log in** — open `http://127.0.0.1:8080` and sign in with a phone number
   listed in `allowed_phones`: Telegram sends you a login code; enter it (and
   your 2FA password, if enabled). The MTProto session persists to
   `session_path`; the issued web token stays valid for `token_ttl_secs`.


### Storage channels

After signing in you are asked to pick one or more **storage channels** (any
channel or group the account can post to, plus Saved Messages). The choice is
stored in SurrealDB and uploads are spread across the selected channels
round-robin; each file remembers which channel holds it, so downloads and
deletes keep working even if you change the selection later.
Settings lives under `/settings` with two categories:

- **Telegram** — pick storage channels, and manage download bots. Bots
  download files through their own rate limits, so several spread the load;
  adding a bot invites it into every storage channel as an admin. No token
  yet? A guided **@BotFather** chat in the UI walks you through `/newbot`
  and offers one-click add of the minted token.
- **Uploads** — split-upload threshold and auto-upload routing rules
  (mime-prefix → folder), as tabs.

### Split uploads

Files larger than the split threshold upload as parallel parts — one per
download bot plus your own account — and are re-joined transparently when
streamed or downloaded; parts are deleted together with the file. The
threshold only affects new uploads; existing files keep their layout.

### Troubleshooting login

If the server logs `AUTH_KEY_UNREGISTERED`, the stored MTProto session is stale
(a previous login never completed, or the session was revoked in Telegram).
The next sign-in attempt discards it automatically and creates a fresh one —
just log in again through the web UI.

## Usage
- **Upload:** drag & drop or browse; progress is tracked via `X-File-Size`.
- **Stream/download:** every file gets a stable URL `/api/files/{id}/raw`;
  add `?dl=1` to force a download. Files are private by default — URLs need
  the session (Authorization header), a short-lived media token, or a share
  link; flip visibility to public for shareable links. Streams resume
  transparently when Telegram's file references expire mid-transfer.
- **Search:** substring match on file names from the search box.

## API sketch

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/api/auth/phone` | — | Start Telegram login (phone must be in `allowed_phones`) |
| POST | `/api/auth/code` | — | Submit login code; returns token or `password_required` |
| POST | `/api/auth/password` | — | Submit 2FA password; returns token |
| GET | `/api/me` | token | Connection status + user info |
| GET/POST | `/api/channels` | token | List candidate dialogs / save storage-channel selection |
| GET/POST/DELETE | `/api/bot`(`/ {id}`) | token | Manage download bots |
| POST | `/api/botfather` | token | Relay one message to @BotFather; returns its reply |
| GET/PUT | `/api/settings` | token | Upload-split threshold (`split_mb`, 0 = off) |
| GET/PUT | `/api/rules` | token | Auto-upload routing rules |
| GET | `/api/files?q&folder&limit&offset` | token | List/search files (folder id, empty = root) |
| POST | `/api/files` | token | Upload (`X-File-Size` required, `X-Folder` optional) |
| PATCH | `/api/files/{id}/move` `/visibility` | token | Move file / toggle public-private |
| DELETE | `/api/files/{id}` | token | Delete file + Telegram parts |
| GET | `/api/files/{id}/raw` `/thumb` | — / token / `?mt=` / `?sig=` | Stream or thumbnail; public files need nothing |
| GET | `/api/files/{id}/link` | token | Mint time-limited share URL |
| GET/POST | `/api/folders`(`/{id}`) | token | List / create / delete folders |
| GET | `/api/avatar` `/media-token` | token | Profile photo bytes / short-lived media token |
| POST | `/api/config/reload` | token | Re-read config.toml (runtime fields hot-apply) |
| GET | `/health` | — | Liveness probe |

## Development

```sh
cargo test            # offline unit tests (auth, config, db roundtrip)
cargo clippy
cd web && nub run dev  # Vite dev server with HMR (proxies /api to :8080) — `npm run dev` etc. work the same
```

The database is embedded — no external SurrealDB server. Both `data/` files
are safe to back up together; deleting them resets metadata (files remain in
Telegram) and logs you out of MTProto.

## Notes

- Files are private by default; their URLs require the session, a
  short-lived media token (`?mt=`), or a time-limited share link. Only files
  explicitly made public are link-shareable. Either way, keep the server on
  a trusted network or front it with your own auth proxy.
- Bot tokens added through the settings UI are stored **plaintext** in the
  embedded SurrealDB — treat `data/` like any other secret store.
- Uploads are streamed: memory use stays constant regardless of file size.
- Telegram free-account limits apply (2 GiB/file, ~4 GiB/day for free
  accounts); premium accounts raise both but this server caps at 2 GiB.
