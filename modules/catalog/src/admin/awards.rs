//! Админка наград: премии, церемонии, категории и номинации.
//!
//! Номинации меняют карточки работ и профили людей (блок «Награды») и фасеты поиска, поэтому
//! после каждой записи затронутые сущности и люди уходят в [`publish::run`] — и до записи, и
//! после неё: номинацию могли снять с одной работы и повесить на другую.

use crate::awards::{Award, AwardEditionDetail, AWARD_COLUMNS};
use crate::jobs::Reporter;
use crate::models::EntityKind;
use crate::publish::{self, Touched};
use crate::validate::{self, MAX_LONG_TEXT, MAX_TITLE};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::{AdminUser, AppError, AppResult, AppState};
use sqlx::{PgExecutor, Postgres, Transaction};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Сколько номинаций принимает одна церемония за раз: «Оскар» — около 120.
const MAX_NOMINATIONS: usize = 2_000;

/// Новая премия.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateAward {
    #[schema(example = "oscar")]
    pub slug: String,
    #[schema(example = "Оскар")]
    pub title: String,
    pub full_title: Option<String>,
    /// Типы работ премии: вкладки раздела. Пустой список — премия не попадёт ни на одну вкладку.
    #[serde(default)]
    pub kinds: Vec<EntityKind>,
    pub country: Option<String>,
    pub founded: Option<i32>,
    pub description: Option<String>,
    pub logo_url: Option<String>,
    pub site_url: Option<String>,
    #[serde(default)]
    pub position: i32,
}

/// Правка премии: переданные поля, `null` — стереть.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateAward {
    pub slug: Option<String>,
    pub title: Option<String>,
    #[serde(default, deserialize_with = "crate::models::nullable")]
    #[schema(value_type = Option<String>)]
    pub full_title: Option<Option<String>>,
    pub kinds: Option<Vec<EntityKind>>,
    #[serde(default, deserialize_with = "crate::models::nullable")]
    #[schema(value_type = Option<String>)]
    pub country: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::models::nullable")]
    #[schema(value_type = Option<i32>)]
    pub founded: Option<Option<i32>>,
    #[serde(default, deserialize_with = "crate::models::nullable")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::models::nullable")]
    #[schema(value_type = Option<String>)]
    pub logo_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::models::nullable")]
    #[schema(value_type = Option<String>)]
    pub site_url: Option<Option<String>>,
    pub position: Option<i32>,
}

/// Церемония: год задаётся в пути при правке и в теле при создании.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateEdition {
    #[schema(example = 2024)]
    pub year: i32,
    pub season_year: Option<i32>,
    pub number: Option<i32>,
    pub held_on: Option<NaiveDate>,
    pub place: Option<String>,
    pub cover_url: Option<String>,
}

/// Правка церемонии.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateEdition {
    #[serde(default, deserialize_with = "crate::models::nullable")]
    #[schema(value_type = Option<i32>)]
    pub season_year: Option<Option<i32>>,
    #[serde(default, deserialize_with = "crate::models::nullable")]
    #[schema(value_type = Option<i32>)]
    pub number: Option<Option<i32>>,
    #[serde(default, deserialize_with = "crate::models::nullable")]
    #[schema(value_type = Option<NaiveDate>)]
    pub held_on: Option<Option<NaiveDate>>,
    #[serde(default, deserialize_with = "crate::models::nullable")]
    #[schema(value_type = Option<String>)]
    pub place: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::models::nullable")]
    #[schema(value_type = Option<String>)]
    pub cover_url: Option<Option<String>>,
}

/// Новая категория премии.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateCategory {
    #[schema(example = "best-picture")]
    pub slug: String,
    #[schema(example = "Лучший фильм")]
    pub title: String,
    #[serde(default)]
    pub position: i32,
}

/// Правка категории.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateCategory {
    pub slug: Option<String>,
    pub title: Option<String>,
    pub position: Option<i32>,
}

/// Номинация в теле запроса: работа и человек — по slug (ссылки импорта) или по id.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NominationInput {
    pub entity_id: Option<Uuid>,
    #[schema(example = "oppenheimer-2023")]
    pub entity_slug: Option<String>,
    pub person_id: Option<Uuid>,
    pub person_slug: Option<String>,
    /// Кого/что номинировали, если в каталоге этого нет.
    pub name: Option<String>,
    #[schema(example = "режиссёр")]
    pub role: Option<String>,
    pub title_override: Option<String>,
    #[serde(default)]
    pub won: bool,
    #[serde(default)]
    pub position: i32,
}

