//! Внешние источники медиа по HTTP: TMDB (фильмы и сериалы), IGDB (игры), Open Library (книги),
//! Rutube (запасной трейлер) и проверка, живы ли ссылки. Ключи — из окружения; нет ключа —
//! источник пропускается, остальное работает.

use super::pick;
use super::{Finder, Liveness, Target};
use crate::metadata::{TrailerProvider, TrailerSource};
use crate::models::EntityKind;
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::OnceCell;

const TMDB_API: &str = "https://api.themoviedb.org/3";
const TMDB_IMAGES: &str = "https://image.tmdb.org/t/p/w780";

/// Ключи источников (`infra/.env`, в проде — окружение приложения).
#[derive(Clone, Default)]
pub struct Keys {
    /// API key (v3) или Read Access Token (v4) из настроек TMDB.
    pub tmdb: Option<String>,
    /// IGDB авторизуется через приложение Twitch.
    pub twitch_client_id: Option<String>,
    pub twitch_client_secret: Option<String>,
}

impl Keys {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self {
            tmdb: var("TMDB_API_KEY"),
            twitch_client_id: var("TWITCH_CLIENT_ID"),
            twitch_client_secret: var("TWITCH_CLIENT_SECRET"),
        }
    }

    pub fn has_igdb(&self) -> bool {
        self.twitch_client_id.is_some() && self.twitch_client_secret.is_some()
    }
}

pub struct HttpFinder {
    http: Client,
    keys: Keys,
    igdb_token: OnceCell<Option<String>>,
}

