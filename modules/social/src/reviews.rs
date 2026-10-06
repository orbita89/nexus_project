//! Рецензии и оценки. Чтение без авторизации, своя рецензия — любой вошедший.

use crate::events;
use crate::models::{
    ListReviewsQuery, PageQuery, PutReview, RatingSummary, Review, ReviewRow, ReviewSort,
    REVIEW_COLUMNS,
};
use crate::refs;
use crate::validate::{self, MAX_REVIEW_BODY};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser};

/// Рецензии на сущность. По умолчанию только с текстом и новые сверху.
#[utoipa::path(
    get, path = "/entities/{slug}/reviews", tag = "reviews",
    params(
        ("slug" = String, Path, description = "slug сущности", example = "dune-2021"),
        ListReviewsQuery,
    ),
    responses(
        (status = 200, description = "Страница рецензий", body = Page<Review>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn list_for_entity(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<ListReviewsQuery>,
) -> AppResult<Json<Page<Review>>> {
    let entity = refs::entity(&state, &slug).await?;
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let all = query.all.unwrap_or(false);
    let order = match query.sort.unwrap_or_default() {
        ReviewSort::New => "created_at DESC, id",
        ReviewSort::RatingDesc => "rating DESC NULLS LAST, created_at DESC, id",
        ReviewSort::RatingAsc => "rating ASC NULLS LAST, created_at DESC, id",
    };
    const FILTER: &str = "FROM reviews WHERE entity_id = $1 AND ($2 OR body IS NOT NULL)";

    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) {FILTER}"))
        .bind(entity.id)
        .bind(all)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<ReviewRow> = sqlx::query_as(&format!(
        "SELECT {REVIEW_COLUMNS} {FILTER} ORDER BY {order} LIMIT $3 OFFSET $4"
    ))
    .bind(entity.id)
    .bind(all)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page {
        items: refs::reviews(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Сводка оценок сущности: средняя, количество, распределение.
#[utoipa::path(
    get, path = "/entities/{slug}/rating", tag = "reviews",
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "Сводка", body = RatingSummary),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn rating(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<RatingSummary>> {
    let entity = refs::entity(&state, &slug).await?;
    let counts: Vec<(i16, i64)> = sqlx::query_as(
        "SELECT rating, count(*) FROM reviews
         WHERE entity_id = $1 AND rating IS NOT NULL GROUP BY rating",
    )
    .bind(entity.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(summary(&counts)))
}

/// Сводка из пар «оценка — сколько раз».
fn summary(counts: &[(i16, i64)]) -> RatingSummary {
    let mut distribution = vec![0; 10];
    let (mut count, mut sum) = (0, 0);
    for &(rating, n) in counts {
        if let Some(slot) = usize::try_from(rating - 1)
            .ok()
            .and_then(|i| distribution.get_mut(i))
        {
            *slot += n;
            count += n;
            sum += i64::from(rating) * n;
        }
    }
    let average = (count > 0).then(|| (sum as f64 / count as f64 * 10.0).round() / 10.0);
    RatingSummary {
        average,
        count,
        distribution,
    }
}

/// Своя рецензия на сущность.
#[utoipa::path(
    get, path = "/entities/{slug}/review", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "Рецензия", body = Review),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена или рецензии нет", body = ErrorBody),
    )
)]
pub async fn get_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<Review>> {
    let entity = refs::entity(&state, &slug).await?;
    let row: Option<ReviewRow> = sqlx::query_as(&format!(
        "SELECT {REVIEW_COLUMNS} FROM reviews WHERE user_id = $1 AND entity_id = $2"
    ))
    .bind(user.id)
    .bind(entity.id)
    .fetch_optional(&state.db)
    .await?;
    let row = row.ok_or(AppError::NotFound)?;
    Ok(Json(refs::review(&state, row).await?))
}