/// Категория церемонии целиком — тело `PUT .../nominations`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CategoryNominations {
    #[schema(example = "best-picture")]
    pub slug: String,
    /// Название: с ним категория, которой у премии ещё нет, заводится на месте (путь импорта).
    pub title: Option<String>,
    pub position: Option<i32>,
    pub nominations: Vec<NominationInput>,
}

/// Церемония целиком: прежние номинации заменяются этими.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutNominations {
    pub categories: Vec<CategoryNominations>,
}

/// Снести премию вместе с церемониями и номинациями.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ForceQuery {
    /// Без него премия с номинациями не удаляется (409).
    pub force: Option<bool>,
}

// ------------------------------------------------------------------ премия

/// Завести премию.
#[utoipa::path(
    post, operation_id = "create_award", path = "/admin/awards", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = CreateAward,
    responses(
        (status = 201, description = "Создана", body = Award),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "slug занят", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(req): JsonBody<CreateAward>,
) -> AppResult<(StatusCode, Json<Award>)> {
    validate::slug("slug", &req.slug)?;
    let title = validate::required("title", &req.title, MAX_TITLE)?;
    let full_title = validate::optional("full_title", req.full_title, MAX_TITLE)?;
    let country = validate::optional("country", req.country, MAX_TITLE)?;
    let description = validate::optional("description", req.description, MAX_LONG_TEXT)?;
    let logo_url = validate::url("logo_url", req.logo_url)?;
    let site_url = validate::url("site_url", req.site_url)?;
    founded(req.founded)?;

    let award: Award = sqlx::query_as(&format!(
        "INSERT INTO awards (slug, title, full_title, kinds, country, founded, description,
                             logo_url, site_url, position)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING {}",
        AWARD_COLUMNS.replace("a.", "")
    ))
    .bind(&req.slug)
    .bind(title)
    .bind(full_title)
    .bind(kinds(&req.kinds))
    .bind(country)
    .bind(req.founded)
    .bind(description)
    .bind(logo_url)
    .bind(site_url)
    .bind(req.position)
    .fetch_one(&state.db)
    .await
    .map_err(validate::conflict("slug is already taken"))?;

    publish_award(&state, award.id).await?;
    tracing::info!(admin_id = %admin.id, award_id = %award.id, slug = %award.slug, "award created");
    Ok((StatusCode::CREATED, Json(award)))
}

