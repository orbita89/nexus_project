//! Админка тегов.

use super::affected_entities;
use crate::models::{CreateTag, Tag, UpdateTag};
use crate::search;
use crate::validate::{self, MAX_TITLE};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AdminUser, AppError, AppResult, AppState};
use uuid::Uuid;

const TAG_ENTITIES: &str = "SELECT entity_id FROM entity_tags WHERE tag_id = $1";

/// Создать тег.
#[utoipa::path(
    post, operation_id = "create_tag", path = "/admin/tags", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = CreateTag,
    responses(
        (status = 201, description = "Создан", body = Tag),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "slug занят", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(req): JsonBody<CreateTag>,
) -> AppResult<(StatusCode, Json<Tag>)> {
    validate::slug("slug", &req.slug)?;
    let name = validate::required("name", &req.name, MAX_TITLE)?;

    let tag: Tag =
        sqlx::query_as("INSERT INTO tags (slug, name) VALUES ($1, $2) RETURNING id, slug, name")
            .bind(&req.slug)
            .bind(name)
            .fetch_one(&state.db)
            .await
            .map_err(validate::conflict("slug is already taken"))?;

    tracing::info!(admin_id = %admin.id, tag_id = %tag.id, slug = %tag.slug, "tag created");
    Ok((StatusCode::CREATED, Json(tag)))
}

/// Переименовать тег или сменить slug.
#[utoipa::path(
    patch, operation_id = "update_tag", path = "/admin/tags/{id}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id тега")),
    request_body = UpdateTag,
    responses(
        (status = 200, description = "Изменён", body = Tag),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найден", body = ErrorBody),
        (status = 409, description = "slug занят", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<UpdateTag>,
) -> AppResult<Json<Tag>> {
    if let Some(slug) = &req.slug {
        validate::slug("slug", slug)?;
    }
    let name = req
        .name
        .map(|name| validate::required("name", &name, MAX_TITLE))
        .transpose()?;

    let tag: Option<Tag> = sqlx::query_as(
        "UPDATE tags SET slug = COALESCE($2, slug), name = COALESCE($3, name)
         WHERE id = $1 RETURNING id, slug, name",
    )
    .bind(id)
    .bind(&req.slug)
    .bind(name)
    .fetch_optional(&state.db)
    .await
    .map_err(validate::conflict("slug is already taken"))?;
    let tag = tag.ok_or(AppError::NotFound)?;

    // slug и название тега есть в поисковых документах сущностей.
    let ids = affected_entities(&state.db, TAG_ENTITIES, id).await?;
    search::sync(&state, &ids).await;
    tracing::info!(admin_id = %admin.id, tag_id = %id, "tag updated");
    Ok(Json(tag))
}

/// Удалить тег (снимается со всех сущностей).
#[utoipa::path(
    delete, operation_id = "delete_tag", path = "/admin/tags/{id}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id тега")),
    responses(
        (status = 204, description = "Удалён"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найден", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let ids = affected_entities(&state.db, TAG_ENTITIES, id).await?;
    let deleted = sqlx::query("DELETE FROM tags WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    search::sync(&state, &ids).await;
    tracing::info!(admin_id = %admin.id, tag_id = %id, "tag deleted");
    Ok(StatusCode::NO_CONTENT)
}
