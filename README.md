# ii-drive

A personal file drive backed by Telegram, in the spirit of
[teldrive](https://github.com/teldrive/teldrive). Files are uploaded as
documents to a Telegram chat of your choice (default: Saved Messages) and
streamed back on demand — Telegram is the storage backend, this server is the
interface.
- **Backend:** Rust, [axum](https://github.com/tokio-rs/axum) +
  [grammers](https://github.com/Lonami/grammers) (MTProto), embedded
  [SurrealDB](https://surrealdb.com) (SurrealKv) for metadata.
- **Frontend:** [SvelteKit](https://kit.svelte.dev) (Svelte 5) with
  `adapter-static` — a pure-SPA build served straight from the Rust binary.
- **Max file size:** 2 GiB (Telegram bot-free account upload limit).

## Requirements

- Rust 1.85+ (2021 edition works; built with 1.98)
- [nub](https://crates.io/crates/nub) (all-in-one Node.js toolkit) for the web UI
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
   | `storage_chat` | `me` | Fallback storage target when the user has not picked channels |
   | `max_file_size` | `2GiB` | Upload cap; `2GiB`, `500MiB`, `2GB` (=2·10⁹), plain bytes |
   | `web_dist` | `web/dist` | Built SPA folder; API-only if missing |

2. **Build the web UI:**

   ```sh
   cd web && nub install && nub run build && cd ..
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
deletes keep working even if you change the selection later. Until a selection
is made, files go to `storage_chat` from the config.

### Split uploads

In **Settings** you can set a split threshold (e.g. 250 MB, with quick
presets; 0 = off). Files larger than the threshold are cut into parts that
upload **in parallel** over separate connections instead of one long stream —
typically much faster for big files. With several download bots configured,
each part can also be fetched by a different bot under its own rate limit,
which speeds downloads up as well. Parts are re-joined transparently when the
file is streamed or downloaded, and deleted together with the file. The
threshold only affects new uploads; existing files keep their layout.

### Troubleshooting login

If the server logs `AUTH_KEY_UNREGISTERED`, the stored MTProto session is stale
(a previous login never completed, or the session was revoked in Telegram).
The next sign-in attempt discards it automatically and creates a fresh one —
just log in again through the web UI.

## Usage
- **Upload:** drag & drop or browse; progress is tracked via `X-File-Size`.
- **Stream/download:** every file gets a stable public URL
  `/api/files/{id}/raw`; add `?dl=1` to force a download. Streams resume
  transparently when Telegram's file references expire mid-transfer.
- **Search:** substring match on file names from the search box.

## API sketch

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/api/auth/phone` | — | Start Telegram login (phone must be in `allowed_phones`) |
| POST | `/api/auth/code` | — | Submit login code; returns token or `password_required` |
| POST | `/api/auth/password` | — | Submit 2FA password; returns token |
| GET | `/api/files?q&folder&limit&offset` | token | List/search files (folder id, empty = root) |
| POST | `/api/files` | token | Upload (`X-File-Size` required, `X-Folder` optional) |
| GET/POST | `/api/folders` | token | List / create folders (nested via `parent`) |
| DELETE | `/api/folders/{id}` | token | Delete folder (must be empty) |
| DELETE | `/api/files/{id}` | token | Delete file + Telegram message |
| GET | `/api/files/{id}/raw` | — | Stream file (public, shareable) |
| GET/PUT | `/api/settings` | token | Upload-split threshold (`split_mb`, 0 = off) |
| GET | `/health` | — | Liveness probe |

## Development

```sh
cargo test            # offline unit tests (auth, config, db roundtrip)
cargo clippy
cd web && nub run dev  # Vite dev server with HMR (proxies /api to :8080)
```

The database is embedded — no external SurrealDB server. Both `data/` files
are safe to back up together; deleting them resets metadata (files remain in
Telegram) and logs you out of MTProto.

## Notes

- The raw endpoint is unauthenticated by design so links can be shared; keep
  the server on a trusted network or front it with your own auth proxy if
  that's not acceptable.
- Bot tokens added through the settings UI are stored **plaintext** in the
  embedded SurrealDB — treat `data/` like any other secret store.
- Uploads are streamed: memory use stays constant regardless of file size.
- Telegram free-account limits apply (2 GiB/file, ~4 GiB/day for free
  accounts); premium accounts raise both but this server caps at 2 GiB.
