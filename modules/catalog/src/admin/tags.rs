//! Админка тегов.

use super::{affected_entities, nullable_param};
use crate::jobs::Reporter;
use crate::models::{CreateTag, Tag, UpdateTag};
use crate::publish::{self, Touched};
use crate::validate::{self, MAX_LONG_TEXT, MAX_TITLE};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AdminUser, AppError, AppResult, AppState};
use uuid::Uuid;

const TAG_ENTITIES: &str = "SELECT entity_id FROM entity_tags WHERE tag_id = $1";

/// Переиндексация и ISR сущностей тега — в фоне, ответ уходит сразу.
///
/// Работа пропорциональна числу сущностей тега: у «Боевика» это 13 328 фильмов, их участники и
/// связанные — в обработчике запроса столько не публикуется, шлюз отвечает 504. Slug'и самих
/// сущностей при правке тега не меняются, поэтому адреса «до» и «после» совпадают и сбор
/// [`Touched`] тоже уходит в фон. Упавший шаг — в логе: индекс догонит перестройка, статику —
/// следующая правка.
fn spawn_publish(state: &AppState, ids: Vec<Uuid>, reason: &'static str) {
    if ids.is_empty() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let entities = ids.len();
        let touched = match Touched::collect(&state.db, &ids, &[]).await {
            Ok(touched) => touched,
            Err(error) => {
                tracing::warn!(%error, reason, "tag publish: collecting touched pages failed");
                return;
            }
        };
        let ok = publish::run(&state, touched, &Reporter::silent()).await;
        tracing::info!(reason, entities, ok, "tag publish finished");
    });
}

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
    let description = validate::optional("description", req.description, MAX_LONG_TEXT)?;

    let tag: Tag = sqlx::query_as(
        "INSERT INTO tags (slug, name, kind, description) VALUES ($1, $2, $3, $4)
         RETURNING id, slug, name, kind, description",
    )
    .bind(&req.slug)
    .bind(name)
    .bind(req.kind)
    .bind(description)
    .fetch_one(&state.db)
    .await
    .map_err(validate::conflict("slug is already taken"))?;

    tracing::info!(admin_id = %admin.id, tag_id = %tag.id, slug = %tag.slug, "tag created");
    Ok((StatusCode::CREATED, Json(tag)))
}

/// Переименовать тег, сменить slug или вид (`genre` / `tag`).
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
    let description = nullable_param(
        req.description
            .map(|value| validate::optional("description", value, MAX_LONG_TEXT))
            .transpose()?,
    );

    // Задеты ли карточки и индекс: slug тега — фильтр в поисковых документах сущностей, название
    // и вид (жанры в шапке) — на их страницах. Правка одного описания не меняет ни того, ни
    // другого: тег показывает его только на своей странице.
    #[derive(sqlx::FromRow)]
    struct Updated {
        #[sqlx(flatten)]
        tag: Tag,
        republish: bool,
    }

    let updated: Option<Updated> = sqlx::query_as(
        "WITH before AS (SELECT id, slug, name, kind FROM tags WHERE id = $1)
         UPDATE tags SET slug = COALESCE($2, tags.slug), name = COALESCE($3, tags.name),
                         kind = COALESCE($4, tags.kind),
                         description = CASE WHEN $5 THEN $6 ELSE tags.description END
         FROM before WHERE tags.id = before.id
         RETURNING tags.id, tags.slug, tags.name, tags.kind, tags.description,
                   (tags.slug, tags.name, tags.kind)
                       IS DISTINCT FROM (before.slug, before.name, before.kind) AS republish",
    )
    .bind(id)
    .bind(&req.slug)
    .bind(name)
    .bind(req.kind)
    .bind(description.0)
    .bind(description.1)
    .fetch_optional(&state.db)
    .await
    .map_err(validate::conflict("slug is already taken"))?;
    let Updated { tag, republish } = updated.ok_or(AppError::NotFound)?;

    if republish {
        let ids = affected_entities(&state.db, TAG_ENTITIES, id).await?;
        spawn_publish(&state, ids, "tag updated");
    }
    tracing::info!(admin_id = %admin.id, tag_id = %id, republish, "tag updated");
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
    // Сущности запоминаются до удаления: каскад снимает тег со всех, а переотправить их документы
    // (slug тега — фильтр) и пересобрать страницы нужно уже без него.
    let ids = affected_entities(&state.db, TAG_ENTITIES, id).await?;
    let deleted = sqlx::query("DELETE FROM tags WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    spawn_publish(&state, ids, "tag deleted");
    tracing::info!(admin_id = %admin.id, tag_id = %id, "tag deleted");
    Ok(StatusCode::NO_CONTENT)
}