impl HttpFinder {
    pub fn new(keys: Keys) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("NexusMediaBot/1.0 (+https://github.com/orbita89/nexus_project)")
            .build()
            .expect("http client");
        Self {
            http,
            keys,
            igdb_token: OnceCell::new(),
        }
    }

    async fn get_json<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Option<T> {
        let response = request.send().await.ok()?;
        if !response.status().is_success() {
            tracing::debug!(status = %response.status(), url = %response.url(), "media source error");
            return None;
        }
        response.json().await.ok()
    }

    // -------------------------------------------------------------- TMDB

    fn tmdb(&self, path: &str) -> Option<reqwest::RequestBuilder> {
        let key = self.keys.tmdb.as_deref()?;
        let request = self.http.get(format!("{TMDB_API}{path}"));
        // Read Access Token (v4) — JWT, передаётся заголовком; API key (v3) — параметром.
        Some(if key.starts_with("eyJ") {
            request.bearer_auth(key)
        } else {
            request.query(&[("api_key", key)])
        })
    }

    /// id фильма или сериала в TMDB: ищем по оригинальному названию, затем по русскому.
    async fn tmdb_id(&self, target: &Target) -> Option<(&'static str, u64)> {
        let (kind, search, year_param) = match target.kind {
            EntityKind::Movie => ("movie", "/search/movie", "year"),
            EntityKind::Series => ("tv", "/search/tv", "first_air_date_year"),
            _ => return None,
        };
        #[derive(Deserialize)]
        struct Results {
            results: Vec<pick::TmdbSearchResult>,
        }
        for query in target.titles() {
            let mut request = self.tmdb(search)?.query(&[("query", query)]);
            if let Some(year) = target.year {
                request = request.query(&[(year_param, year.to_string())]);
            }
            let found: Option<Results> = self.get_json(request).await;
            if let Some(id) = found.and_then(|r| pick::tmdb_match(&r.results, target.year)) {
                return Some((kind, id));
            }
        }
        None
    }

    async fn tmdb_poster(&self, target: &Target) -> Option<String> {
        let (kind, id) = self.tmdb_id(target).await?;
        #[derive(Deserialize)]
        struct Images {
            posters: Vec<pick::TmdbImage>,
        }
        let request = self
            .tmdb(&format!("/{kind}/{id}/images"))?
            .query(&[("include_image_language", "ru,en,null")]);
        let images: Images = self.get_json(request).await?;
        pick::best_poster(&images.posters).map(|path| format!("{TMDB_IMAGES}{path}"))
    }

    async fn tmdb_trailer(&self, target: &Target) -> Option<String> {
        let (kind, id) = self.tmdb_id(target).await?;
        #[derive(Deserialize)]
        struct Videos {
            results: Vec<pick::TmdbVideo>,
        }
        let request = self
            .tmdb(&format!("/{kind}/{id}/videos"))?
            .query(&[("include_video_language", "ru,en")]);
        let videos: Videos = self.get_json(request).await?;
        pick::official_youtube_trailer(&videos.results).map(str::to_string)
    }

    // -------------------------------------------------------------- IGDB

    async fn igdb_token(&self) -> Option<String> {
        self.igdb_token
            .get_or_init(|| async {
                let (id, secret) = (
                    self.keys.twitch_client_id.as_deref()?,
                    self.keys.twitch_client_secret.as_deref()?,
                );
                #[derive(Deserialize)]
                struct Token {
                    access_token: String,
                }
                let request = self.http.post("https://id.twitch.tv/oauth2/token").query(&[
                    ("client_id", id),
                    ("client_secret", secret),
                    ("grant_type", "client_credentials"),
                ]);
                let token: Option<Token> = self.get_json(request).await;
                if token.is_none() {
                    tracing::warn!(
                        "IGDB: could not get a Twitch token, check TWITCH_CLIENT_ID/SECRET"
                    );
                }
                token.map(|t| t.access_token)
            })
            .await
            .clone()
    }

    async fn igdb_game(&self, target: &Target) -> Option<pick::IgdbGame> {
        if target.kind != EntityKind::Game {
            return None;
        }
        let token = self.igdb_token().await?;
        let client_id = self.keys.twitch_client_id.as_deref()?;
        for query in target.titles() {
            let query = query.replace('"', "");
            let body = format!(
                "search \"{query}\"; fields first_release_date,cover.image_id,videos.name,videos.video_id; limit 5;"
            );
            let request = self
                .http
                .post("https://api.igdb.com/v4/games")
                .header("Client-ID", client_id)
                .bearer_auth(&token)
                .body(body);
            let games: Option<Vec<pick::IgdbGame>> = self.get_json(request).await;
            let found = games.and_then(|games| {
                let year = target.year;
                games
                    .into_iter()
                    .find(|g| pick::year_matches(pick::igdb_year(g), year))
            });
            if found.is_some() {
                return found;
            }
        }
        None
    }

    // -------------------------------------------------------------- Open Library

    async fn openlibrary_cover(&self, target: &Target) -> Option<String> {
        if let Some(isbn) = &target.isbn {
            let url = format!("https://covers.openlibrary.org/b/isbn/{isbn}-L.jpg");
            if self.image_alive(&url).await == Liveness::Alive {
                return Some(url);
            }
        }
        #[derive(Deserialize)]
        struct Search {
            docs: Vec<Doc>,
        }
        #[derive(Deserialize)]
        struct Doc {
            cover_i: Option<u64>,
            first_publish_year: Option<i32>,
        }
        for title in target.titles() {
            let mut request = self
                .http
                .get("https://openlibrary.org/search.json")
                .query(&[
                    ("title", title),
                    ("fields", "cover_i,first_publish_year"),
                    ("limit", "10"),
                ]);
            if let Some(author) = target.authors.first() {
                request = request.query(&[("author", author.as_str())]);
            }
            let Some(search) = self.get_json::<Search>(request).await else {
                continue;
            };
            let cover = search
                .docs
                .iter()
                .find(|d| {
                    d.cover_i.is_some() && pick::year_matches(d.first_publish_year, target.year)
                })
                .or_else(|| search.docs.iter().find(|d| d.cover_i.is_some()))
                .and_then(|d| d.cover_i);
            if let Some(id) = cover {
                return Some(format!("https://covers.openlibrary.org/b/id/{id}-L.jpg"));
            }
        }
        None
    }

    // -------------------------------------------------------------- Rutube

    async fn rutube_search(&self, target: &Target) -> Option<String> {
        #[derive(Deserialize)]
        struct Search {
            results: Vec<pick::RutubeVideo>,
        }
        let suffix = match target.kind {
            EntityKind::Series => "сериал трейлер",
            EntityKind::Game => "игра трейлер",
            _ => "трейлер",
        };
        let titles: Vec<&str> = target.titles().collect();
        for title in &titles {
            let query = match target.year {
                Some(year) => format!("{title} {year} {suffix}"),
                None => format!("{title} {suffix}"),
            };
            let request = self
                .http
                .get("https://rutube.ru/api/search/video/")
                .query(&[("query", query)]);
            let Some(search) = self.get_json::<Search>(request).await else {
                continue;
            };
            let work = match target.kind {
                EntityKind::Series => pick::Work::Series,
                EntityKind::Game => pick::Work::Game,
                _ => pick::Work::Movie,
            };
            if let Some(id) = pick::rutube_trailer(
                &search.results,
                &titles,
                target.year,
                work,
                target.has_namesake,
            ) {
                return Some(id.to_string());
            }
        }
        None
    }
}