/// Поправить премию.
#[utoipa::path(
    patch, operation_id = "update_award", path = "/admin/awards/{slug}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug премии", example = "oscar")),
    request_body = UpdateAward,
    responses(
        (status = 200, description = "Изменена", body = Award),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой премии", body = ErrorBody),
        (status = 409, description = "slug занят", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<UpdateAward>,
) -> AppResult<Json<Award>> {
    if let Some(slug) = &req.slug {
        validate::slug("slug", slug)?;
    }
    let title = req
        .title
        .map(|title| validate::required("title", &title, MAX_TITLE))
        .transpose()?;
    let (set_full_title, full_title) = super::nullable_param(req.full_title);
    let full_title = validate::optional("full_title", full_title, MAX_TITLE)?;
    let (set_country, country) = super::nullable_param(req.country);
    let country = validate::optional("country", country, MAX_TITLE)?;
    let (set_founded, founded_value) = super::nullable_param(req.founded);
    founded(founded_value)?;
    let (set_description, description) = super::nullable_param(req.description);
    let description = validate::optional("description", description, MAX_LONG_TEXT)?;
    let (set_logo, logo_url) = super::nullable_param(req.logo_url);
    let logo_url = validate::url("logo_url", logo_url)?;
    let (set_site, site_url) = super::nullable_param(req.site_url);
    let site_url = validate::url("site_url", site_url)?;

    let award: Option<Award> = sqlx::query_as(&format!(
        "UPDATE awards SET
            slug = COALESCE($2, slug),
            title = COALESCE($3, title),
            full_title = CASE WHEN $4 THEN $5 ELSE full_title END,
            kinds = COALESCE($6, kinds),
            country = CASE WHEN $7 THEN $8 ELSE country END,
            founded = CASE WHEN $9 THEN $10 ELSE founded END,
            description = CASE WHEN $11 THEN $12 ELSE description END,
            logo_url = CASE WHEN $13 THEN $14 ELSE logo_url END,
            site_url = CASE WHEN $15 THEN $16 ELSE site_url END,
            position = COALESCE($17, position)
         WHERE slug = $1 RETURNING {}",
        AWARD_COLUMNS.replace("a.", "")
    ))
    .bind(&slug)
    .bind(&req.slug)
    .bind(title)
    .bind(set_full_title)
    .bind(full_title)
    .bind(req.kinds.as_deref().map(kinds))
    .bind(set_country)
    .bind(country)
    .bind(set_founded)
    .bind(founded_value)
    .bind(set_description)
    .bind(description)
    .bind(set_logo)
    .bind(logo_url)
    .bind(set_site)
    .bind(site_url)
    .bind(req.position)
    .fetch_optional(&state.db)
    .await
    .map_err(validate::conflict("slug is already taken"))?;
    let award = award.ok_or(AppError::NotFound)?;

    // Название и логотип премии стоят в блоке «Награды» карточек — их страницы пересобираются.
    publish_award(&state, award.id).await?;
    tracing::info!(admin_id = %admin.id, award_id = %award.id, "award updated");
    Ok(Json(award))
}

/// Удалить премию вместе с церемониями, категориями и номинациями.
#[utoipa::path(
    delete, operation_id = "delete_award", path = "/admin/awards/{slug}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug премии", example = "oscar"), ForceQuery),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой премии", body = ErrorBody),
        (status = 409, description = "Есть номинации, нужен ?force=true", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(slug): Path<String>,
    Query(query): Query<ForceQuery>,
) -> AppResult<StatusCode> {
    let award = crate::awards::by_slug(&state.db, &slug).await?;
    let touched = award_touched(&state, award.id).await?;
    if !query.force.unwrap_or(false) && !touched_is_empty(&touched) {
        return Err(AppError::Conflict(
            "award has nominations, pass ?force=true".into(),
        ));
    }

    let mut tx = state.db.begin().await?;
    // Номинации держит категория (FK RESTRICT) — сносим их первыми.
    sqlx::query(
        "DELETE FROM award_nominations n USING award_editions ed
          WHERE ed.id = n.edition_id AND ed.award_id = $1",
    )
    .bind(award.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM awards WHERE id = $1")
        .bind(award.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    publish::run(&state, touched, &Reporter::silent()).await;
    tracing::info!(admin_id = %admin.id, award_id = %award.id, "award deleted");
    Ok(StatusCode::NO_CONTENT)
}

// --------------------------------------------------------------- церемония

/// Завести церемонию.
#[utoipa::path(
    post, operation_id = "create_award_edition", path = "/admin/awards/{slug}/editions",
    tag = "catalog-admin", security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug премии", example = "oscar")),
    request_body = CreateEdition,
    responses(
        (status = 201, description = "Создана", body = AwardEditionDetail),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой премии", body = ErrorBody),
        (status = 409, description = "Церемония этого года уже есть", body = ErrorBody),
    )
)]
pub async fn create_edition(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<CreateEdition>,
) -> AppResult<(StatusCode, Json<AwardEditionDetail>)> {
    let award = crate::awards::by_slug(&state.db, &slug).await?;
    year(req.year)?;
    if let Some(season) = req.season_year {
        year(season)?;
    }
    let place = validate::optional("place", req.place, MAX_TITLE)?;
    let cover_url = validate::url("cover_url", req.cover_url)?;

    sqlx::query(
        "INSERT INTO award_editions (award_id, year, season_year, number, held_on, place,
                                     cover_url)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(award.id)
    .bind(req.year)
    .bind(req.season_year)
    .bind(req.number)
    .bind(req.held_on)
    .bind(place)
    .bind(cover_url)
    .execute(&state.db)
    .await
    .map_err(validate::conflict("edition of this year already exists"))?;

    publish_award(&state, award.id).await?;
    tracing::info!(admin_id = %admin.id, award = %slug, year = req.year, "award edition created");
    let edition = crate::awards::edition_detail(&state.db, &slug, req.year).await?;
    Ok((StatusCode::CREATED, Json(edition)))
}

