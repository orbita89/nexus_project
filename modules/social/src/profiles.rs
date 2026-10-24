//! Социальный профиль пользователя: счётчики и отношение вошедшего к нему (подписан ли).

use crate::models::{Relation, SocialProfile};
use crate::refs;
use axum::extract::State;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::Path;
use shared::{AppResult, AppState, AuthUser};

/// Профиль: сколько подписчиков, подписок, рецензий, коллекций, тем и сообщений форума; вошедшему — подписан ли он
/// и подписан ли пользователь на него. Шапка профиля и кнопка «Подписаться» — одним запросом.
#[utoipa::path(
    get, operation_id = "get_profile", path = "/users/{username}", tag = "users",
    security((), ("bearer" = [])),
    params(("username" = String, Path, description = "username", example = "author")),
    responses(
        (status = 200, description = "Профиль", body = SocialProfile),
        (status = 401, description = "Токен передан, но недействителен", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(username): Path<String>,
) -> AppResult<Json<SocialProfile>> {
    let user = refs::user(&state, &username).await?;
    let viewer = viewer.map(|viewer| viewer.id);
    let is_owner = viewer == Some(user.id);

    let row: Counts = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM follows WHERE followee_id = $1) AS followers_count,
            (SELECT count(*) FROM follows WHERE follower_id = $1) AS following_count,
            (SELECT count(*) FROM reviews WHERE user_id = $1) AS reviews_count,
            (SELECT count(*) FROM collections WHERE user_id = $1 AND (is_public OR $2))
                AS collections_count,
            (SELECT count(*) FROM forum_threads WHERE author_id = $1) AS threads_count,
            (SELECT count(*) FROM forum_posts WHERE author_id = $1 AND deleted_at IS NULL)
                AS posts_count,
            EXISTS (SELECT 1 FROM follows WHERE follower_id = $3 AND followee_id = $1) AS following,
            EXISTS (SELECT 1 FROM follows WHERE follower_id = $1 AND followee_id = $3) AS followed_by,
            EXISTS (SELECT 1 FROM user_blocks WHERE user_id = $3 AND blocked_id = $1) AS blocked,
            EXISTS (SELECT 1 FROM user_blocks WHERE user_id = $1 AND blocked_id = $3) AS blocked_by",
    )
    .bind(user.id)
    .bind(is_owner)
    .bind(viewer)
    .fetch_one(&state.db)
    .await?;

    // Гостю и самому себе отношение не показываем.
    let relation = (viewer.is_some() && !is_owner).then_some(Relation {
        following: row.following,
        followed_by: row.followed_by,
    });
    Ok(Json(SocialProfile {
        user,
        followers_count: row.followers_count,
        following_count: row.following_count,
        reviews_count: row.reviews_count,
        collections_count: row.collections_count,
        threads_count: row.threads_count,
        posts_count: row.posts_count,
        // Гостю и самому себе блокировок не показываем — как и отношения.
        blocked: relation.is_some() && row.blocked,
        blocked_by: relation.is_some().then_some(row.blocked_by),
        relation,
    }))
}

#[derive(sqlx::FromRow)]
struct Counts {
    followers_count: i64,
    following_count: i64,
    reviews_count: i64,
    collections_count: i64,
    threads_count: i64,
    posts_count: i64,
    following: bool,
    followed_by: bool,
    blocked: bool,
    blocked_by: bool,
}

/// Длина запроса и размер выдачи поиска пользователей.
pub const MAX_USER_QUERY: usize = 50;
const USER_SEARCH_LIMIT: i64 = 8;
const MAX_USER_SEARCH_LIMIT: i64 = 50;

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UserSearchQuery {
    /// Начало username или имени, 1–50 символов.
    #[param(example = "ali")]
    pub q: String,
    /// 1–50, по умолчанию 8.
    pub limit: Option<i64>,
}

/// Поиск пользователей по username и имени — автодополнение «@» в чате. Отдаёт только
/// активных; данные — через `UserDirectory` (таблица `users` принадлежит `auth`).
#[utoipa::path(
    get, operation_id = "search_users", path = "/users", tag = "users",
    params(UserSearchQuery),
    responses(
        (status = 200, description = "Найденные пользователи", body = Vec<shared::directory::UserRef>),
        (status = 400, description = "Пустой или слишком длинный `q`, неверный `limit`", body = ErrorBody),
    )
)]
pub async fn search(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    shared::extract::Query(query): shared::extract::Query<UserSearchQuery>,
) -> AppResult<Json<Vec<shared::directory::UserRef>>> {
    let q = crate::validate::required("q", &query.q, MAX_USER_QUERY)?;
    let limit = match query.limit {
        None => USER_SEARCH_LIMIT,
        Some(limit) if (1..=MAX_USER_SEARCH_LIMIT).contains(&limit) => limit,
        Some(_) => {
            return Err(shared::AppError::BadRequest(format!(
                "limit must be between 1 and {MAX_USER_SEARCH_LIMIT}"
            ))
            .on_field("limit"))
        }
    };
    // Блокировки живут в social: кого я заблокировал и кто заблокировал меня — не показываем,
    // иначе человек найдёт собеседника и упрётся в 403 без объяснения.
    let exclude = match &viewer {
        Some(user) => crate::dm::hidden_from(&state, user.id).await?,
        None => Vec::new(),
    };
    Ok(Json(state.users.search(&q, limit, &exclude).await?))
}
