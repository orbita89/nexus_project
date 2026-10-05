//! Запросы и ответы API модуля.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::Role;
use uuid::Uuid;

/// Колонки `users` для [`UserView`]. citext приводим к text: так sqlx читает их как `String`.
pub const USER_COLUMNS: &str = "id, email::text AS email, username::text AS username, \
     display_name, avatar_url, role, is_active, created_at";

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct UserView {
    pub id: Uuid,
    pub email: String,
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub role: Role,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub username: String,
    pub password: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// Email или username.
    pub login: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    /// Через сколько секунд истекает access-токен.
    pub expires_in: i64,
    pub refresh_token: String,
    pub user: UserView,
}

#[derive(Debug, Deserialize)]
pub struct ListUsersQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct SetRoleRequest {
    pub role: Role,
}
