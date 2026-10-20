//! Интересы: на какие сущности, людей (автор, режиссёр, актёр) и теги подписан пользователь.
//! Списки личные — видит только владелец. По интересам `realtime` подписывает соединения
//! пользователя на каналы `entity:`, `person:`, `tag:`; из них же — «мои чаты» (`chat`) и лента.
//!
//! Не больше 500 интересов каждого вида. Добавление и удаление публикуют `interest.added` /
//! `interest.removed` с `entity_id`, `person_id` или `tag_id`.

use crate::events;
use crate::models::{Interest, InterestRow, PageQuery, PersonInterest, TagInterest, TargetRow};
use crate::refs;
use crate::validate::{self, MAX_INTERESTS};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser};
use uuid::Uuid;

/// Вид интереса: своя таблица, колонка и ключ в событии.
#[derive(Clone, Copy)]
enum Kind {
    Entity,
    Person,
    Tag,
}

impl Kind {
    fn table(self) -> &'static str {
        match self {
            Kind::Entity => "user_interests",
            Kind::Person => "user_person_interests",
            Kind::Tag => "user_tag_interests",
        }
    }

    /// Колонка id и ключ в данных события.
    fn column(self) -> &'static str {
        match self {
            Kind::Entity => "entity_id",
            Kind::Person => "person_id",
            Kind::Tag => "tag_id",
        }
    }
}

async fn since(
    state: &AppState,
    user: Uuid,
    kind: Kind,
    id: Uuid,
) -> AppResult<Option<DateTime<Utc>>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT created_at FROM {} WHERE user_id = $1 AND {} = $2",
        kind.table(),
        kind.column()
    ))
    .bind(user)
    .bind(id)
    .fetch_optional(&state.db)
    .await?)
}

async fn count(state: &AppState, user: Uuid, kind: Kind) -> AppResult<i64> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT count(*) FROM {} WHERE user_id = $1",
        kind.table()
    ))
    .bind(user)
    .fetch_one(&state.db)
    .await?)
}

/// Страница своих интересов вида `kind`, новые сверху: (строки, всего).
async fn page(
    state: &AppState,
    user: Uuid,
    kind: Kind,
    query: PageQuery,
) -> AppResult<(Vec<TargetRow>, i64, i64, i64)> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let total = count(state, user, kind).await?;
    let column = kind.column();
    let rows = sqlx::query_as(&format!(
        "SELECT {column} AS id, created_at FROM {} WHERE user_id = $1
         ORDER BY created_at DESC, {column} LIMIT $2 OFFSET $3",
        kind.table()
    ))
    .bind(user)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok((rows, total, limit, offset))
}

/// Добавить → 204 (повторно — тоже 204); уже 500 интересов этого вида — 400.
async fn add_interest(state: &AppState, user: Uuid, kind: Kind, id: Uuid) -> AppResult<StatusCode> {
    let (table, column) = (kind.table(), kind.column());
    let mut tx = state.db.begin().await?;
    // Параллельные добавления одного пользователя идут по очереди: лимит не превысить.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    let exists: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM {table} WHERE user_id = $1 AND {column} = $2)"
    ))
    .bind(user)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if exists {
        return Ok(StatusCode::NO_CONTENT);
    }
    let count: i64 =
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE user_id = $1"))
            .bind(user)
            .fetch_one(&mut *tx)
            .await?;
    if count >= MAX_INTERESTS {
        return Err(AppError::BadRequest(format!(
            "no more than {MAX_INTERESTS} interests"
        )));
    }
    sqlx::query(&format!(
        "INSERT INTO {table} (user_id, {column}) VALUES ($1, $2)"
    ))
    .bind(user)
    .bind(id)
    .execute(&mut *tx)
    .await
    .map_err(validate::missing_reference)?;
    tx.commit().await?;
    events::publish(state, events::interest("interest.added", user, column, id));
    Ok(StatusCode::NO_CONTENT)
}

