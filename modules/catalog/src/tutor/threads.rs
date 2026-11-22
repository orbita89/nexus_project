//! Обсуждения книги с ИИ (`ai_threads`, `ai_messages`): только свои, только вошедшим.
//!
//! Вопрос — `POST /tutor/{id}/messages`, ответ — потоком (SSE): события `delta` (кусок текста),
//! `done` (ответ сохранён) или `error`. Ответ сохраняется, даже если читатель закрыл вкладку.
//! Что уходит в модель и почему это дёшево — `prompt.rs`.

use std::convert::Infallible;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use chrono::{DateTime, Utc};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use shared::directory::EntityRef;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser};
use tokio::sync::mpsc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::llm;
use super::prompt::{self, Book, Stored};
use super::providers;
use super::templates::Templates;

/// Обсуждений одного читателя — не больше (защита от мусора).
const MAX_THREADS: i64 = 500;
/// Сообщений в одном обсуждении — не больше: дальше — новое обсуждение.
const MAX_MESSAGES: i32 = 400;

/// Обсуждение.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct AiThread {
    pub id: Uuid,
    pub title: String,
    pub messages_count: i32,
    /// Расход токенов обсуждения (по данным провайдера): вход и выход.
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const THREAD_COLUMNS: &str =
    "id, title, messages_count, tokens_in, tokens_out, created_at, updated_at";

/// Обсуждение в своём списке (профиль): то же плюс книга.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiThreadWithEntity {
    pub entity: EntityRef,
    #[serde(flatten)]
    pub thread: AiThread,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct AiMessage {
    pub id: Uuid,
    /// `user` или `assistant`.
    pub role: String,
    pub content: String,
    /// Вопрос из читалки: выделенный фрагмент и глава.
    pub quote: Option<String>,
    pub chapter: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Обсуждение целиком: книга и все сообщения.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiThreadDetail {
    pub entity: EntityRef,
    #[serde(flatten)]
    pub thread: AiThread,
    pub messages: Vec<AiMessage>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAiThread {
    /// Пусто — по первому вопросу.
    pub title: Option<String>,
}

/// Вопрос.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SendAiMessage {
    pub text: String,
    /// Выделенный в читалке фрагмент (≤ 1500 символов, длиннее — обрежется).
    pub quote: Option<String>,
    pub chapter: Option<String>,
    /// Докуда дочитал читатель (0–1): ИИ не спойлерит дальше.
    pub fraction: Option<f32>,
    /// Свой провайдер; не задан — по умолчанию.
    pub provider_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AiThreadsQuery {
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Книга для обсуждения: id и то, что нужно промпту.
struct BookInfo {
    id: Uuid,
    book: Book,
}

async fn book(state: &AppState, slug: &str) -> AppResult<BookInfo> {
    let row: Option<(Uuid, String, Option<i32>)> = sqlx::query_as(
        "SELECT id, title, extract(year FROM release_date)::int
         FROM entities WHERE slug = $1 AND kind = 'book'",
    )
    .bind(slug)
    .fetch_optional(&state.db)
    .await?;
    let (id, title, year) = row.ok_or(AppError::NotFound)?;
    book_by_id(state, id, title, year).await
}

async fn book_by_id(
    state: &AppState,
    id: Uuid,
    title: String,
    year: Option<i32>,
) -> AppResult<BookInfo> {
    let authors: Vec<String> = sqlx::query_scalar(
        "SELECT p.full_name FROM entity_credits c JOIN people p ON p.id = c.person_id
         WHERE c.entity_id = $1 AND c.role = 'author' ORDER BY c.position LIMIT 3",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(BookInfo {
        id,
        book: Book {
            title,
            authors,
            year,
        },
    })
}

async fn entity_ref(state: &AppState, id: Uuid) -> AppResult<EntityRef> {
    state
        .entities
        .by_ids(&[id])
        .await?
        .remove(&id)
        .ok_or(AppError::NotFound)
}

/// Своё обсуждение и его книга (`entity_id`, summary, summarized).
async fn own_thread(
    state: &AppState,
    user: Uuid,
    id: Uuid,
) -> AppResult<(AiThread, Uuid, String, i32)> {
    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(flatten)]
        thread: AiThread,
        entity_id: Uuid,
        summary: String,
        summarized: i32,
    }
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT {THREAD_COLUMNS}, entity_id, summary, summarized
         FROM ai_threads WHERE id = $1 AND user_id = $2"
    ))
    .bind(id)
    .bind(user)
    .fetch_optional(&state.db)
    .await?;
    let row = row.ok_or(AppError::NotFound)?;
    Ok((row.thread, row.entity_id, row.summary, row.summarized))
}

