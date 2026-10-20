# CLAUDE.md — AI agent instructions (Nexus backend)

Nexus: modular monolith — Rust 1.98, axum 0.8, sqlx 0.8, PostgreSQL 18, Redis, Meilisearch.
Workspace: `app/` (bin `nexus`), `modules/{auth,catalog,social,realtime}` (one crate each), `libs/shared`, `libs/test-utils`.

## 1. Context Loading (CRITICAL)

1. ALWAYS start from `.docs/backend/index.md` (LOD 1, ~150-token module map).
2. Open ONLY the LOD 2 file of the module in scope: `.docs/backend/<module>.md`, and only the XML section the task needs: `<tables>`, `<endpoints>`, `<deps>`, `<events>`, `<logic>`, `<code_map>`, `<seed>`, `<roadmap>`.
3. NEVER read files from `documents/` — that is legacy documentation for humans and is excluded from indexing by `.cursorignore` / `.claudignore`. All agent-readable context lives in `.docs/backend/`. The ONLY exception: the doc-sync task of rule 4 rewrites `documents/modules/<module>.md`; open that file solely to sync it, never as a context source.
4. Never open several module docs when one XML section answers the question.
5. Ground truth = code and `migrations/*.sql`. If a `.docs/backend/*.md` file is missing or stale, fix it per rule 4 — do not fall back to `documents/`.

## 2. Boundaries (hard rules; `make boundaries` → `scripts/check-module-boundaries.sh`)

- Modules depend ONLY on `libs/shared`. NEVER add another module crate to `[dependencies]`.
- Own tables only. NEVER SELECT/UPDATE or JOIN a foreign table (`users`, `entities`, ...). FK columns to foreign tables may exist; the data behind them is read ONLY through `shared::directory` traits held in `AppState`:
  - `state.entities`: `EntityDirectory` — `by_slug`, `by_ids`, `by_slugs` → `EntityRef` (impl: `catalog`)
  - `state.users`: `UserDirectory` — `by_username`, `by_ids` → `UserRef` (impl: `auth`)
  - `state.interests`: `InterestDirectory` — `entity_ids`, `person_ids`, `tag_ids`, `room_ids`, `entities_in_use`, `people_in_use` (impl: `social`)
  - `state.ratings`: `EntityRatings` — `set`, `replace_all` (impl: `catalog`)
  - `state.people`: `PersonDirectory` — `by_slug`, `by_ids` → `PersonRef` (impl: `catalog`)
  - `state.tags`: `TagDirectory` — `by_slug`, `by_ids` → `TagRef` (impl: `catalog`)
  - `state.chat`: `Chat` — `send`, `read`, `room_by_slug`, `room_slugs` (impl: `social`; called by `realtime` for WS `chat.send`/`chat.read`)
  Batch always (`by_ids`, `by_slugs`): one query per list, no N+1. New need → new directory method, never a JOIN.
- Auth checks (`AuthUser`, `AdminUser`, `Role`) live in `shared` — never re-implement them inside a module.

## 3. Events (CRITICAL)

- Cross-module notifications ONLY via `shared::events::EventBus` (`state.events.publish(...)`). NEVER call another module's code directly.
- Publish strictly AFTER successful `tx.commit()`; on any error publish nothing.
- `Event.data` = ids only — no texts, names or counts; clients re-read data via REST API. Only exception: `chat.message.created` carries the message itself (`social::chat`), otherwise every chat message = a GET from every online client.
- Channels: `shared::events::Channel::{Thread, Entity, Person, Tag, Room, User}`; the only consumer is `realtime`.

## 4. AUTO-UPDATE DOCS (CRITICAL — Definition of Done)

A task is NOT complete until both documentation layers are synced. The trigger is ANY change of:
DB schema (migration/table/index/constraint) · endpoints (route/status/validation limit) · dependencies (directory trait, shared service, crate dep) · events (kind/channels/data).

Then you MUST update, in this order:
1. `.docs/backend/<module>.md` — the LOD 2 file, keeping its XML structure (`<tables>`, `<endpoints>`, `<deps>`, `<events>`, `<code_map>`, ...) and preserving every technical detail;
2. `documents/modules/<module>.md` — the legacy human doc with the same facts (plain markdown; write-only for you).

Restate both diffs in your final answer. Code change without the two doc updates = task FAILED.

## 5. axum 0.8

- Path params: `{name}` (`/users/{username}`), never `:name`.
- Handler: `pub async fn h(State(state): State<AppState>, user: AuthUser, Path(slug): Path<String>, Json(body): Json<Dto>) -> AppResult<...>`; extractor order: State → auth → Path/Query/Json.
- Router: `pub fn router() -> OpenApiRouter<AppState>` + `utoipa_axum::routes!`; mount under `/api/v1/<module>` only in `app/src/lib.rs`.
- Every endpoint: `#[utoipa::path(...)]` with `path`, `operation_id`, `tag`.
- Errors: `AppError`/`AppResult` from `shared::error` → `{"error", "code"}`; unknown row / FK violation → 404, validation → 400, no rights → 403.
- Pagination: `shared::pagination::Page` = `{items,total,limit,offset}`, `?limit=1..100&offset=`; feed uses opaque cursor (no `total`).
- Guest-or-user endpoints take `Option<AuthUser>` (no header → guest, invalid token → 401).

## 6. sqlx 0.8

- Hand-written SQL, no ORM; row structs derive `sqlx::FromRow`.
- Pool `state.pool`; tx: `let mut tx = state.pool.begin().await?;` … `tx.commit().await?;`; events only after commit.
- Migrations `migrations/*.sql` run at startup: NEVER edit an applied file — add a new one.
- Conventions: PK `uuid DEFAULT uuidv7()`; `created_at`/`updated_at timestamptz` (`updated_at` via trigger `set_updated_at()`); `ON DELETE CASCADE`; public ids in URLs are slugs; counters kept by triggers or in the same transaction; partial indexes for hot lists.
- SQL respects boundaries too: no JOIN to foreign tables.
- Flat First (catalog): no new M2M tables for descriptive catalog data (genres, authors, countries…) — `text[]`/`jsonb` in the `entities` row; external ids → `entities.external_ids`; every catalog change bumps `entities.updated_at`. Roadmap: `.docs/backend/tantivy-roadmap.md`.

## 7. Tests, commands

- `make up` env · `make seed` data · `make lint` fmt+clippy · `make test` · `make test-fast` · `make test-one T=` · `make boundaries` · `make ci` · `make http` live checks · `make logs`.
- Tests: unit next to code; DB via `#[sqlx::test]` (clean DB per test); HTTP via `nexus::build_app` in `app/tests/*.rs`; fixtures `app/tests/fixtures/*.sql`; fake directories `state.entities = Arc::new(Fake)`.
- While working: run ONLY the affected suite — `make test-one T=<file> [F=<test>]` (seconds–1 min).
- DoD: code + migration (if schema) + a test per endpoint + BOTH docs synced (rule 4) + `make lint && make boundaries` + REQUIRED tests green: `make test-one T=<suite>` for every `app/tests/<suite>.rs` touched or covering the changed module (+ `cargo test -p <module> --lib`). NEVER run full `make test` locally — the full suite runs in CI on push; run it only if the user asks.