/// Поправить церемонию.
#[utoipa::path(
    patch, operation_id = "update_award_edition", path = "/admin/awards/{slug}/editions/{year}",
    tag = "catalog-admin", security(("bearer" = [])),
    params(
        ("slug" = String, Path, description = "slug премии", example = "oscar"),
        ("year" = i32, Path, description = "год церемонии", example = 2024),
    ),
    request_body = UpdateEdition,
    responses(
        (status = 200, description = "Изменена", body = AwardEditionDetail),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой церемонии", body = ErrorBody),
    )
)]
pub async fn update_edition(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((slug, year_of)): Path<(String, i32)>,
    JsonBody(req): JsonBody<UpdateEdition>,
) -> AppResult<Json<AwardEditionDetail>> {
    let award = crate::awards::by_slug(&state.db, &slug).await?;
    let id = edition_id(&state.db, &slug, year_of).await?;
    let (set_season, season_year) = super::nullable_param(req.season_year);
    if let Some(season) = season_year {
        year(season)?;
    }
    let (set_number, number) = super::nullable_param(req.number);
    let (set_held_on, held_on) = super::nullable_param(req.held_on);
    let (set_place, place) = super::nullable_param(req.place);
    let place = validate::optional("place", place, MAX_TITLE)?;
    let (set_cover, cover_url) = super::nullable_param(req.cover_url);
    let cover_url = validate::url("cover_url", cover_url)?;

    sqlx::query(
        "UPDATE award_editions SET
            season_year = CASE WHEN $2 THEN $3 ELSE season_year END,
            number = CASE WHEN $4 THEN $5 ELSE number END,
            held_on = CASE WHEN $6 THEN $7 ELSE held_on END,
            place = CASE WHEN $8 THEN $9 ELSE place END,
            cover_url = CASE WHEN $10 THEN $11 ELSE cover_url END
          WHERE id = $1",
    )
    .bind(id)
    .bind(set_season)
    .bind(season_year)
    .bind(set_number)
    .bind(number)
    .bind(set_held_on)
    .bind(held_on)
    .bind(set_place)
    .bind(place)
    .bind(set_cover)
    .bind(cover_url)
    .execute(&state.db)
    .await?;

    // Номер, дата и обложка церемонии стоят на странице премии и в плитке года.
    let touched = edition_touched(&state, award.id, id).await?;
    publish::run(&state, touched, &Reporter::silent()).await;
    tracing::info!(admin_id = %admin.id, award = %slug, year = year_of, "award edition updated");
    Ok(Json(
        crate::awards::edition_detail(&state.db, &slug, year_of).await?,
    ))
}

/// Удалить церемонию вместе с её номинациями.
#[utoipa::path(
    delete, operation_id = "delete_award_edition", path = "/admin/awards/{slug}/editions/{year}",
    tag = "catalog-admin", security(("bearer" = [])),
    params(
        ("slug" = String, Path, description = "slug премии", example = "oscar"),
        ("year" = i32, Path, description = "год церемонии", example = 2024),
    ),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой церемонии", body = ErrorBody),
    )
)]
pub async fn delete_edition(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((slug, year_of)): Path<(String, i32)>,
) -> AppResult<StatusCode> {
    let award = crate::awards::by_slug(&state.db, &slug).await?;
    let id = edition_id(&state.db, &slug, year_of).await?;
    let touched = edition_touched(&state, award.id, id).await?;

    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM award_nominations WHERE edition_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM award_editions WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    publish::run(&state, touched, &Reporter::silent()).await;
    tracing::info!(admin_id = %admin.id, award = %slug, year = year_of, "award edition deleted");
    Ok(StatusCode::NO_CONTENT)
}

// --------------------------------------------------------------- категория

