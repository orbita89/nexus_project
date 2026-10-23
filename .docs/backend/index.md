# Nexus — LOD 1 (backend map)

Modular monolith: Rust, axum 0.8, sqlx 0.8, PostgreSQL 18. Rules: `../CLAUDE.md`.

| Module | Base path | Owns | LOD 2 |
|---|---|---|---|
| auth | `/api/v1/auth` | users, login, JWT, roles, profile | auth.md |
| catalog | `/api/v1/catalog` | entities, people, tags, search, cold archive (Tantivy), LibGen book import | catalog.md |
| social | `/api/v1/social` | reviews, follows, collections, forum, feed, chat (комнаты, поиск, упоминания) | social.md |
| realtime | `/ws` | WebSocket channels, event delivery | realtime.md |

Foreign reads → `shared::directory`; notifications → `shared::events::EventBus`.

Future (план, не спринт): перенос каталога в Tantivy и правило Flat First — `tantivy-roadmap.md`.
