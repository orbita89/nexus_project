//! Полные данные TMDB для фильма, который возвращается из архива на сайт.
//!
//! Один запрос `GET /3/movie/{id}?language=ru-RU&append_to_response=<всё>` (как
//! `scripts/catalog-import/tmdb_full.py`) и раскладка в одной транзакции с вставкой фильма:
//! `entities` (русское название, оригинальное, описание, дата, постер, `metadata`: длительность,
//! страны, студии, возрастной рейтинг, трейлеры), `entity_tmdb`, сырьё `entity_tmdb_data`,
//! `entity_stats` (IMDb id, popularity, число оценок TMDB), жанры → теги `genre`, титры: режиссёры,
//! сценаристы, продюсеры, композиторы, операторы, монтажёры, затем весь актёрский состав. Люди —
//! по TMDB id (`catalog_import.people`), новые создаются (slug из имени, занят —
//! `<slug>-<tmdb_id>`, фото `h632`). Правила полей — те же, что в `tmdb_full.py`: импорт и
//! возврат из архива дают одинаковые карточки.
//!
//! Не делается (в отличие от `tmdb_full.py`): сиквелы по коллекциям TMDB (поиск по сырью всех
//! фильмов — без индекса) и рейтинг IMDb (придёт с «Обновить рейтинги»).

use super::ArchiveDoc;
use crate::models::EntityKind;
use chrono::NaiveDate;
use serde_json::{json, Map, Value};
use shared::Config;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

const MOVIE_APPEND: &str = "credits,release_dates,keywords,translations,alternative_titles,images,\
                            videos,external_ids,watch/providers,recommendations,similar,reviews,lists";
const IMAGE_LANGUAGES: &str = "ru,en,null";
const POSTER: &str = "https://image.tmdb.org/t/p/w780";
const PROFILE: &str = "https://image.tmdb.org/t/p/h632";
/// Запрос к TMDB: пользователь ждёт страницу, дольше ждать незачем.
const TIMEOUT: Duration = Duration::from_secs(8);

const MAX_TITLE: usize = 300;
const MAX_TEXT: usize = 20_000;
const MAX_TRAILERS: usize = 5;
const MAX_RUNTIME: i64 = 10_000;
const MAX_STUDIOS: usize = 20;
const MAX_STUDIO: usize = 120;
const MAX_SLUG_BASE: usize = 80;

/// Должности TMDB → роль в `entity_credits`.
const CREW_ROLES: [(&str, &str); 11] = [
    ("Director", "director"),
    ("Screenplay", "writer"),
    ("Writer", "writer"),
    ("Story", "writer"),
    ("Novel", "writer"),
    ("Author", "writer"),
    ("Producer", "producer"),
    ("Original Music Composer", "composer"),
    ("Music", "composer"),
    ("Director of Photography", "cinematographer"),
    ("Editor", "editor"),
];
const ROLE_ORDER: [&str; 6] = [
    "director",
    "writer",
    "producer",
    "composer",
    "cinematographer",
    "editor",
];

/// Жанры TMDB → slug тега (как `GENRE_SLUGS` в `import_tmdb.py`: совпадают с уже созданными).
fn genre_slug(id: i64) -> Option<&'static str> {
    Some(match id {
        28 => "action",
        12 => "adventure",
        16 => "animation",
        35 => "comedy",
        80 => "crime",
        99 => "documentary",
        18 => "drama",
        10751 => "family",
        14 => "fantasy",
        36 => "history",
        27 => "horror",
        10402 => "music",
        9648 => "detective",
        10749 => "romance",
        878 => "sci-fi",
        10770 => "tv-movie",
        53 => "thriller",
        10752 => "war",
        37 => "western",
        _ => return None,
    })
}

#[derive(Debug)]
pub enum TmdbError {
    /// Нет `TMDB_API_KEY`.
    Disabled,
    /// Сеть, таймаут, 5xx, 429, неверный ключ: фильм пока не возвращается.
    Unavailable(String),
}

impl std::fmt::Display for TmdbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "TMDB_API_KEY is not set"),
            Self::Unavailable(reason) => write!(f, "TMDB is unavailable: {reason}"),
        }
    }
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .expect("http client")
    })
}

/// Полный ответ TMDB по фильму; `None` — фильма в TMDB нет (404).
pub async fn fetch_movie(config: &Config, tmdb_id: i32) -> Result<Option<Value>, TmdbError> {
    tmdb_get(
        config,
        &format!("/movie/{tmdb_id}"),
        &[
            ("language", "ru-RU"),
            ("append_to_response", MOVIE_APPEND),
            ("include_image_language", IMAGE_LANGUAGES),
            ("include_video_language", IMAGE_LANGUAGES),
        ],
    )
    .await
}

