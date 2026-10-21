//! Баннеры (промо и новости) на страницах сайта: `GET /banners?path=` / `?page=` — публичный
//! список, правка — `admin::banners`. Таблица `banners` (v2: вид, HTML, слайды, кнопки, тема,
//! цели показа, приоритет — миграция `catalog_banners_v2`).

use crate::validate::{self, MAX_TITLE, MAX_URL};
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::Query;
use shared::{AppError, AppResult, AppState};
use sqlx::types::Json as SqlJson;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Страницы сайта для цели `{type:"page"}` и `?page=`.
pub const PAGES: [&str; 6] = ["home", "films", "series", "books", "games", "people"];
/// Пресеты оформления: те же id, что у тем особых коллекций (общий список фронтенда).
pub const THEMES: [&str; 8] = [
    "default",
    "halloween",
    "romance",
    "new-year",
    "horror",
    "sci-fi",
    "family",
    "summer",
];
/// Сколько баннеров отдаёт публичный список.
pub const PUBLIC_LIMIT: usize = 20;
pub const MAX_SLIDES: usize = 10;
pub const MAX_BUTTONS: usize = 3;
pub const MAX_TARGETS: usize = 20;
pub const MAX_PATTERN: usize = 200;
pub const MAX_LABEL: usize = 60;
/// Лимит `content_html` после санитизации (символы).
pub const MAX_HTML: usize = 20_000;
/// Лимит `custom_html` (символы; до и после санитизации).
pub const MAX_CUSTOM_HTML: usize = 100_000;
/// Пути, где показывается цель `{type:"all"}`: главные страницы разделов.
pub const ALL_PATHS: [&str; 6] = ["/", "/films", "/series", "/books", "/games", "/people"];
/// Разделы, где баннеры не показываются никогда (кроме `exclude_paths` баннера).
const ALWAYS_EXCLUDED: [&str; 1] = ["/admin/*"];

/// Колонки ответа; `page` — первый `targets[].page` (переходный период).
pub(crate) const COLUMNS: &str = "id, \
     (SELECT t->>'page' FROM jsonb_array_elements(targets) t WHERE t->>'type' = 'page' LIMIT 1) \
     AS page, kind, title, subtitle, image_url, link_url, content_html, custom_html, slides, buttons, \
     theme, targets, exclude_paths, priority, position, is_active, starts_at, ends_at, created_at, \
     updated_at";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum BannerKind {
    /// Картинка (+ `content_html` поверх). По умолчанию.
    Image,
    /// HTML-текст `content_html`.
    Html,
    /// Слайды `slides` (1..10).
    Slides,
    /// Готовая вёрстка `custom_html` (свои стили и кнопки); прочие поля показа не используются,
    /// `title` — подпись для админки.
    Custom,
}

