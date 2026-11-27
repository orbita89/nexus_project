//! Админка слоя Б единого индекса: из чего он состоит, сколько памяти занимает каждая часть,
//! память процесса и что слой Б знает о конкретной сущности или человеке (номер, флаги, балл,
//! позиции в готовых порядках, фасеты, сохранённая карточка Tantivy).

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::Query;
use shared::{AdminUser, AppError, AppResult, AppState};
use utoipa::{IntoParams, ToSchema};

use crate::index::sidecar::{FLAG_AUTHOR, FLAG_COVER, FLAG_DESCRIPTION};

/// Часть слоя Б в памяти.
#[derive(Debug, Serialize, ToSchema)]
pub struct SidecarPart {
    /// `meta`, `base_rank`, `order:popular`, `facet:tag`, `people:fame`…
    pub name: String,
    pub bytes: u64,
    /// Элементов: номеров в порядке, значений фасета, людей в роли.
    pub items: u64,
    /// Лежит в отображённом файле: в куче процесса нет, в RAM — только прочитанные страницы.
    pub mapped: bool,
}

/// Память процесса (Linux `/proc/self`), байт.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct ProcessMemory {
    /// Всё, что процесс держит в RAM.
    pub rss: u64,
    /// Куча и прочая анонимная память (слой Б, кэши, буферы, освобождённое, но не отданное ОС).
    pub anon: u64,
    /// Отображённые файлы (индекс Tantivy с диска; ОС вытесняет их при нехватке памяти).
    pub file: u64,
}

/// Слой Б целиком.
#[derive(Debug, Serialize, ToSchema)]
pub struct SidecarView {
    pub process: ProcessMemory,
    pub entities: u64,
    /// Длина плоских массивов (наибольший номер + 1).
    pub entity_slots: u64,
    pub people: u64,
    pub people_slots: u64,
    pub entities_bytes: u64,
    pub people_bytes: u64,
    /// Из частей слоя Б: в куче процесса и в отображённых файлах.
    pub heap_bytes: u64,
    pub mapped_bytes: u64,
    /// Записей в кэше ответов: страницы поиска, ряды выдачи, страницы людей.
    pub cache_pages: u64,
    pub cache_ranked: u64,
    pub cache_people: u64,
    pub parts: Vec<SidecarPart>,
}

fn process_memory() -> ProcessMemory {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
        return ProcessMemory::default();
    };
    let kb = |key: &str| {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    ProcessMemory {
        rss: kb("VmRSS:"),
        anon: kb("RssAnon:"),
        file: kb("RssFile:"),
    }
}

fn open(state: &AppState) -> AppResult<std::sync::Arc<crate::index::CatalogIndex>> {
    match crate::index::sync::open(state) {
        Some(Ok(index)) => Ok(index),
        _ => Err(AppError::Unavailable("index is not enabled".into())),
    }
}