/// Свои обсуждения книги, свежие сверху.
#[utoipa::path(
    get, operation_id = "list_book_ai_threads", path = "/books/{slug}/tutor", tag = "ai-tutor",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug книги", example = "master-i-margarita")),
    responses(
        (status = 200, body = Vec<AiThread>),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Книга не найдена", body = ErrorBody),
    )
)]
pub async fn list_book(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<Vec<AiThread>>> {
    let info = book(&state, &slug).await?;
    let rows = sqlx::query_as(&format!(
        "SELECT {THREAD_COLUMNS} FROM ai_threads WHERE user_id = $1 AND entity_id = $2
         ORDER BY updated_at DESC LIMIT 100"
    ))
    .bind(user.id)
    .bind(info.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Начать обсуждение книги.
#[utoipa::path(
    post, operation_id = "create_ai_thread", path = "/books/{slug}/tutor", tag = "ai-tutor",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug книги", example = "master-i-margarita")),
    request_body = CreateAiThread,
    responses(
        (status = 201, body = AiThread),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Книга не найдена", body = ErrorBody),
        (status = 409, description = "Слишком много обсуждений", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<CreateAiThread>,
) -> AppResult<(StatusCode, Json<AiThread>)> {
    let info = book(&state, &slug).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_threads WHERE user_id = $1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    if count >= MAX_THREADS {
        return Err(AppError::Conflict(format!(
            "не больше {MAX_THREADS} обсуждений — удалите старые в профиле"
        )));
    }
    let title = req
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| prompt::clip(t, 200))
        .unwrap_or_else(|| "Обсуждение".into());
    let row = sqlx::query_as(&format!(
        "INSERT INTO ai_threads (user_id, entity_id, title) VALUES ($1, $2, $3)
         RETURNING {THREAD_COLUMNS}"
    ))
    .bind(user.id)
    .bind(info.id)
    .bind(&title)
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// Свои обсуждения во всех книгах, свежие сверху: раздел профиля.
#[utoipa::path(
    get, operation_id = "list_my_ai_threads", path = "/me/tutor", tag = "ai-tutor",
    security(("bearer" = [])),
    params(AiThreadsQuery),
    responses(
        (status = 200, body = Page<AiThreadWithEntity>),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list_own(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<AiThreadsQuery>,
) -> AppResult<Json<Page<AiThreadWithEntity>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(flatten)]
        thread: AiThread,
        entity_id: Uuid,
    }
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_threads WHERE user_id = $1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT {THREAD_COLUMNS}, entity_id FROM ai_threads WHERE user_id = $1
         ORDER BY updated_at DESC, id LIMIT $2 OFFSET $3"
    ))
    .bind(user.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.entity_id).collect();
    let entities = state.entities.by_ids(&ids).await?;
    let items = rows
        .into_iter()
        .filter_map(|r| {
            Some(AiThreadWithEntity {
                entity: entities.get(&r.entity_id)?.clone(),
                thread: r.thread,
            })
        })
        .collect();
    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Обсуждение со всеми сообщениями.
#[utoipa::path(
    get, operation_id = "get_ai_thread", path = "/tutor/{id}", tag = "ai-tutor",
    security(("bearer" = [])),
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = AiThreadDetail),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такого своего обсуждения", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<AiThreadDetail>> {
    let (thread, entity_id, _, _) = own_thread(&state, user.id, id).await?;
    let messages = sqlx::query_as(
        "SELECT id, role, content, quote, chapter, created_at FROM ai_messages
         WHERE thread_id = $1 ORDER BY created_at, id",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(AiThreadDetail {
        entity: entity_ref(&state, entity_id).await?,
        thread,
        messages,
    }))
}

