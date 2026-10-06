//! Ограничение частоты запросов к входу, регистрации и письмам — защита от перебора паролей
//! и от рассылки писем на чужой адрес.
//!
//! Счётчики в памяти процесса: при нескольких экземплярах приложения у каждого свои.
//! Когда экземпляров станет несколько, перенести в Redis.

use axum::http::HeaderMap;
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};
use shared::{AppError, AppResult};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const fn nz(n: u32) -> NonZeroU32 {
    match NonZeroU32::new(n) {
        Some(n) => n,
        None => panic!("quota must be positive"),
    }
}

pub struct RateLimits {
    /// Все запросы к входу/регистрации/письмам с одного IP: 30 в минуту.
    per_ip: DefaultKeyedRateLimiter<String>,
    /// Попытки входа в один аккаунт с любых IP: 10 подряд, дальше одна в 90 с (~10 за 15 минут).
    per_login: DefaultKeyedRateLimiter<String>,
    /// Письма на один адрес: 5 подряд, дальше одно в 12 минут.
    per_email: DefaultKeyedRateLimiter<String>,
    checks: AtomicU64,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            per_ip: RateLimiter::keyed(Quota::per_minute(nz(30))),
            per_login: RateLimiter::keyed(
                Quota::with_period(Duration::from_secs(90))
                    .expect("non-zero period")
                    .allow_burst(nz(10)),
            ),
            per_email: RateLimiter::keyed(Quota::per_hour(nz(5))),
            checks: AtomicU64::new(0),
        }
    }
}

impl RateLimits {
    /// Любой запрос к чувствительным эндпоинтам — лимит на IP.
    pub fn check_ip(&self, headers: &HeaderMap) -> AppResult<()> {
        let ip = client_ip(headers).unwrap_or_else(|| "unknown".to_string());
        self.check(&self.per_ip, ip)
    }

    /// Попытка входа в конкретный аккаунт (email или username).
    pub fn check_login(&self, login: &str) -> AppResult<()> {
        self.check(&self.per_login, login.trim().to_lowercase())
    }

    /// Отправка письма на адрес.
    pub fn check_email(&self, email: &str) -> AppResult<()> {
        self.check(&self.per_email, email.trim().to_lowercase())
    }

    fn check(&self, limiter: &DefaultKeyedRateLimiter<String>, key: String) -> AppResult<()> {
        self.forget_stale_keys();
        limiter
            .check_key(&key)
            .map_err(|_| AppError::TooManyRequests)
    }

    /// Счётчики хранятся на каждый ключ; давно не встречавшиеся периодически выбрасываем,
    /// чтобы память не росла от каждого нового IP.
    fn forget_stale_keys(&self) {
        if self.checks.fetch_add(1, Ordering::Relaxed) % 1000 == 999 {
            for limiter in [&self.per_ip, &self.per_login, &self.per_email] {
                limiter.retain_recent();
                limiter.shrink_to_fit();
            }
        }
    }
}

/// IP клиента из `X-Real-IP`, который выставляет nginx. Без nginx заголовку верить нельзя:
/// клиент подставит любой.
pub fn client_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-real-ip")
        .and_then(|value| value.to_str().ok())
        .and_then(|ip| ip.trim().parse::<std::net::IpAddr>().ok())
        .map(|ip| ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_attempts_are_limited_per_account_case_insensitively() {
        let limits = RateLimits::default();
        for _ in 0..10 {
            assert!(limits.check_login("Neo").is_ok());
        }
        assert!(matches!(
            limits.check_login("NEO"),
            Err(AppError::TooManyRequests)
        ));
        assert!(limits.check_login("trinity").is_ok());
    }

    #[test]
    fn emails_are_limited_per_address() {
        let limits = RateLimits::default();
        for _ in 0..5 {
            assert!(limits.check_email("a@b.co").is_ok());
        }
        assert!(limits.check_email("A@B.CO").is_err());
    }

    #[test]
    fn ignores_garbage_ip_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "not-an-ip".parse().unwrap());
        assert_eq!(client_ip(&headers), None);
        headers.insert("x-real-ip", "10.0.0.1".parse().unwrap());
        assert_eq!(client_ip(&headers).as_deref(), Some("10.0.0.1"));
    }
}
