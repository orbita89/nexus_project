//! Связанные подборки: какие коллекции админ показывает рядом с данной («Похожие подборки») и в
//! каком порядке. Связь направленная (обратную админ добавляет сам), до [`MAX_LINKS`] у коллекции.
//! Связывать можно только публичные; связанная, ставшая приватной, пропускается в выдаче (связь
//! остаётся и вернётся, когда коллекцию откроют), удалённая уходит каскадом. Позиции всегда
//! 0, 1, 2, … без пропусков.

use crate::collections;
use crate::models::{AddRelated, Collection, CollectionRow, SetRelated, COLLECTION_COLUMNS};
use crate::refs;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AdminUser, AppError, AppResult, AppState, AuthUser};
use sqlx::{PgConnection, PgExecutor};
use std::collections::HashSet;
use uuid::Uuid;

/// Больше связей у одной коллекции нельзя.
pub const MAX_LINKS: usize = 12;

/// Публичные связанные коллекции по порядку.
pub(crate) async fn related(state: &AppState, id: Uuid) -> AppResult<Vec<Collection>> {
    let rows: Vec<CollectionRow> = sqlx::query_as(&format!(
        "SELECT {COLLECTION_COLUMNS} FROM collection_links l
         JOIN collections c ON c.id = l.related_id
         WHERE l.collection_id = $1 AND c.is_public
         ORDER BY l.position, l.created_at"
    ))
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    refs::collections(state, rows).await
}

fn invalid(field: &str, message: impl Into<String>) -> AppError {
    AppError::Invalid {
        field: field.into(),
        message: message.into(),
    }
}

/// Коллекция-источник под блокировкой (связи меняются по очереди), иначе 404.
async fn lock(tx: &mut PgConnection, id: Uuid) -> AppResult<()> {
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM collections WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(tx)
            .await?;
    found.map(|_| ()).ok_or(AppError::NotFound)
}

/// Связываемые коллекции существуют (иначе 404) и публичны (иначе 400 `collection_private`).
async fn check_targets(db: impl PgExecutor<'_>, ids: &[Uuid]) -> AppResult<()> {
    let found: Vec<(Uuid, bool)> =
        sqlx::query_as("SELECT id, is_public FROM collections WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(db)
            .await?;
    if found.len() != ids.len() {
        return Err(AppError::NotFound);
    }
    if found.iter().any(|(_, public)| !public) {
        return Err(
            AppError::BadRequest("only public collections can be related".into())
                .with_code("collection_private"),
        );
    }
    Ok(())
}

/// Позиции подряд с 0 в текущем порядке.
async fn renumber(tx: &mut PgConnection, id: Uuid) -> AppResult<()> {
    sqlx::query(
        "UPDATE collection_links l SET position = s.n
         FROM (SELECT related_id,
                      row_number() OVER (ORDER BY position, created_at, related_id) - 1 AS n
               FROM collection_links WHERE collection_id = $1) s
         WHERE l.collection_id = $1 AND l.related_id = s.related_id AND l.position <> s.n",
    )
    .bind(id)
    .execute(tx)
    .await?;
    Ok(())
}

/// Связанные подборки коллекции по порядку: только публичные; связей нет — `[]`. Публичную
/// коллекцию кэширует CDN (`s-maxage=600`).
#[utoipa::path(
    get, operation_id = "list_related_collections", path = "/collections/{id}/related", tag = "collections",
    params(("id" = Uuid, Path, description = "id коллекции")),
    responses(
        (status = 200, description = "Связанные коллекции", body = Vec<Collection>),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Нет коллекции или она чужая приватная", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    let row = collections::load(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if !row.is_public && viewer.is_none_or(|viewer| viewer.id != row.user_id) {
        return Err(AppError::NotFound);
    }
    let body = Json(related(&state, id).await?);
    Ok(if row.is_public {
        ([(header::CACHE_CONTROL, "public, s-maxage=600")], body).into_response()
    } else {
        body.into_response()
    })
}