impl BannerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BannerKind::Image => "image",
            BannerKind::Html => "html",
            BannerKind::Slides => "slides",
            BannerKind::Custom => "custom",
        }
    }

    /// Из значения колонки `kind` (неизвестное — `image`).
    pub fn parse(value: &str) -> BannerKind {
        match value {
            "html" => BannerKind::Html,
            "slides" => BannerKind::Slides,
            "custom" => BannerKind::Custom,
            _ => BannerKind::Image,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BannerSlide {
    /// `http(s)://`.
    pub image_url: Option<String>,
    /// ≤ 300.
    pub title: Option<String>,
    pub subtitle: Option<String>,
    /// Санитизируется как `Banner.content_html`.
    pub content_html: Option<String>,
    /// `http(s)://` или путь `/…` (не `//`).
    pub link_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ButtonStyle {
    Primary,
    Secondary,
    Ghost,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BannerButton {
    /// 1..60 символов.
    pub label: String,
    /// `http(s)://` или путь `/…` (не `//`).
    pub url: String,
    pub style: ButtonStyle,
}

/// Где показывать баннер.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BannerTarget {
    /// Главные страницы разделов: `/`, `/films`, `/series`, `/books`, `/games`, `/people`.
    All,
    /// Страница: `home` (`/`), `films` (`/films`), `series`, `books`, `games`, `people`.
    Page { page: String },
    /// Шаблон пути: начинается с `/`, `*` — любой хвост (`/collections/*`), без `?` и `#`, ≤ 200.
    Path { pattern: String },
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct Banner {
    pub id: Uuid,
    /// Переходный период: первый `targets[].page` или `null`.
    #[schema(example = "films")]
    pub page: Option<String>,
    #[schema(value_type = BannerKind)]
    pub kind: String,
    pub title: String,
    pub subtitle: Option<String>,
    /// Обязателен только при `kind=image`.
    pub image_url: Option<String>,
    /// `http(s)://...` или путь сайта `/films/dune-2021`.
    pub link_url: Option<String>,
    /// Санитизированный HTML (allowlist).
    pub content_html: Option<String>,
    /// Готовая вёрстка при `kind=custom` (мягкая санитизация: свои стили, `<style>`, svg;
    /// без скриптов). Фронт показывает в `<iframe sandbox srcdoc>`.
    pub custom_html: Option<String>,
    #[schema(value_type = Vec<BannerSlide>)]
    pub slides: SqlJson<Vec<BannerSlide>>,
    #[schema(value_type = Vec<BannerButton>)]
    pub buttons: SqlJson<Vec<BannerButton>>,
    /// Пресет оформления (`banners::THEMES`).
    #[schema(example = "halloween")]
    pub theme: Option<String>,
    #[schema(value_type = Vec<BannerTarget>)]
    pub targets: SqlJson<Vec<BannerTarget>>,
    /// Шаблоны путей, где баннер не показывать (`/admin/*` исключён всегда).
    pub exclude_paths: Vec<String>,
    /// Больше — выше в списке.
    pub priority: i32,
    pub position: i32,
    pub is_active: bool,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BannersQuery {
    /// Путь страницы сайта: `/films/dune-2021` (query и `#` отбрасываются). Нужен `path` или `page`.
    pub path: Option<String>,
    /// Совместимость: `home`, `films`, `series`, `books`, `games`, `people` (= путь `/`, `/films`, …).
    pub page: Option<String>,
}

/// Проверенные поля баннера целиком (создание или текущие значения + PATCH).
pub(crate) struct Draft {
    pub kind: BannerKind,
    pub title: String,
    pub subtitle: Option<String>,
    pub image_url: Option<String>,
    pub link_url: Option<String>,
    pub content_html: Option<String>,
    pub custom_html: Option<String>,
    pub slides: Vec<BannerSlide>,
    pub buttons: Vec<BannerButton>,
    pub theme: Option<String>,
    pub targets: Vec<BannerTarget>,
    pub exclude_paths: Vec<String>,
    pub priority: i32,
    pub position: i32,
    pub is_active: bool,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
}

fn invalid(field: impl Into<String>, message: impl Into<String>) -> AppError {
    AppError::Invalid {
        field: field.into(),
        message: message.into(),
    }
}

impl Draft {
    /// Проверить и нормализовать все поля; ошибка — 400 `validation` с `field`.
    pub(crate) fn validate(self) -> Result<Draft, AppError> {
        let title =
            validate::required("title", &self.title, MAX_TITLE).map_err(|e| e.on_field("title"))?;
        let subtitle = optional_text("subtitle", self.subtitle)?;
        let image_url = image("image_url", self.image_url)?;
        if self.kind == BannerKind::Image && image_url.is_none() {
            return Err(invalid("image_url", "image_url is required for kind=image"));
        }
        let link_url = link("link_url", self.link_url)?;
        let content_html = sanitize_html("content_html", self.content_html)?;
        if self.kind == BannerKind::Html && content_html.is_none() {
            return Err(invalid(
                "content_html",
                "content_html must not be empty for kind=html",
            ));
        }
        let custom_html = sanitize_custom_html("custom_html", self.custom_html)?;
        if self.kind == BannerKind::Custom && custom_html.is_none() {
            return Err(invalid(
                "custom_html",
                "custom_html must not be empty for kind=custom",
            ));
        }

        if self.slides.len() > MAX_SLIDES {
            return Err(invalid("slides", format!("at most {MAX_SLIDES} slides")));
        }
        if self.kind == BannerKind::Slides && self.slides.is_empty() {
            return Err(invalid(
                "slides",
                "slides must not be empty for kind=slides",
            ));
        }
        let slides = self
            .slides
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                let f = |name: &str| format!("slides[{i}].{name}");
                Ok(BannerSlide {
                    image_url: image(&f("image_url"), s.image_url)?,
                    title: optional_text(&f("title"), s.title)?,
                    subtitle: optional_text(&f("subtitle"), s.subtitle)?,
                    content_html: sanitize_html(&f("content_html"), s.content_html)?,
                    link_url: link(&f("link_url"), s.link_url)?,
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;

        if self.buttons.len() > MAX_BUTTONS {
            return Err(invalid("buttons", format!("at most {MAX_BUTTONS} buttons")));
        }
        let buttons = self
            .buttons
            .into_iter()
            .enumerate()
            .map(|(i, b)| {
                let label = validate::required("label", &b.label, MAX_LABEL)
                    .map_err(|e| e.on_field(format!("buttons[{i}].label")))?;
                let url = link(&format!("buttons[{i}].url"), Some(b.url))?
                    .ok_or_else(|| invalid(format!("buttons[{i}].url"), "url must not be empty"))?;
                Ok(BannerButton {
                    label,
                    url,
                    style: b.style,
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;

        let theme = match self.theme.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(t) if THEMES.contains(&t) => Some(t.to_string()),
            Some(_) => {
                return Err(invalid(
                    "theme",
                    format!("theme must be one of: {}", THEMES.join(", ")),
                ))
            }
        };

        if self.targets.is_empty() || self.targets.len() > MAX_TARGETS {
            return Err(invalid(
                "targets",
                format!("targets must have 1-{MAX_TARGETS} items"),
            ));
        }
        let targets = self
            .targets
            .into_iter()
            .enumerate()
            .map(|(i, t)| match t {
                BannerTarget::All => Ok(BannerTarget::All),
                BannerTarget::Page { page: p } => page(&p)
                    .map(|page| BannerTarget::Page { page })
                    .map_err(|e| e.on_field(format!("targets[{i}].page"))),
                BannerTarget::Path { pattern: p } => pattern(&format!("targets[{i}].pattern"), &p)
                    .map(|pattern| BannerTarget::Path { pattern }),
            })
            .collect::<Result<Vec<_>, AppError>>()?;

        if self.exclude_paths.len() > MAX_TARGETS {
            return Err(invalid(
                "exclude_paths",
                format!("at most {MAX_TARGETS} exclude_paths"),
            ));
        }
        let exclude_paths = self
            .exclude_paths
            .iter()
            .enumerate()
            .map(|(i, p)| pattern(&format!("exclude_paths[{i}]"), p))
            .collect::<Result<Vec<_>, AppError>>()?;

        window(self.starts_at, self.ends_at).map_err(|e| e.on_field("ends_at"))?;

        Ok(Draft {
            kind: self.kind,
            title,
            subtitle,
            image_url,
            link_url,
            content_html,
            custom_html,
            slides,
            buttons,
            theme,
            targets,
            exclude_paths,
            priority: self.priority,
            position: self.position,
            is_active: self.is_active,
            starts_at: self.starts_at,
            ends_at: self.ends_at,
        })
    }
}

/// `page` из списка [`PAGES`], иначе 400.
pub(crate) fn page(value: &str) -> Result<String, AppError> {
    if PAGES.contains(&value) {
        Ok(value.to_string())
    } else {
        Err(AppError::BadRequest(format!(
            "page must be one of: {}",
            PAGES.join(", ")
        )))
    }
}

fn optional_text(field: &str, value: Option<String>) -> Result<Option<String>, AppError> {
    validate::optional(field, value, MAX_TITLE).map_err(|e| e.on_field(field))
}

fn image(field: &str, value: Option<String>) -> Result<Option<String>, AppError> {
    validate::url(field, value).map_err(|e| e.on_field(field))
}

/// Ссылка: `http(s)://` или путь сайта с `/` (не `//`), без пробелов.
fn link(field: &str, value: Option<String>) -> Result<Option<String>, AppError> {
    match value.as_deref().map(str::trim) {
        Some(path) if path.starts_with('/') && !path.starts_with("//") => {
            let path = validate::required(field, path, MAX_URL).map_err(|e| e.on_field(field))?;
            if path.contains(char::is_whitespace) {
                return Err(invalid(field, format!("{field} must not contain spaces")));
            }
            Ok(Some(path))
        }
        _ => validate::url(field, value).map_err(|e| e.on_field(field)),
    }
}

/// Шаблон пути: `/…`, ≤ 200, без пробелов, `?`, `#`.
fn pattern(field: &str, value: &str) -> Result<String, AppError> {
    let value = value.trim();
    let ok = value.starts_with('/')
        && value.chars().count() <= MAX_PATTERN
        && !value.contains(|c: char| c.is_whitespace() || c == '?' || c == '#');
    if ok {
        Ok(value.to_string())
    } else {
        Err(invalid(
            field,
            format!(
                "{field} must start with / and be ≤ {MAX_PATTERN} chars without spaces, ? and #"
            ),
        ))
    }
}

fn window(
    starts_at: Option<DateTime<Utc>>,
    ends_at: Option<DateTime<Utc>>,
) -> Result<(), AppError> {
    match (starts_at, ends_at) {
        (Some(start), Some(end)) if end <= start => Err(AppError::BadRequest(
            "ends_at must be later than starts_at".into(),
        )),
        _ => Ok(()),
    }
}

/// Ссылка внутри HTML: `http(s)://…` или путь `/…` (не `//`).
fn safe_href(value: &str) -> bool {
    let value = value.trim();
    (value.starts_with('/') && !value.starts_with("//"))
        || value.starts_with("https://")
        || value.starts_with("http://")
}

/// Классы оформления из allowlist: `text-*`, `font-*` (в т.ч. `text-center|left|right`).
fn safe_classes(value: &str) -> Option<String> {
    let kept: Vec<&str> = value
        .split_ascii_whitespace()
        .filter(|c| {
            (c.starts_with("text-") || c.starts_with("font-"))
                && c.len() <= 40
                && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .collect();
    (!kept.is_empty()).then(|| kept.join(" "))
}

/// Allowlist-санитизация HTML (ammonia): теги `p, br, h2-h4, strong, em, b, i, u, s, span, ul,
/// ol, li, a, img, small, mark`; атрибуты `a[href]`, `img[src(https), alt, width, height]`,
/// `class` (только `text-*`/`font-*`). Остальное вырезается (`script`/`style` — вместе с
/// содержимым). Пусто после очистки — `None`; > 20 000 символов — 400.
pub(crate) fn sanitize_html(
    field: &str,
    value: Option<String>,
) -> Result<Option<String>, AppError> {
    let Some(raw) = value else { return Ok(None) };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let tags: HashSet<&str> = [
        "p", "br", "h2", "h3", "h4", "strong", "em", "b", "i", "u", "s", "span", "ul", "ol", "li",
        "a", "img", "small", "mark",
    ]
    .into_iter()
    .collect();
    let tag_attributes: HashMap<&str, HashSet<&str>> = [
        ("a", ["href"].into_iter().collect()),
        (
            "img",
            ["src", "alt", "width", "height"].into_iter().collect(),
        ),
    ]
    .into_iter()
    .collect();
    let clean = ammonia::Builder::default()
        .tags(tags)
        .tag_attributes(tag_attributes)
        .generic_attributes(["class"].into_iter().collect())
        .url_schemes(["http", "https"].into_iter().collect())
        .link_rel(None)
        .attribute_filter(|element, attribute, value| match (element, attribute) {
            ("a", "href") => safe_href(value).then_some(Cow::Borrowed(value)),
            ("img", "src") => value
                .trim()
                .starts_with("https://")
                .then_some(Cow::Borrowed(value)),
            ("img", "width" | "height") => {
                (!value.is_empty() && value.len() <= 5 && value.bytes().all(|b| b.is_ascii_digit()))
                    .then_some(Cow::Borrowed(value))
            }
            (_, "class") => safe_classes(value).map(Cow::Owned),
            _ => Some(Cow::Borrowed(value)),
        })
        .clean(&raw)
        .to_string();
    let clean = clean.trim();
    if clean.is_empty() {
        return Ok(None);
    }
    if clean.chars().count() > MAX_HTML {
        return Err(invalid(
            field,
            format!("{field} is longer than {MAX_HTML} chars after sanitization"),
        ));
    }
    Ok(Some(clean.to_string()))
}

/// Ссылка в `custom_html`: `http(s)://…` или путь `/…`; у `img[src]` ещё `data:image/*`,
/// у svg `use[href]` — `#id`.
fn custom_url(element: &str, attribute: &str, value: &str) -> bool {
    let v = value.trim();
    safe_href(v)
        || (element == "img"
            && attribute == "src"
            && v.get(..11)
                .is_some_and(|p| p.eq_ignore_ascii_case("data:image/")))
        || (element == "use" && v.starts_with('#'))
}

/// `srcset`: каждый кандидат — `http(s)://…` или `/…`.
fn safe_srcset(value: &str) -> bool {
    value.split(',').all(|candidate| {
        candidate
            .split_ascii_whitespace()
            .next()
            .is_some_and(safe_href)
    })
}

/// Удалить все вхождения `needle` (ASCII, без учёта регистра).
fn remove_ci(css: &str, needle: &str) -> String {
    let lower = css.to_ascii_lowercase();
    let mut out = String::with_capacity(css.len());
    let mut last = 0;
    for (i, _) in lower.match_indices(needle) {
        if i >= last {
            out.push_str(&css[last..i]);
            last = i + needle.len();
        }
    }
    out.push_str(&css[last..]);
    out
}

/// `@import …` целиком (до `;` или конца строки).
fn strip_imports(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(i) = rest.to_ascii_lowercase().find("@import") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find([';', '\n']).map_or(tail.len(), |e| e + 1);
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// `url(…)` сохраняет аргумент только `http(s)://…` или `/…`, иначе — `url()`.
fn strip_urls(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(i) = rest.to_ascii_lowercase().find("url(") {
        out.push_str(&rest[..i + 4]);
        let tail = &rest[i + 4..];
        let end = tail.find(')').unwrap_or(tail.len());
        let arg = tail[..end].trim().trim_matches(['"', '\'']).trim();
        if safe_href(arg) && !arg.contains(['"', '\'', '(', ' ']) {
            out.push_str(arg);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Очистка CSS (`style="…"` и `<style>`): убираются комментарии и `\`-экраны, вырезаются
/// `expression(`, `javascript:`, `vbscript:`, `behavior:`, `-moz-binding`, `@import …;`,
/// аргумент `url(…)` не `http(s)://` / `/…`. Повторяется до неподвижной точки.
pub(crate) fn sanitize_css(css: &str) -> String {
    let mut css = css.replace(['\\', '\0'], "");
    loop {
        let mut next = String::with_capacity(css.len());
        let mut rest = css.as_str();
        while let Some(i) = rest.find("/*") {
            next.push_str(&rest[..i]);
            let after = &rest[i + 2..];
            rest = after.find("*/").map_or("", |e| &after[e + 2..]);
        }
        next.push_str(rest);
        for needle in [
            "expression(",
            "javascript:",
            "vbscript:",
            "behavior:",
            "-moz-binding",
        ] {
            next = remove_ci(&next, needle);
        }
        next = strip_urls(&strip_imports(&next));
        if next == css {
            return css;
        }
        css = next;
    }
}

/// Содержимое каждого `<style>…</style>` (как его сериализует ammonia) — через [`sanitize_css`].
fn sanitize_style_blocks(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find("<style") {
        let Some(open) = rest[i..].find('>') else {
            break;
        };
        let body_start = i + open + 1;
        out.push_str(&rest[..body_start]);
        let body = &rest[body_start..];
        let end = body.find("</style").unwrap_or(body.len());
        out.push_str(&sanitize_css(&body[..end]));
        rest = &body[end..];
    }
    out.push_str(rest);
    out
}

/// Мягкая санитизация `custom_html` (ammonia, отдельный builder): теги вёрстки (`div, section,
/// article, header, footer, nav, aside, main, p, span, h1-h6, a, img, picture, source, figure,
/// ul/ol/li, table…`, `svg` с фигурами и градиентами) и `<style>`; атрибуты `class, style, id,
/// href, src, srcset, alt, width, height, aria-*, data-*, target, rel` (+ `title, role, lang,
/// dir, sizes, media, type, loading, decoding, colspan, rowspan` и атрибуты рисования svg).
/// `script, iframe, object, embed, form, input, base, meta, link` (и `noscript, template,
/// textarea, select, button, frame, frameset, applet`) вырезаются с содержимым, `on*` — всегда;
/// URL — только `http(s)://` и `/…` (`data:image/*` — только `img[src]`, `#id` — svg `use`);
/// CSS — [`sanitize_css`]. Пусто — `None`; > 100 000 символов (до или после) — 400.
pub(crate) fn sanitize_custom_html(
    field: &str,
    value: Option<String>,
) -> Result<Option<String>, AppError> {
    let Some(raw) = value else { return Ok(None) };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let too_long = || {
        invalid(
            field,
            format!("{field} must be at most {MAX_CUSTOM_HTML} chars"),
        )
    };
    if raw.chars().count() > MAX_CUSTOM_HTML {
        return Err(too_long());
    }
    let tags: HashSet<&str> = [
        "div",
        "section",
        "article",
        "header",
        "footer",
        "nav",
        "aside",
        "main",
        "p",
        "span",
        "br",
        "hr",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "strong",
        "em",
        "b",
        "i",
        "u",
        "s",
        "small",
        "mark",
        "sub",
        "sup",
        "del",
        "ins",
        "abbr",
        "time",
        "blockquote",
        "q",
        "cite",
        "code",
        "pre",
        "ul",
        "ol",
        "li",
        "dl",
        "dt",
        "dd",
        "a",
        "img",
        "picture",
        "source",
        "figure",
        "figcaption",
        "table",
        "thead",
        "tbody",
        "tfoot",
        "tr",
        "th",
        "td",
        "caption",
        "colgroup",
        "col",
        "wbr",
        "style",
        "svg",
        "g",
        "path",
        "circle",
        "ellipse",
        "line",
        "polyline",
        "polygon",
        "rect",
        "text",
        "tspan",
        "defs",
        "linearGradient",
        "radialGradient",
        "stop",
        "clipPath",
        "mask",
        "pattern",
        "symbol",
        "use",
        "title",
        "desc",
    ]
    .into_iter()
    .collect();
    let generic: HashSet<&str> = [
        "class",
        "style",
        "id",
        "href",
        "src",
        "srcset",
        "alt",
        "width",
        "height",
        "target",
        "rel",
        "title",
        "role",
        "lang",
        "dir",
        "sizes",
        "media",
        "type",
        "loading",
        "decoding",
        "colspan",
        "rowspan",
        "viewBox",
        "xmlns",
        "fill",
        "fill-opacity",
        "fill-rule",
        "clip-rule",
        "clip-path",
        "mask",
        "stroke",
        "stroke-width",
        "stroke-linecap",
        "stroke-linejoin",
        "stroke-opacity",
        "stroke-dasharray",
        "d",
        "cx",
        "cy",
        "r",
        "rx",
        "ry",
        "x",
        "y",
        "x1",
        "y1",
        "x2",
        "y2",
        "points",
        "transform",
        "opacity",
        "offset",
        "stop-color",
        "stop-opacity",
        "gradientUnits",
        "gradientTransform",
        "preserveAspectRatio",
        "font-size",
        "font-weight",
        "text-anchor",
        "dominant-baseline",
    ]
    .into_iter()
    .collect();
    let removed: HashSet<&str> = [
        "script", "iframe", "object", "embed", "form", "input", "base", "meta", "link", "noscript",
        "template", "textarea", "select", "button", "frame", "frameset", "applet",
    ]
    .into_iter()
    .collect();
    let clean = ammonia::Builder::default()
        .tags(tags)
        .tag_attributes(HashMap::new())
        .generic_attributes(generic)
        .generic_attribute_prefixes(["data-", "aria-"].into_iter().collect())
        .clean_content_tags(removed)
        .url_schemes(["http", "https", "data"].into_iter().collect())
        .link_rel(None)
        .strip_comments(true)
        .attribute_filter(|element, attribute, value| match attribute {
            "href" | "src" | "xlink:href" => {
                custom_url(element, attribute, value).then_some(Cow::Borrowed(value))
            }
            "srcset" => safe_srcset(value).then_some(Cow::Borrowed(value)),
            "style" => Some(Cow::Owned(sanitize_css(value))),
            "target" => matches!(value.trim(), "_blank" | "_self" | "_top" | "_parent")
                .then_some(Cow::Borrowed(value)),
            _ if attribute.to_ascii_lowercase().starts_with("on") => None,
            _ => Some(Cow::Borrowed(value)),
        })
        .clean(&raw)
        .to_string();
    let clean = sanitize_style_blocks(&clean);
    let clean = clean.trim();
    if clean.is_empty() {
        return Ok(None);
    }
    if clean.chars().count() > MAX_CUSTOM_HTML {
        return Err(too_long());
    }
    Ok(Some(clean.to_string()))
}

/// Путь без query/`#` и без хвостового `/` (кроме корня).
fn normalize_path(path: &str) -> String {
    let path = path.split(['?', '#']).next().unwrap_or("");
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.into()
    }
}

/// Шаблон с `*` (любая последовательность символов, включая `/`).
fn glob(pattern: &str, path: &str) -> bool {
    let (p, s) = (pattern.as_bytes(), path.as_bytes());
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] != b'*' && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&b| b == b'*')
}

/// `/admin/*` и, как у шаблона `/x/*`, сам `/x` (раздел целиком).
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let pattern = normalize_path(pattern);
    glob(&pattern, path)
        || pattern
            .strip_suffix("/*")
            .is_some_and(|base| base == path || (base.is_empty() && path == "/"))
}

/// Страница для цели `{type:"page"}`: `/` → home, `/films` → films, …
fn page_of_path(path: &str) -> Option<&'static str> {
    if path == "/" {
        return Some("home");
    }
    PAGES
        .iter()
        .copied()
        .find(|p| *p != "home" && path.strip_prefix('/') == Some(p))
}

/// Показывать ли баннер на нормализованном пути `path`.
fn shows_on(banner: &Banner, path: &str) -> bool {
    if banner
        .exclude_paths
        .iter()
        .any(|p| pattern_matches(p, path))
    {
        return false;
    }
    let page = page_of_path(path);
    banner.targets.iter().any(|t| match t {
        BannerTarget::All => ALL_PATHS.contains(&path),
        BannerTarget::Page { page: p } => page == Some(p.as_str()),
        BannerTarget::Path { pattern } => pattern_matches(pattern, path),
    })
}

/// Баннеры страницы сайта: активные, сейчас в окне показа (`starts_at`/`ends_at`, пустые — без
/// границы), совпал хотя бы один target и не совпал `exclude_paths` (`/admin/*` — всегда пусто);
/// `priority desc`, затем `position`; до 20. `Cache-Control: public, max-age=60`.
#[utoipa::path(
    get, operation_id = "list_banners", path = "/banners", tag = "catalog",
    params(BannersQuery),
    responses(
        (status = 200, description = "Баннеры страницы", body = [Banner],
         headers(("Cache-Control" = String, description = "public, max-age=60"))),
        (status = 400, description = "Нет path/page, неизвестная page или path не с `/`", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<BannersQuery>,
) -> AppResult<impl IntoResponse> {
    let path = match (query.path.as_deref().map(str::trim), query.page.as_deref()) {
        (Some(path), _) if !path.is_empty() => {
            if !path.starts_with('/') || path.chars().count() > MAX_URL {
                return Err(invalid(
                    "path",
                    "path must start with / and be ≤ 2048 chars",
                ));
            }
            normalize_path(path)
        }
        (_, Some(p)) => match page(p).map_err(|e| e.on_field("page"))?.as_str() {
            "home" => "/".to_string(),
            other => format!("/{other}"),
        },
        _ => return Err(invalid("path", "path or page is required")),
    };

    let banners: Vec<Banner> = if ALWAYS_EXCLUDED.iter().any(|p| pattern_matches(p, &path)) {
        Vec::new()
    } else {
        let all: Vec<Banner> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM banners
             WHERE is_active
               AND (starts_at IS NULL OR starts_at <= now())
               AND (ends_at IS NULL OR ends_at > now())
             ORDER BY priority DESC, position, created_at, id
             LIMIT 1000"
        ))
        .fetch_all(&state.db)
        .await?;
        all.into_iter()
            .filter(|b| shows_on(b, &path))
            .take(PUBLIC_LIMIT)
            .collect()
    };
    Ok((
        [(header::CACHE_CONTROL, "public, max-age=60")],
        Json(banners),
    ))
}
