//! Хеширование паролей (Argon2id). Пароль в открытом виде нигде не хранится и не логируется.

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use shared::{AppError, AppResult};
use std::sync::OnceLock;

/// Argon2 намеренно медленный (десятки миллисекунд CPU), поэтому считаем в blocking-пуле,
/// чтобы не тормозить остальные запросы на этом потоке.
pub async fn hash(password: String) -> AppResult<String> {
    tokio::task::spawn_blocking(move || hash_blocking(&password))
        .await
        .map_err(|e| AppError::Internal(format!("hash task: {e}")))?
}

/// `stored_hash = None` — пользователь не найден. Всё равно считаем хеш, чтобы по времени
/// ответа нельзя было понять, существует ли такой логин.
pub async fn verify(password: String, stored_hash: Option<String>) -> AppResult<bool> {
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

pub fn hash_blocking(password: &str) -> AppResult<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| AppError::Internal(format!("argon2: {e}")))
}

fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_blocking("dummy-password").expect("hash dummy password"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn verifies_own_hash_only() {
        let stored = hash("correct horse".into()).await.unwrap();
        assert!(verify("correct horse".into(), Some(stored.clone()))
            .await
            .unwrap());
        assert!(!verify("wrong".into(), Some(stored)).await.unwrap());
    }

    #[tokio::test]
    async fn unknown_user_never_verifies() {
        assert!(!verify("dummy-password".into(), None).await.unwrap());
    }
}