/// Убрать → 204 (даже если не было).
async fn remove_interest(
    state: &AppState,
    user: Uuid,
    kind: Kind,
    id: Uuid,
) -> AppResult<StatusCode> {
    let deleted = sqlx::query(&format!(
        "DELETE FROM {} WHERE user_id = $1 AND {} = $2",
        kind.table(),
        kind.column()
    ))
    .bind(user)
    .bind(id)
    .execute(&state.db)
    .await?;
    if deleted.rows_affected() > 0 {
        events::publish(
            state,
            events::interest("interest.removed", user, kind.column(), id),
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- сущности

/// Свои интересы-сущности, новые сверху.
#[utoipa::path(
    get, path = "/interests", tag = "interests",
    security(("bearer" = [])),
    params(PageQuery),
    responses(
        (status = 200, description = "Страница интересов", body = Page<Interest>),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list_own(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<Interest>>> {
    let (rows, total, limit, offset) = page(&state, user.id, Kind::Entity, query).await?;
    let rows = rows
        .into_iter()
        .map(|row| InterestRow {
            entity_id: row.id,
            created_at: row.created_at,
        })
        .collect();
    Ok(Json(Page {
        items: refs::interests(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Есть ли сущность в своих интересах: кнопка «Следить» на карточке.
#[utoipa::path(
    get, operation_id = "get_own_interest", path = "/entities/{slug}/interest", tag = "interests",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "В интересах", body = Interest),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена или её нет в интересах", body = ErrorBody),
    )
)]
pub async fn get_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<Interest>> {
    let entity = refs::entity(&state, &slug).await?;
    let since = since(&state, user.id, Kind::Entity, entity.id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(Interest { entity, since }))
}

/// Добавить сущность в интересы → 204 (повторно — тоже 204).
#[utoipa::path(
    put, path = "/entities/{slug}/interest", tag = "interests",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 204, description = "В интересах"),
        (status = 400, description = "Уже 500 интересов", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn add(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    add_interest(&state, user.id, Kind::Entity, entity.id).await
}

/// Убрать сущность из интересов → 204 (даже если её там не было).
#[utoipa::path(
    delete, path = "/entities/{slug}/interest", tag = "interests",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 204, description = "Не в интересах"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn remove(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    remove_interest(&state, user.id, Kind::Entity, entity.id).await
}

// ---------------------------------------------------------------- люди

/// Свои интересы-люди, новые сверху.
#[utoipa::path(
    get, operation_id = "list_own_person_interests", path = "/interests/people", tag = "interests",
    security(("bearer" = [])),
    params(PageQuery),
    responses(
        (status = 200, description = "Страница интересов", body = Page<PersonInterest>),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list_own_people(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<PersonInterest>>> {
    let (rows, total, limit, offset) = page(&state, user.id, Kind::Person, query).await?;
    let people = refs::people(&state, rows.iter().map(|row| row.id).collect()).await?;
    let items = rows
        .into_iter()
        .filter_map(|row| {
            Some(PersonInterest {
                person: people.get(&row.id)?.clone(),
                since: row.created_at,
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

/// Есть ли человек в своих интересах: кнопка «Следить» на его странице.
#[utoipa::path(
    get, operation_id = "get_own_person_interest", path = "/people/{slug}/interest", tag = "interests",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug человека", example = "denis-villeneuve")),
    responses(
        (status = 200, description = "В интересах", body = PersonInterest),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Человек не найден или его нет в интересах", body = ErrorBody),
    )
)]
pub async fn get_own_person(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<PersonInterest>> {
    let person = refs::person(&state, &slug).await?;
    let since = since(&state, user.id, Kind::Person, person.id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(PersonInterest { person, since }))
}

/// Добавить человека в интересы → 204 (повторно — тоже 204).
#[utoipa::path(
    put, operation_id = "add_person_interest", path = "/people/{slug}/interest", tag = "interests",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug человека", example = "denis-villeneuve")),
    responses(
        (status = 204, description = "В интересах"),
        (status = 400, description = "Уже 500 людей в интересах", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Человек не найден", body = ErrorBody),
    )
)]
pub async fn add_person(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let person = refs::person(&state, &slug).await?;
    add_interest(&state, user.id, Kind::Person, person.id).await
}

/// Убрать человека из интересов → 204 (даже если его там не было).
#[utoipa::path(
    delete, operation_id = "remove_person_interest", path = "/people/{slug}/interest", tag = "interests",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug человека", example = "denis-villeneuve")),
    responses(
        (status = 204, description = "Не в интересах"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Человек не найден", body = ErrorBody),
    )
)]
pub async fn remove_person(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let person = refs::person(&state, &slug).await?;
    remove_interest(&state, user.id, Kind::Person, person.id).await
}

// ---------------------------------------------------------------- теги

/// Свои интересы-теги и жанры, новые сверху.
#[utoipa::path(
    get, operation_id = "list_own_tag_interests", path = "/interests/tags", tag = "interests",
    security(("bearer" = [])),
    params(PageQuery),
    responses(
        (status = 200, description = "Страница интересов", body = Page<TagInterest>),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list_own_tags(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<TagInterest>>> {
    let (rows, total, limit, offset) = page(&state, user.id, Kind::Tag, query).await?;
    let tags = refs::tags(&state, rows.iter().map(|row| row.id).collect()).await?;
    let items = rows
        .into_iter()
        .filter_map(|row| {
            Some(TagInterest {
                tag: tags.get(&row.id)?.clone(),
                since: row.created_at,
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

/// Есть ли тег в своих интересах.
#[utoipa::path(
    get, operation_id = "get_own_tag_interest", path = "/tags/{slug}/interest", tag = "interests",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug тега", example = "sci-fi")),
    responses(
        (status = 200, description = "В интересах", body = TagInterest),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Тег не найден или его нет в интересах", body = ErrorBody),
    )
)]
pub async fn get_own_tag(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<TagInterest>> {
    let tag = refs::tag(&state, &slug).await?;
    let since = since(&state, user.id, Kind::Tag, tag.id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(TagInterest { tag, since }))
}

/// Добавить тег в интересы → 204 (повторно — тоже 204).
#[utoipa::path(
    put, operation_id = "add_tag_interest", path = "/tags/{slug}/interest", tag = "interests",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug тега", example = "sci-fi")),
    responses(
        (status = 204, description = "В интересах"),
        (status = 400, description = "Уже 500 тегов в интересах", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Тег не найден", body = ErrorBody),
    )
)]
pub async fn add_tag(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let tag = refs::tag(&state, &slug).await?;
    add_interest(&state, user.id, Kind::Tag, tag.id).await
}

/// Убрать тег из интересов → 204 (даже если его там не было).
#[utoipa::path(
    delete, operation_id = "remove_tag_interest", path = "/tags/{slug}/interest", tag = "interests",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug тега", example = "sci-fi")),
    responses(
        (status = 204, description = "Не в интересах"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Тег не найден", body = ErrorBody),
    )
)]
pub async fn remove_tag(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let tag = refs::tag(&state, &slug).await?;
    remove_interest(&state, user.id, Kind::Tag, tag.id).await
}