/// Завести категорию премии.
#[utoipa::path(
    post, operation_id = "create_award_category", path = "/admin/awards/{slug}/categories",
    tag = "catalog-admin", security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug премии", example = "oscar")),
    request_body = CreateCategory,
    responses(
        (status = 201, description = "Создана", body = CreateCategory),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой премии", body = ErrorBody),
        (status = 409, description = "slug категории занят", body = ErrorBody),
    )
)]
pub async fn create_category(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<CreateCategory>,
) -> AppResult<(StatusCode, Json<CreateCategory>)> {
    let award = crate::awards::by_slug(&state.db, &slug).await?;
    validate::slug("slug", &req.slug)?;
    let title = validate::required("title", &req.title, MAX_TITLE)?;

    sqlx::query(
        "INSERT INTO award_categories (award_id, slug, title, position) VALUES ($1, $2, $3, $4)",
    )
    .bind(award.id)
    .bind(&req.slug)
    .bind(&title)
    .bind(req.position)
    .execute(&state.db)
    .await
    .map_err(validate::conflict("category slug is already taken"))?;

    tracing::info!(admin_id = %admin.id, award = %slug, category = %req.slug, "category created");
    Ok((StatusCode::CREATED, Json(CreateCategory { title, ..req })))
}

/// Переименовать категорию или сменить порядок.
#[utoipa::path(
    patch, operation_id = "update_award_category",
    path = "/admin/awards/{slug}/categories/{category_slug}",
    tag = "catalog-admin", security(("bearer" = [])),
    params(
        ("slug" = String, Path, description = "slug премии", example = "oscar"),
        ("category_slug" = String, Path, description = "slug категории", example = "best-picture"),
    ),
    request_body = UpdateCategory,
    responses(
        (status = 200, description = "Изменена", body = CreateCategory),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой категории", body = ErrorBody),
        (status = 409, description = "slug категории занят", body = ErrorBody),
    )
)]
pub async fn update_category(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((slug, category_slug)): Path<(String, String)>,
    JsonBody(req): JsonBody<UpdateCategory>,
) -> AppResult<Json<CreateCategory>> {
    let award = crate::awards::by_slug(&state.db, &slug).await?;
    if let Some(slug) = &req.slug {
        validate::slug("slug", slug)?;
    }
    let title = req
        .title
        .map(|title| validate::required("title", &title, MAX_TITLE))
        .transpose()?;

    let row: Option<(String, String, i32)> = sqlx::query_as(
        "UPDATE award_categories SET slug = COALESCE($3, slug), title = COALESCE($4, title),
                                     position = COALESCE($5, position)
          WHERE award_id = $1 AND slug = $2 RETURNING slug, title, position",
    )
    .bind(award.id)
    .bind(&category_slug)
    .bind(&req.slug)
    .bind(title)
    .bind(req.position)
    .fetch_optional(&state.db)
    .await
    .map_err(validate::conflict("category slug is already taken"))?;
    let (slug_now, title, position) = row.ok_or(AppError::NotFound)?;

    // Название категории стоит в блоке «Награды» карточек.
    publish_award(&state, award.id).await?;
    tracing::info!(admin_id = %admin.id, award = %slug, category = %slug_now, "category updated");
    Ok(Json(CreateCategory {
        slug: slug_now,
        title,
        position,
    }))
}

/// Удалить категорию. С номинациями — 409: сначала разобрать их.
#[utoipa::path(
    delete, operation_id = "delete_award_category",
    path = "/admin/awards/{slug}/categories/{category_slug}",
    tag = "catalog-admin", security(("bearer" = [])),
    params(
        ("slug" = String, Path, description = "slug премии", example = "oscar"),
        ("category_slug" = String, Path, description = "slug категории", example = "best-picture"),
    ),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой категории", body = ErrorBody),
        (status = 409, description = "В категории есть номинации", body = ErrorBody),
    )
)]
pub async fn delete_category(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((slug, category_slug)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    let award = crate::awards::by_slug(&state.db, &slug).await?;
    let id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM award_categories WHERE award_id = $1 AND slug = $2")
            .bind(award.id)
            .bind(&category_slug)
            .fetch_optional(&state.db)
            .await?;
    let id = id.ok_or(AppError::NotFound)?;
    let used: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM award_nominations WHERE category_id = $1)",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    if used {
        return Err(AppError::Conflict("category has nominations".into()));
    }
    sqlx::query("DELETE FROM award_categories WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;

    tracing::info!(admin_id = %admin.id, award = %slug, category = %category_slug, "category deleted");
    Ok(StatusCode::NO_CONTENT)
}

