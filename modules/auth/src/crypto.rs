//! Пароли (Argon2id) и одноразовые токены (refresh, ссылки из писем).
//! Ни пароль, ни токен в открытом виде не хранятся и не логируются.

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use sha2::{Digest, Sha256};
use shared::{AppError, AppResult};
use std::sync::OnceLock;
use uuid::Uuid;

/// Argon2 намеренно медленный (десятки миллисекунд CPU), поэтому считаем в blocking-пуле,
/// чтобы не тормозить остальные запросы на этом потоке.
pub async fn hash_password(password: String) -> AppResult<String> {
    tokio::task::spawn_blocking(move || hash_password_blocking(&password))
        .await
        .map_err(|e| AppError::Internal(format!("hash task: {e}")))?
}

/// `stored_hash = None` — пользователя нет или у него не задан пароль. Хеш всё равно считаем,
/// чтобы по времени ответа нельзя было понять, существует ли такой логин.
pub async fn verify_password(password: String, stored_hash: Option<String>) -> AppResult<bool> {
    tokio::task::spawn_blocking(move || {
        let hash = stored_hash.as_deref().unwrap_or_else(|| dummy_hash());
        let ok = Argon2::default()
            .verify_password(password.as_bytes(), hash)
            .is_ok();
        ok && stored_hash.is_some()
    })
    .await
    .map_err(|e| AppError::Internal(format!("verify task: {e}")))
}

pub fn hash_password_blocking(password: &str) -> AppResult<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| AppError::Internal(format!("argon2: {e}")))
}

fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password_blocking("dummy-password").expect("hash dummy password"))
}

/// 244 случайных бита из ОС (две UUID v4), 64 hex-символа — подобрать перебором нереально.
pub fn generate_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// В БД кладём SHA-256 от токена: утечка таблицы не даёт войти. Медленный хеш (как для паролей)
/// не нужен: у токена высокая энтропия, перебор бессмыслен.
pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn verifies_own_hash_only() {
        let stored = hash_password("correct horse".into()).await.unwrap();
        assert!(
            verify_password("correct horse".into(), Some(stored.clone()))
                .await
                .unwrap()
        );
        assert!(!verify_password("wrong".into(), Some(stored)).await.unwrap());
    }

    #[tokio::test]
    async fn missing_hash_never_verifies() {
        assert!(!verify_password("dummy-password".into(), None)
            .await
            .unwrap());
    }

    #[test]
    fn tokens_are_unique_and_hashed_deterministically() {
        let (a, b) = (generate_token(), generate_token());
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_ne!(hash_token(&a), a);
    }
}
