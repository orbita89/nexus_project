//! Аналитика профиля: что пользователь оценил, что посмотрел и что собирается.
//!
//! **По умолчанию аналитика приватна.** Чужой видит её, только если владелец включил
//! `stats_public` (`PATCH /api/v1/auth/me`); admin видит всегда. Иначе — 403 `stats_private`,
//! а не 404: профиль существует и открыт, закрыта именно статистика.
//!
//! Тип произведения (`movie`, `book`, …) лежит в `entities` — чужой таблице. Разрез по типам
//! берётся у каталога одним вызовом `EntityDirectory::kind_counts`, а не JOIN'ом.

use crate::models::{
    ByKind, CommentCounts, MarkCounts, MarkCountsRow, MarkRow, MarkedEntity, MarksQuery,
    ProfileRatings, ProfileStats, RatedEntity, RatedRow, RatingsSort, Reactions, UserRatingsQuery,
};
use crate::refs;
use axum::extract::State;
use axum::Json;
use shared::directory::UserRef;
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser, Role};
use uuid::Uuid;

/// Пользователь профиля и право смотреть его статистику. Владельцу и админу — всегда,
/// остальным — только при `stats_public`.
async fn visible(
    state: &AppState,
    username: &str,
    viewer: Option<&AuthUser>,
) -> AppResult<UserRef> {
    let user = refs::user(state, username).await?;
    if viewer.is_some_and(|viewer| viewer.id == user.id || viewer.role >= Role::Admin) {
        return Ok(user);
    }
    let details = state
        .users
        .details(user.id)
        .await?
        .ok_or(AppError::NotFound)?;
    if details.stats_public {
        Ok(user)
    } else {
        Err(AppError::Forbidden.with_code("stats_private"))
    }
}

/// Сводка: оценки, рецензии, отметки, реакции, комментарии, разрез по типам контента.
#[utoipa::path(
    get, operation_id = "get_profile_stats", path = "/users/{username}/stats", tag = "users",
    security((), ("bearer" = [])),
    params(("username" = String, Path, description = "username", example = "user")),
    responses(
        (status = 200, description = "Аналитика профиля", body = ProfileStats),
        (status = 401, description = "Токен передан, но недействителен", body = ErrorBody),
        (status = 403, description = "Аналитика закрыта владельцем (`stats_private`)", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(username): Path<String>,
) -> AppResult<Json<ProfileStats>> {
    let user = visible(&state, &username, viewer.as_ref()).await?;

    let ratings: Vec<(i16, i64)> = sqlx::query_as(
        "SELECT rating, count(*) FROM reviews
         WHERE user_id = $1 AND rating IS NOT NULL GROUP BY rating",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    let reviews_with_text: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM reviews WHERE user_id = $1 AND body IS NOT NULL AND body <> ''",
    )
    .bind(user.id)
    .fetch_one(&state.db)
    .await?;
    let marks: Vec<MarkCountsRow> = sqlx::query_as(
        "SELECT kind, count(*) AS count FROM entity_marks WHERE user_id = $1 GROUP BY kind",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    let reactions: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kind, count(*) FROM entity_reactions WHERE user_id = $1 GROUP BY kind",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    let comments: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM review_comments
              WHERE author_id = $1 AND deleted_at IS NULL),
            (SELECT count(*) FROM collection_comments
              WHERE author_id = $1 AND deleted_at IS NULL)",
    )
    .bind(user.id)
    .fetch_one(&state.db)
    .await?;

    // Типы контента — у каталога: по одному вызову на разрез, без JOIN к `entities`.
    let rated_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT entity_id FROM reviews WHERE user_id = $1 AND rating IS NOT NULL",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    let done_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT entity_id FROM entity_marks WHERE user_id = $1 AND kind = 'done'",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(ProfileStats {
        ratings: summary(&ratings),
        reviews_with_text,
        marks: mark_counts(&marks),
        reactions: reaction_counts(&reactions),
        comments: CommentCounts {
            reviews: comments.0,
            collections: comments.1,
        },
        by_kind: ByKind {
            rated: state.entities.kind_counts(&rated_ids).await?,
            done: state.entities.kind_counts(&done_ids).await?,
        },
    }))
}