// --------------------------------------------------------------- номинации

/// Заменить номинации церемонии целиком — основной путь импорта.
#[utoipa::path(
    put, operation_id = "put_award_nominations", path = "/admin/awards/{slug}/{year}/nominations",
    tag = "catalog-admin", security(("bearer" = [])),
    params(
        ("slug" = String, Path, description = "slug премии", example = "oscar"),
        ("year" = i32, Path, description = "год церемонии", example = 2024),
    ),
    request_body = PutNominations,
    responses(
        (status = 200, description = "Церемония заменена", body = AwardEditionDetail),
        (status = 400, description = "Неверные поля или неизвестный slug", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой церемонии", body = ErrorBody),
    )
)]
pub async fn put_nominations(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((slug, year_of)): Path<(String, i32)>,
    JsonBody(req): JsonBody<PutNominations>,
) -> AppResult<Json<AwardEditionDetail>> {
    let award = crate::awards::by_slug(&state.db, &slug).await?;
    let edition = edition_id(&state.db, &slug, year_of).await?;
    let count: usize = req.categories.iter().map(|c| c.nominations.len()).sum();
    if count > MAX_NOMINATIONS {
        return Err(AppError::BadRequest(format!(
            "no more than {MAX_NOMINATIONS} nominations per edition"
        )));
    }
    // До записи: номинации могли снять с работы — её карточку тоже пересобрать.
    let before = edition_touched(&state, award.id, edition).await?;

    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM award_nominations WHERE edition_id = $1")
        .bind(edition)
        .execute(&mut *tx)
        .await?;
    for category in &req.categories {
        validate::slug("categories[].slug", &category.slug)?;
        let category_id = category_id(&mut tx, award.id, category).await?;
        for (index, nomination) in category.nominations.iter().enumerate() {
            insert_nomination(&mut tx, edition, category_id, index, nomination).await?;
        }
    }
    tx.commit().await?;

    let mut touched = edition_touched(&state, award.id, edition).await?;
    touched = merge_touched(&state, before, touched).await?;
    publish::run(&state, touched, &Reporter::silent()).await;
    tracing::info!(
        admin_id = %admin.id, award = %slug, year = year_of, nominations = count,
        "award edition nominations replaced"
    );
    Ok(Json(
        crate::awards::edition_detail(&state.db, &slug, year_of).await?,
    ))
}

/// Добавить одну номинацию — точечная правка поверх импорта.
#[utoipa::path(
    post, operation_id = "create_award_nomination",
    path = "/admin/awards/{slug}/{year}/nominations",
    tag = "catalog-admin", security(("bearer" = [])),
    params(
        ("slug" = String, Path, description = "slug премии", example = "oscar"),
        ("year" = i32, Path, description = "год церемонии", example = 2024),
        ("category" = String, Query, description = "slug категории", example = "best-picture"),
    ),
    request_body = NominationInput,
    responses(
        (status = 201, description = "Создана", body = AwardEditionDetail),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой церемонии или категории", body = ErrorBody),
    )
)]
pub async fn create_nomination(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((slug, year_of)): Path<(String, i32)>,
    Query(query): Query<CategoryQuery>,
    JsonBody(req): JsonBody<NominationInput>,
) -> AppResult<(StatusCode, Json<AwardEditionDetail>)> {
    let award = crate::awards::by_slug(&state.db, &slug).await?;
    let edition = edition_id(&state.db, &slug, year_of).await?;
    let category: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM award_categories WHERE award_id = $1 AND slug = $2")
            .bind(award.id)
            .bind(&query.category)
            .fetch_optional(&state.db)
            .await?;
    let category = category.ok_or(AppError::NotFound)?;

    let mut tx = state.db.begin().await?;
    insert_nomination(&mut tx, edition, category, req.position as usize, &req).await?;
    tx.commit().await?;

    let touched = edition_touched(&state, award.id, edition).await?;
    publish::run(&state, touched, &Reporter::silent()).await;
    tracing::info!(admin_id = %admin.id, award = %slug, year = year_of, "nomination created");
    Ok((
        StatusCode::CREATED,
        Json(crate::awards::edition_detail(&state.db, &slug, year_of).await?),
    ))
}

