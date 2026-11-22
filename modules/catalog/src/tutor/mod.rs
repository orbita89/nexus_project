//! ИИ-преподаватель: обсуждение книги с ИИ на ключе самого пользователя.
//!
//! - `providers` — свои ключи (`/me/ai-providers`), зашифрованы в Postgres;
//! - `threads` — обсуждения и вопросы потоком (`/books/{slug}/tutor`, `/tutor/{id}`, `/me/tutor`);
//! - `prompt` — что уходит в модель (коротко и дёшево при любой длине чата);
//! - `templates` — тексты промптов: встроенные и правки из админки (`/admin/ai/prompts`);
//! - `llm` — вызов OpenAI-совместимого API или Anthropic.
//!
//! Только вошедшим; пока только книги (`kind = 'book'`).

mod llm;
pub mod prompt;
pub mod providers;
pub mod templates;
pub mod threads;
