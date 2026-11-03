//! Лайки профилей. Только лайк: дизлайка у пользователя нет — оценивают человека, а не
//! рецензию, и минус здесь работал бы как травля. Себе поставить нельзя.
//!
//! Счётчик и «лайкнул ли я» приходят в шапке профиля (`GET /users/{username}`) — отдельного
//! запроса под кнопку не нужно. Список лайкнувших открыт всем.

use crate::models::{
    CollectionLike, CollectionLikes, EntityLikes, GivenLike, GivenLikeRow, GivenLikesQuery, Like,
    LikeTarget, LikeTargetKind, PageQuery,
};
use crate::refs;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser};
use std::collections::HashMap;
use uuid::Uuid;

/// Лайкнуть профиль. Повторный лайк ничего не меняет.
#[utoipa::path(
    put, operation_id = "like_user", path = "/users/{username}/like", tag = "users",
    security(("bearer" = [])),
    params(("username" = String, Path, description = "Кого лайкнуть", example = "author")),
    responses(
        (status = 204, description = "Лайкнут"),
        (status = 400, description = "Лайк самому себе (`like_self`)", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn like(
    State(state): State<AppState>,
    user: AuthUser,
    Path(username): Path<String>,
) -> AppResult<StatusCode> {
    let liked = refs::user(&state, &username).await?;
    if liked.id == user.id {
        return Err(AppError::BadRequest("cannot like yourself".into()).with_code("like_self"));
    }
    sqlx::query(
        "INSERT INTO user_likes (user_id, liked_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(user.id)
    .bind(liked.id)
    .execute(&state.db)
    .await
    .map_err(crate::validate::missing_reference)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Снять свой лайк. Если его не было — тоже 204.
#[utoipa::path(
    delete, operation_id = "unlike_user", path = "/users/{username}/like", tag = "users",
    security(("bearer" = [])),
    params(("username" = String, Path, description = "С кого снять лайк", example = "author")),
    responses(
        (status = 204, description = "Лайка нет"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn unlike(
    State(state): State<AppState>,
    user: AuthUser,
    Path(username): Path<String>,
) -> AppResult<StatusCode> {
    let liked = refs::user(&state, &username).await?;
    sqlx::query("DELETE FROM user_likes WHERE user_id = $1 AND liked_id = $2")
        .bind(user.id)
        .bind(liked.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Кто лайкнул профиль, новые сверху.
#[utoipa::path(
    get, operation_id = "list_user_likes", path = "/users/{username}/likes", tag = "users",
    params(
        ("username" = String, Path, description = "username", example = "author"),
        PageQuery,
    ),
    responses(
        (status = 200, description = "Страница лайкнувших", body = Page<Like>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<Like>>> {
    let liked = refs::user(&state, &username).await?;
    let (limit, offset) = page_bounds(query.limit, query.offset);

    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM user_likes WHERE liked_id = $1")
        .bind(liked.id)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT user_id, created_at FROM user_likes WHERE liked_id = $1
         ORDER BY created_at DESC, user_id LIMIT $2 OFFSET $3",
    )
    .bind(liked.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    // Пользователи — одним вызовом справочника; удалённый прямо сейчас просто выпадает.
    let users = refs::users(&state, rows.iter().map(|(id, _)| *id).collect()).await?;
    let items = rows
        .into_iter()
        .filter_map(|(id, since)| {
            Some(Like {
                user: users.get(&id)?.clone(),
                since,
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

// ---------------------------------------------------------------- подборки

/// Подборка, если её видит `viewer` (публичная или своя), иначе 404.
async fn visible_collection(
    state: &AppState,
    id: Uuid,
    viewer: Option<&AuthUser>,
) -> AppResult<()> {
    let found: Option<(Uuid, bool)> =
        sqlx::query_as("SELECT user_id, is_public FROM collections WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    match found {
        Some((owner, is_public))
            if is_public || viewer.is_some_and(|viewer| viewer.id == owner) =>
        {
            Ok(())
        }
        _ => Err(AppError::NotFound),
    }
}

/// Голоса за подборку: сколько «нравится», сколько «не нравится» и свой голос.
#[utoipa::path(
    get, operation_id = "get_collection_likes", path = "/collections/{id}/likes",
    tag = "collections",
    security((), ("bearer" = [])),
    params(("id" = Uuid, Path, description = "id подборки")),
    responses(
        (status = 200, description = "Сводка", body = CollectionLikes),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Нет подборки или она чужая приватная", body = ErrorBody),
    )
)]
pub async fn collection_summary(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<CollectionLikes>> {
    visible_collection(&state, id, viewer.as_ref()).await?;
    let (likes, dislikes): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE value = 1), count(*) FILTER (WHERE value = -1)
           FROM collection_likes WHERE collection_id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    let mine: Option<i16> = match &viewer {
        None => None,
        Some(user) => {
            sqlx::query_scalar(
                "SELECT value FROM collection_likes WHERE collection_id = $1 AND user_id = $2",
            )
            .bind(id)
            .bind(user.id)
            .fetch_optional(&state.db)
            .await?
        }
    };
    Ok(Json(CollectionLikes {
        likes,
        dislikes,
        mine,
    }))
}

/// Поставить подборке «нравится» или «не нравится»; другой голос заменяет прежний.
#[utoipa::path(
    put, operation_id = "like_collection", path = "/collections/{id}/like", tag = "collections",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id подборки")),
    request_body = CollectionLike,
    responses(
        (status = 204, description = "Голос учтён"),
        (status = 400, description = "`value` не `1` и не `-1`", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет подборки или она чужая приватная", body = ErrorBody),
    )
)]
pub async fn like_collection(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<CollectionLike>,
) -> AppResult<StatusCode> {
    if req.value != 1 && req.value != -1 {
        return Err(AppError::BadRequest("value must be 1 or -1".into()).on_field("value"));
    }
    visible_collection(&state, id, Some(&user)).await?;
    sqlx::query(
        "INSERT INTO collection_likes (collection_id, user_id, value) VALUES ($1, $2, $3)
         ON CONFLICT (collection_id, user_id) DO UPDATE SET value = EXCLUDED.value,
             created_at = now()
         WHERE collection_likes.value <> EXCLUDED.value",
    )
    .bind(id)
    .bind(user.id)
    .bind(req.value)
    .execute(&state.db)
    .await
    .map_err(crate::validate::missing_reference)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Снять свой голос с подборки → 204 (даже если его не было).
#[utoipa::path(
    delete, operation_id = "unlike_collection", path = "/collections/{id}/like",
    tag = "collections",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id подборки")),
    responses(
        (status = 204, description = "Голоса нет"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет подборки или она чужая приватная", body = ErrorBody),
    )
)]
pub async fn unlike_collection(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    visible_collection(&state, id, Some(&user)).await?;
    sqlx::query("DELETE FROM collection_likes WHERE collection_id = $1 AND user_id = $2")
        .bind(id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- люди каталога

/// Лайкнуть человека каталога. Дизлайка здесь нет — как и у профилей.
#[utoipa::path(
    put, operation_id = "like_person", path = "/people/{slug}/like", tag = "people",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug человека", example = "denis-villeneuve")),
    responses(
        (status = 204, description = "Лайкнут"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Человека нет или он скрыт", body = ErrorBody),
    )
)]
pub async fn like_person(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let person = refs::person(&state, &slug).await?;
    sqlx::query(
        "INSERT INTO person_likes (person_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(person.id)
    .bind(user.id)
    .execute(&state.db)
    .await
    .map_err(crate::validate::missing_reference)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Снять свой лайк с человека → 204 (даже если его не было).
#[utoipa::path(
    delete, operation_id = "unlike_person", path = "/people/{slug}/like", tag = "people",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug человека", example = "denis-villeneuve")),
    responses(
        (status = 204, description = "Лайка нет"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Человека нет или он скрыт", body = ErrorBody),
    )
)]
pub async fn unlike_person(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let person = refs::person(&state, &slug).await?;
    sqlx::query("DELETE FROM person_likes WHERE person_id = $1 AND user_id = $2")
        .bind(person.id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Кто лайкнул человека, новые сверху.
#[utoipa::path(
    get, operation_id = "list_person_likes", path = "/people/{slug}/likes", tag = "people",
    params(
        ("slug" = String, Path, description = "slug человека", example = "denis-villeneuve"),
        PageQuery,
    ),
    responses(
        (status = 200, description = "Страница лайкнувших", body = Page<Like>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 404, description = "Человека нет или он скрыт", body = ErrorBody),
    )
)]
pub async fn person_likes(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<Like>>> {
    let person = refs::person(&state, &slug).await?;
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM person_likes WHERE person_id = $1")
        .bind(person.id)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT user_id, created_at FROM person_likes WHERE person_id = $1
         ORDER BY created_at DESC, user_id LIMIT $2 OFFSET $3",
    )
    .bind(person.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let users = refs::users(&state, rows.iter().map(|(id, _)| *id).collect()).await?;
    let items = rows
        .into_iter()
        .filter_map(|(id, since)| {
            Some(Like {
                user: users.get(&id)?.clone(),
                since,
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

// ---------------------------------------------------------------- произведения

/// Лайкнуть произведение. Повторный лайк ничего не меняет. Дизлайка нет — как у людей.
///
/// 409 `not_released` здесь не нужен (в отличие от оценок и реакций): понравиться может и
/// трейлер ещё не вышедшего фильма.
#[utoipa::path(
    put, operation_id = "like_entity", path = "/entities/{slug}/like", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 204, description = "Лайкнуто"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn like_entity(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    sqlx::query(
        "INSERT INTO entity_likes (entity_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(entity.id)
    .bind(user.id)
    .execute(&state.db)
    .await
    .map_err(crate::validate::missing_reference)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Снять свой лайк с произведения → 204 (даже если его не было).
#[utoipa::path(
    delete, operation_id = "unlike_entity", path = "/entities/{slug}/like", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 204, description = "Лайка нет"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn unlike_entity(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    sqlx::query("DELETE FROM entity_likes WHERE entity_id = $1 AND user_id = $2")
        .bind(entity.id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Сколько лайков у произведения и свой. То же число — в сводке оценок (`likes_count`),
/// чтобы карточке хватало одного запроса.
#[utoipa::path(
    get, operation_id = "get_entity_likes", path = "/entities/{slug}/likes", tag = "reviews",
    security((), ("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "Лайки", body = EntityLikes),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn entity_summary(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(slug): Path<String>,
) -> AppResult<Json<EntityLikes>> {
    let entity = refs::entity(&state, &slug).await?;
    let (likes, mine): (i64, bool) = sqlx::query_as(
        "SELECT count(*), bool_or(user_id = $2) IS TRUE FROM entity_likes WHERE entity_id = $1",
    )
    .bind(entity.id)
    .bind(viewer.map(|user| user.id))
    .fetch_one(&state.db)
    .await?;
    Ok(Json(EntityLikes { likes, mine }))
}

// ---------------------------------------------------------------- «Что нравится»

/// Три источника одной лентой: лайки подборок, людей и произведений.
const GIVEN: &str = "
    SELECT 'collection' AS source, collection_id AS target_id, value, created_at
      FROM collection_likes WHERE user_id = $1
    UNION ALL
    SELECT 'person', person_id, 1::smallint, created_at
      FROM person_likes WHERE user_id = $1
    UNION ALL
    SELECT 'entity', entity_id, 1::smallint, created_at
      FROM entity_likes WHERE user_id = $1";

/// Что пользователь лайкнул: раздел «Что нравится» в аналитике профиля.
///
/// Закрытость — как у остальной аналитики (`stats::visible`): владелец и admin всегда,
/// остальные при `stats_public`.
#[utoipa::path(
    get, operation_id = "list_given_likes", path = "/users/{username}/likes/given", tag = "users",
    security((), ("bearer" = [])),
    params(
        ("username" = String, Path, description = "username", example = "user"),
        GivenLikesQuery,
    ),
    responses(
        (status = 200, description = "Страница лайков", body = Page<GivenLike>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 401, description = "Токен передан, но недействителен", body = ErrorBody),
        (status = 403, description = "Аналитика закрыта владельцем (`stats_private`)", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn given(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(username): Path<String>,
    Query(query): Query<GivenLikesQuery>,
) -> AppResult<Json<Page<GivenLike>>> {
    let user = crate::stats::visible(&state, &username, viewer.as_ref()).await?;
    let (limit, offset) = page_bounds(query.limit, query.offset);

    let source = match query.target {
        None => None,
        Some(LikeTargetKind::Collection) => Some("collection"),
        Some(LikeTargetKind::Person) => Some("person"),
        Some(LikeTargetKind::Entity) => Some("entity"),
    };
    let filter = "FROM (".to_string() + GIVEN + ") AS feed WHERE ($2::text IS NULL OR source = $2)";

    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) {filter}"))
        .bind(user.id)
        .bind(source)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<GivenLikeRow> = sqlx::query_as(&format!(
        "SELECT * {filter} ORDER BY created_at DESC, target_id DESC LIMIT $3 OFFSET $4"
    ))
    .bind(user.id)
    .bind(source)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page {
        items: with_targets(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Раскрывает цели пакетно: подборки — своя таблица, люди — справочник каталога.
async fn with_targets(state: &AppState, rows: Vec<GivenLikeRow>) -> AppResult<Vec<GivenLike>> {
    let ids = |source: &str| -> Vec<Uuid> {
        rows.iter()
            .filter(|row| row.source == source)
            .map(|row| row.target_id)
            .collect()
    };
    let collection_ids = ids("collection");
    let collections: HashMap<Uuid, String> = if collection_ids.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as("SELECT id, title FROM collections WHERE id = ANY($1)")
            .bind(&collection_ids)
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .collect()
    };
    let people = refs::people(state, ids("person")).await?;
    let entities = refs::entities(state, ids("entity")).await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let target = match row.source.as_str() {
                "collection" => LikeTarget::Collection {
                    collection_id: row.target_id,
                    title: collections.get(&row.target_id)?.clone(),
                },
                "person" => LikeTarget::Person {
                    person: people.get(&row.target_id)?.clone(),
                },
                "entity" => LikeTarget::Entity {
                    entity: entities.get(&row.target_id)?.clone(),
                },
                _ => return None,
            };
            Some(GivenLike {
                target,
                value: row.value,
                created_at: row.created_at,
            })
        })
        .collect())
}