/// `GET {TMDB_API_URL}{path}` с ключом: JSON ответа, `None` — 404.
pub(crate) async fn tmdb_get(
    config: &Config,
    path: &str,
    params: &[(&str, &str)],
) -> Result<Option<Value>, TmdbError> {
    let key = config.tmdb_api_key.as_deref().ok_or(TmdbError::Disabled)?;
    let request = client()
        .get(format!("{}{path}", config.tmdb_api_url))
        .query(params);
    // Read Access Token (v4) — JWT, заголовком; API key (v3) — параметром.
    let request = if key.starts_with("eyJ") {
        request.bearer_auth(key)
    } else {
        request.query(&[("api_key", key)])
    };
    let response = request
        .send()
        .await
        .map_err(|e| TmdbError::Unavailable(e.to_string()))?;
    match response.status() {
        status if status.is_success() => response
            .json()
            .await
            .map(Some)
            .map_err(|e| TmdbError::Unavailable(format!("bad response: {e}"))),
        reqwest::StatusCode::NOT_FOUND => Ok(None),
        status => Err(TmdbError::Unavailable(format!("HTTP {status}"))),
    }
}

// ---------------------------------------------------------------- поля

fn clean(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn truncate(text: String, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => text[..i].trim_end().to_string(),
        None => text,
    }
}

fn has_cyrillic(text: &str) -> bool {
    text.chars()
        .any(|c| matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё'))
}

fn unique<T: Eq + std::hash::Hash + Clone>(values: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|v| seen.insert(v.clone()))
        .collect()
}

/// Поля строки `entities` по ответу TMDB и документу архива.
#[derive(Debug, PartialEq)]
pub(crate) struct FilmFields {
    pub title: String,
    pub original_title: Option<String>,
    pub description: Option<String>,
    pub release_date: Option<NaiveDate>,
    pub cover_url: Option<String>,
    pub metadata: Value,
}