/// Удалить своё обсуждение.
#[utoipa::path(
    delete, operation_id = "delete_ai_thread", path = "/tutor/{id}", tag = "ai-tutor",
    security(("bearer" = [])),
    params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "Удалено"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такого своего обсуждения", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let deleted: Option<Uuid> =
        sqlx::query_scalar("DELETE FROM ai_threads WHERE id = $1 AND user_id = $2 RETURNING id")
            .bind(id)
            .bind(user.id)
            .fetch_optional(&state.db)
            .await?;
    deleted.ok_or(AppError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Задать вопрос. Ответ — потоком (`text/event-stream`):
/// `event: delta` `data: {"text": "…"}` — кусок ответа;
/// `event: done` `data: {"id", "tokens_in", "tokens_out"}` — ответ сохранён;
/// `event: error` `data: {"message"}` — модель не ответила (вопрос сохранён).
#[utoipa::path(
    post, operation_id = "send_ai_message", path = "/tutor/{id}/messages", tag = "ai-tutor",
    security(("bearer" = [])),
    params(("id" = Uuid, Path)),
    request_body = SendAiMessage,
    responses(
        (status = 200, description = "Поток ответа (SSE)", content_type = "text/event-stream", body = String),
        (status = 400, description = "Пустой или слишком длинный вопрос", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такого своего обсуждения", body = ErrorBody),
        (status = 409, description = "Нет своего ключа ИИ (`no_ai_provider`) или обсуждение переполнено", body = ErrorBody),
        (status = 503, description = "ИИ не настроен на сервере", body = ErrorBody),
    )
)]
pub async fn send(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<SendAiMessage>,
) -> AppResult<(
    [(axum::http::HeaderName, &'static str); 1],
    Sse<impl Stream<Item = Result<Event, Infallible>>>,
)> {
    let text = req.text.trim();
    if text.is_empty() || text.chars().count() > prompt::MAX_QUESTION {
        return Err(AppError::Invalid {
            field: "text".into(),
            message: format!("вопрос 1–{} символов", prompt::MAX_QUESTION),
        });
    }
    let text = text.to_string();
    let quote = req
        .quote
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(|q| prompt::clip(q, prompt::MAX_QUOTE - 1));
    let chapter = req
        .chapter
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| prompt::clip(c, 299));

    let (thread, entity_id, summary, summarized) = own_thread(&state, user.id, id).await?;
    if thread.messages_count >= MAX_MESSAGES {
        return Err(AppError::Conflict(
            "обсуждение слишком длинное — начните новое".into(),
        ));
    }
    let provider = providers::resolve(&state, user.id, req.provider_id).await?;
    let (title, year): (String, Option<i32>) = sqlx::query_as(
        "SELECT title, extract(year FROM release_date)::int FROM entities WHERE id = $1",
    )
    .bind(entity_id)
    .fetch_one(&state.db)
    .await?;
    let info = book_by_id(&state, entity_id, title, year).await?;

    // История — только окно последних реплик: остальное уже в summary.
    let mut history: Vec<Stored> =
        sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
            "SELECT role, content, quote, chapter FROM ai_messages WHERE thread_id = $1
         ORDER BY created_at DESC, id DESC LIMIT $2",
        )
        .bind(id)
        .bind(prompt::WINDOW as i64)
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .rev()
        .map(|(role, content, quote, chapter)| Stored {
            role,
            content,
            quote,
            chapter,
        })
        .collect();

    // Вопрос сохраняем сразу: ответ может и не прийти, а вопрос читатель увидит в истории.
    sqlx::query(
        "INSERT INTO ai_messages (thread_id, role, content, quote, chapter) VALUES ($1, 'user', $2, $3, $4)",
    )
    .bind(id)
    .bind(&text)
    .bind(&quote)
    .bind(&chapter)
    .execute(&state.db)
    .await?;
    let first = thread.messages_count == 0;
    sqlx::query(
        "UPDATE ai_threads SET messages_count = messages_count + 1,
             title = CASE WHEN $2 AND title = 'Обсуждение' THEN $3 ELSE title END
         WHERE id = $1",
    )
    .bind(id)
    .bind(first)
    .bind(prompt::title(&text))
    .execute(&state.db)
    .await?;

    history.push(Stored {
        role: "user".into(),
        content: text,
        quote,
        chapter,
    });
    // Тексты промптов — из админки (для книг), иначе встроенные.
    let templates = Templates::load(&state.db, "book").await?;
    let system = prompt::system(&templates, &info.book, req.fraction, &summary);
    let turns = prompt::window(&templates, &history);
    let messages_after = thread.messages_count + 1;

    let (events_tx, events_rx) = mpsc::channel::<Event>(64);
    let db = state.db.clone();
    tokio::spawn(async move {
        let http = providers::http();
        let (delta_tx, mut delta_rx) = mpsc::channel::<String>(64);
        let forward = {
            let events = events_tx.clone();
            tokio::spawn(async move {
                while let Some(text) = delta_rx.recv().await {
                    let event = Event::default()
                        .event("delta")
                        .data(json!({ "text": text }).to_string());
                    // Читатель ушёл — не страшно: ответ дочитаем и сохраним.
                    let _ = events.send(event).await;
                }
            })
        };
        let result = llm::stream(
            &http,
            &provider,
            &system,
            &turns,
            prompt::ANSWER_TOKENS,
            &delta_tx,
        )
        .await;
        drop(delta_tx);
        let _ = forward.await;
        match result {
            Ok((answer, usage)) if !answer.is_empty() => {
                let saved: Result<Uuid, sqlx::Error> = async {
                    let mid: Uuid = sqlx::query_scalar(
                        "INSERT INTO ai_messages (thread_id, role, content, tokens_in, tokens_out)
                         VALUES ($1, 'assistant', $2, $3, $4) RETURNING id",
                    )
                    .bind(id)
                    .bind(prompt::clip(&answer, 19_000))
                    .bind(usage.input)
                    .bind(usage.output)
                    .fetch_one(&db)
                    .await?;
                    sqlx::query(
                        "UPDATE ai_threads SET messages_count = messages_count + 1,
                             tokens_in = tokens_in + $2, tokens_out = tokens_out + $3
                         WHERE id = $1",
                    )
                    .bind(id)
                    .bind(usage.input as i64)
                    .bind(usage.output as i64)
                    .execute(&db)
                    .await?;
                    Ok(mid)
                }
                .await;
                let event = match saved {
                    Ok(mid) => Event::default().event("done").data(
                        json!({ "id": mid, "tokens_in": usage.input, "tokens_out": usage.output })
                            .to_string(),
                    ),
                    Err(e) => {
                        tracing::error!(thread = %id, %e, "ai answer not saved");
                        error_event("ответ не сохранился")
                    }
                };
                let _ = events_tx.send(event).await;
                // После ответа — пересжать историю, если вне окна накопилось.
                compact(
                    &db,
                    &http,
                    &templates,
                    &provider,
                    id,
                    &summary,
                    summarized,
                    messages_after + 1,
                )
                .await;
            }
            Ok(_) => {
                let _ = events_tx
                    .send(error_event("модель вернула пустой ответ"))
                    .await;
            }
            Err(error) => {
                tracing::warn!(thread = %id, %error, "ai answer failed");
                let _ = events_tx.send(error_event(&error)).await;
            }
        }
    });

    let stream = futures_util::stream::unfold(events_rx, |mut rx| async move {
        rx.recv().await.map(|event| (Ok(event), rx))
    });
    // nginx по умолчанию копит ответ в буфере — поток пришёл бы одним куском в конце.
    let no_buffering = [(
        axum::http::HeaderName::from_static("x-accel-buffering"),
        "no",
    )];
    Ok((
        no_buffering,
        Sse::new(stream).keep_alive(KeepAlive::default()),
    ))
}