/// Какая категория — для точечного добавления номинации.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CategoryQuery {
    #[param(example = "best-picture")]
    pub category: String,
}

/// Удалить одну номинацию.
#[utoipa::path(
    delete, operation_id = "delete_award_nomination", path = "/admin/awards/nominations/{id}",
    tag = "catalog-admin", security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id номинации")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой номинации", body = ErrorBody),
    )
)]
pub async fn delete_nomination(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let row: Option<(Option<Uuid>, Option<Uuid>, Uuid)> = sqlx::query_as(
        "SELECT n.entity_id, n.person_id, ed.award_id FROM award_nominations n
           JOIN award_editions ed ON ed.id = n.edition_id WHERE n.id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let (entity_id, person_id, award_id) = row.ok_or(AppError::NotFound)?;
    sqlx::query("DELETE FROM award_nominations WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;

    let touched = Touched::collect_awards(
        &state.db,
        &entity_id.into_iter().collect::<Vec<_>>(),
        &person_id.into_iter().collect::<Vec<_>>(),
        &[award_id],
        &[],
    )
    .await?;
    publish::run(&state, touched, &Reporter::silent()).await;
    tracing::info!(admin_id = %admin.id, nomination_id = %id, "nomination deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------ помощь

fn kinds(kinds: &[EntityKind]) -> Vec<String> {
    let mut kinds: Vec<String> = kinds
        .iter()
        .map(|kind| kind.as_str().to_owned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    kinds.sort();
    kinds
}

fn founded(value: Option<i32>) -> AppResult<()> {
    match value {
        Some(year) if !(1800..=2100).contains(&year) => Err(AppError::BadRequest(
            "founded must be between 1800 and 2100".into(),
        )),
        _ => Ok(()),
    }
}

fn year(value: i32) -> AppResult<()> {
    if (1880..=2100).contains(&value) {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "year must be between 1880 and 2100".into(),
        ))
    }
}

/// id церемонии премии или 404.
async fn edition_id(db: impl PgExecutor<'_>, slug: &str, year: i32) -> AppResult<Uuid> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT ed.id FROM award_editions ed JOIN awards a ON a.id = ed.award_id
          WHERE a.slug = $1 AND ed.year = $2",
    )
    .bind(slug)
    .bind(year)
    .fetch_optional(db)
    .await?;
    id.ok_or(AppError::NotFound)
}

/// id категории премии: с `title` в теле заводится, если её ещё нет (путь импорта).
async fn category_id(
    tx: &mut Transaction<'_, Postgres>,
    award: Uuid,
    category: &CategoryNominations,
) -> AppResult<Uuid> {
    let id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM award_categories WHERE award_id = $1 AND slug = $2")
            .bind(award)
            .bind(&category.slug)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(id) = id {
        if let Some(position) = category.position {
            sqlx::query("UPDATE award_categories SET position = $2 WHERE id = $1")
                .bind(id)
                .bind(position)
                .execute(&mut **tx)
                .await?;
        }
        return Ok(id);
    }
    let Some(title) = &category.title else {
        return Err(AppError::BadRequest(format!(
            "unknown category '{}': pass title to create it",
            category.slug
        )));
    };
    let title = validate::required("categories[].title", title, MAX_TITLE)?;
    Ok(sqlx::query_scalar(
        "INSERT INTO award_categories (award_id, slug, title, position)
             VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(award)
    .bind(&category.slug)
    .bind(title)
    .bind(category.position.unwrap_or(0))
    .fetch_one(&mut **tx)
    .await?)
}

/// Одна номинация: slug'и работы и человека разворачиваются в id, неизвестный slug — 400.
async fn insert_nomination(
    tx: &mut Transaction<'_, Postgres>,
    edition: Uuid,
    category: Uuid,
    index: usize,
    input: &NominationInput,
) -> AppResult<()> {
    let entity_id = match (&input.entity_id, &input.entity_slug) {
        (Some(id), _) => Some(*id),
        (None, Some(slug)) => Some(resolve(tx, "entities", "entity_slug", slug).await?),
        (None, None) => None,
    };
    let person_id = match (&input.person_id, &input.person_slug) {
        (Some(id), _) => Some(*id),
        (None, Some(slug)) => Some(resolve(tx, "people", "person_slug", slug).await?),
        (None, None) => None,
    };
    let name = validate::optional("name", input.name.clone(), MAX_TITLE)?;
    if entity_id.is_none() && person_id.is_none() && name.is_none() {
        return Err(AppError::BadRequest(
            "nomination needs entity, person or name".into(),
        ));
    }
    let role = validate::optional("role", input.role.clone(), MAX_TITLE)?;
    let title_override =
        validate::optional("title_override", input.title_override.clone(), MAX_TITLE)?;

    sqlx::query(
        "INSERT INTO award_nominations (edition_id, category_id, entity_id, person_id, name,
                                        role, title_override, won, position)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(edition)
    .bind(category)
    .bind(entity_id)
    .bind(person_id)
    .bind(name)
    .bind(role)
    .bind(title_override)
    .bind(input.won)
    .bind(if input.position != 0 {
        input.position
    } else {
        index as i32
    })
    .execute(&mut **tx)
    .await
    // FK: работа или человек исчезли между разбором тела и вставкой.
    .map_err(|error| match &error {
        sqlx::Error::Database(db) if db.is_foreign_key_violation() => {
            AppError::BadRequest("unknown entity or person".into())
        }
        _ => AppError::Database(error),
    })?;
    Ok(())
}

async fn resolve(
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
    field: &str,
    slug: &str,
) -> AppResult<Uuid> {
    let id: Option<Uuid> = sqlx::query_scalar(&format!("SELECT id FROM {table} WHERE slug = $1"))
        .bind(slug)
        .fetch_optional(&mut **tx)
        .await?;
    id.ok_or_else(|| AppError::BadRequest(format!("unknown {field} '{slug}'")))
}

/// Работы и люди церемонии: их карточки показывают блок «Награды».
async fn edition_touched(state: &AppState, award: Uuid, edition: Uuid) -> AppResult<Touched> {
    let entities: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT entity_id FROM award_nominations
          WHERE edition_id = $1 AND entity_id IS NOT NULL",
    )
    .bind(edition)
    .fetch_all(&state.db)
    .await?;
    let people: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT person_id FROM award_nominations
          WHERE edition_id = $1 AND person_id IS NOT NULL",
    )
    .bind(edition)
    .fetch_all(&state.db)
    .await?;
    Ok(Touched::collect_awards(&state.db, &entities, &people, &[award], &[edition]).await?)
}

/// Всё, чего касается премия целиком.
async fn award_touched(state: &AppState, award: Uuid) -> AppResult<Touched> {
    let entities: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT n.entity_id FROM award_nominations n
           JOIN award_editions ed ON ed.id = n.edition_id
          WHERE ed.award_id = $1 AND n.entity_id IS NOT NULL",
    )
    .bind(award)
    .fetch_all(&state.db)
    .await?;
    let people: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT n.person_id FROM award_nominations n
           JOIN award_editions ed ON ed.id = n.edition_id
          WHERE ed.award_id = $1 AND n.person_id IS NOT NULL",
    )
    .bind(award)
    .fetch_all(&state.db)
    .await?;
    Ok(Touched::collect_awards(&state.db, &entities, &people, &[award], &[]).await?)
}

/// Правка премии задевает все её карточки: название и логотип стоят в блоке «Награды».
async fn publish_award(state: &AppState, award: Uuid) -> AppResult<()> {
    let touched = award_touched(state, award).await?;
    publish::run(state, touched, &Reporter::silent()).await;
    Ok(())
}

fn touched_is_empty(touched: &Touched) -> bool {
    touched.entities.is_empty() && touched.people.is_empty()
}

/// Объединяет два набора затронутого (до и после записи).
async fn merge_touched(state: &AppState, before: Touched, after: Touched) -> AppResult<Touched> {
    let entities: Vec<Uuid> = before.entities.into_iter().chain(after.entities).collect();
    let people: Vec<Uuid> = before.people.into_iter().chain(after.people).collect();
    let awards: Vec<Uuid> = before.awards.into_iter().chain(after.awards).collect();
    let editions: Vec<Uuid> = before.editions.into_iter().chain(after.editions).collect();
    Ok(Touched::collect_awards(&state.db, &entities, &people, &awards, &editions).await?)
}
