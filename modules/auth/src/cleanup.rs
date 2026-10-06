//! Фоновая чистка: просроченные refresh-токены и ссылки из писем.

use sqlx::PgPool;
use std::time::Duration;

const INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Запускает чистку раз в час (первый проход — сразу). Несколько экземпляров приложения
/// могут чистить одновременно: DELETE идемпотентен.
pub fn spawn(db: PgPool) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INTERVAL);
        loop {
            ticker.tick().await;
            match run(&db).await {
                Ok(deleted) if deleted.total() > 0 => {
                    tracing::info!(?deleted, "expired auth tokens deleted");
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "auth cleanup failed"),
            }
        }
    });
}

#[derive(Debug, PartialEq, Eq)]
pub struct Deleted {
    pub sessions: u64,
    pub email_tokens: u64,
    pub oauth_states: u64,
}

impl Deleted {
    fn total(&self) -> u64 {
        self.sessions + self.email_tokens + self.oauth_states
    }
}

/// Удаляет то, что уже не может пригодиться.
///
/// Отозванные, но ещё не истёкшие refresh-токены оставляем: по ним ловится повторное
/// использование украденного токена.
pub async fn run(db: &PgPool) -> Result<Deleted, sqlx::Error> {
    let sessions = sqlx::query("DELETE FROM refresh_tokens WHERE expires_at < now()")
        .execute(db)
        .await?
        .rows_affected();
    let email_tokens = sqlx::query("DELETE FROM email_tokens WHERE expires_at < now()")
        .execute(db)
        .await?
        .rows_affected();
    let oauth_states = sqlx::query("DELETE FROM oauth_states WHERE expires_at < now()")
        .execute(db)
        .await?
        .rows_affected();
    Ok(Deleted {
        sessions,
        email_tokens,
        oauth_states,
    })
}
