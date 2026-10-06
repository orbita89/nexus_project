//! Общение с провайдером: обмен кода на токен и получение профиля.

use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use shared::config::{OAuthKind, OAuthProviderConfig};

/// Профиль пользователя у провайдера, приведённый к общему виду.
#[derive(Debug)]
pub struct Profile {
    pub provider_user_id: String,
    pub email: Option<String>,
    /// Провайдер подтвердил, что адрес принадлежит пользователю. Только такой адрес можно
    /// использовать для привязки к существующему аккаунту — иначе это захват чужого аккаунта.
    pub email_verified: bool,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

/// Ошибка общения с провайдером. Подробности — только в лог.
#[derive(Debug)]
pub struct ProviderError(pub String);

impl<E: std::fmt::Display> From<E> for ProviderError {
    fn from(error: E) -> Self {
        Self(error.to_string())
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Authorization code → access-токен провайдера (с PKCE code_verifier).
pub async fn exchange_code(
    http: &Client,
    provider: &OAuthProviderConfig,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<String, ProviderError> {
    let response = http
        .post(&provider.token_url)
        // GitHub без этого заголовка отвечает form-urlencoded.
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &provider.client_id),
            ("client_secret", &provider.client_secret),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(ProviderError(format!("token endpoint {status}: {body}")));
    }
    let token: TokenResponse = serde_json::from_str(&body)
        .map_err(|e| ProviderError(format!("token response: {e}: {body}")))?;
    Ok(token.access_token)
}

pub async fn fetch_profile(
    http: &Client,
    provider: &OAuthProviderConfig,
    access_token: &str,
) -> Result<Profile, ProviderError> {
    match provider.kind {
        OAuthKind::Oidc => {
            let info = get_json(
                http,
                &provider.userinfo_url,
                &format!("Bearer {access_token}"),
            )
            .await?;
            Ok(Profile {
                provider_user_id: string(&info, "sub").ok_or("userinfo without sub")?,
                email: string(&info, "email"),
                email_verified: info["email_verified"].as_bool().unwrap_or(false),
                name: string(&info, "name"),
                avatar_url: string(&info, "picture"),
            })
        }
        OAuthKind::Github => {
            let auth = format!("Bearer {access_token}");
            let user = get_json(http, &provider.userinfo_url, &auth).await?;
            // Адрес из профиля может быть скрыт или не подтверждён — берём основной подтверждённый.
            let emails =
                get_json(http, &format!("{}/emails", provider.userinfo_url), &auth).await?;
            let email = emails.as_array().and_then(|emails| {
                emails
                    .iter()
                    .find(|e| e["primary"] == true && e["verified"] == true)
                    .and_then(|e| string(e, "email"))
            });
            Ok(Profile {
                provider_user_id: user["id"]
                    .as_i64()
                    .map(|id| id.to_string())
                    .ok_or("github user without id")?,
                email_verified: email.is_some(),
                email,
                name: string(&user, "name").or_else(|| string(&user, "login")),
                avatar_url: string(&user, "avatar_url"),
            })
        }
        OAuthKind::Yandex => {
            let info = get_json(
                http,
                &provider.userinfo_url,
                &format!("OAuth {access_token}"),
            )
            .await?;
            let avatar_url = string(&info, "default_avatar_id")
                .filter(|_| info["is_avatar_empty"] != true)
                .map(|id| format!("https://avatars.yandex.net/get-yapic/{id}/islands-200"));
            Ok(Profile {
                provider_user_id: string(&info, "id").ok_or("yandex user without id")?,
                // Адрес аккаунта Яндекса подтверждён самим Яндексом.
                email_verified: info["default_email"].is_string(),
                email: string(&info, "default_email"),
                name: string(&info, "real_name").or_else(|| string(&info, "display_name")),
                avatar_url,
            })
        }
    }
}

async fn get_json(http: &Client, url: &str, authorization: &str) -> Result<Value, ProviderError> {
    let response = http
        .get(url)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .header(reqwest::header::ACCEPT, "application/json")
        // GitHub API отклоняет запросы без User-Agent.
        .header(reqwest::header::USER_AGENT, "nexus")
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(ProviderError(format!(
            "{url} {status}: {}",
            response.text().await?
        )));
    }
    Ok(response.json().await?)
}

fn string(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}
