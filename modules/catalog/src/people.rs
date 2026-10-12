//! Люди: список с поиском по имени и карточка с фильмографией. Чтение без авторизации.

use crate::entities::like_pattern;
use crate::models::{
    page_bounds, ListPeopleQuery, Page, Person, PersonCreditRow, PersonDetail, PersonSummary,
    ENTITY_SUMMARY_COLUMNS, PERSON_COLUMNS,
};
use axum::extract::State;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::{AppError, AppResult, AppState, AuthUser};

/// Публичный человек: есть работа в активной сущности или работ нет вовсе (заведён в админке).
/// Счётчики — `person_credit_counts`, их держат триггеры.
const PUBLIC_PERSON: &str = "NOT EXISTS (SELECT 1 FROM person_credit_counts pc
                                    WHERE pc.person_id = p.id AND pc.active_credits = 0
                                      AND pc.credits > 0)";

/// Список людей по алфавиту. Только с работами в активных сущностях (или вовсе без работ).
#[utoipa::path(
    get, operation_id = "list_people", path = "/people", tag = "catalog",
    params(ListPeopleQuery),
    responses((status = 200, description = "Страница людей", body = Page<PersonSummary>))
)]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListPeopleQuery>,
) -> AppResult<Json<Page<PersonSummary>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let pattern = query.q.as_deref().and_then(like_pattern);

    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM people p WHERE ($1::text IS NULL OR p.full_name ILIKE $1)
         AND {PUBLIC_PERSON}"
    ))
    .bind(&pattern)
    .fetch_one(&state.db)
    .await?;
    let items = sqlx::query_as(&format!(
        "SELECT p.id, p.slug, p.full_name, p.birth_date, p.photo_url FROM people p
         WHERE ($1::text IS NULL OR p.full_name ILIKE $1) AND {PUBLIC_PERSON}
         ORDER BY p.full_name, p.id LIMIT $2 OFFSET $3"
    ))
    .bind(&pattern)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Карточка человека: данные и все его работы любых типов, новые сверху.
///
/// Читается из PostgreSQL, без кэша и Meilisearch: публичные страницы отдаёт статика
/// фронтенда (SSG/ISR), сюда приходят её пересборка и админка.
#[utoipa::path(
    get, operation_id = "get_person", path = "/people/{slug}", tag = "catalog",
    params(("slug" = String, Path, description = "slug человека", example = "denis-villeneuve")),
    security((), ("bearer" = [])),
    responses(
        (status = 200, description = "Карточка", body = PersonDetail),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Не найден или все работы скрыты", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(slug): Path<String>,
) -> AppResult<Json<PersonDetail>> {
    let admin = crate::entities::is_admin(viewer.as_ref());
    let person: Option<Person> = sqlx::query_as(&format!(
        "SELECT {PERSON_COLUMNS} FROM people p WHERE p.slug = $1 AND ($2 OR {PUBLIC_PERSON})"
    ))
    .bind(&slug)
    .bind(admin)
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(
        detail(&state.db, person.ok_or(AppError::NotFound)?, admin).await?,
    ))
}

/// Карточка из БД: фильмография — только активные сущности (админу — все).
async fn detail(db: &sqlx::PgPool, person: Person, with_inactive: bool) -> AppResult<PersonDetail> {
    let rows: Vec<PersonCreditRow> = sqlx::query_as(&format!(
        "SELECT c.id, c.role, c.character_name, {ENTITY_SUMMARY_COLUMNS}
         FROM entity_credits c JOIN entities e ON e.id = c.entity_id
         WHERE c.person_id = $1 AND ($2 OR e.active)
         ORDER BY e.release_date DESC NULLS LAST, e.title, c.position, c.id"
    ))
    .bind(person.id)
    .bind(with_inactive)
    .fetch_all(db)
    .await?;
    Ok(PersonDetail {
        person,
        credits: rows.into_iter().map(Into::into).collect(),
    })
}
