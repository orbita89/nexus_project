//! Админка связанных произведений: сиквелы, приквелы, ремейки. Связь меняет карточки обеих
//! сущностей — публикуются обе.

use crate::entities::detail_by_id;
use crate::jobs::Reporter;
use crate::models::{PutRelation, RelatedWork, RelationKind, RelationQuery};
use crate::publish::{self, Touched};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::{AdminUser, AppError, AppResult, AppState};
use uuid::Uuid;

/// Строка `entity_relations`: `(from, to, kind)`. Приквел — сиквел в обратную сторону.
fn stored(id: Uuid, other: Uuid, kind: RelationKind) -> (Uuid, Uuid, &'static str) {
    match kind {
        RelationKind::Sequel => (id, other, "sequel"),
        RelationKind::Prequel => (other, id, "sequel"),
        RelationKind::Remake => (id, other, "remake"),
    }
}

/// Связать сущность с другой: `to_id` — её сиквел, приквел или ремейк.
#[utoipa::path(
    put, operation_id = "put_entity_relation", path = "/admin/entities/{id}/relations",
    tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сущности")),
    request_body = PutRelation,
    responses(
        (status = 200, description = "Связанные произведения сущности после правки (общая лента для карусели)", body = Vec<RelatedWork>),
        (status = 400, description = "Неверные поля или `to_id` не найдена", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
        (status = 409, description = "Связь с собой (`self_relation`) или такая связь уже есть (`conflict`)", body = ErrorBody),
    )
)]
pub async fn put(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<PutRelation>,
) -> AppResult<Json<Vec<RelatedWork>>> {
    if req.to_id == id {
        return Err(
            AppError::Conflict("entity cannot relate to itself".into()).with_code("self_relation")
        );
    }
    let position = req.position.unwrap_or(0);
    if position < 0 {
        return Err(AppError::BadRequest("position must not be negative".into()));
    }
    let (from, to, kind) = stored(id, req.to_id, req.kind);

    let mut tx = state.db.begin().await?;
    // Обе сущности на месте до конца транзакции: параллельное удаление не проскочит.
    let found: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM entities WHERE id = ANY($1) FOR KEY SHARE")
            .bind([id, req.to_id])
            .fetch_all(&mut *tx)
            .await?;
    if !found.contains(&id) {
        return Err(AppError::NotFound);
    }
    if !found.contains(&req.to_id) {
        return Err(AppError::BadRequest("to_id: entity not found".into()));
    }
    // Связь в любую сторону того же вида: повтор, сиквел наоборот, тот же ремейк с другой стороны.
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM entity_relations WHERE kind = $3
                        AND ((from_id = $1 AND to_id = $2) OR (from_id = $2 AND to_id = $1)))",
    )
    .bind(from)
    .bind(to)
    .bind(kind)
    .fetch_one(&mut *tx)
    .await?;
    if exists {
        return Err(AppError::Conflict("relation already exists".into()));
    }
    sqlx::query(
        "INSERT INTO entity_relations (from_id, to_id, kind, position) VALUES ($1, $2, $3, $4)",
    )
    .bind(from)
    .bind(to)
    .bind(kind)
    .bind(position)
    .execute(&mut *tx)
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            AppError::Conflict("relation already exists".into())
        }
        _ => AppError::Database(error),
    })?;
    tx.commit().await?;

    publish::run(
        &state,
        Touched::entities(&[id, req.to_id]),
        &Reporter::silent(),
    )
    .await;
    tracing::info!(admin_id = %admin.id, entity_id = %id, to_id = %req.to_id, ?req.kind, "relation added");
    Ok(Json(detail_by_id(&state.db, id).await?.related))
}

/// Убрать связь с `to_id` этого вида (с какой бы стороны она ни была записана).
#[utoipa::path(
    delete, operation_id = "delete_entity_relation", path = "/admin/entities/{id}/relations/{to_id}",
    tag = "catalog-admin",
    security(("bearer" = [])),
    params(
        ("id" = Uuid, Path, description = "id сущности"),
        ("to_id" = Uuid, Path, description = "id связанной сущности"),
        RelationQuery,
    ),
    responses(
        (status = 204, description = "Удалена"),
        (status = 400, description = "Нет или неверный `kind`", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Такой связи нет", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((id, to_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<RelationQuery>,
) -> AppResult<StatusCode> {
    let (from, to, kind) = stored(id, to_id, query.kind);
    // Сиквел и приквел — одна строка, направление важно; ремейк — в любую сторону.
    let deleted = sqlx::query(
        "DELETE FROM entity_relations WHERE kind = $3
         AND ((from_id = $1 AND to_id = $2) OR ($3 = 'remake' AND from_id = $2 AND to_id = $1))",
    )
    .bind(from)
    .bind(to)
    .bind(kind)
    .execute(&state.db)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    publish::run(&state, Touched::entities(&[id, to_id]), &Reporter::silent()).await;
    tracing::info!(admin_id = %admin.id, entity_id = %id, %to_id, ?query.kind, "relation deleted");
    Ok(StatusCode::NO_CONTENT)
}