fn error_event(message: &str) -> Event {
    Event::default()
        .event("error")
        .data(json!({ "message": message }).to_string())
}

/// Пересжать summary, если вне окна последних реплик накопилось [`prompt::SUMMARIZE_AFTER`].
/// Ошибка не страшна: попробуем после следующего ответа.
async fn compact(
    db: &sqlx::PgPool,
    http: &reqwest::Client,
    templates: &Templates,
    provider: &llm::Provider,
    id: Uuid,
    summary: &str,
    summarized: i32,
    total: i32,
) {
    let outside = (total as usize).saturating_sub(prompt::WINDOW);
    if outside < summarized as usize + prompt::SUMMARIZE_AFTER {
        return;
    }
    let dropped: Vec<Stored> = match sqlx::query_as::<_, (String, String)>(
        "SELECT role, content FROM ai_messages WHERE thread_id = $1
         ORDER BY created_at, id OFFSET $2 LIMIT $3",
    )
    .bind(id)
    .bind(summarized as i64)
    .bind((outside - summarized as usize) as i64)
    .fetch_all(db)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|(role, content)| Stored {
                role,
                content,
                quote: None,
                chapter: None,
            })
            .collect(),
        Err(_) => return,
    };
    let (system, turns) = prompt::summarize(templates, summary, &dropped);
    let Ok((next, usage)) =
        llm::complete(http, provider, &system, &turns, prompt::SUMMARY_TOKENS).await
    else {
        return;
    };
    let _ = sqlx::query(
        "UPDATE ai_threads SET summary = $2, summarized = $3,
             tokens_in = tokens_in + $4, tokens_out = tokens_out + $5
         WHERE id = $1",
    )
    .bind(id)
    .bind(prompt::clip(&next, prompt::SUMMARY_CHARS))
    .bind(outside as i32)
    .bind(usage.input as i64)
    .bind(usage.output as i64)
    .execute(db)
    .await;
}
