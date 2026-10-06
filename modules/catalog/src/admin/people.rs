//! Админка людей.

use super::{affected_entities, nullable_param};
use crate::models::{CreatePerson, Person, UpdatePerson, PERSON_COLUMNS};
use crate::search;
use crate::validate::{self, MAX_LONG_TEXT, MAX_TITLE};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AdminUser, AppError, AppResult, AppState};
use uuid::Uuid;

const PERSON_ENTITIES: &str = "SELECT DISTINCT entity_id FROM entity_credits WHERE person_id = $1";

/// Добавить человека.
#[utoipa::path(
    post, operation_id = "create_person", path = "/admin/people", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = CreatePerson,
    responses(
        (status = 201, description = "Добавлен", body = Person),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "slug занят", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(req): JsonBody<CreatePerson>,
) -> AppResult<(StatusCode, Json<Person>)> {
    validate::slug("slug", &req.slug)?;
    let full_name = validate::required("full_name", &req.full_name, MAX_TITLE)?;
    let photo_url = validate::url("photo_url", req.photo_url)?;
    let bio = validate::optional("bio", req.bio, MAX_LONG_TEXT)?;

    let person: Person = sqlx::query_as(&format!(
        "INSERT INTO people AS p (slug, full_name, birth_date, photo_url, bio)
         VALUES ($1, $2, $3, $4, $5) RETURNING {PERSON_COLUMNS}"
    ))
    .bind(&req.slug)
    .bind(full_name)
    .bind(req.birth_date)
    .bind(photo_url)
    .bind(bio)
    .fetch_one(&state.db)
    .await
    .map_err(validate::conflict("slug is already taken"))?;

    tracing::info!(admin_id = %admin.id, person_id = %person.id, "person created");
    Ok((StatusCode::CREATED, Json(person)))
}

/// Изменить человека. Переданные поля заменяются, `null` очищает необязательное поле.
#[utoipa::path(
    patch, operation_id = "update_person", path = "/admin/people/{id}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id человека")),
    request_body = UpdatePerson,
    responses(
        (status = 200, description = "Изменён", body = Person),
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
    JsonBody(req): JsonBody<UpdatePerson>,
) -> AppResult<Json<Person>> {
    if let Some(slug) = &req.slug {
        validate::slug("slug", slug)?;
    }
    let full_name = req
        .full_name
        .map(|name| validate::required("full_name", &name, MAX_TITLE))
        .transpose()?;
    let birth_date = nullable_param(req.birth_date);
    let photo_url = nullable_param(
        req.photo_url
            .map(|v| validate::url("photo_url", v))
            .transpose()?,
    );
    let bio = nullable_param(
        req.bio
            .map(|v| validate::optional("bio", v, MAX_LONG_TEXT))
            .transpose()?,
    );
    let renamed = full_name.is_some();

    let person: Option<Person> = sqlx::query_as(&format!(
        "UPDATE people AS p SET
             slug = COALESCE($2, slug),
             full_name = COALESCE($3, full_name),
             birth_date = CASE WHEN $4 THEN $5 ELSE birth_date END,
             photo_url = CASE WHEN $6 THEN $7 ELSE photo_url END,
             bio = CASE WHEN $8 THEN $9 ELSE bio END
         WHERE id = $1 RETURNING {PERSON_COLUMNS}"
    ))
    .bind(id)
    .bind(&req.slug)
    .bind(full_name)
    .bind(birth_date.0)
    .bind(birth_date.1)
    .bind(photo_url.0)
    .bind(photo_url.1)
    .bind(bio.0)
    .bind(bio.1)
    .fetch_optional(&state.db)
    .await
    .map_err(validate::conflict("slug is already taken"))?;
    let person = person.ok_or(AppError::NotFound)?;

    // Имена участников есть в поисковых документах их сущностей.
    if renamed {
        let ids = affected_entities(&state.db, PERSON_ENTITIES, id).await?;
        search::sync(&state, &ids).await;
    }
    tracing::info!(admin_id = %admin.id, person_id = %id, "person updated");
    Ok(Json(person))
}

/// Удалить человека вместе с его участием в произведениях.
#[utoipa::path(
    delete, operation_id = "delete_person", path = "/admin/people/{id}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id человека")),
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
    let ids = affected_entities(&state.db, PERSON_ENTITIES, id).await?;
    let deleted = sqlx::query("DELETE FROM people WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    search::sync(&state, &ids).await;
    tracing::info!(admin_id = %admin.id, person_id = %id, "person deleted");
    Ok(StatusCode::NO_CONTENT)
}