/// Состав слоя Б и память по частям.
#[utoipa::path(
    get, operation_id = "get_catalog_sidecar", path = "/admin/search/sidecar", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Слой Б: части и память", body = SidecarView),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 503, description = "Индекс выключен", body = ErrorBody),
    )
)]
pub async fn view(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
) -> AppResult<Json<SidecarView>> {
    let index = open(&state)?;
    let entities = index.entities();
    let people = index.people();
    let to_part = |p: crate::index::sidecar::MemPart| SidecarPart {
        name: p.name,
        bytes: p.bytes,
        items: p.items,
        mapped: p.mapped,
    };
    let entity_parts: Vec<SidecarPart> = entities.parts().into_iter().map(to_part).collect();
    let people_parts: Vec<SidecarPart> = people.parts().into_iter().map(to_part).collect();
    let es = entities.stats();
    let ps = people.stats();
    let (pages, ranked, cached_people) = index.cache_sizes();
    Ok(Json(SidecarView {
        process: process_memory(),
        entities: es.entities,
        entity_slots: es.slots,
        people: ps.people,
        people_slots: ps.slots,
        entities_bytes: entity_parts.iter().map(|p| p.bytes).sum(),
        people_bytes: people_parts.iter().map(|p| p.bytes).sum(),
        heap_bytes: entity_parts
            .iter()
            .chain(&people_parts)
            .filter(|p| !p.mapped)
            .map(|p| p.bytes)
            .sum(),
        mapped_bytes: entity_parts
            .iter()
            .chain(&people_parts)
            .filter(|p| p.mapped)
            .map(|p| p.bytes)
            .sum(),
        cache_pages: pages as u64,
        cache_ranked: ranked as u64,
        cache_people: cached_people as u64,
        parts: entity_parts.into_iter().chain(people_parts).collect(),
    }))
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct InspectQuery {
    /// Slug сущности или человека.
    pub slug: String,
}

/// Позиция в готовом порядке.
#[derive(Debug, Serialize, ToSchema)]
pub struct OrderPosition {
    /// `recommended`, `new`, `title`, `popular`, `rating`, `trending`, `top_rated`, `awarded`,
    /// у людей — `fame`.
    pub order: String,
    /// С нуля; `null` — не входит в порядок (например, нет даты для `new`).
    pub position: Option<u32>,
    pub of: u64,
}

/// Что слой Б знает об одной сущности или человеке.
#[derive(Debug, Serialize, ToSchema)]
pub struct SidecarItem {
    /// `entity` или `person`.
    pub space: String,
    pub slug: String,
    pub title: String,
    pub kind: String,
    /// Номер: индекс в массивах слоя Б и бит в битовых картах.
    pub ord: u32,
    /// Виден в выдаче.
    pub visible: bool,
    /// Ручное место (0 — нет).
    pub manual_rank: u16,
    pub has_cover: bool,
    pub has_description: bool,
    pub has_author: bool,
    /// Популярность в поиске (`rank_score` × 10⁶; у людей — известность).
    pub popularity: f64,
    /// Балл «Рекомендуемые» из слоя Б (×1000) и он же в БД.
    pub rank: u64,
    pub db_rank_score: Option<f64>,
    /// Дата выхода в днях от начала эры (0 — нет).
    pub release_day: u32,
    pub positions: Vec<OrderPosition>,
    /// Значения фасетов, в которых он есть (`kind:movie`, `tag:…`, `year:2010`).
    pub facets: Vec<String>,
    /// Карточка, сохранённая в Tantivy (поле `card`): её отдают поиск и подсказки.
    pub card: Option<serde_json::Value>,
}

/// Слой Б об одной сущности или человеке по slug.
#[utoipa::path(
    get, operation_id = "inspect_catalog_sidecar", path = "/admin/search/sidecar/item", tag = "catalog-admin",
    security(("bearer" = [])),
    params(InspectQuery),
    responses(
        (status = 200, description = "Данные слоя Б", body = SidecarItem),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такого slug или нет номера", body = ErrorBody),
        (status = 503, description = "Индекс выключен", body = ErrorBody),
    )
)]
pub async fn item(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    Query(query): Query<InspectQuery>,
) -> AppResult<Json<SidecarItem>> {
    let index = open(&state)?;
    let slug = query.slug.trim().to_owned();
    let entity: Option<(i32, String, String, Option<f64>)> = sqlx::query_as(
        "SELECT o.ord, e.title, e.kind::text, s.rank_score
         FROM entities e
         JOIN catalog_entity_ords o ON o.entity_id = e.id
         LEFT JOIN entity_stats s ON s.entity_id = e.id
         WHERE e.slug = $1",
    )
    .bind(&slug)
    .fetch_optional(&state.db)
    .await?;
    if let Some((ord, title, kind, db_rank_score)) = entity {
        let ord = ord as u32;
        let side = index.entities();
        let Some(found) = side.inspect(ord) else {
            return Err(AppError::NotFound);
        };
        let card = card(&index, false, ord).await;
        return Ok(Json(SidecarItem {
            space: "entity".into(),
            slug,
            title,
            kind,
            ord,
            visible: found.visible,
            manual_rank: found.meta.manual_rank,
            has_cover: found.meta.flags & FLAG_COVER != 0,
            has_description: found.meta.flags & FLAG_DESCRIPTION != 0,
            has_author: found.meta.flags & FLAG_AUTHOR != 0,
            popularity: f64::from(found.meta.popularity),
            rank: found.rank,
            db_rank_score,
            release_day: found.release_day,
            positions: found
                .positions
                .into_iter()
                .map(|(order, position, of)| OrderPosition {
                    order: order.into(),
                    position,
                    of,
                })
                .collect(),
            facets: found.facets,
            card,
        }));
    }
    let person: Option<(i32, String)> = sqlx::query_as(
        "SELECT o.ord, p.full_name FROM people p
         JOIN catalog_person_ords o ON o.person_id = p.id WHERE p.slug = $1",
    )
    .bind(&slug)
    .fetch_optional(&state.db)
    .await?;
    let Some((ord, title)) = person else {
        return Err(AppError::NotFound);
    };
    let ord = ord as u32;
    let people = index.people();
    let Some((visible, fame, position, role)) = people.inspect(ord) else {
        return Err(AppError::NotFound);
    };
    let card = card(&index, true, ord).await;
    Ok(Json(SidecarItem {
        space: "person".into(),
        slug,
        title,
        kind: role.clone().unwrap_or_default(),
        ord,
        visible,
        manual_rank: 0,
        has_cover: false,
        has_description: false,
        has_author: false,
        popularity: f64::from(fame),
        rank: 0,
        db_rank_score: None,
        release_day: 0,
        positions: vec![OrderPosition {
            order: "fame".into(),
            position,
            of: people.len(),
        }],
        facets: role
            .map(|r| vec![format!("known_for:{r}")])
            .unwrap_or_default(),
        card,
    }))
}

