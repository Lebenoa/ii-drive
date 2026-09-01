# AGENTS.md

Instructions for AI coding agents working in this repository. Read this
before changing anything; it encodes decisions that are easy to undo by
accident.

## What this is

**ii-drive** is a personal file drive backed by Telegram (in the spirit
of [teldrive](https://github.com/tgdrive/teldrive)): files are uploaded
as documents to Telegram *storage channels* and streamed back on demand.
One Rust binary serves the HTTP API and the SvelteKit SPA
(`web/dist`). Multi-tenant (several Telegram accounts at once), with an
embedded SurrealDB store, a download-bot pool, split/parallel uploads,
at-rest encryption, and AVIF thumbnails. The product-level details live
in `README.md`; this file is about working **on** the code.

## Golden rule: test with `cargo nextest`

**Always run tests with `cargo nextest run`. Never use `cargo test`.**

```sh
cargo nextest run            # whole suite
cargo nextest run db::tg_session   # one module
cargo nextest run -E 'test(unreadable_session_rows_are_dropped)'
```

If nextest is missing: `cargo install cargo-nextest --locked`.

Why this is mandatory, not a preference:

- Every test here runs its own tokio runtime and many touch embedded
  database engines. `cargo test` runs them all in **one process**;
  a test that wedges (or a binding that conflicts with another test's
  engine) stalls or poisons the entire run, and one bad test can hang
  the whole suite.
- nextest runs each test in its **own process**, so the blast radius of
  a hang is that one test, and engine/runtime state never leaks between
  tests.

The same rule applies to the sibling `mtprsto` checkout.

## Commands

| Task | Command |
|---|---|
| Tests | `cargo nextest run` |
| One test / module | `cargo nextest run <filter>` |
| Fast compile check | `cargo check` |
| Lint (strict) | `cargo strict-lint` |
| Lint (plain) | `cargo clippy --all-targets` |
| Run the server | `cargo run` (reads `config.toml`; `II_DRIVE_CONFIG` env var overrides the path) |
| Web frontend build | `cd web && <pkg mgr> install && <pkg mgr> run build` → `web/dist`, served by the binary from `web_dist` |

The web frontend is SvelteKit + adapter-static (Svelte 5, Vite). Lockfiles
for both `bun` and `nub` are present; use whichever is wired on your
machine. Rust code never imports web code — the binary only serves
`web/dist`.

## Testing rules

- Unit tests live next to the code they test, in a `mod tests`, and are
  named as sentences: `closing_releases_the_session_file`,
  `failed_attempts_only_block_the_number_that_failed`.
- `#[tokio::test]` defaults to a **current-thread** runtime. Any test
  that goes through the synchronous session-storage bridge (anything
  that ends up in `block_in_place`) **must** declare
  `#[tokio::test(flavor = "multi_thread")]`, or it will deadlock.
- Embedded SurrealDB: only **one SurrealKv engine may be open per store
  path**; opening a second has crashed runs here. Tests use in-memory
  scratch stores (`db::open_mem`, `db::connect_mem`) or the file-backed
  serialization harness (`db::harness::EngineGuard`). Never point a test
  at `data/`.
- Handler tests share the process-wide state via
  `state::with_state`; they must scope rows with `state::next_uid()` and
  must not assume a fresh store. State-logic tests use
  `AppState::scratch(open_mem())` for isolation.
- Never bridge sync→async with `block_on` on the calling thread. The
  Surreal handle is bound to the runtime that connected it; under a
  current-thread runtime that is the very thread being parked, so it
  deadlocks. The approved pattern is
  `tokio::task::block_in_place(|| Handle::current().block_on(fut))`
  (see `src/tg/session.rs`, `DbSessions`).

## Architecture

```
src/
  main.rs      boot: config → state → db::connect → hub::attach_session → sweep task → axum
  config.rs    config.toml (paths anchored to the config file's directory)
  state.rs     AppState: db handle, token epochs, TgHub, instance cache;
               LazyLock STATE + with_state/scratch test fixtures
  app.rs       axum router, SPA serving from web_dist
  auth.rs      session tokens (per-account epoch revocation)
  crypt.rs     NaCl secretbox at-rest encryption, teldrive-compatible framing
  db/          embedded SurrealDB (SurrealKv): files, folders, settings,
               bots, tg_session, schema migrations
  tg/          Telegram layer (see below)
  routes/      HTTP handlers, thumbnails, sweeps
  stream.rs    range streaming from Telegram with file-reference refetch
```

`src/tg/` is the largest surface:

- `mod.rs` — `TgManager` (one account: lazy client, peer cache, bot
  map) and thin wrappers over mtprsto. **App glue only** — raw TL
  building, response parsing, error classification, and wire-format
  helpers belong in the mtprsto library, not here.
- `hub.rs` — `TgHub`: multi-account registry, login flows with
  brute-force throttles, claim (filing a login's session row under its
  account), restore at boot, one-time import of legacy session files.
- `session.rs` — `DbSessions`: mtprsto `SessionStorage` backed by the
  `tg_session` table. Sessions are rows, not files. The sync→async
  bridge pattern lives here.
- `bots.rs` — download-bot pool (each bot its own session row);
  `channels.rs` — storage-chat discovery; `transfer.rs` — upload and
  delete paths; `login.rs` — phone-code/password flow; `botfather.rs` —
  guided @BotFather conversation.

Sessions are **rows in `tg_session`** (`kind`: account / bot / pending,
`owner`, opaque `SessionData` JSON). No session files: claim is a row
re-key, logout is a row delete, and `sessions/` on disk is legacy data
imported once at boot then set aside as `sessions.imported`.

## The mtprsto dependency

mtprsto (the MTProto library) is a **sibling checkout**: `../mtprsto`,
branch `docs-layer`, referenced as a git dependency. Local iteration on
an unpushed library commit uses a `[patch."https://github.com/Lebenoa/mtprsto"]`
section in `Cargo.toml` — it is dev-only and must never be committed as
part of an unrelated change.

Split rule: if code knows TL constructors, byte layouts, or response
shapes, it belongs in mtprsto; if it knows HTTP, storage chats, or the
web client, it stays here. Wire notes worth keeping in mind:

- The server dropped the legacy `channels.getMessages#e5906e3f`
  (`INPUT_METHOD_INVALID_3851447871` — that number is the ctor). Both
  getMessages builders now use the current schema shapes
  (`#ad8c9a23` / `#63c66506`, ids wrapped in `inputMessageID#a676a322`),
  and `channels.deleteMessages` uses `#84c1fd4e` (no flags word; the old
  `#84c1f4e6` had transposed digits and was never real). Verify any ctor
  against the `.tl` schemas in the mtprsto repo before changing it — the
  two mistakes above both passed layout tests for months.
- `rpc::build_get_dialogs` takes `folder_id: Option<i32>` **and**
  `offset_peer` — conditional TL fields serialize before mandatory ones
  (this exact ordering was once a real bug).

Order of operations when the library changes: commit mtprsto → push
`docs-layer` → drop the `[patch]` → `cargo update -p mtprsto` → run
`cargo nextest run` in **both** repos.

## SurrealDB gotchas

- Every `Surreal` clone is its **own session**: namespace/database
  selection does not carry across clones. Any new handle must go through
  `db::attach_session()` (the hub does this lazily; managers do it in
  `open_conn`). Symptom of forgetting: `Specify a namespace to use`.
- Query results are taken as raw `serde_json::Value` and converted with
  serde (see `db::files::to_row`); SurrealDB projects unset fields as
  null, so optional columns use the `null_as_*` deserializer helpers.
- Runtime settings (upload cap, thumbs, sweep schedule) live in the DB
  and are cached in `AppState`; `config.toml` holds only boot-time
  values.

## Conventions

- Comments explain **why**, never narrate the diff. Doc comments on
  public items; `# Errors` sections on fallible public functions in the
  library.
- **`cargo strict-lint` is the lint gate, and the tree is clean under
  it — keep it that way.** It is a user-level cargo alias
  (`~/.cargo/config.toml`) running clippy with
  `-D pedantic -D nursery -D unwrap_used -D expect_used -D
  indexing_slicing -D arithmetic_side_effects -D as_conversions -D
  unreachable -D unimplemented -D todo -D string_slice -D
  panic_in_result_fn -D panic -D exit` denied. When a lint is
  genuinely wrong for a provable invariant, add the narrowest possible
  `#[allow(clippy::…)]` with a justification comment — that is the
  established pattern (`// bounded by the directory's entry count`).
  Never re-widen an existing allow; prefer real fixes
  (`saturating_add`, `try_into`, explicit match arms) over allows.
- Telegram-layer errors flow as `Result<_, String>`; auth-dead failures
  collapse to `SESSION_INVALID_MSG` via `friendly()` so the API can map
  them to HTTP 401 structurally. The DB layer uses `db::DbError`.
- Commits follow Conventional Commits (`feat(scope): …`, imperative,
  ≤50-char subject). `commit.gpgsign` is on and signs via the user's
  SSH agent; if signing fails, report it and let the user decide — do
  not silently commit with `--no-gpg-sign`.
- CRLF/LF warnings from git on Windows are benign; do not "fix" line
  endings.
- Product-facing changes belong in `README.md` too; this file is for
  agents, not a changelog.

## Runtime layout (do not commit)

`data/drive.surrealkv` (the store), `data/thumbs/` (generated previews),
`data/sessions/` + `sessions.imported` (legacy session files),
`config.toml` (see `config.example.toml`). The binary creates all of
these; they are environment, not source.
