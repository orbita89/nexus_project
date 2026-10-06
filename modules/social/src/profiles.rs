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
            EXISTS (SELECT 1 FROM follows WHERE follower_id = $1 AND followee_id = $3) AS followed_by",
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
}
