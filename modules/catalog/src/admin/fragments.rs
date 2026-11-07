//! Админка фрагментов полотна: список заменяется целиком, как серии сезона.
//!
//! Координаты и подписи пишет человек (машина не знает, где в толпе спрятан автопортрет),
//! но редактор правит их пачкой — перетащил рамки, сохранил весь набор.

use crate::fragments::{self, Crop, Fragment, FRAGMENT_COLUMNS};
use crate::jobs::Reporter;
use crate::models::EntityKind;
use crate::publish::{self, Touched};
use crate::validate;
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AdminUser, AppError, AppResult, AppState};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetFragments {
    /// Фрагменты целиком: присланный список заменяет текущий. Пустой — снять все.
    pub fragments: Vec<FragmentInput>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FragmentInput {
    #[schema(example = "Скрытый автопортрет")]
    pub title: String,
    /// До 600 символов.
    pub description: Option<String>,
    /// Прямоугольник в долях исходника: `x + width` и `y + height` не больше 1.
    pub crop: Crop,
    /// Порядок рассказа; не передан — по порядку в списке.
    pub position: Option<i32>,
}

/// Заменить фрагменты полотна целиком.
#[utoipa::path(
    put, operation_id = "set_entity_fragments", path = "/admin/entities/{id}/fragments",
    tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id картины")),
    request_body = SetFragments,
    responses(
        (status = 200, description = "Фрагменты полотна", body = Vec<Fragment>),
        (status = 400, description = "Неверные поля, кроп вне полотна или не картина", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn put(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<SetFragments>,
) -> AppResult<Json<Vec<Fragment>>> {
    if req.fragments.len() > fragments::MAX_FRAGMENTS {
        return Err(AppError::BadRequest(format!(
            "no more than {} fragments per entity",
            fragments::MAX_FRAGMENTS
        )));
    }
    let rows = req
        .fragments
        .into_iter()
        .enumerate()
        .map(|(index, input)| check(input, index))
        .collect::<AppResult<Vec<_>>>()?;

    let mut tx = state.db.begin().await?;
    // FOR UPDATE: параллельное удаление сущности не проскочит между проверкой и вставкой.
    let kind: Option<EntityKind> =
        sqlx::query_scalar("SELECT kind FROM entities WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if !fragments::supported(kind.ok_or(AppError::NotFound)?) {
        return Err(AppError::BadRequest(
            "fragments are only for paintings".into(),
        ));
    }
    sqlx::query("DELETE FROM entity_fragments WHERE entity_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for row in &rows {
        sqlx::query(
            "INSERT INTO entity_fragments
                 (entity_id, title, description, crop_x, crop_y, crop_width, crop_height, position)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(&row.title)
        .bind(&row.description)
        .bind(row.crop.x)
        .bind(row.crop.y)
        .bind(row.crop.width)
        .bind(row.crop.height)
        .bind(row.position)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    publish::run(&state, Touched::entities(&[id]), &Reporter::silent()).await;
    tracing::info!(admin_id = %admin.id, entity_id = %id, fragments = rows.len(), "fragments set");
    let rows: Vec<fragments::FragmentRow> = sqlx::query_as(&format!(
        "SELECT {FRAGMENT_COLUMNS} FROM entity_fragments WHERE entity_id = $1
         ORDER BY position, id"
    ))
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

/// Проверенный фрагмент.
struct Row {
    title: String,
    description: Option<String>,
    crop: Crop,
    position: i32,
}

fn check(input: FragmentInput, index: usize) -> AppResult<Row> {
    let crop = input.crop;
    for (field, value) in [("x", crop.x), ("y", crop.y)] {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(AppError::BadRequest(format!(
                "crop.{field} must be between 0 and 1"
            )));
        }
    }
    for (field, value) in [("width", crop.width), ("height", crop.height)] {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) || value <= 0.0 {
            return Err(AppError::BadRequest(format!(
                "crop.{field} must be greater than 0 and at most 1"
            )));
        }
    }
    // Прямоугольник целиком внутри полотна: вылезший за край IIIF-сервер молча обрежет, и
    // фронтенд покажет не то место.
    if crop.x + crop.width > 1.0 || crop.y + crop.height > 1.0 {
        return Err(AppError::BadRequest(
            "crop must stay inside the image: x + width and y + height must be at most 1".into(),
        ));
    }
    Ok(Row {
        title: validate::required("title", &input.title, fragments::MAX_FRAGMENT_TITLE)?,
        description: validate::optional(
            "description",
            input.description,
            fragments::MAX_FRAGMENT_DESCRIPTION,
        )?,
        crop,
        position: input.position.unwrap_or(index as i32),
    })
}
