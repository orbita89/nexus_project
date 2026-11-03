//! Реакции зрителей на произведение: любое эмодзи, сколько угодно разных от одного зрителя
//! (как в Slack). Счётчики — `GET /entities/{slug}/reactions` и сводка оценок; своя отметка —
//! поле `mine` там же.
//!
//! Старые четыре вида (🔥 😢 🤔 😴) никуда не делись: это обычные значения `emoji`, поэтому
//! `reactions` в сводке оценок продолжает считаться по ним.

use crate::models::{
    CollectionReactions, DeleteReaction, EmojiReaction, EntityReactions, Reaction, ReactionItem,
    ReactionKind, Reactions,
};
use crate::{events, refs, validate};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AppError, AppResult, AppState, AuthUser};
use uuid::Uuid;

/// Реакции произведения и своя отметка. Публичный: у гостя все `mine: false`.
#[utoipa::path(
    get, operation_id = "list_entity_reactions", path = "/entities/{slug}/reactions",
    tag = "reviews",
    security((), ("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "Реакции", body = EntityReactions),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(slug): Path<String>,
) -> AppResult<Json<EntityReactions>> {
    let entity = refs::entity(&state, &slug).await?;
    let items = items(&state, entity.id, viewer.map(|user| user.id)).await?;
    Ok(Json(EntityReactions { items }))
}

/// Реакции произведения: по убыванию числа, при равенстве — чья первая появилась раньше.
pub(crate) async fn items(
    state: &AppState,
    entity_id: Uuid,
    viewer: Option<Uuid>,
) -> AppResult<Vec<ReactionItem>> {
    Ok(sqlx::query_as(
        "SELECT emoji, count(*) AS count,
                -- У гостя $2 = NULL, bool_or тогда тоже NULL: IS TRUE делает из него false.
                bool_or(user_id = $2) IS TRUE AS mine
           FROM entity_reactions WHERE entity_id = $1
          GROUP BY emoji
          ORDER BY count(*) DESC, min(created_at), emoji",
    )
    .bind(entity_id)
    .bind(viewer)
    .fetch_all(&state.db)
    .await?)
}

/// Своя реакция одним из четырёх старых видов.
///
/// Устарело: реакций у зрителя теперь может быть несколько, и эмодзи — любое. Отдаёт самую
/// раннюю свою реакцию, если это 🔥 😢 🤔 😴, иначе 404; весь набор — в `GET …/reactions`.
#[utoipa::path(
    get, operation_id = "get_own_reaction", path = "/entities/{slug}/reaction", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "Реакция", body = Reaction),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена или своей реакции из четырёх нет", body = ErrorBody),
    )
)]
pub async fn get_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<Reaction>> {
    let entity = refs::entity(&state, &slug).await?;
    let emojis: Vec<String> = sqlx::query_scalar(
        "SELECT emoji FROM entity_reactions WHERE entity_id = $1 AND user_id = $2
         ORDER BY created_at, emoji",
    )
    .bind(entity.id)
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    let kind = emojis
        .iter()
        .find_map(|emoji| ReactionKind::from_emoji(emoji))
        .ok_or(AppError::NotFound)?;
    Ok(Json(Reaction { kind }))
}

/// Поставить реакцию. Несколько разных можно, повторная та же ничего не меняет.
/// До выхода — нельзя, как и оценки.
#[utoipa::path(
    put, operation_id = "put_reaction", path = "/entities/{slug}/reaction", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    request_body = EmojiReaction,
    responses(
        (status = 204, description = "Реакция сохранена"),
        (status = 400, description = "Не эмодзи (`code`: `bad_emoji`)", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
        (status = 409, description = "Ещё не вышла (`not_released`) или у произведения уже 30 разных эмодзи (`reaction_limit`)", body = ErrorBody),
    )
)]
pub async fn put(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<EmojiReaction>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    refs::require_released(&entity)?;
    let emoji = validate::emoji(&req.emoji)?;

    let mut tx = state.db.begin().await?;
    // Предел разных эмодзи у произведения: когда он достигнут, новое принимается, только если
    // такое уже кто-то ставил. Считаем в той же транзакции — иначе два запроса проскочат разом.
    let known: Option<bool> = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM entity_reactions WHERE entity_id = $1 AND emoji = $2)
          WHERE (SELECT count(DISTINCT emoji) FROM entity_reactions WHERE entity_id = $1) >= $3",
    )
    .bind(entity.id)
    .bind(&emoji)
    .bind(validate::MAX_ENTITY_EMOJI)
    .fetch_optional(&mut *tx)
    .await?;
    if known == Some(false) {
        return Err(AppError::Conflict(format!(
            "entity already has {} different emoji",
            validate::MAX_ENTITY_EMOJI
        ))
        .with_code("reaction_limit"));
    }
    let added = sqlx::query(
        "INSERT INTO entity_reactions (entity_id, user_id, emoji) VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(entity.id)
    .bind(user.id)
    .bind(&emoji)
    .execute(&mut *tx)
    .await
    .map_err(validate::missing_reference)?
    .rows_affected()
        > 0;
    tx.commit().await?;

    if added {
        events::publish(&state, events::entity_reacted(entity.id, user.id));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Убрать свою реакцию → 204 (даже если её не было). С телом `{"emoji": "❤️"}` снимается
/// именно она, без тела — все свои реакции на это произведение.
#[utoipa::path(
    delete, operation_id = "delete_reaction", path = "/entities/{slug}/reaction", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    request_body(content = DeleteReaction, description = "Какую снять; без тела — все свои"),
    responses(
        (status = 204, description = "Реакции нет"),
        (status = 400, description = "Не эмодзи (`code`: `bad_emoji`)", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    body: Option<JsonBody<DeleteReaction>>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    let emoji = match body.map(|JsonBody(req)| req).unwrap_or_default().emoji {
        Some(emoji) => Some(validate::emoji(&emoji)?),
        None => None,
    };
    let removed = sqlx::query(
        "DELETE FROM entity_reactions
          WHERE entity_id = $1 AND user_id = $2 AND ($3::text IS NULL OR emoji = $3)",
    )
    .bind(entity.id)
    .bind(user.id)
    .bind(&emoji)
    .execute(&state.db)
    .await?
    .rows_affected()
        > 0;
    if removed {
        events::publish(&state, events::entity_reacted(entity.id, user.id));
    }
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