/// Заменить весь список связей: порядок в `ids` — порядок показа (перетаскивание в админке).
#[utoipa::path(
    put, operation_id = "admin_set_related_collections", path = "/admin/collections/{id}/related",
    tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    request_body = SetRelated,
    responses(
        (status = 200, description = "Актуальный список", body = Vec<Collection>),
        (status = 400, description = "`validation` с `field: ids` — сама коллекция, повтор, больше 12; `collection_private` — связываемая приватная", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет коллекции или одной из связываемых", body = ErrorBody),
    )
)]
pub async fn set(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<SetRelated>,
) -> AppResult<Json<Vec<Collection>>> {
    if req.ids.contains(&id) {
        return Err(invalid("ids", "a collection cannot be related to itself"));
    }
    if req.ids.iter().collect::<HashSet<_>>().len() != req.ids.len() {
        return Err(invalid("ids", "ids must not repeat"));
    }
    if req.ids.len() > MAX_LINKS {
        return Err(invalid(
            "ids",
            format!("no more than {MAX_LINKS} related collections"),
        ));
    }
    let mut tx = state.db.begin().await?;
    lock(&mut tx, id).await?;
    check_targets(&mut *tx, &req.ids).await?;
    sqlx::query("DELETE FROM collection_links WHERE collection_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO collection_links (collection_id, related_id, position)
         SELECT $1, related_id, n::int - 1 FROM unnest($2::uuid[]) WITH ORDINALITY AS t(related_id, n)",
    )
    .bind(id)
    .bind(&req.ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(admin_id = %admin.id, collection_id = %id, count = req.ids.len(), "related collections set by admin");
    Ok(Json(related(&state, id).await?))
}

/// Добавить одну связь: в конец или на `position` со сдвигом остальных. Уже есть — не меняется.
#[utoipa::path(
    put, operation_id = "admin_add_related_collection", path = "/admin/collections/{id}/related/{related_id}",
    tag = "social-admin",
    security(("bearer" = [])),
    params(
        ("id" = Uuid, Path, description = "id коллекции"),
        ("related_id" = Uuid, Path, description = "id связываемой коллекции"),
    ),
    request_body(content = AddRelated, description = "Можно не передавать тело: в конец"),
    responses(
        (status = 200, description = "Актуальный список", body = Vec<Collection>),
        (status = 400, description = "`validation`: `field: related_id` — сама коллекция, `field: ids` — уже 12 связей; `collection_private` — связываемая приватная", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет коллекции или связываемой", body = ErrorBody),
    )
)]
pub async fn add(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((id, related_id)): Path<(Uuid, Uuid)>,
    body: Option<JsonBody<AddRelated>>,
) -> AppResult<Json<Vec<Collection>>> {
    let req = body.map(|JsonBody(req)| req).unwrap_or_default();
    if related_id == id {
        return Err(invalid(
            "related_id",
            "a collection cannot be related to itself",
        ));
    }
    let mut tx = state.db.begin().await?;
    lock(&mut tx, id).await?;
    check_targets(&mut *tx, &[related_id]).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM collection_links
                        WHERE collection_id = $1 AND related_id = $2)",
    )
    .bind(id)
    .bind(related_id)
    .fetch_one(&mut *tx)
    .await?;
    if !exists {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM collection_links WHERE collection_id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if count as usize >= MAX_LINKS {
            return Err(invalid(
                "ids",
                format!("no more than {MAX_LINKS} related collections"),
            ));
        }
        let position = req.position.map_or(count, |p| i64::from(p).clamp(0, count)) as i32;
        sqlx::query(
            "UPDATE collection_links SET position = position + 1
             WHERE collection_id = $1 AND position >= $2",
        )
        .bind(id)
        .bind(position)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO collection_links (collection_id, related_id, position) VALUES ($1, $2, $3)",
        )
        .bind(id)
        .bind(related_id)
        .bind(position)
        .execute(&mut *tx)
        .await?;
        renumber(&mut tx, id).await?;
        tracing::info!(admin_id = %admin.id, collection_id = %id, %related_id, "related collection added by admin");
    }
    tx.commit().await?;
    Ok(Json(related(&state, id).await?))
}

/// Убрать связь → 204 (даже если её не было).
#[utoipa::path(
    delete, operation_id = "admin_remove_related_collection", path = "/admin/collections/{id}/related/{related_id}",
    tag = "social-admin",
    security(("bearer" = [])),
    params(
        ("id" = Uuid, Path, description = "id коллекции"),
        ("related_id" = Uuid, Path, description = "id связанной коллекции"),
    ),
    responses(
        (status = 204, description = "Связи нет"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет коллекции", body = ErrorBody),
    )
)]
pub async fn remove(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((id, related_id)): Path<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    lock(&mut tx, id).await?;
    let deleted =
        sqlx::query("DELETE FROM collection_links WHERE collection_id = $1 AND related_id = $2")
            .bind(id)
            .bind(related_id)
            .execute(&mut *tx)
            .await?;
    if deleted.rows_affected() > 0 {
        renumber(&mut tx, id).await?;
        tracing::info!(admin_id = %admin.id, collection_id = %id, %related_id, "related collection removed by admin");
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