/// Что пользователь оценил: произведение, оценка и есть ли текст.
#[utoipa::path(
    get, operation_id = "list_user_ratings", path = "/users/{username}/ratings", tag = "users",
    security((), ("bearer" = [])),
    params(
        ("username" = String, Path, description = "username", example = "user"),
        UserRatingsQuery,
    ),
    responses(
        (status = 200, description = "Страница оценок", body = Page<RatedEntity>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 401, description = "Токен передан, но недействителен", body = ErrorBody),
        (status = 403, description = "Аналитика закрыта владельцем (`stats_private`)", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn ratings(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(username): Path<String>,
    Query(query): Query<UserRatingsQuery>,
) -> AppResult<Json<Page<RatedEntity>>> {
    let user = visible(&state, &username, viewer.as_ref()).await?;
    let (limit, offset) = page_bounds(query.limit, query.offset);
    const FILTER: &str = "FROM reviews WHERE user_id = $1 AND rating IS NOT NULL";

    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) {FILTER}"))
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    // `id` вторым ключом — порядок не плавает между страницами при равных оценках.
    let order = match query.sort.unwrap_or_default() {
        RatingsSort::New => "created_at DESC, id",
        RatingsSort::RatingDesc => "rating DESC, created_at DESC, id",
        RatingsSort::RatingAsc => "rating ASC, created_at DESC, id",
    };
    let rows: Vec<RatedRow> = sqlx::query_as(&format!(
        "SELECT entity_id, rating, (body IS NOT NULL AND body <> '') AS has_text, created_at
         {FILTER} ORDER BY {order} LIMIT $2 OFFSET $3"
    ))
    .bind(user.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page {
        items: refs::rated(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Отметки пользователя: «посмотрю позже» и «просмотрено». Своё — `GET /users/me/marks`.
#[utoipa::path(
    get, operation_id = "list_user_marks", path = "/users/{username}/marks", tag = "users",
    security((), ("bearer" = [])),
    params(
        ("username" = String, Path, description = "username", example = "user"),
        MarksQuery,
    ),
    responses(
        (status = 200, description = "Страница отметок", body = Page<MarkedEntity>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 401, description = "Токен передан, но недействителен", body = ErrorBody),
        (status = 403, description = "Аналитика закрыта владельцем (`stats_private`)", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn marks(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(username): Path<String>,
    Query(query): Query<MarksQuery>,
) -> AppResult<Json<Page<MarkedEntity>>> {
    let user = visible(&state, &username, viewer.as_ref()).await?;
    let (limit, offset) = page_bounds(query.limit, query.offset);
    const FILTER: &str = "FROM entity_marks WHERE user_id = $1 AND ($2::text IS NULL OR kind = $2)";

    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) {FILTER}"))
        .bind(user.id)
        .bind(query.kind)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<MarkRow> = sqlx::query_as(&format!(
        "SELECT entity_id, kind, created_at {FILTER}
         ORDER BY created_at DESC, entity_id LIMIT $3 OFFSET $4"
    ))
    .bind(user.id)
    .bind(query.kind)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page {
        items: refs::marks(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Среднее и распределение из пар «оценка — сколько раз».
fn summary(counts: &[(i16, i64)]) -> ProfileRatings {
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
    ProfileRatings {
        average,
        count,
        distribution,
    }
}

fn mark_counts(rows: &[MarkCountsRow]) -> MarkCounts {
    let mut counts = MarkCounts::default();
    for row in rows {
        match row.kind.as_str() {
            "later" => counts.later = row.count,
            "done" => counts.done = row.count,
            _ => {}
        }
    }
    counts
}

fn reaction_counts(rows: &[(String, i64)]) -> Reactions {
    let mut counts = Reactions::default();
    for (kind, n) in rows {
        match kind.as_str() {
            "fire" => counts.fire = *n,
            "sad" => counts.sad = *n,
            "think" => counts.think = *n,
            "bored" => counts.bored = *n,
            _ => {}
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_summary_has_no_average() {
        let summary = summary(&[]);
        assert_eq!(summary.average, None);
        assert_eq!(summary.count, 0);
        assert_eq!(summary.distribution, vec![0; 10]);
    }

    #[test]
    fn average_is_rounded_to_one_decimal() {
        let summary = summary(&[(8, 2), (9, 1)]);
        assert_eq!(summary.average, Some(8.3));
        assert_eq!(summary.count, 3);
        assert_eq!(summary.distribution[7], 2);
        assert_eq!(summary.distribution[8], 1);
    }

    /// Оценка вне 1–10 в базу не попадёт (CHECK), но расчёт на ней не должен падать.
    #[test]
    fn out_of_range_rating_is_ignored() {
        assert_eq!(summary(&[(0, 5), (11, 5)]).count, 0);
    }

    #[test]
    fn counts_fill_only_known_kinds() {
        let marks = mark_counts(&[
            MarkCountsRow {
                kind: "done".into(),
                count: 3,
            },
            MarkCountsRow {
                kind: "later".into(),
                count: 1,
            },
        ]);
        assert_eq!((marks.later, marks.done), (1, 3));
        let reactions = reaction_counts(&[("fire".into(), 2), ("unknown".into(), 9)]);
        assert_eq!((reactions.fire, reactions.sad), (2, 0));
    }
}
