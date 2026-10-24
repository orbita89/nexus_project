//! Реакции зрителей на произведение: 🔥 😢 🤔 😴. Одна на зрителя; счётчики — в сводке
//! `GET /entities/{slug}/rating`, своя реакция — отдельным запросом (сводка одинакова для всех).

use crate::models::{CollectionReactions, Reaction, ReactionKind, Reactions};
use crate::refs;
use crate::validate;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AppError, AppResult, AppState, AuthUser};

/// Своя реакция на произведение.
#[utoipa::path(
    get, operation_id = "get_own_reaction", path = "/entities/{slug}/reaction", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "Реакция", body = Reaction),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена или реакции нет", body = ErrorBody),
    )
)]
pub async fn get_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<Reaction>> {
    let entity = refs::entity(&state, &slug).await?;
    let kind: Option<ReactionKind> = sqlx::query_scalar(
        "SELECT kind FROM entity_reactions WHERE entity_id = $1 AND user_id = $2",
    )
    .bind(entity.id)
    .bind(user.id)
    .fetch_optional(&state.db)
    .await?;
    let kind = kind.ok_or(AppError::NotFound)?;
    Ok(Json(Reaction { kind }))
}

/// Поставить реакцию; другая заменяет прежнюю. До выхода — нельзя, как и оценки.
#[utoipa::path(
    put, operation_id = "put_reaction", path = "/entities/{slug}/reaction", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    request_body = Reaction,
    responses(
        (status = 200, description = "Реакция сохранена", body = Reaction),
        (status = 400, description = "Неизвестная реакция", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
        (status = 409, description = "Ещё не вышла (`code`: `not_released`)", body = ErrorBody),
    )
)]
pub async fn put(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<Reaction>,
) -> AppResult<Json<Reaction>> {
    let entity = refs::entity(&state, &slug).await?;
    refs::require_released(&entity)?;
    sqlx::query(
        "INSERT INTO entity_reactions (entity_id, user_id, kind) VALUES ($1, $2, $3)
         ON CONFLICT (entity_id, user_id) DO UPDATE SET kind = EXCLUDED.kind, created_at = now()
         WHERE entity_reactions.kind <> EXCLUDED.kind",
    )
    .bind(entity.id)
    .bind(user.id)
    .bind(req.kind)
    .execute(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    Ok(Json(req))
}

/// Убрать свою реакцию → 204 (даже если её не было).
#[utoipa::path(
    delete, operation_id = "delete_reaction", path = "/entities/{slug}/reaction", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 204, description = "Реакции нет"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    sqlx::query("DELETE FROM entity_reactions WHERE entity_id = $1 AND user_id = $2")
        .bind(entity.id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- реакции на коллекции

/// Коллекция, если её видит `viewer` (публичная или своя), иначе 404.
async fn visible_collection(state: &AppState, id: uuid::Uuid, viewer: &AuthUser) -> AppResult<()> {
    let found: Option<(uuid::Uuid, bool)> =
        sqlx::query_as("SELECT user_id, is_public FROM collections WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    match found {
        Some((owner, is_public)) if is_public || owner == viewer.id => Ok(()),
        _ => Err(AppError::NotFound),
    }
}

/// Сводка реакций на коллекцию: сколько каких и своя (вошедшему).
#[utoipa::path(
    get, operation_id = "get_collection_reactions", path = "/collections/{id}/reactions",
    tag = "collections",
    security((), ("bearer" = [])),
    params(("id" = uuid::Uuid, Path, description = "id коллекции")),
    responses(
        (status = 200, description = "Сводка", body = CollectionReactions),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Нет коллекции или она чужая приватная", body = ErrorBody),
    )
)]
pub async fn collection_summary(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(id): Path<uuid::Uuid>,
) -> AppResult<Json<CollectionReactions>> {
    // Гостю коллекция видна, только если она публичная: проверку делает тот же запрос.
    let found: Option<(uuid::Uuid, bool)> =
        sqlx::query_as("SELECT user_id, is_public FROM collections WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    match found {
        Some((owner, is_public))
            if is_public || viewer.as_ref().is_some_and(|user| user.id == owner) => {}
        _ => return Err(AppError::NotFound),
    }

    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kind, count(*) FROM collection_reactions WHERE collection_id = $1 GROUP BY kind",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let mut counts = Reactions::default();
    for (kind, n) in rows {
        match kind.as_str() {
            "fire" => counts.fire = n,
            "sad" => counts.sad = n,
            "think" => counts.think = n,
            "bored" => counts.bored = n,
            _ => {}
        }
    }
    let mine: Option<ReactionKind> =
        match &viewer {
            None => None,
            Some(user) => sqlx::query_scalar(
                "SELECT kind FROM collection_reactions WHERE collection_id = $1 AND user_id = $2",
            )
            .bind(id)
            .bind(user.id)
            .fetch_optional(&state.db)
            .await?,
        };
    Ok(Json(CollectionReactions {
        reactions: counts,
        mine,
    }))
}

/// Поставить реакцию на коллекцию; другая заменяет прежнюю.
#[utoipa::path(
    put, operation_id = "put_collection_reaction", path = "/collections/{id}/reaction",
    tag = "collections",
    security(("bearer" = [])),
    params(("id" = uuid::Uuid, Path, description = "id коллекции")),
    request_body = Reaction,
    responses(
        (status = 200, description = "Реакция сохранена", body = Reaction),
        (status = 400, description = "Неизвестная реакция", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет коллекции или она чужая приватная", body = ErrorBody),
    )
)]
pub async fn put_collection(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<uuid::Uuid>,
    JsonBody(req): JsonBody<Reaction>,
) -> AppResult<Json<Reaction>> {
    visible_collection(&state, id, &user).await?;
    sqlx::query(
        "INSERT INTO collection_reactions (collection_id, user_id, kind) VALUES ($1, $2, $3)
         ON CONFLICT (collection_id, user_id) DO UPDATE SET kind = EXCLUDED.kind,
             created_at = now()
         WHERE collection_reactions.kind <> EXCLUDED.kind",
    )
    .bind(id)
    .bind(user.id)
    .bind(req.kind)
    .execute(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    Ok(Json(req))
}

/// Убрать свою реакцию с коллекции → 204 (даже если её не было).
#[utoipa::path(
    delete, operation_id = "delete_collection_reaction", path = "/collections/{id}/reaction",
    tag = "collections",
    security(("bearer" = [])),
    params(("id" = uuid::Uuid, Path, description = "id коллекции")),
    responses(
        (status = 204, description = "Реакции нет"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет коллекции или она чужая приватная", body = ErrorBody),
    )
)]
pub async fn delete_collection(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<uuid::Uuid>,
) -> AppResult<StatusCode> {
    visible_collection(&state, id, &user).await?;
    sqlx::query("DELETE FROM collection_reactions WHERE collection_id = $1 AND user_id = $2")
        .bind(id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