pub(crate) fn film_fields(doc: &ArchiveDoc, data: &Value) -> FilmFields {
    // Нет русского перевода у TMDB — оставляем русское название из архива.
    let title = match clean(data.get("title")) {
        Some(title) if !(has_cyrillic(&doc.title) && !has_cyrillic(&title)) => title,
        _ => doc.title.clone(),
    };
    let mut metadata = Map::new();
    if let Some(runtime) = data["runtime"]
        .as_i64()
        .filter(|r| (1..=MAX_RUNTIME).contains(r))
    {
        metadata.insert("runtime_min".into(), json!(runtime));
    }
    let countries: Vec<String> = unique(
        data["production_countries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["iso_3166_1"].as_str())
            .map(|code| code.trim().to_uppercase())
            .filter(|code| code.len() == 2 && code.chars().all(|c| c.is_ascii_uppercase())),
    );
    if !countries.is_empty() {
        metadata.insert("countries".into(), json!(countries));
    }
    let studios: Vec<String> = unique(
        data["production_companies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| clean(c.get("name")))
            .map(|name| truncate(name, MAX_STUDIO)),
    )
    .into_iter()
    .take(MAX_STUDIOS)
    .collect();
    if !studios.is_empty() {
        metadata.insert("studios".into(), json!(studios));
    }
    if let Some(rating) = age_rating(data) {
        metadata.insert("age_rating".into(), json!(rating));
    }
    let trailers = trailers(data);
    if !trailers.is_empty() {
        metadata.insert("trailers".into(), json!(trailers));
    }
    FilmFields {
        title: truncate(title, MAX_TITLE),
        original_title: clean(data.get("original_title"))
            .or_else(|| doc.original_title.clone())
            .map(|t| truncate(t, MAX_TITLE)),
        description: clean(data.get("overview")).map(|d| truncate(d, MAX_TEXT)),
        release_date: data["release_date"]
            .as_str()
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .or(doc.release_date),
        cover_url: data["poster_path"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(|p| format!("{POSTER}{p}"))
            .or_else(|| doc.cover_url.clone()),
        metadata: Value::Object(metadata),
    }
}

/// Возрастной рейтинг: российский, иначе американский (первый непустой).
fn age_rating(data: &Value) -> Option<String> {
    let results = data["release_dates"]["results"].as_array()?;
    for country in ["RU", "US"] {
        let releases = results
            .iter()
            .filter(|r| r["iso_3166_1"] == country)
            .flat_map(|r| r["release_dates"].as_array().into_iter().flatten());
        for release in releases {
            if let Some(cert) = clean(release.get("certification")) {
                if !matches!(cert.to_uppercase().as_str(), "NR" | "UNRATED") {
                    return Some(truncate(cert, 16));
                }
            }
        }
    }
    None
}

/// YouTube-трейлеры: официальные первыми, русские раньше английских, до [`MAX_TRAILERS`].
fn trailers(data: &Value) -> Vec<Value> {
    let mut videos: Vec<&Value> = data["videos"]["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["site"] == "YouTube" && v["type"] == "Trailer")
        .filter(|v| v["key"].as_str().is_some_and(|k| k.len() == 11))
        .collect();
    let lang = |v: &Value| match v["iso_639_1"].as_str() {
        Some("ru") => 0,
        Some("en") => 1,
        _ => 2,
    };
    videos.sort_by_key(|v| {
        (
            !v["official"].as_bool().unwrap_or(false),
            lang(v),
            v["published_at"].as_str().unwrap_or_default().to_string(),
        )
    });
    unique(videos.iter().filter_map(|v| v["key"].as_str()))
        .into_iter()
        .take(MAX_TRAILERS)
        .map(|key| json!({ "provider": "youtube", "id": key }))
        .collect()
}

/// Участник из титров TMDB.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Credit {
    pub tmdb_id: i64,
    pub name: Option<String>,
    pub original_name: Option<String>,
    pub profile_path: Option<String>,
    pub role: &'static str,
    pub character: Option<String>,
}

/// Титры по порядку: съёмочная группа по [`ROLE_ORDER`], затем актёры по `order`.
pub(crate) fn film_credits(data: &Value) -> Vec<Credit> {
    let person = |c: &Value, role: &'static str, character: Option<String>| {
        Some(Credit {
            tmdb_id: c["id"].as_i64()?,
            name: clean(c.get("name")),
            original_name: clean(c.get("original_name")),
            profile_path: clean(c.get("profile_path")),
            role,
            character,
        })
    };
    let crew = data["credits"]["crew"].as_array().into_iter().flatten();
    let mut by_role: HashMap<&str, Vec<Credit>> = HashMap::new();
    for c in crew {
        let job = c["job"].as_str().unwrap_or_default();
        if let Some((_, role)) = CREW_ROLES.iter().find(|(j, _)| *j == job) {
            by_role
                .entry(role)
                .or_default()
                .extend(person(c, role, None));
        }
    }
    let mut cast: Vec<&Value> = data["credits"]["cast"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    cast.sort_by_key(|c| c["order"].as_i64().unwrap_or(1 << 30));
    ROLE_ORDER
        .iter()
        .flat_map(|role| by_role.remove(role).unwrap_or_default())
        .chain(cast.into_iter().filter_map(|c| {
            let character = clean(c.get("character")).map(|t| truncate(t, MAX_TITLE));
            person(c, "actor", character)
        }))
        .collect()
}

/// «Дюна» → dyuna, «Timothée Chalamet» → timothee-chalamet, «J.R.R. Tolkien» → jrr-tolkien (как
/// `slugify` в `import_tmdb.py`). Буквы других алфавитов отбрасываются — может вернуть пустую.
pub(crate) fn slugify(text: &str) -> String {
    let translit = |c: char| -> Option<&'static str> {
        Some(match c {
            'а' => "a",
            'б' => "b",
            'в' => "v",
            'г' => "g",
            'д' => "d",
            'е' | 'ё' => "e",
            'ж' => "zh",
            'з' => "z",
            'и' => "i",
            'й' => "y",
            'к' => "k",
            'л' => "l",
            'м' => "m",
            'н' => "n",
            'о' => "o",
            'п' => "p",
            'р' => "r",
            'с' => "s",
            'т' => "t",
            'у' => "u",
            'ф' => "f",
            'х' => "kh",
            'ц' => "ts",
            'ч' => "ch",
            'ш' => "sh",
            'щ' => "shch",
            'ъ' | 'ь' => "",
            'ы' => "y",
            'э' => "e",
            'ю' => "yu",
            'я' => "ya",
            'і' => "i",
            'ї' => "yi",
            'є' => "ye",
            'ґ' => "g",
            'ў' => "u",
            'ß' => "ss",
            'æ' => "ae",
            'œ' => "oe",
            'ø' => "o",
            'đ' => "d",
            'ł' => "l",
            'þ' => "th",
            'ð' => "d",
            _ => return None,
        })
    };
    let mut out = String::new();
    for c in text.to_lowercase().chars() {
        match translit(c) {
            Some(latin) => out.push_str(latin),
            None => out.push(c),
        }
    }
    let ascii: String = out.nfkd().filter(char::is_ascii).collect();
    let mut slug = String::new();
    for c in ascii.chars().filter(|c| !matches!(c, '\'' | '.')) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    slug[..slug.len().min(MAX_SLUG_BASE)]
        .trim_end_matches('-')
        .to_string()
}

// ---------------------------------------------------------------- запись

/// Вставляет фильм архива в каталог: строка `entities` (`active`, `active_manual` — сборка
/// горячего слоя его не уберёт), связь с TMDB, popularity; с `data` — всё из TMDB (см. описание
/// модуля). Возвращает id сущности. slug или TMDB id уже в каталоге — ошибка уникальности.
pub(crate) async fn insert_film(
    tx: &mut sqlx::PgConnection,
    doc: &ArchiveDoc,
    data: Option<&Value>,
) -> sqlx::Result<Uuid> {
    let fields = match data {
        Some(data) => film_fields(doc, data),
        None => FilmFields {
            title: doc.title.clone(),
            original_title: doc.original_title.clone(),
            description: None,
            release_date: doc.release_date,
            cover_url: doc.cover_url.clone(),
            metadata: json!({}),
        },
    };
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO entities (kind, slug, title, original_title, description, release_date,
                               cover_url, metadata, active, active_manual)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, true, true) RETURNING id",
    )
    .bind(doc.kind)
    .bind(&doc.slug)
    .bind(&fields.title)
    .bind(&fields.original_title)
    .bind(&fields.description)
    .bind(fields.release_date)
    .bind(&fields.cover_url)
    .bind(&fields.metadata)
    .fetch_one(&mut *tx)
    .await?;

    if let Some(tmdb_id) = doc.tmdb_id {
        sqlx::query("INSERT INTO entity_tmdb (entity_id, tmdb_id) VALUES ($1, $2)")
            .bind(id)
            .bind(tmdb_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM archived_tmdb WHERE tmdb_id = $1")
            .bind(tmdb_id)
            .execute(&mut *tx)
            .await?;
    }
    let imdb_id = data
        .and_then(|d| clean(d.get("imdb_id")).or_else(|| clean(d["external_ids"].get("imdb_id"))));
    sqlx::query(
        "INSERT INTO entity_stats (entity_id, imdb_id, tmdb_popularity, tmdb_votes)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(imdb_id)
    .bind(
        data.and_then(|d| d["popularity"].as_f64())
            .unwrap_or(doc.popularity),
    )
    .bind(
        data.and_then(|d| d["vote_count"].as_i64())
            .map(|v| v as i32),
    )
    .execute(&mut *tx)
    .await?;

    let Some(data) = data else { return Ok(id) };
    if let Some(tmdb_id) = doc.tmdb_id {
        sqlx::query("INSERT INTO entity_tmdb_data (entity_id, tmdb_id, data) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(tmdb_id)
            .bind(data)
            .execute(&mut *tx)
            .await?;
    }
    insert_genres(tx, id, data).await?;
    insert_credits(tx, id, &film_credits(data)).await?;
    Ok(id)
}

/// Жанры TMDB → теги `genre`: нет такого тега — создаётся (существующие не меняются).
async fn insert_genres(tx: &mut sqlx::PgConnection, id: Uuid, data: &Value) -> sqlx::Result<()> {
    let (mut slugs, mut names) = (Vec::new(), Vec::new());
    for genre in data["genres"].as_array().into_iter().flatten() {
        let Some(genre_id) = genre["id"].as_i64() else {
            continue;
        };
        let slug = genre_slug(genre_id)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("genre-{genre_id}"));
        let name = clean(genre.get("name")).unwrap_or_else(|| slug.clone());
        let mut chars = name.chars();
        let name = chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or(name);
        if !slugs.contains(&slug) {
            slugs.push(slug);
            names.push(name);
        }
    }
    if slugs.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO tags (slug, name, kind) SELECT s, n, 'genre' FROM unnest($1::text[], $2::text[])
             AS u(s, n)
         ON CONFLICT (slug) DO NOTHING",
    )
    .bind(&slugs)
    .bind(&names)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO entity_tags (entity_id, tag_id) SELECT $1, id FROM tags WHERE slug = ANY($2)
         ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(&slugs)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Люди по TMDB id (новые создаются) и титры с позициями по ролям.
async fn insert_credits(
    tx: &mut sqlx::PgConnection,
    id: Uuid,
    credits: &[Credit],
) -> sqlx::Result<()> {
    if credits.is_empty() {
        return Ok(());
    }
    let tmdb_ids: Vec<i32> = unique(credits.iter().map(|c| c.tmdb_id as i32));
    let mut people: HashMap<i32, Uuid> = sqlx::query_as::<_, (i32, Uuid)>(
        "SELECT tmdb_id, person_id FROM catalog_import.people WHERE tmdb_id = ANY($1)",
    )
    .bind(&tmdb_ids)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();

    // Новые люди: slug из имени, занят — с TMDB id.
    let new: Vec<&Credit> = unique(credits.iter().map(|c| c.tmdb_id))
        .into_iter()
        .filter(|tmdb_id| !people.contains_key(&(*tmdb_id as i32)))
        .filter_map(|tmdb_id| credits.iter().find(|c| c.tmdb_id == tmdb_id))
        .collect();
    if !new.is_empty() {
        let base = |c: &Credit| {
            let slug = c
                .name
                .as_deref()
                .map(slugify)
                .filter(|s| !s.is_empty())
                .or_else(|| c.original_name.as_deref().map(slugify))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("person-{}", c.tmdb_id));
            slug[..slug.len().min(90)].to_string()
        };
        let candidates: Vec<String> = new
            .iter()
            .flat_map(|c| [base(c), format!("{}-{}", base(c), c.tmdb_id)])
            .collect();
        let mut taken: HashSet<String> =
            sqlx::query_scalar("SELECT slug FROM people WHERE slug = ANY($1)")
                .bind(&candidates)
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .collect();
        let (mut slugs, mut names, mut photos, mut links) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for c in new {
            let Some(slug) = [base(c), format!("{}-{}", base(c), c.tmdb_id)]
                .into_iter()
                .find(|s| !taken.contains(s))
            else {
                continue;
            };
            taken.insert(slug.clone());
            let name = c
                .name
                .clone()
                .or_else(|| c.original_name.clone())
                .unwrap_or_else(|| format!("TMDB {}", c.tmdb_id));
            slugs.push(slug.clone());
            names.push(truncate(name, MAX_TITLE));
            photos.push(c.profile_path.as_ref().map(|p| format!("{PROFILE}{p}")));
            links.push((slug, c.tmdb_id as i32));
        }
        let created: HashMap<String, Uuid> = sqlx::query_as::<_, (Uuid, String)>(
            "INSERT INTO people (slug, full_name, photo_url)
             SELECT * FROM unnest($1::text[], $2::text[], $3::text[])
             ON CONFLICT (slug) DO NOTHING RETURNING id, slug",
        )
        .bind(&slugs)
        .bind(&names)
        .bind(&photos)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|(id, slug)| (slug, id))
        .collect();
        let (person_ids, link_tmdb): (Vec<Uuid>, Vec<i32>) = links
            .iter()
            .filter_map(|(slug, tmdb_id)| Some((*created.get(slug)?, *tmdb_id)))
            .unzip();
        sqlx::query(
            "INSERT INTO catalog_import.people (person_id, tmdb_id, created)
             SELECT p, t, true FROM unnest($1::uuid[], $2::int[]) AS u(p, t)
             ON CONFLICT DO NOTHING",
        )
        .bind(&person_ids)
        .bind(&link_tmdb)
        .execute(&mut *tx)
        .await?;
        people.extend(link_tmdb.into_iter().zip(person_ids));
    }

    let (mut person_ids, mut roles, mut characters, mut positions) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut seen = HashSet::new();
    let mut next: HashMap<&str, i32> = HashMap::new();
    for c in credits {
        let Some(person) = people.get(&(c.tmdb_id as i32)) else {
            continue;
        };
        if !seen.insert((*person, c.role, c.character.clone())) {
            continue;
        }
        let position = next.entry(c.role).or_insert(0);
        person_ids.push(*person);
        roles.push(c.role);
        characters.push(c.character.clone());
        positions.push(*position);
        *position += 1;
    }
    sqlx::query(
        "INSERT INTO entity_credits (entity_id, person_id, role, character_name, position)
         SELECT $1, * FROM unnest($2::uuid[], $3::text[], $4::text[], $5::int[])",
    )
    .bind(id)
    .bind(&person_ids)
    .bind(&roles)
    .bind(&characters)
    .bind(&positions)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Тип в документе архива — всегда фильм, но запрос TMDB есть только для них.
pub(crate) fn is_movie(doc: &ArchiveDoc) -> bool {
    doc.kind == EntityKind::Movie
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> ArchiveDoc {
        ArchiveDoc {
            slug: "night-visitor-1971".into(),
            title: "Ночной посетитель".into(),
            original_title: Some("The Night Visitor".into()),
            kind: EntityKind::Movie,
            tmdb_id: Some(42491),
            release_date: NaiveDate::from_ymd_opt(1971, 2, 4),
            popularity: 1.5,
            cover_url: Some("https://img.test/old.jpg".into()),
        }
    }

    #[test]
    fn slugs_like_the_import_script() {
        assert_eq!(slugify("Дюна"), "dyuna");
        assert_eq!(slugify("Timothée Chalamet"), "timothee-chalamet");
        assert_eq!(slugify("J.R.R. Tolkien"), "jrr-tolkien");
        assert_eq!(slugify("Щука и Ёж"), "shchuka-i-ezh");
        assert_eq!(slugify("北野武"), "");
        assert_eq!(slugify("  --A  b--  "), "a-b");
    }

    #[test]
    fn fields_from_tmdb() {
        let data = json!({
            "title": "The Night Visitor",
            "original_title": "The Night Visitor",
            "overview": " Описание. ",
            "release_date": "1971-02-05",
            "poster_path": "/p.jpg",
            "runtime": 106,
            "production_countries": [{"iso_3166_1": "se"}, {"iso_3166_1": "SE"}, {"iso_3166_1": "USA"}],
            "production_companies": [{"name": "Hemisphere"}, {"name": " "}],
            "release_dates": {"results": [
                {"iso_3166_1": "US", "release_dates": [{"certification": "NR"}, {"certification": "PG"}]},
                {"iso_3166_1": "RU", "release_dates": [{"certification": ""}]},
            ]},
            "videos": {"results": [
                {"site": "YouTube", "type": "Trailer", "key": "aaaaaaaaaaa", "official": false, "iso_639_1": "ru"},
                {"site": "YouTube", "type": "Trailer", "key": "bbbbbbbbbbb", "official": true, "iso_639_1": "en"},
                {"site": "YouTube", "type": "Teaser", "key": "ccccccccccc", "official": true},
                {"site": "YouTube", "type": "Trailer", "key": "short", "official": true},
            ]},
        });
        let fields = film_fields(&doc(), &data);
        // У TMDB нет русского названия — остаётся русское из архива.
        assert_eq!(fields.title, "Ночной посетитель");
        assert_eq!(fields.description.as_deref(), Some("Описание."));
        assert_eq!(fields.release_date, NaiveDate::from_ymd_opt(1971, 2, 5));
        assert_eq!(
            fields.cover_url.as_deref(),
            Some("https://image.tmdb.org/t/p/w780/p.jpg")
        );
        assert_eq!(
            fields.metadata,
            json!({
                "runtime_min": 106,
                "countries": ["SE"],
                "studios": ["Hemisphere"],
                "age_rating": "PG",
                "trailers": [
                    {"provider": "youtube", "id": "bbbbbbbbbbb"},
                    {"provider": "youtube", "id": "aaaaaaaaaaa"},
                ],
            })
        );
        let ru = film_fields(&doc(), &json!({ "title": "Ночной гость" }));
        assert_eq!(ru.title, "Ночной гость");
        assert_eq!(ru.cover_url.as_deref(), Some("https://img.test/old.jpg"));
    }

    #[test]
    fn credits_crew_by_role_then_cast_by_order() {
        let data = json!({"credits": {
            "crew": [
                {"id": 3, "name": "Writer", "job": "Screenplay"},
                {"id": 1, "name": "Director", "job": "Director"},
                {"id": 9, "name": "Grip", "job": "Grip"},
            ],
            "cast": [
                {"id": 5, "name": "Second", "character": "B", "order": 1},
                {"id": 4, "name": "First", "character": "A", "order": 0},
            ],
        }});
        let credits: Vec<(i64, &str)> = film_credits(&data)
            .iter()
            .map(|c| (c.tmdb_id, c.role))
            .collect();
        assert_eq!(
            credits,
            [(1, "director"), (3, "writer"), (4, "actor"), (5, "actor")]
        );
    }
}