async fn card(
    index: &std::sync::Arc<crate::index::CatalogIndex>,
    person: bool,
    ord: u32,
) -> Option<serde_json::Value> {
    let index = index.clone();
    let text = tokio::task::spawn_blocking(move || index.stored_card(person, ord))
        .await
        .ok()?
        .ok()??;
    serde_json::from_str(&text).ok()
}

/// Файл каталога индекса.
#[derive(Debug, Serialize, ToSchema)]
pub struct IndexFile {
    pub name: String,
    pub bytes: u64,
    /// `A` — Tantivy (текст и карточки), `B` — слой Б (фасеты, сортировки).
    pub layer: String,
    /// `mmap` — читается с диска страницами; `ram` — читается в кучу при открытии; `meta`.
    pub access: String,
}

/// Поле Tantivy и его место на диске.
#[derive(Debug, Serialize, ToSchema)]
pub struct IndexFieldView {
    pub name: String,
    pub termdict: u64,
    pub postings: u64,
    pub positions: u64,
    pub fast: u64,
    pub fieldnorms: u64,
}

/// Готовый порядок слоя Б и его начало.
#[derive(Debug, Serialize, ToSchema)]
pub struct OrderView {
    /// `recommended`, `new`, `title`, `popular`, `rating`, `trending`, `top_rated`, `awarded`.
    pub name: String,
    pub len: u64,
    pub head: Vec<OrderHead>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OrderHead {
    pub ord: u32,
    pub title: String,
}

/// Группа фильтров и где она хранится.
#[derive(Debug, Serialize, ToSchema)]
pub struct FilterGroupView {
    /// `kind`, `tag`, `studio`, `person`…
    pub group: String,
    /// `ram` — битовые карты слоя Б; `tantivy` — термы поля `facet` слоя А.
    pub storage: String,
    /// Значений (у Tantivy — термов, сумма по сегментам).
    pub values: u64,
    /// Байт в памяти (у Tantivy — 0: на диске).
    pub bytes: u64,
    /// Самые частые значения: значение и сколько у него сущностей (у Tantivy — приблизительно).
    pub top: Vec<FilterValue>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FilterValue {
    pub value: String,
    pub count: u64,
}

/// Как устроен поиск сейчас: файлы, поля, порядки, фильтры.
#[derive(Debug, Serialize, ToSchema)]
pub struct SearchLayout {
    pub dir: String,
    pub docs: u64,
    pub segments: u64,
    pub store_bytes: u64,
    pub files: Vec<IndexFile>,
    pub fields: Vec<IndexFieldView>,
    pub orders: Vec<OrderView>,
    pub filters: Vec<FilterGroupView>,
    /// Индекс только читается (прод).
    pub readonly: bool,
}

fn classify(name: &str) -> (&'static str, &'static str) {
    if name.starts_with("sidecar-") {
        if name.ends_with(".cols") {
            ("B", "mmap")
        } else {
            ("B", "ram")
        }
    } else if name.ends_with(".json") || name.starts_with('.') || name == "schema-version" {
        ("A", "meta")
    } else {
        ("A", "mmap")
    }
}

/// Устройство поиска: файлы каталога индекса (слой А и слой Б, как читаются), поля Tantivy,
/// готовые порядки с первыми записями, группы фильтров и где они хранятся.
#[utoipa::path(
    get, operation_id = "get_search_layout", path = "/admin/search/layout", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Устройство поиска", body = SearchLayout),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 503, description = "Индекс выключен", body = ErrorBody),
    )
)]
pub async fn layout(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
) -> AppResult<Json<SearchLayout>> {
    let index = open(&state)?;
    let readonly = crate::index::sync::readonly(&state);
    let layout = tokio::task::spawn_blocking(move || -> Result<SearchLayout, String> {
        let stats = index.stats().map_err(|e| e.to_string())?;
        let dir = index.dir().to_path_buf();
        let mut files: Vec<IndexFile> = std::fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let meta = entry.metadata().ok()?;
                meta.is_file().then(|| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let (layer, access) = classify(&name);
                    IndexFile {
                        layer: layer.into(),
                        access: access.into(),
                        bytes: meta.len(),
                        name,
                    }
                })
            })
            .collect();
        files.sort_by_key(|f| std::cmp::Reverse(f.bytes));
        let fields = stats
            .fields
            .iter()
            .map(|(name, f)| IndexFieldView {
                name: name.clone(),
                termdict: f.termdict,
                postings: f.postings,
                positions: f.positions,
                fast: f.fast,
                fieldnorms: f.fieldnorms,
            })
            .collect();
        let orders = index
            .order_heads(5)
            .into_iter()
            .map(|(name, len, head)| OrderView {
                name: name.into(),
                len,
                head: head
                    .into_iter()
                    .map(|(ord, title)| OrderHead { ord, title })
                    .collect(),
            })
            .collect();
        let to_values = |list: Vec<(String, u64)>| {
            list.into_iter()
                .map(|(value, count)| FilterValue { value, count })
                .collect::<Vec<_>>()
        };
        let entities = index.entities();
        let side = entities.stats();
        let mut tops = entities.top_values(12);
        let mut filters: Vec<FilterGroupView> = side
            .facets
            .into_iter()
            .map(|(group, g)| FilterGroupView {
                top: to_values(tops.remove(&group).unwrap_or_default()),
                group,
                storage: "ram".into(),
                values: g.values,
                bytes: g.bytes,
            })
            .collect();
        let people = index.people().stats();
        filters.push(FilterGroupView {
            group: "known_for".into(),
            storage: "ram".into(),
            values: people.roles.len() as u64,
            bytes: 0,
            top: to_values(people.roles.into_iter().take(12).collect()),
        });
        for (group, values, top) in index.facet_term_counts(12).map_err(|e| e.to_string())? {
            filters.push(FilterGroupView {
                group,
                storage: "tantivy".into(),
                values,
                bytes: 0,
                top: to_values(top),
            });
        }
        Ok(SearchLayout {
            dir: dir.to_string_lossy().into_owned(),
            docs: stats.docs,
            segments: stats.segments as u64,
            store_bytes: stats.store_bytes,
            files,
            fields,
            orders,
            filters,
            readonly,
        })
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .map_err(AppError::Internal)?;
    Ok(Json(layout))
}
