//! Провайдеры ИИ для служебных задач каталога (`catalog_ai_providers`): разметка книг
//! (`scripts/catalog-import/books_llm.py`) и всё, что появится дальше. Настройки задаёт админ
//! (`/admin/ai/providers`, `nexus ai add`), скрипты читают их из БД — ключи в командах не нужны.
//!
//! Ключи шифрует сам Postgres (pgcrypto, `pgp_sym_encrypt`) секретом `AI_SECRET_KEY`: в Rust и
//! в Python один и тот же SQL, своя криптография не нужна. Наружу — только подсказка
//! `sk-…a1b2` и число ключей.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use shared::{AppError, AppState};
use utoipa::ToSchema;

/// Протокол API провайдера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AiKind {
    /// Anthropic Messages API.
    Anthropic,
    /// OpenAI-совместимый Chat Completions: OpenAI, DeepSeek, OpenRouter, Qwen, Gemini, Ollama.
    Openai,
}

/// Усилие рассуждения модели: `none` — без размышлений (дешевле всего), `low` — дёшево и
/// быстро (по умолчанию), `high` — тщательнее.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AiEffort {
    None,
    Low,
    Medium,
    High,
}

impl AiEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            AiEffort::None => "none",
            AiEffort::Low => "low",
            AiEffort::Medium => "medium",
            AiEffort::High => "high",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(AiEffort::None),
            "low" => Some(AiEffort::Low),
            "medium" => Some(AiEffort::Medium),
            "high" => Some(AiEffort::High),
            _ => None,
        }
    }
}

impl AiKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AiKind::Anthropic => "anthropic",
            AiKind::Openai => "openai",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "anthropic" => Some(AiKind::Anthropic),
            "openai" => Some(AiKind::Openai),
            _ => None,
        }
    }
}

/// Провайдер без ключей — то, что видит админка.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct AiProvider {
    #[schema(example = "deepseek")]
    pub slug: String,
    #[schema(example = "openai")]
    pub kind: String,
    /// Адрес API; `null` — адрес провайдера по умолчанию.
    #[schema(example = "https://api.deepseek.com")]
    pub base_url: Option<String>,
    #[schema(example = "deepseek-chat")]
    pub model: String,
    /// Начало и конец первого ключа: `sk-…a1b2`.
    #[schema(example = "sk-…a1b2")]
    pub keys_hint: String,
    pub keys_count: i32,
    /// Одновременных запросов.
    pub workers: i32,
    /// Записей в одном запросе (книг в пачке).
    pub per_request: i32,
    /// `low` | `medium` | `high`.
    #[schema(example = "low")]
    pub effort: String,
    pub is_default: bool,
}

/// Новый провайдер или замена полей (`None` — поле не меняется).
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct AiProviderInput {
    pub kind: Option<AiKind>,
    /// Пустая строка — адрес по умолчанию.
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// Ключи; несколько — запросы идут по очереди через все. Заменяют прежние целиком.
    pub api_keys: Option<Vec<String>>,
    /// 1..=64.
    pub workers: Option<i32>,
    /// 1..=500.
    pub per_request: Option<i32>,
    /// Новый провайдер — `low`, если не задано.
    pub effort: Option<AiEffort>,
    pub is_default: Option<bool>,
}

const COLUMNS: &str =
    "slug, kind, base_url, model, keys_hint, keys_count, workers, per_request, effort, is_default";

fn secret(state: &AppState) -> Result<&str, AppError> {
    state
        .config
        .ai_secret_key
        .as_deref()
        .ok_or_else(|| AppError::Unavailable("AI_SECRET_KEY is not set".into()))
}

/// `sk-…a1b2`: короткие ключи — только звёздочки.
pub fn hint(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() < 12 {
        return "…".into();
    }
    let head: String = chars[..3].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

fn clean_keys(keys: &[String]) -> Result<Vec<String>, AppError> {
    let keys: Vec<String> = keys
        .iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    if keys.is_empty() || keys.len() > 50 {
        return Err(AppError::BadRequest("1..=50 api keys".into()).on_field("api_keys"));
    }
    if keys.iter().any(|k| k.contains(',') || k.len() > 500) {
        return Err(AppError::BadRequest("bad api key".into()).on_field("api_keys"));
    }
    Ok(keys)
}

fn check(input: &AiProviderInput) -> Result<(), AppError> {
    if let Some(url) = input.base_url.as_deref().filter(|u| !u.is_empty()) {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(
                AppError::BadRequest("base_url must be http(s)".into()).on_field("base_url")
            );
        }
    }
    if let Some(model) = &input.model {
        if model.trim().is_empty() || model.len() > 200 {
            return Err(AppError::BadRequest("model is required".into()).on_field("model"));
        }
    }
    if input.workers.is_some_and(|w| !(1..=64).contains(&w)) {
        return Err(AppError::BadRequest("workers must be 1..=64".into()).on_field("workers"));
    }
    if input.per_request.is_some_and(|n| !(1..=500).contains(&n)) {
        return Err(
            AppError::BadRequest("per_request must be 1..=500".into()).on_field("per_request")
        );
    }
    Ok(())
}

pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 40
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !slug.starts_with('-')
}

fn base_url(input: Option<&str>) -> Option<String> {
    input
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
}

pub async fn list(state: &AppState) -> Result<Vec<AiProvider>, AppError> {
    Ok(sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM catalog_ai_providers ORDER BY is_default DESC, slug"
    ))
    .fetch_all(&state.db)
    .await?)
}

/// Завести провайдера. Первый заведённый становится провайдером по умолчанию.
pub async fn create(
    state: &AppState,
    slug: &str,
    input: AiProviderInput,
) -> Result<AiProvider, AppError> {
    if !valid_slug(slug) {
        return Err(AppError::BadRequest("slug: a-z, 0-9, '-', up to 40".into()).on_field("slug"));
    }
    check(&input)?;
    let kind = input
        .kind
        .ok_or_else(|| AppError::BadRequest("kind is required".into()).on_field("kind"))?;
    let model = input
        .model
        .as_deref()
        .map(str::trim)
        .ok_or_else(|| AppError::BadRequest("model is required".into()).on_field("model"))?;
    let keys = clean_keys(input.api_keys.as_deref().unwrap_or_default())?;
    let secret = secret(state)?;
    let mut tx = state.db.begin().await?;
    let first: bool = sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM catalog_ai_providers)")
        .fetch_one(&mut *tx)
        .await?;
    let is_default = input.is_default.unwrap_or(first);
    if is_default {
        sqlx::query("UPDATE catalog_ai_providers SET is_default = false WHERE is_default")
            .execute(&mut *tx)
            .await?;
    }
    let row: Option<AiProvider> = sqlx::query_as(&format!(
        "INSERT INTO catalog_ai_providers
             (slug, kind, base_url, model, api_keys, keys_hint, keys_count, workers, per_request,
              is_default, effort)
         VALUES ($1, $2, $3, $4, pgp_sym_encrypt($5, $6), $7, $8, $9, $10, $11, $12)
         ON CONFLICT (slug) DO NOTHING
         RETURNING {COLUMNS}"
    ))
    .bind(slug)
    .bind(kind.as_str())
    .bind(base_url(input.base_url.as_deref()))
    .bind(model)
    .bind(keys.join(","))
    .bind(secret)
    .bind(hint(&keys[0]))
    .bind(keys.len() as i32)
    .bind(input.workers.unwrap_or(8))
    .bind(input.per_request.unwrap_or(100))
    .bind(is_default)
    .bind(input.effort.unwrap_or(AiEffort::Low).as_str())
    .fetch_optional(&mut *tx)
    .await?;
    let row = row.ok_or_else(|| AppError::Conflict(format!("provider {slug} exists")))?;
    tx.commit().await?;
    Ok(row)
}

/// Поменять переданные поля. `None` — провайдера нет.
pub async fn update(
    state: &AppState,
    slug: &str,
    input: AiProviderInput,
) -> Result<AiProvider, AppError> {
    check(&input)?;
    let keys = input.api_keys.as_deref().map(clean_keys).transpose()?;
    let secret = match &keys {
        Some(_) => Some(secret(state)?),
        None => None,
    };
    let mut tx = state.db.begin().await?;
    if input.is_default == Some(true) {
        sqlx::query(
            "UPDATE catalog_ai_providers SET is_default = false WHERE is_default AND slug <> $1",
        )
        .bind(slug)
        .execute(&mut *tx)
        .await?;
    }
    let row: Option<AiProvider> = sqlx::query_as(&format!(
        "UPDATE catalog_ai_providers SET
             kind = coalesce($2, kind),
             base_url = CASE WHEN $3 THEN $4 ELSE base_url END,
             model = coalesce($5, model),
             api_keys = CASE WHEN $6::text IS NULL THEN api_keys ELSE pgp_sym_encrypt($6, $7) END,
             keys_hint = coalesce($8, keys_hint),
             keys_count = coalesce($9, keys_count),
             workers = coalesce($10, workers),
             per_request = coalesce($11, per_request),
             is_default = coalesce($12, is_default),
             effort = coalesce($13, effort)
         WHERE slug = $1
         RETURNING {COLUMNS}"
    ))
    .bind(slug)
    .bind(input.kind.map(AiKind::as_str))
    .bind(input.base_url.is_some())
    .bind(base_url(input.base_url.as_deref()))
    .bind(input.model.as_deref().map(str::trim))
    .bind(keys.as_ref().map(|k| k.join(",")))
    .bind(secret)
    .bind(keys.as_ref().map(|k| hint(&k[0])))
    .bind(keys.as_ref().map(|k| k.len() as i32))
    .bind(input.workers)
    .bind(input.per_request)
    .bind(input.is_default)
    .bind(input.effort.map(AiEffort::as_str))
    .fetch_optional(&mut *tx)
    .await?;
    let row = row.ok_or(AppError::NotFound)?;
    tx.commit().await?;
    Ok(row)
}

