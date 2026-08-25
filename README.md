# ii-drive

A personal file drive backed by Telegram, in the spirit of
[teldrive](https://github.com/tgdrive/teldrive). Files are uploaded as
documents to Telegram **storage channels** you pick after signing in and
streamed back on demand — Telegram is the storage backend, this server is
the interface.

## Features

- **Multi-account concurrent sessions** — every allowed phone number signs in simultaneously, with isolated files, folders, bots, routing rules and settings.
- **Download bot pool** — add several bots to spread upload and download load across separate MTProto connections for higher throughput.
- **Split uploads** — files above a threshold are cut into parts (at most 64) and uploaded in parallel, round-robin across storage channels.
- **Transparent chunking** — `max_file_size` above Telegram's per-document cap is honored by splitting into parts (≤64); parts are re-joined on download and deleted together.
- **Thumbnail extraction** — audio cover art is parsed straight from ID3/FLAC bytes (no ffmpeg); videos and images get background ffmpeg thumbnails.
- **On-demand i18n** — only English ships; other languages download at runtime from GitHub, so translations improve without rebuilding.
- **Guided @BotFather chat** — create or import download bots through a resumable in-app conversation with @BotFather.
- **Developer mode** — `/internal-db` gives admin users a SurrealDB browser; endpoints answer only to `admin_phones`.
- **Resumable uploads** — the `spill` strategy buffers to disk so uploads keep draining even if Telegram is slow.
- **Streaming with resume** — files stream back on demand and resume transparently when Telegram's file references expire mid-transfer.

## Comparison with Teldrive

| Aspect | ii-drive | Teldrive |
|---|---|---|
| **Language** | Rust | Go |
| **Database** | Embedded SurrealDB (SurrealKv) — no external service | PostgreSQL; sessions may use Postgres/Bolt/memory |
| **Frontend** | SvelteKit SPA, served from the binary | Separate React + Vite UI |
| **Accounts** | Multi-tenant: several Telegram accounts signed in at once, fully isolated | Multi-user via username allowlist, data isolated per `user_id` |
| **Bots** | Built-in @BotFather chat; guided create/import; auto-invite into channels | Bot tokens added via UI, no @BotFather flow |
| **Rclone / WebDAV** | No — pure HTTP API + web UI | Yes — rclone remote integration |
| **Chunking** | Configurable split threshold, ≤64 parts, round-robin across channels, rotating bot sessions | Chunked uploads; configurable threads, retries, retention; optional encryption |
| **Thumbnails** | Audio cover art from ID3/FLAC; ffmpeg for video/images | Optional imgproxy image resizing/thumbnails |
| **Deployment** | Single binary + `web/dist` folder + embedded DB file; TOML config | Binary + separate React UI + PostgreSQL; optional Redis, imgproxy |
| **Config reload** | `POST /api/config/reload` hot-applies runtime fields | Restart required |
| **Admin tooling** | `/internal-db` (SurrealQL browser), `/api/config/reload` | CLI check/clean utilities |
| **Max file size** | 2 GiB default, configurable; larger files chunk into parts | 2 GB per Telegram document, chunked |
| **Upload strategy** | `stream` (relay) or `spill` (disk-buffer) | Stream with configurable buffers |

## Get started

### What you need

- A bundle for your platform from
  [GitHub Releases](https://github.com/Lebenoa/ii-drive/releases):
  `ii-drive-<version>-windows-x64.zip` or `ii-drive-<version>-linux-x64.tar.gz`.
  Everything is inside — server, web UI, config template. No Rust, Node or
  build tools required.
- A Telegram **api_id / api_hash** — create an app at
  <https://my.telegram.org/apps>
- A Telegram account (a normal user account, not a bot) and its phone number

Prefer building from source? See [Development](#development).

### 1. Unpack the bundle

Download and extract the archive for your platform. You get one folder:

```
ii-drive[.exe]          the server (web UI already included)
config.example.toml     configuration template
web/dist/               the built web app
locales/en.json         English dictionary
README.md
```

Keep the files together — relative paths resolve beside the executable.

### 2. Configure

```sh
cp config.example.toml config.toml
```

Open `config.toml` and set three things:

| Key | Where to get it |
|---|---|
| `api_id` / `api_hash` | from my.telegram.org (step above) |
| `allowed_phones` | your phone number(s), e.g. `["+15551234567"]` — only these may sign in |

**Recommended:** set `secret` yourself — any long random string. If you
leave it unset, a random one is generated and stored in `secret.key` beside
the database, which works fine but is one more file to back up.

Everything else has sane defaults. The full option table is under
[Configuration reference](#configuration-reference).

### 3. Run

```sh
./ii-drive              # Windows: ii-drive.exe
```

The server listens on `http://127.0.0.1:8080` unless you changed
`host`/`port`.

### 4. Sign in

Open `http://127.0.0.1:8080`, enter your phone number, and confirm with the
code Telegram sends you (plus your 2FA password, if enabled). You are now
signed in to the drive.

### 5. Pick storage channels

Right after login the UI asks you to choose one or more **storage channels** —
these are where your files actually live inside Telegram. You can pick any
channel you created or administer, or Saved Messages. No channels selected =
no uploads.

### 6. Add download bots (recommended)

Under **Settings → Telegram**, add one or more bots — either import an
existing one or let the built-in @BotFather chat create one for you. Bots are
invited into your storage channels automatically and give uploads and
downloads extra parallel connections; more bots means more speed. Skip this
and everything still works through your own account.

That's it — drag files into the drive to upload them.

### Configuration reference

1. **Configure** — copy the example and edit:

   ```sh
   cp config.example.toml config.toml
   ```

   At minimum set `api_id`, `api_hash`, and `allowed_phones`. A session
   `secret` is generated automatically on first start (the secret signs
   web tokens; logging in happens through Telegram).

   | Option | Default | Meaning |
   |---|---|---|
   | `host` / `port` | `127.0.0.1:8080` | HTTP bind address |
   | `api_id` / `api_hash` | — | Telegram app credentials (required) |
   | `secret` | auto-generated, stored in `secret.key` | HMAC key for session tokens |
   | `allowed_phones` | `[]` | Phone numbers that may log in; any number of them may be signed in at once |
   | `admin_phones` | `[]` | Phone numbers (same format as `allowed_phones`) that may use the operator endpoints; empty means nobody can |
   | `token_ttl_secs` | 30 days | Web session lifetime |
   | `db_path` | `data/drive.surrealkv` | Embedded metadata store |
   | `session_path` | `data/session.db` | Legacy session path kept for compatibility; per-account sessions live in `sessions/` beside it |
   | `max_file_size` | `2GiB` | Upload cap; `2GiB`, `500MiB`, `2GB` (=2·10⁹), plain bytes |
   | `web_dist` | `web/dist` | Built SPA folder; API-only if missing |
   | `locales_dir` | `locales` | Web-UI translation files, served under `/locales/`; downloaded on demand |
   | `media_thumbs` | `true` | ffmpeg image/video thumbnails; audio cover art is extracted regardless |
   | `upload_strategy` | `stream` | How an accepted upload reaches Telegram: `stream` relays the body directly, `spill` buffers to disk first so all parts drain at full rate |
   | `spill_dir` | `data/spill` | Directory for in-flight upload buffers (`spill` strategy + resumable uploads) |

### Multi-account notes

Every allowed phone number can be signed in **at the same time**. Each
account gets its own MTProto session under `<session_path's directory>/sessions/<telegram-user-id>.db`,
and its own files, folders, storage channels, download bots, routing rules
and upload-split threshold — an account never sees another's. Sessions are
restored on start, so a restart does not log anybody out; `POST
/api/auth/logout` drops just the calling account.


### Storage channels

After signing in you are asked to pick one or more **storage channels**. Only
chats the account can actually wire bots into are offered — ones you created,
or where you hold admin rights to invite users and promote admins — plus
Saved Messages; a group you merely joined is not a usable target. The choice
is stored in SurrealDB and uploads are spread across the selected channels
round-robin; each file remembers which channel holds it, so downloads and
deletes keep working even if you change the selection later.
Settings lives under `/settings` with three categories:

- **Telegram** — pick storage channels, and manage download bots. Bots
  download files through their own rate limits, so several spread the load;
  adding a bot invites it into every storage channel as an admin. You can
  **import an existing bot** (its token is fetched from @BotFather for you)
  or create a new one through a guided **@BotFather** chat that walks
  `/newbot` and offers one-click add of the minted token. That conversation
  is saved: close the dialog mid-question and the card offers to resume it
  rather than leaving @BotFather waiting, and an explicit cancel tells
  @BotFather to drop it.
- **Uploads** — split-upload threshold and auto-upload routing rules
  (mime-prefix → folder).
- **Other** — developer mode, which unlocks `/internal-db`: browse the
  embedded tables and run SurrealQL directly. Those queries span every
  signed-in account, so the endpoints behind it answer only to numbers in
  `admin_phones` and read as missing to anybody else.

### Split uploads

Files larger than the split threshold are cut into parts of that size — at
most 64 — and uploaded in parallel, round-robin across the selected
channels. Each part goes out on a **rotating bot session**, so with several
bots the parts travel over separate MTProto connections instead of
pipelining through one; your own account is the fallback when no bots are
configured, when none of them can reach the target, and always for Saved
Messages. The same pool pays off on the way back: each part message can be
fetched by a different bot under its own rate limit.

Files above Telegram's per-document cap are always chunked, and keep all
their parts in one channel. Parts are re-joined transparently when streamed
or downloaded, and are deleted together with the file. The threshold only
affects new uploads; existing files keep their layout.

### Thumbnails

Previews come from three places, in this order:

1. **Telegram's own stripped thumbnail**, when it made one for the uploaded
   document (typical for jpeg and video) — free, stored as-is.
2. **Embedded cover art** for `audio/*` uploads: ID3v2.3/2.4 `APIC` frames
   (mp3 and friends) and FLAC `PICTURE` blocks, parsed straight out of the
   first 512 KiB of the stream. Telegram makes no thumbnail for audio, so
   this is the only source for music. It is pure byte parsing — no ffmpeg,
   no extra download — and therefore runs regardless of `media_thumbs`.
   Art stored beyond that first 512 KiB is not found.
3. **ffmpeg**, in the background, for videos (first frame) and images that
   arrived without a usable thumb. This is the only step `media_thumbs`
   turns off, and the only one needing ffmpeg on `PATH`. AVIF is used when
   the build has libaom, else WebP.

### Translations (i18n)

Only **English** ships with the app: `locales/en.json` is the bundled
fallback dictionary. Every other language is downloaded at runtime, when
the user picks it under **Settings → Other**, straight from this
repository's `locales/` folder on GitHub — so translations improve and new
ones appear without rebuilding or re-releasing anything.

- The switcher's catalog is [`locales/manifest.json`](locales/manifest.json)
  in the repo; add your language there.
- Dictionaries are nested JSON flattened to dot keys; `{name}`-style
  placeholders are interpolated; `_meta.name` (display name) is metadata,
  not a message.
- English always loads first as the key-fallback net; if a download fails,
  the UI keeps working on English.

Adding a language = two commits: `locales/th.json` (copy of `en.json`,
translated) plus an entry in `locales/manifest.json`. No rebuild.

The server still serves whatever sits in `locales_dir` under
`/locales/…` — that is how the bundled `en.json` reaches the UI, and it
works as an offline override point (`manifest.json` there is reserved and
never listed as a language).

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
| POST | `/api/auth/phone` | — | Start Telegram login (phone must be in `allowed_phones`); returns a `login_id` |
| POST | `/api/auth/code` | — | Submit `{login_id, code}`; returns token or `password_required` |
| POST | `/api/auth/password` | — | Submit `{login_id, password}` (2FA); returns token |
| POST | `/api/auth/logout` | token | Sign this account out; other signed-in accounts are untouched |
| GET | `/api/me` | token | Connection status + user info |
| GET/POST | `/api/channels` | token | List candidate dialogs / save storage-channel selection |
| POST | `/api/channels/create` | token | Create a new broadcast channel and use it as storage |
| GET/POST/DELETE | `/api/bot`(`/ {id}`) | token | Manage download bots |
| POST | `/api/botfather` | token | Relay one message to @BotFather; returns its reply + saved draft |
| GET | `/api/botfather/bots` | token | Bots this account owns, parsed from @BotFather's `/mybots` |
| POST | `/api/botfather/token` | token | Fetch one owned bot's API token via the @BotFather menus |
| GET/DELETE | `/api/botfather/draft` | token | Resume an unfinished `/newbot` chat / `/cancel` it and forget |
| GET/PUT | `/api/settings` | token | This account's upload-split threshold (`split_mb`, 0 = off) |
| GET/PUT | `/api/rules` | token | Auto-upload routing rules |
| GET | `/api/files?q&folder&limit&offset` | token | List/search files (folder id, empty = root) |
| POST | `/api/files` | token | Upload (`X-File-Size` required, `X-Folder` optional) |
| PATCH | `/api/files/{id}/move` `/visibility` | token | Move file / toggle public-private |
| DELETE | `/api/files/{id}` | token | Delete file + Telegram parts |
| GET | `/api/files/{id}/raw` `/thumb` | — / token / `?mt=` / `?sig=` | Stream or thumbnail; public files need nothing |
| GET | `/api/files/{id}/link` | token | Mint time-limited share URL |
| GET/POST | `/api/folders`(`/{id}`) | token | List / create / delete folders |
| GET | `/api/avatar` `/media-token` | token | Profile photo bytes / short-lived media token |
| POST | `/api/config/reload` | token + admin | Re-read config.toml (runtime fields hot-apply) |
| GET | `/api/limits` | — | Upload cap, so the UI can reject oversized files early |
| GET | `/locales/manifest.json` | — | Languages available in `locales_dir`, with display names |
| GET | `/locales/{lang}.json` | — | One translation dictionary; the UI downloads it on language change |
| GET/POST | `/api/internal-db/tables` `/query` | token + admin | Developer mode: list tables / run raw SurrealQL. Unrestricted, cross-tenant — callers outside `admin_phones` get 404 |
| GET | `/health` | — | Liveness probe |

## Development

### Requirements

- Rust 1.85+ (the crate is edition 2024; built with 1.98)
- Node.js 20.19+ (or 22.12+) for the web UI — any package manager/runner
  works (npm, pnpm, bun, yarn, nub, …); examples use `nub`
- A Telegram **api_id / api_hash** from <https://my.telegram.org/apps>
- A Telegram account (user account, not a bot) for uploads

### Building from source

```sh
cd web && nub install && nub run build && cd ..   # or: npm/pnpm/bun run build
cargo build --release
./target/release/ii-drive
```

The release bundle layout matches the repo defaults, so a source build runs
the same way: keep `web/dist` and `locales/en.json` beside the binary, or
point `web_dist`/`locales_dir` in config.toml at them.
### Commands

```sh
cargo test            # offline unit tests (auth, config, db, art, stream, routes)
cargo clippy
cd web && nub run dev  # Vite dev server with HMR (proxies /api to :8080) — `npm run dev` etc. work the same
```

## Technology Stack

- **Backend:** Rust, [axum](https://github.com/tokio-rs/axum) +
  [grammers](https://github.com/Lonami/grammers) (MTProto), embedded
  [SurrealDB](https://surrealdb.com) (SurrealKv) for metadata.
- **Frontend:** [SvelteKit](https://kit.svelte.dev) (Svelte 5) with
  `adapter-static` — a pure-SPA build served by the Rust server from `web_dist`.
- **Max file size:** 2 GiB default (Telegram free-account per-file limit);
  configurable, larger files upload via transparent chunking.
- **Thumbnails:** ffmpeg (optional, on `PATH`) for video/images; audio cover art parsed from ID3/FLAC.
- **Build:** Rust 1.85+ (edition 2024), Node.js 20.19+ (or 22.12+) for the web UI (SvelteKit 2 / Vite 8).
- **Deployment:** single binary plus a `web/dist` assets folder and an embedded SurrealDB file — no external services.

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
- Uploads stream in `stream` mode: memory use stays constant regardless of file size; `spill` mode buffers the whole file to disk instead.
- Telegram free-account hard limit: 2 GiB per file. No published daily cap —
  abuse triggers flood wait errors. The server defaults `max_file_size` to
  2 GiB (configurable); larger files upload via transparent chunking.
