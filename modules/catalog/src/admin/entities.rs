//! Админка сущностей: создание, изменение, удаление, теги и участники.

use super::nullable_param;
use crate::entities::{detail_by_id, tags_of, CREDIT_COLUMNS};
use crate::models::{
    AddCredit, CreateEntity, EntityCredit, EntityCreditRow, EntityDetail, EntityKind, SetTags, Tag,
    UpdateEntity,
};
use crate::validate::{self, MAX_LONG_TEXT, MAX_TITLE};
use crate::{metadata, search};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AdminUser, AppError, AppResult, AppState};
use sqlx::PgConnection;
use uuid::Uuid;

/// Создать сущность. `metadata` проверяется по схеме своего `kind`.
#[utoipa::path(
    post, path = "/admin/entities", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = CreateEntity,
    responses(
        (status = 201, description = "Создана", body = EntityDetail),
        (status = 400, description = "Неверные поля, metadata или неизвестный тег", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "slug занят", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(req): JsonBody<CreateEntity>,
) -> AppResult<(StatusCode, Json<EntityDetail>)> {
    validate::slug("slug", &req.slug)?;
    let title = validate::required("title", &req.title, MAX_TITLE)?;
    let original_title = validate::optional("original_title", req.original_title, MAX_TITLE)?;
    let description = validate::optional("description", req.description, MAX_LONG_TEXT)?;
    let cover_url = validate::url("cover_url", req.cover_url)?;
    let metadata = metadata::validate(req.kind, req.metadata.unwrap_or_else(|| json!({})))?;

    let mut tx = state.db.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO entities
             (kind, slug, title, original_title, description, release_date, cover_url, metadata)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(req.kind)
    .bind(&req.slug)
    .bind(title)
    .bind(original_title)
    .bind(description)
    .bind(req.release_date)
    .bind(cover_url)
    .bind(metadata)
    .fetch_one(&mut *tx)
    .await
    .map_err(validate::conflict("slug is already taken"))?;
    replace_tags(&mut tx, id, &req.tags).await?;
    tx.commit().await?;

    search::sync(&state, &[id]).await;
    tracing::info!(admin_id = %admin.id, entity_id = %id, slug = %req.slug, "entity created");
    Ok((
        StatusCode::CREATED,
        Json(detail_by_id(&state.db, id).await?),
    ))
}

/// Изменить сущность. Переданные поля заменяются, `null` очищает необязательное поле;
/// `metadata` заменяется целиком. `kind` не меняется.
#[utoipa::path(
    patch, path = "/admin/entities/{id}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сущности")),
    request_body = UpdateEntity,
    responses(
        (status = 200, description = "Изменена", body = EntityDetail),
        (status = 400, description = "Неверные поля или metadata", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
        (status = 409, description = "slug занят", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<UpdateEntity>,
) -> AppResult<Json<EntityDetail>> {
    let kind: Option<EntityKind> = sqlx::query_scalar("SELECT kind FROM entities WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let kind = kind.ok_or(AppError::NotFound)?;

    if let Some(slug) = &req.slug {
        validate::slug("slug", slug)?;
    }
    let title = req
        .title
        .map(|title| validate::required("title", &title, MAX_TITLE))
        .transpose()?;
    let original_title = nullable_param(
        req.original_title
            .map(|v| validate::optional("original_title", v, MAX_TITLE))
            .transpose()?,
    );
    let description = nullable_param(
        req.description
            .map(|v| validate::optional("description", v, MAX_LONG_TEXT))
            .transpose()?,
    );
    let release_date = nullable_param(req.release_date);
    let cover_url = nullable_param(
        req.cover_url
            .map(|v| validate::url("cover_url", v))
            .transpose()?,
    );
    let metadata = req
        .metadata
        .map(|value| metadata::validate(kind, value))
        .transpose()?;

    let updated = sqlx::query(
        "UPDATE entities SET
             slug = COALESCE($2, slug),
             title = COALESCE($3, title),
             original_title = CASE WHEN $4 THEN $5 ELSE original_title END,
             description = CASE WHEN $6 THEN $7 ELSE description END,
             release_date = CASE WHEN $8 THEN $9 ELSE release_date END,
             cover_url = CASE WHEN $10 THEN $11 ELSE cover_url END,
             metadata = COALESCE($12, metadata)
         WHERE id = $1",
    )
    .bind(id)
    .bind(&req.slug)
    .bind(title)
    .bind(original_title.0)
    .bind(original_title.1)
    .bind(description.0)
    .bind(description.1)
    .bind(release_date.0)
    .bind(release_date.1)
    .bind(cover_url.0)
    .bind(cover_url.1)
    .bind(metadata)
    .execute(&state.db)
    .await
    .map_err(validate::conflict("slug is already taken"))?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }

    search::sync(&state, &[id]).await;
    tracing::info!(admin_id = %admin.id, entity_id = %id, "entity updated");
    Ok(Json(detail_by_id(&state.db, id).await?))
}

/// Удалить сущность. Теги, участники, рецензии и элементы коллекций удаляются каскадно.
#[utoipa::path(
    delete, path = "/admin/entities/{id}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сущности")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let deleted = sqlx::query("DELETE FROM entities WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    search::sync(&state, &[id]).await;
    tracing::info!(admin_id = %admin.id, entity_id = %id, "entity deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// Заменить теги сущности целиком.
#[utoipa::path(
    put, path = "/admin/entities/{id}/tags", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сущности")),
    request_body = SetTags,
    responses(
        (status = 200, description = "Новые теги сущности", body = Vec<Tag>),
        (status = 400, description = "Неизвестный тег", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn set_tags(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<SetTags>,
) -> AppResult<Json<Vec<Tag>>> {
    let mut tx = state.db.begin().await?;
    // FOR UPDATE: параллельное удаление сущности не проскочит между проверкой и вставкой.
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM entities WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    exists.ok_or(AppError::NotFound)?;
    replace_tags(&mut tx, id, &req.tags).await?;
    tx.commit().await?;

    search::sync(&state, &[id]).await;
    tracing::info!(admin_id = %admin.id, entity_id = %id, tags = ?req.tags, "entity tags set");
    Ok(Json(tags_of(&state.db, id).await?))
}

/// Заменяет теги сущности. Неизвестный slug — 400 со списком таких slug'ов.
async fn replace_tags(conn: &mut PgConnection, entity_id: Uuid, slugs: &[String]) -> AppResult<()> {
    let mut slugs: Vec<&str> = slugs.iter().map(String::as_str).collect();
    slugs.sort_unstable();
    slugs.dedup();

    let found: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, slug FROM tags WHERE slug = ANY($1)")
            .bind(&slugs)
            .fetch_all(&mut *conn)
            .await?;
    if found.len() != slugs.len() {
        let unknown: Vec<&str> = slugs
            .into_iter()
            .filter(|slug| !found.iter().any(|(_, s)| s == slug))
            .collect();
        return Err(AppError::BadRequest(format!(
            "unknown tags: {}",
            unknown.join(", ")
        )));
    }
    let ids: Vec<Uuid> = found.into_iter().map(|(id, _)| id).collect();

    sqlx::query("DELETE FROM entity_tags WHERE entity_id = $1")
        .bind(entity_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("INSERT INTO entity_tags (entity_id, tag_id) SELECT $1, unnest($2::uuid[])")
        .bind(entity_id)
        .bind(&ids)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Добавить участника: человек, роль, персонаж (для актёров), порядок в титрах.
#[utoipa::path(
    post, path = "/admin/entities/{id}/credits", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сущности")),
    request_body = AddCredit,
    responses(
        (status = 201, description = "Добавлен", body = EntityCredit),
        (status = 400, description = "Неверные поля или человек не найден", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
        (status = 409, description = "Такое участие уже есть", body = ErrorBody),
    )
)]
pub async fn add_credit(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<AddCredit>,
) -> AppResult<(StatusCode, Json<EntityCredit>)> {
    let role = validate::role(&req.role)?;
    let character_name = validate::optional("character_name", req.character_name, MAX_TITLE)?;
    let position = req.position.unwrap_or(0);
    if position < 0 {
        return Err(AppError::BadRequest("position must not be negative".into()));
    }

    let credit_id: Uuid = sqlx::query_scalar(
        "INSERT INTO entity_credits (entity_id, person_id, role, character_name, position)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(id)
    .bind(req.person_id)
    .bind(role)
    .bind(character_name)
    .bind(position)
    .fetch_one(&state.db)
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            AppError::Conflict("credit already exists".into())
        }
        sqlx::Error::Database(db) if db.is_foreign_key_violation() => match db.constraint() {
            Some("entity_credits_person_id_fkey") => {
                AppError::BadRequest("person not found".into())
            }
            _ => AppError::NotFound,
        },
        _ => AppError::Database(error),
    })?;

    let row: EntityCreditRow = sqlx::query_as(&format!(
        "SELECT {CREDIT_COLUMNS} FROM entity_credits c JOIN people p ON p.id = c.person_id
         WHERE c.id = $1"
    ))
    .bind(credit_id)
    .fetch_one(&state.db)
    .await?;

    search::sync(&state, &[id]).await;
    tracing::info!(admin_id = %admin.id, entity_id = %id, %credit_id, "credit added");
    Ok((StatusCode::CREATED, Json(row.into())))
}

/// Удалить участника.
#[utoipa::path(
    delete, path = "/admin/entities/{id}/credits/{credit_id}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(
        ("id" = Uuid, Path, description = "id сущности"),
        ("credit_id" = Uuid, Path, description = "id участия (`credits[].id` в карточке)"),
    ),
    responses(
        (status = 204, description = "Удалён"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найден", body = ErrorBody),
    )
)]
pub async fn delete_credit(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((id, credit_id)): Path<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    let deleted = sqlx::query("DELETE FROM entity_credits WHERE id = $2 AND entity_id = $1")
        .bind(id)
        .bind(credit_id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    search::sync(&state, &[id]).await;
    tracing::info!(admin_id = %admin.id, entity_id = %id, %credit_id, "credit deleted");
    Ok(StatusCode::NO_CONTENT)
}