/// Поставить оценку и/или написать рецензию. Повторный вызов заменяет рецензию целиком.
#[utoipa::path(
    put, path = "/entities/{slug}/review", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    request_body = PutReview,
    responses(
        (status = 201, description = "Создана", body = Review),
        (status = 200, description = "Заменена", body = Review),
        (status = 400, description = "Неверные поля или нет ни оценки, ни текста", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn put_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<PutReview>,
) -> AppResult<(StatusCode, Json<Review>)> {
    let rating = validate::rating(req.rating)?;
    let body = validate::optional("body", req.body, MAX_REVIEW_BODY)?;
    if rating.is_none() && body.is_none() {
        return Err(AppError::BadRequest("rating or body is required".into()));
    }
    let entity = refs::entity(&state, &slug).await?;

    // xmax = 0 только у только что вставленной строки: так отличаем создание от замены.
    let saved: Saved = sqlx::query_as(&format!(
        "INSERT INTO reviews (user_id, entity_id, rating, body) VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id, entity_id) DO UPDATE SET rating = EXCLUDED.rating, body = EXCLUDED.body
         RETURNING {REVIEW_COLUMNS}, (xmax = 0) AS created"
    ))
    .bind(user.id)
    .bind(entity.id)
    .bind(rating)
    .bind(&body)
    .fetch_one(&state.db)
    .await
    .map_err(validate::missing_reference)?;

    let (status, kind) = if saved.created {
        tracing::info!(user_id = %user.id, review_id = %saved.review.id, "review created");
        (StatusCode::CREATED, "review.created")
    } else {
        (StatusCode::OK, "review.updated")
    };
    events::publish(
        &state,
        events::review(kind, saved.review.id, entity.id, user.id),
    );
    Ok((status, Json(refs::review(&state, saved.review).await?)))
}

#[derive(sqlx::FromRow)]
struct Saved {
    #[sqlx(flatten)]
    review: ReviewRow,
    created: bool,
}

/// Удалить свою рецензию (вместе с оценкой).
#[utoipa::path(
    delete, path = "/entities/{slug}/review", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена или рецензии нет", body = ErrorBody),
    )
)]
pub async fn delete_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    let deleted: Option<uuid::Uuid> = sqlx::query_scalar(
        "DELETE FROM reviews WHERE user_id = $1 AND entity_id = $2 RETURNING id",
    )
    .bind(user.id)
    .bind(entity.id)
    .fetch_optional(&state.db)
    .await?;
    let id = deleted.ok_or(AppError::NotFound)?;
    events::publish(
        &state,
        events::review("review.deleted", id, entity.id, user.id),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Рецензии и оценки пользователя, новые сверху (включая оценки без текста).
#[utoipa::path(
    get, path = "/users/{username}/reviews", tag = "reviews",
    params(
        ("username" = String, Path, description = "username", example = "user"),
        PageQuery,
    ),
    responses(
        (status = 200, description = "Страница рецензий", body = Page<Review>),
        (status = 404, description = "Пользователь не найден", body = ErrorBody),
    )
)]
pub async fn list_for_user(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<Review>>> {
    let author = refs::user(&state, &username).await?;
    let (limit, offset) = page_bounds(query.limit, query.offset);

    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM reviews WHERE user_id = $1")
        .bind(author.id)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<ReviewRow> = sqlx::query_as(&format!(
        "SELECT {REVIEW_COLUMNS} FROM reviews WHERE user_id = $1
         ORDER BY created_at DESC, id LIMIT $2 OFFSET $3"
    ))
    .bind(author.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page {
        items: refs::reviews(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}

#[cfg(test)]
mod tests {
    use super::summary;

    #[test]
    fn empty_summary() {
        let s = summary(&[]);
        assert_eq!(s.average, None);
        assert_eq!(s.count, 0);
        assert_eq!(s.distribution, vec![0; 10]);
    }

    #[test]
    fn average_is_rounded_to_tenths() {
        // (10 + 8 + 7) / 3 = 8.333...
        let s = summary(&[(10, 1), (8, 1), (7, 1)]);
        assert_eq!(s.average, Some(8.3));
        assert_eq!(s.count, 3);
        assert_eq!(s.distribution, vec![0, 0, 0, 0, 0, 0, 1, 1, 0, 1]);
        // 8.25 -> 8.3 (половина — вверх).
        assert_eq!(summary(&[(9, 1), (8, 3)]).average, Some(8.3));
    }
}
