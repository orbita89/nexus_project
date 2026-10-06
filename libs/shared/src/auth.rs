//! Проверка доступа: роли, JWT access-токены и extractor'ы для хендлеров.
//!
//! Здесь только то, что нужно всем модулям: проверить токен и роль. Выдача токенов
//! (вход, refresh, выход) — в модуле `auth`.

use crate::{AppError, AppResult, AppState};
use axum::extract::FromRequestParts;
use axum::http::{header, request::Parts};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Время жизни access-токена. Короткое: отозвать JWT нельзя, поэтому смена роли
/// или блокировка вступают в силу не позже, чем через это время.
pub const ACCESS_TOKEN_TTL_SECS: i64 = 15 * 60;

/// Роль пользователя. Порядок вариантов задаёт старшинство: `User < Author < Admin`,
/// так что проверка «не ниже автора» — это `role >= Role::Author`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    sqlx::Type,
    utoipa::ToSchema,
)]
#[sqlx(type_name = "user_role", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Отзывы, оценки, коллекции, ответы в темах форума.
    User,
    /// Плюс создание форумов и тем.
    Author,
    /// Плюс админские эндпоинты.
    Admin,
}

/// Содержимое access-токена.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// id пользователя.
    pub sub: Uuid,
    pub role: Role,
    /// id сессии (строка в `refresh_tokens`): по нему видно, какая сессия текущая.
    pub sid: Uuid,
    pub iat: i64,
    pub exp: i64,
}

/// Ключи и правила проверки JWT (HS256, общий секрет из `JWT_SECRET`).
pub struct Jwt {
    encoding: EncodingKey,
    decoding: DecodingKey,
    validation: Validation,
}

impl Jwt {
    pub fn new(secret: &[u8]) -> Self {
        Self {
            encoding: EncodingKey::from_secret(secret),
            decoding: DecodingKey::from_secret(secret),
            validation: Validation::new(Algorithm::HS256),
        }
    }

    /// Выпускает access-токен на [`ACCESS_TOKEN_TTL_SECS`].
    pub fn issue(&self, user_id: Uuid, role: Role, session_id: Uuid) -> AppResult<String> {
        let now = chrono::Utc::now().timestamp();
        self.encode(&Claims {
            sub: user_id,
            role,
            sid: session_id,
            iat: now,
            exp: now + ACCESS_TOKEN_TTL_SECS,
        })
    }

    /// Проверяет подпись и срок действия. Любая проблема с токеном — 401.
    pub fn verify(&self, token: &str) -> AppResult<Claims> {
        jsonwebtoken::decode::<Claims>(token, &self.decoding, &self.validation)
            .map(|data| data.claims)
            .map_err(|_| AppError::Unauthorized)
    }

    fn encode(&self, claims: &Claims) -> AppResult<String> {
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), claims, &self.encoding)
            .map_err(|e| AppError::Internal(format!("jwt encode: {e}")))
    }
}

/// Аутентифицированный пользователь из заголовка `Authorization: Bearer <access-токен>`.
/// Нет токена или он недействителен — хендлер не вызывается, клиент получает 401.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub id: Uuid,
    pub role: Role,
    /// Сессия, которой выдан токен.
    pub session_id: Uuid,
}

impl AuthUser {
    /// Проверка роли внутри хендлера: `user.require(Role::Author)?`.
    pub fn require(&self, min: Role) -> AppResult<()> {
        if self.role >= min {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> AppResult<Self> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(AppError::Unauthorized)?;

        let claims = state.jwt.verify(token)?;
        Ok(Self {
            id: claims.sub,
            role: claims.role,
            session_id: claims.sid,
        })
    }
}

/// Пользователь с ролью `admin`. Для админских эндпоинтов: остальные получают 403.
#[derive(Debug, Clone, Copy)]
pub struct AdminUser(pub AuthUser);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> AppResult<Self> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        user.require(Role::Admin)?;
        Ok(Self(user))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt() -> Jwt {
        Jwt::new(b"test-secret-test-secret-test-secret")
    }

    #[test]
    fn roles_are_ordered_by_privilege() {
        assert!(Role::User < Role::Author);
        assert!(Role::Author < Role::Admin);
    }

    #[test]
    fn issued_token_verifies() {
        let id = Uuid::new_v4();
        let sid = Uuid::new_v4();
        let claims = jwt()
            .verify(&jwt().issue(id, Role::Author, sid).unwrap())
            .unwrap();
        assert_eq!(claims.sub, id);
        assert_eq!(claims.role, Role::Author);
        assert_eq!(claims.sid, sid);
    }

    #[test]
    fn token_signed_with_other_secret_is_rejected() {
        let token = Jwt::new(b"another-secret-another-secret-123")
            .issue(Uuid::new_v4(), Role::Admin, Uuid::new_v4())
            .unwrap();
        assert!(matches!(jwt().verify(&token), Err(AppError::Unauthorized)));
    }

    #[test]
    fn expired_token_is_rejected() {
        // Запас больше стандартного leeway (60 с) в jsonwebtoken.
        let past = chrono::Utc::now().timestamp() - 3600;
        let token = jwt()
            .encode(&Claims {
                sub: Uuid::new_v4(),
                role: Role::User,
                sid: Uuid::new_v4(),
                iat: past - ACCESS_TOKEN_TTL_SECS,
                exp: past,
            })
            .unwrap();
        assert!(matches!(jwt().verify(&token), Err(AppError::Unauthorized)));
    }

    #[test]
    fn require_checks_minimum_role() {
        let author = AuthUser {
            id: Uuid::new_v4(),
            role: Role::Author,
            session_id: Uuid::new_v4(),
        };
        assert!(author.require(Role::User).is_ok());
        assert!(author.require(Role::Author).is_ok());
        assert!(matches!(
            author.require(Role::Admin),
            Err(AppError::Forbidden)
        ));
    }
}