pub async fn delete(state: &AppState, slug: &str) -> Result<(), AppError> {
    let done = sqlx::query("DELETE FROM catalog_ai_providers WHERE slug = $1")
        .bind(slug)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Провайдер с расшифрованными ключами — только для вызовов API.
struct Resolved {
    kind: String,
    base_url: Option<String>,
    model: String,
    keys: String,
}

/// Итог пробного запроса.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiTest {
    /// Все ключи ответили.
    pub ok: bool,
    /// По ключу: `null` — ответил, иначе ошибка провайдера (без самого ключа).
    pub errors: Vec<Option<String>>,
    /// Ответ модели на первом рабочем ключе.
    pub reply: Option<String>,
}

/// Пробный запрос «ответь OK» на каждый ключ провайдера.
pub async fn test(state: &AppState, slug: &str) -> Result<AiTest, AppError> {
    let secret = secret(state)?;
    let row: Option<Resolved> = sqlx::query_as::<_, (String, Option<String>, String, String)>(
        "SELECT kind, base_url, model, pgp_sym_decrypt(api_keys, $2)
         FROM catalog_ai_providers WHERE slug = $1",
    )
    .bind(slug)
    .bind(secret)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| AppError::Unavailable("cannot decrypt api keys: wrong AI_SECRET_KEY".into()))?
    .map(|(kind, base_url, model, keys)| Resolved {
        kind,
        base_url,
        model,
        keys,
    });
    let provider = row.ok_or(AppError::NotFound)?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let mut out = AiTest {
        ok: true,
        errors: Vec::new(),
        reply: None,
    };
    for key in provider.keys.split(',') {
        match ask(&http, &provider, key, "Reply with exactly: OK").await {
            Ok(reply) => {
                out.reply.get_or_insert(reply);
                out.errors.push(None);
            }
            Err(error) => {
                out.ok = false;
                out.errors.push(Some(error.replace(key, "***")));
            }
        }
    }
    Ok(out)
}

async fn ask(
    http: &reqwest::Client,
    provider: &Resolved,
    key: &str,
    prompt: &str,
) -> Result<String, String> {
    let (url, request) = if provider.kind == "anthropic" {
        let base = provider
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com");
        let request = http
            .post(format!("{base}/v1/messages"))
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .json(&serde_json::json!({
                "model": provider.model, "max_tokens": 16,
                "messages": [{"role": "user", "content": prompt}],
            }));
        (base.to_string(), request)
    } else {
        let base = provider
            .base_url
            .as_deref()
            .unwrap_or("https://api.openai.com/v1");
        let request = http
            .post(format!("{base}/chat/completions"))
            .bearer_auth(key)
            .json(&serde_json::json!({
                "model": provider.model, "max_tokens": 16,
                "messages": [{"role": "user", "content": prompt}],
            }));
        (base.to_string(), request)
    };
    let response = request.send().await.map_err(|e| format!("{url}: {e}"))?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        let message = body
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        return Err(format!(
            "HTTP {status}: {}",
            message.chars().take(300).collect::<String>()
        ));
    }
    let text = body
        .pointer("/content/0/text")
        .or_else(|| body.pointer("/choices/0/message/content"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_hides_the_key() {
        assert_eq!(hint("sk-abcdefghijklmnop1234"), "sk-…1234");
        assert_eq!(hint("short"), "…");
    }

    #[test]
    fn slugs_and_inputs_are_checked() {
        assert!(valid_slug("deepseek"));
        assert!(valid_slug("open-router-2"));
        assert!(!valid_slug("Deep"));
        assert!(!valid_slug("-x"));
        assert!(clean_keys(&[" ".into()]).is_err());
        assert_eq!(clean_keys(&[" k1 ".into(), "".into()]).unwrap(), vec!["k1"]);
        let bad = AiProviderInput {
            workers: Some(0),
            ..Default::default()
        };
        assert!(check(&bad).is_err());
        let bad = AiProviderInput {
            base_url: Some("ftp://x".into()),
            ..Default::default()
        };
        assert!(check(&bad).is_err());
    }
}
