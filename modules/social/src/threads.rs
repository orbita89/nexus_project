//! Темы форума. Тема привязана к 1–10 сущностям: тема про «Дюну» видна и у книги, и у фильма.
//! Читать — гость, создавать — роль author и выше, менять и удалять — автор темы.

use crate::models::{
    CreateThread, ListThreadsQuery, PageQuery, PostRow, PostsQuery, Thread, ThreadDetail,
    ThreadRow, ThreadSort, UpdateThread, POST_COLUMNS, THREAD_COLUMNS,
};
use crate::refs;
use crate::validate::{self, MAX_THREAD_BODY, MAX_THREAD_ENTITIES, MAX_THREAD_TITLE};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page, MAX_PAGE_SIZE};
use shared::{AppError, AppResult, AppState, AuthUser, Role};
use sqlx::{PgConnection, PgExecutor};
use std::collections::HashSet;
use uuid::Uuid;

/// Сообщений на странице темы по умолчанию.
const DEFAULT_POSTS_PAGE: i64 = 50;

/// Свежие или активные темы всего форума.
#[utoipa::path(
    get, path = "/threads", tag = "forum",
    params(ListThreadsQuery),
    responses(
        (status = 200, description = "Страница тем", body = Page<Thread>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListThreadsQuery>,
) -> AppResult<Json<Page<Thread>>> {
    page(
        &state,
        "true",
        None,
        query.sort.unwrap_or_default(),
        query.limit,
        query.offset,
    )
    .await
}

/// Темы, к которым привязана сущность (в том числе вместе с другими).
#[utoipa::path(
    get, path = "/entities/{slug}/threads", tag = "forum",
    params(
        ("slug" = String, Path, description = "slug сущности", example = "dune-2021"),
        ListThreadsQuery,
    ),
    responses(
        (status = 200, description = "Страница тем", body = Page<Thread>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn list_for_entity(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<ListThreadsQuery>,
) -> AppResult<Json<Page<Thread>>> {
    let entity = refs::entity(&state, &slug).await?;
    let filter = "EXISTS (SELECT 1 FROM forum_thread_entities te
                  WHERE te.thread_id = t.id AND te.entity_id = $1)";
    let sort = query.sort.unwrap_or_default();
    page(
        &state,
        filter,
        Some(entity.id),
        sort,
        query.limit,
        query.offset,
    )
    .await
}

/// Темы пользователя, новые сверху.
#[utoipa::path(
    get, path = "/users/{username}/threads", tag = "forum",
    params(("username" = String, Path, description = "username", example = "author"), PageQuery),
    responses(
        (status = 200, description = "Страница тем", body = Page<Thread>),
        (status = 404, description = "Пользователь не найден", body = ErrorBody),
    )
)]
pub async fn list_for_user(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<Thread>>> {
    let author = refs::user(&state, &username).await?;
    let filter = "t.author_id = $1";
    page(
        &state,
        filter,
        Some(author.id),
        ThreadSort::New,
        query.limit,
        query.offset,
    )
    .await
}

/// Страница тем под фильтром `filter` (`$1` — `arg`, если есть).
async fn page(
    state: &AppState,
    filter: &str,
    arg: Option<Uuid>,
    sort: ThreadSort,
    limit: Option<i64>,
    offset: Option<i64>,
) -> AppResult<Json<Page<Thread>>> {
    let (limit, offset) = page_bounds(limit, offset);
    let order = match sort {
        ThreadSort::Active => "t.last_post_at DESC, t.id",
        ThreadSort::New => "t.created_at DESC, t.id",
    };
    // $1 привязывается всегда (без аргумента — NULL), чтобы номера $2/$3 не зависели от фильтра.
    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM forum_threads t WHERE {filter}"
    ))
    .bind(arg)
    .fetch_one(&state.db)
    .await?;
    let rows: Vec<ThreadRow> = sqlx::query_as(&format!(
        "SELECT {THREAD_COLUMNS} FROM forum_threads t WHERE {filter}
         ORDER BY {order} LIMIT $2 OFFSET $3"
    ))
    .bind(arg)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page {
        items: cards(state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Тема с текстом и страницей сообщений по времени.
#[utoipa::path(
    get, path = "/threads/{id}", tag = "forum",
    params(("id" = Uuid, Path, description = "id темы"), PostsQuery),
    responses(
        (status = 200, description = "Тема", body = ThreadDetail),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 404, description = "Тема не найдена", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<PostsQuery>,
) -> AppResult<Json<ThreadDetail>> {
    let row = load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    Ok(Json(detail(&state, row, query).await?))
}

/// Создать тему. Нужна роль author или admin.
#[utoipa::path(
    post, path = "/threads", tag = "forum",
    security(("bearer" = [])),
    request_body = CreateThread,
    responses(
        (status = 201, description = "Создана", body = ThreadDetail),
        (status = 400, description = "Неверные поля или неизвестная сущность", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль author", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(req): JsonBody<CreateThread>,
) -> AppResult<(StatusCode, Json<ThreadDetail>)> {
    user.require(Role::Author)?;
    let title = validate::required("title", &req.title, MAX_THREAD_TITLE)?;
    let body = validate::required("body", &req.body, MAX_THREAD_BODY)?;
    let entities = resolve_entities(&state, &req.entities).await?;

    let mut tx = state.db.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO forum_threads (author_id, title, body) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(user.id)
    .bind(title)
    .bind(body)
    .fetch_one(&mut *tx)
    .await
    .map_err(validate::missing_reference)?;
    set_entities(&mut tx, id, &entities).await?;
    tx.commit().await?;

    tracing::info!(user_id = %user.id, thread_id = %id, "thread created");
    let row = load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let detail = detail(
        &state,
        row,
        PostsQuery {
            limit: None,
            offset: None,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(detail)))
}

/// Изменить свою тему: заголовок, текст, набор сущностей. Ставит `edited_at`.
#[utoipa::path(
    patch, path = "/threads/{id}", tag = "forum",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id темы")),
    request_body = UpdateThread,
    responses(
        (status = 200, description = "Изменена", body = ThreadDetail),
        (status = 400, description = "Неверные поля или неизвестная сущность", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужая тема", body = ErrorBody),
        (status = 404, description = "Тема не найдена", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<UpdateThread>,
) -> AppResult<Json<ThreadDetail>> {
    let title = req
        .title
        .map(|title| validate::required("title", &title, MAX_THREAD_TITLE))
        .transpose()?;
    let body = req
        .body
        .map(|body| validate::required("body", &body, MAX_THREAD_BODY))
        .transpose()?;
    let entities = match &req.entities {
        Some(slugs) => Some(resolve_entities(&state, slugs).await?),
        None => None,
    };

    let mut tx = state.db.begin().await?;
    owned(&mut *tx, id, &user).await?;
    if title.is_some() || body.is_some() || entities.is_some() {
        sqlx::query(
            "UPDATE forum_threads SET
                title = COALESCE($2, title), body = COALESCE($3, body), edited_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(title)
        .bind(body)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(entities) = entities {
        sqlx::query("DELETE FROM forum_thread_entities WHERE thread_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        set_entities(&mut tx, id, &entities).await?;
    }
    tx.commit().await?;

    let row = load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let detail = detail(
        &state,
        row,
        PostsQuery {
            limit: None,
            offset: None,
        },
    )
    .await?;
    Ok(Json(detail))
}

/// Удалить свою тему вместе со всеми сообщениями.
#[utoipa::path(
    delete, path = "/threads/{id}", tag = "forum",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id темы")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужая тема", body = ErrorBody),
        (status = 404, description = "Тема не найдена", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    owned(&mut *tx, id, &user).await?;
    sqlx::query("DELETE FROM forum_threads WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(user_id = %user.id, thread_id = %id, "thread deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// slug'и из тела → id сущностей по порядку: 1–10 штук, без повторов, все существуют (иначе 400).
async fn resolve_entities(state: &AppState, slugs: &[String]) -> AppResult<Vec<Uuid>> {
    if slugs.is_empty() || slugs.len() > MAX_THREAD_ENTITIES {
        return Err(AppError::BadRequest(format!(
            "entities must contain 1 to {MAX_THREAD_ENTITIES} slugs"
        )));
    }
    let mut seen = HashSet::new();
    let mut ids = Vec::with_capacity(slugs.len());
    for slug in slugs {
        let entity = state
            .entities
            .by_slug(slug)
            .await?
            .ok_or_else(|| AppError::BadRequest(format!("unknown entity: {slug}")))?;
        if !seen.insert(entity.id) {
            return Err(AppError::BadRequest(format!("duplicate entity: {slug}")));
        }
        ids.push(entity.id);
    }
    Ok(ids)
}

/// Привязать сущности к теме в заданном порядке (старые привязки уже удалены).
async fn set_entities(tx: &mut PgConnection, thread: Uuid, entities: &[Uuid]) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO forum_thread_entities (thread_id, entity_id, position)
         SELECT $1, v.entity_id, (v.n - 1)::smallint
         FROM unnest($2::uuid[]) WITH ORDINALITY AS v(entity_id, n)",
    )
    .bind(thread)
    .bind(entities)
    .execute(tx)
    .await
    .map_err(validate::missing_reference)?;
    Ok(())
}

pub(crate) async fn load(db: impl PgExecutor<'_>, id: Uuid) -> AppResult<Option<ThreadRow>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {THREAD_COLUMNS} FROM forum_threads t WHERE t.id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Тема пользователя `user` (с блокировкой строки до конца транзакции): нет — 404, чужая — 403.
async fn owned(db: impl PgExecutor<'_>, id: Uuid, user: &AuthUser) -> AppResult<()> {
    let author: Option<Uuid> =
        sqlx::query_scalar("SELECT author_id FROM forum_threads WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(db)
            .await?;
    match author {
        Some(author) if author == user.id => Ok(()),
        Some(_) => Err(AppError::Forbidden),
        None => Err(AppError::NotFound),
    }
}

/// Темы для ответа: сущности всех тем страницы — одним запросом.
async fn cards(state: &AppState, rows: Vec<ThreadRow>) -> AppResult<Vec<Thread>> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let links: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT thread_id, entity_id FROM forum_thread_entities
         WHERE thread_id = ANY($1) ORDER BY thread_id, position",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    refs::threads(state, rows, links).await
}

async fn detail(state: &AppState, row: ThreadRow, query: PostsQuery) -> AppResult<ThreadDetail> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_POSTS_PAGE)
        .clamp(1, MAX_PAGE_SIZE);
    let (_, offset) = page_bounds(None, query.offset);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM forum_posts WHERE thread_id = $1")
        .bind(row.id)
        .fetch_one(&state.db)
        .await?;
    let posts: Vec<PostRow> = sqlx::query_as(&format!(
        "SELECT {POST_COLUMNS} FROM forum_posts p WHERE p.thread_id = $1
         ORDER BY p.created_at, p.id LIMIT $2 OFFSET $3"
    ))
    .bind(row.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    let body = row.body.clone();
    let thread = cards(state, vec![row])
        .await?
        .pop()
        .ok_or(AppError::NotFound)?;
    Ok(ThreadDetail {
        thread,
        body,
        posts: Page {
            items: refs::posts(state, posts).await?,
            total,
            limit,
            offset,
        },
    })
}
