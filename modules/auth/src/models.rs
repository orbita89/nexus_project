//! Запросы и ответы API модуля. Описания полей попадают в Swagger.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use shared::Role;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Колонки `users` для [`UserView`]. citext приводим к text: так sqlx читает их как `String`.
pub const USER_COLUMNS: &str = "id, email::text AS email, username::text AS username, \
     display_name, avatar_url, role, is_active, email_verified_at, username_changed_at, created_at";

/// Пользователь. Хеш пароля наружу не отдаётся никогда.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct UserView {
    pub id: Uuid,
    #[schema(example = "user@nexus.local")]
    pub email: String,
    #[schema(example = "user")]
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub role: Role,
    /// `false` — заблокирован админом.
    pub is_active: bool,
    /// `null` — email не подтверждён, вход по паролю запрещён.
    pub email_verified_at: Option<DateTime<Utc>>,
    /// Когда username меняли в последний раз (менять можно раз в 30 дней). `null` — не меняли.
    pub username_changed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RegisterRequest {
    #[schema(example = "neo@example.com")]
    pub email: String,
    /// 3–32 символа: латиница, цифры, `_`, `-`, `.`.
    #[schema(example = "neo")]
    pub username: String,
    /// 8–128 символов.
    #[schema(example = "password123")]
    pub password: String,
    /// До 64 символов.
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginRequest {
    /// Email или username, регистр не важен.
    #[schema(example = "user")]
    pub login: String,
    #[schema(example = "password123")]
    pub password: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// Пара токенов. Access — в заголовок `Authorization: Bearer ...`, refresh — для `/refresh`.
#[derive(Debug, Serialize, ToSchema)]
pub struct TokenResponse {
    pub access_token: String,
    #[schema(example = "Bearer")]
    pub token_type: String,
    /// Через сколько секунд истекает access-токен.
    #[schema(example = 900)]
    pub expires_in: i64,
    pub refresh_token: String,
    pub user: UserView,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct EmailRequest {
    #[schema(example = "neo@example.com")]
    pub email: String,
}

/// Токен из ссылки в письме (параметр `token=`).
#[derive(Debug, Deserialize, ToSchema)]
pub struct EmailTokenRequest {
    pub token: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ResetPasswordRequest {
    /// Токен из письма о сбросе пароля.
    pub token: String,
    /// Новый пароль, 8–128 символов.
    pub password: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    /// 8–128 символов.
    pub new_password: String,
}

/// Активная сессия (устройство).
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct SessionView {
    pub id: Uuid,
    pub user_agent: Option<String>,
    pub ip: Option<String>,
    /// Когда сессия обновлялась в последний раз (refresh-токен выдаётся заново при каждом обновлении).
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Это сессия, с которой сделан запрос.
    #[sqlx(skip)]
    pub current: bool,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct ListUsersQuery {
    /// Сколько вернуть, 1–100 (по умолчанию 50).
    pub limit: Option<i64>,
    /// Сколько пропустить (по умолчанию 0).
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetRoleRequest {
    pub role: Role,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetStatusRequest {
    /// `false` — заблокировать (все сессии пользователя отзываются), `true` — разблокировать.
    pub is_active: bool,
}

/// Изменение своего профиля: переданные поля заменяются, `null` очищает имя и аватар.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateProfileRequest {
    /// 3–32 символа: латиница, цифры, `_`, `-`, `.`. Менять можно раз в 30 дней.
    #[schema(example = "neo")]
    pub username: Option<String>,
    /// До 64 символов. `null` или пустая строка — очистить.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>, example = "Нео")]
    pub display_name: Option<Option<String>>,
    /// Ссылка `https://…` до 500 символов. `null` или пустая строка — очистить.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>, example = "https://example.com/neo.png")]
    pub avatar_url: Option<Option<String>>,
}

/// Отличает «поле не передано» (`None`) от `null` (`Some(None)`).
fn nullable<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangeEmailRequest {
    #[schema(example = "neo@matrix.io")]
    pub new_email: String,
    /// Текущий пароль. Обязателен, если пароль задан.
    pub password: Option<String>,
}

/// Ссылка на страницу провайдера: фронтенд открывает её в браузере.
#[derive(Debug, Serialize, ToSchema)]
pub struct LinkStartResponse {
    pub url: String,
}

/// Привязанный аккаунт провайдера.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct LinkedAccount {
    #[schema(example = "google")]
    pub provider: String,
    /// Email у провайдера на момент привязки.
    pub email: Option<String>,
    pub created_at: DateTime<Utc>,
}
