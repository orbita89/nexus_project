//! Фрагменты полотна: лента «Интересные детали полотна» на странице картины.
//!
//! Фрагмент — прямоугольник на исходнике (`metadata.image_url` / `metadata.iiif_base`), а не
//! отдельно залитый файл: по координатам фронтенд перелетает зумом к этому месту на самом
//! холсте. Координаты — доли 0–1, их переживёт перезаливка исходника в другом разрешении.
//!
//! Фрагмент не сущность: ни оценок, ни рецензий, ни своей страницы. Поэтому он приходит прямо
//! в карточке (`EntityDetail.fragments`) — их мало, и нужны они сразу; отдельный `GET` остаётся
//! для админки и для ленивой подгрузки.

use crate::models::{EntityKind, VISIBLE};
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::Path;
use shared::{AppError, AppResult, AppState, AuthUser};
use sqlx::PgExecutor;
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

/// Фрагментов на картину: лента, а не каталог деталей.
pub const MAX_FRAGMENTS: usize = 20;
pub const MAX_FRAGMENT_TITLE: usize = 120;
pub const MAX_FRAGMENT_DESCRIPTION: usize = 600;

/// Деталь полотна со своим кропом.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Fragment {
    pub id: Uuid,
    #[schema(example = "Скрытый автопортрет")]
    pub title: String,
    #[schema(example = "В глубине толпы, между знаменем и шляпой, видно лицо самого Рембрандта.")]
    pub description: Option<String>,
    pub crop: Crop,
    /// Порядок рассказа: детали идут по смыслу, а не слева направо.
    #[schema(example = 3)]
    pub position: i32,
}

/// Прямоугольник на исходнике в долях его ширины и высоты (0–1), а не в пикселях: исходник
/// могут перезалить в другом разрешении, и доли это переживут.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
pub struct Crop {
    #[schema(example = 0.412, minimum = 0, maximum = 1)]
    pub x: f64,
    #[schema(example = 0.168, minimum = 0, maximum = 1)]
    pub y: f64,
    #[schema(example = 0.09, minimum = 0, maximum = 1)]
    pub width: f64,
    #[schema(example = 0.12, minimum = 0, maximum = 1)]
    pub height: f64,
}

/// Строка `entity_fragments`.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct FragmentRow {
    pub id: Uuid,
    pub entity_id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub crop_x: f64,
    pub crop_y: f64,
    pub crop_width: f64,
    pub crop_height: f64,
    pub position: i32,
}

impl From<FragmentRow> for Fragment {
    fn from(row: FragmentRow) -> Self {
        Self {
            id: row.id,
            title: row.title,
            description: row.description,
            crop: Crop {
                x: row.crop_x,
                y: row.crop_y,
                width: row.crop_width,
                height: row.crop_height,
            },
            position: row.position,
        }
    }
}

pub(crate) const FRAGMENT_COLUMNS: &str = "id, entity_id, title, description, crop_x, crop_y, \
     crop_width, crop_height, position";

/// Фрагменты сущности в порядке `position`.
#[utoipa::path(
    get, operation_id = "list_entity_fragments", path = "/entities/{slug}/fragments",
    tag = "catalog",
    security((), ("bearer" = [])),
    params(("slug" = String, Path, description = "slug картины", example = "night-watch")),
    responses(
        (status = 200, description = "Фрагменты полотна", body = Vec<Fragment>),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Не найдена или скрыта", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(slug): Path<String>,
) -> AppResult<Json<Vec<Fragment>>> {
    let admin = crate::entities::is_admin(viewer.as_ref());
    let found: Option<(Uuid, bool)> = sqlx::query_as(&format!(
        "SELECT e.id, {VISIBLE} FROM entities e WHERE e.slug = $1"
    ))
    .bind(&slug)
    .fetch_optional(&state.db)
    .await?;
    let (id, _) = found
        .filter(|(_, visible)| *visible || admin)
        .ok_or(AppError::NotFound)?;
    Ok(Json(
        of_entities(&state.db, &[id])
            .await?
            .remove(&id)
            .unwrap_or_default(),
    ))
}

/// Фрагменты пачкой: одна выборка на любое число сущностей (карточка, админка, перестройка).
pub(crate) async fn of_entities(
    db: impl PgExecutor<'_>,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<Fragment>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<FragmentRow> = sqlx::query_as(&format!(
        "SELECT {FRAGMENT_COLUMNS} FROM entity_fragments
         WHERE entity_id = ANY($1) ORDER BY position, id"
    ))
    .bind(ids)
    .fetch_all(db)
    .await?;
    let mut by: HashMap<Uuid, Vec<Fragment>> = HashMap::new();
    for row in rows {
        by.entry(row.entity_id).or_default().push(row.into());
    }
    Ok(by)
}

/// Фрагменты бывают только у картин: у фильма и книги кропать нечего.
pub(crate) fn supported(kind: EntityKind) -> bool {
    kind == EntityKind::Painting
}