impl Finder for HttpFinder {
    async fn cover(&self, target: &Target) -> Option<String> {
        let url = match target.kind {
            EntityKind::Movie | EntityKind::Series => self.tmdb_poster(target).await,
            EntityKind::Book => self.openlibrary_cover(target).await,
            EntityKind::Game => self.igdb_game(target).await.and_then(|g| {
                g.cover.map(|c| {
                    format!(
                        "https://images.igdb.com/igdb/image/upload/t_cover_big_2x/{}.jpg",
                        c.image_id
                    )
                })
            }),
        }?;
        // Найденное проверяем сразу: незачем записывать битую ссылку.
        (self.image_alive(&url).await == Liveness::Alive).then_some(url)
    }

    async fn youtube_trailer(&self, target: &Target) -> Option<String> {
        let id = match target.kind {
            EntityKind::Movie | EntityKind::Series => self.tmdb_trailer(target).await,
            EntityKind::Game => self
                .igdb_game(target)
                .await
                .and_then(|g| pick::igdb_trailer(&g).map(str::to_string)),
            EntityKind::Book => None,
        }?;
        let source = TrailerSource {
            provider: TrailerProvider::Youtube,
            id,
        };
        (self.video_alive(&source).await == Liveness::Alive).then_some(source.id)
    }

    async fn rutube_trailer(&self, target: &Target) -> Option<String> {
        if target.kind == EntityKind::Book {
            return None;
        }
        let id = self.rutube_search(target).await?;
        let source = TrailerSource {
            provider: TrailerProvider::Rutube,
            id,
        };
        (self.video_alive(&source).await == Liveness::Alive).then_some(source.id)
    }

    async fn image_alive(&self, url: &str) -> Liveness {
        // Open Library без default=false на несуществующую обложку отдаёт картинку-пустышку 1×1.
        let url = if url.starts_with("https://covers.openlibrary.org/") && !url.contains('?') {
            format!("{url}?default=false")
        } else {
            url.to_string()
        };
        let Ok(response) = self.http.get(&url).send().await else {
            return Liveness::Unknown;
        };
        match response.status() {
            StatusCode::OK => {
                let is_image = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.starts_with("image/"));
                if is_image {
                    Liveness::Alive
                } else {
                    Liveness::Dead
                }
            }
            StatusCode::NOT_FOUND | StatusCode::GONE => Liveness::Dead,
            // 403 бывает от гео- и антибот-защиты CDN: это не повод удалять ссылку.
            _ => Liveness::Unknown,
        }
    }

    async fn video_alive(&self, source: &TrailerSource) -> Liveness {
        match source.provider {
            TrailerProvider::Youtube => {
                // oEmbed отвечает только для существующих видео, которые разрешено встраивать.
                let watch = format!("https://www.youtube.com/watch?v={}", source.id);
                let request = self
                    .http
                    .get("https://www.youtube.com/oembed")
                    .query(&[("url", watch.as_str()), ("format", "json")]);
                match request.send().await.map(|r| r.status()) {
                    Ok(StatusCode::OK) => Liveness::Alive,
                    Ok(
                        StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN,
                    ) => Liveness::Dead,
                    _ => Liveness::Unknown,
                }
            }
            TrailerProvider::Rutube => {
                #[derive(Deserialize)]
                struct Video {
                    #[serde(default)]
                    is_hidden: bool,
                    #[serde(default)]
                    is_deleted: bool,
                }
                let url = format!("https://rutube.ru/api/video/{}/", source.id);
                let Ok(response) = self.http.get(&url).send().await else {
                    return Liveness::Unknown;
                };
                match response.status() {
                    StatusCode::OK => match response.json::<Video>().await {
                        Ok(v) if !v.is_hidden && !v.is_deleted => Liveness::Alive,
                        Ok(_) => Liveness::Dead,
                        Err(_) => Liveness::Unknown,
                    },
                    StatusCode::NOT_FOUND | StatusCode::GONE => Liveness::Dead,
                    _ => Liveness::Unknown,
                }
            }
        }
    }
}
