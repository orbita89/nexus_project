//! Админка книжного поиска: ручной порядок книг (`catalog_manual_ranks`) и пересборка слоя Б.
//!
//! Ручное место — глобальное: книга с местом 1 первой стоит в «Рекомендуемых» и в любом списке
//! по фасетам, куда она входит (внутри фасета — относительный порядок общего списка). В поиске
//! по тексту место поднимает только сильные совпадения (`books::sidecar`).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AdminUser, AppError, AppResult, AppState};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::books::index::BookIndex;
use crate::books::ranks::{self, MAX_RANK};
use crate::books::sync;

/// Книга с ручным местом.
#[derive(Debug, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct BookRank {
    #[schema(example = "master-i-margarita")]
    pub slug: String,
    pub title: String,
    /// 1..=65535: 1 — первое место.
    #[schema(example = 1)]
    pub rank: i32,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BookRanksQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Новый ручной список целиком (прежний удаляется).
#[derive(Debug, Deserialize, ToSchema)]
pub struct PutBookRanks {
    /// До 65 535 книг; slug'и и места не повторяются. Пустой — ручного порядка нет.
    pub items: Vec<BookRankItem>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BookRankItem {
    pub slug: String,
    /// 1..=65535.
    pub rank: i32,
}

/// Ручной порядок книг по местам.
#[utoipa::path(
    get, operation_id = "list_book_ranks", path = "/admin/books/ranks", tag = "catalog-admin",
    security(("bearer" = [])),
    params(BookRanksQuery),
    responses(
        (status = 200, description = "Книги по местам", body = Page<BookRank>),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<BookRanksQuery>,
) -> AppResult<axum::Json<Page<BookRank>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let items: Vec<BookRank> = sqlx::query_as(
        "SELECT e.slug, e.title, r.rank FROM catalog_manual_ranks r
         JOIN entities e ON e.id = r.entity_id
         ORDER BY r.rank LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_manual_ranks")
        .fetch_one(&state.db)
        .await?;
    Ok(axum::Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Заменить ручной порядок книг целиком. Действует сразу (слой Б в RAM), другие реплики
/// подхватывают правку в течение минуты.
#[utoipa::path(
    put, operation_id = "put_book_ranks", path = "/admin/books/ranks", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = PutBookRanks,
    responses(
        (status = 204, description = "Список заменён"),
        (status = 400, description = "Место вне 1..65535, повтор slug'а или места", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Книги с таким slug нет", body = ErrorBody),
    )
)]
pub async fn put(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(req): JsonBody<PutBookRanks>,
) -> AppResult<StatusCode> {
    if req.items.len() > MAX_RANK {
        return Err(AppError::BadRequest(format!("at most {MAX_RANK} books")).on_field("items"));
    }
    let mut slugs = HashSet::new();
    let mut places = HashSet::new();
    for item in &req.items {
        if !(1..=MAX_RANK as i32).contains(&item.rank) {
            return Err(
                AppError::BadRequest(format!("rank must be 1..={MAX_RANK}")).on_field("rank")
            );
        }
        if !slugs.insert(item.slug.as_str()) {
            return Err(
                AppError::BadRequest(format!("slug {} is repeated", item.slug)).on_field("slug"),
            );
        }
        if !places.insert(item.rank) {
            return Err(
                AppError::BadRequest(format!("rank {} is repeated", item.rank)).on_field("rank"),
            );
        }
    }
    let wanted: Vec<String> = req.items.iter().map(|item| item.slug.clone()).collect();
    let found: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, slug FROM entities WHERE kind = 'book' AND slug = ANY($1)")
            .bind(&wanted)
            .fetch_all(&state.db)
            .await?;
    if found.len() != wanted.len() {
        return Err(AppError::NotFound);
    }
    let ids: std::collections::HashMap<&str, Uuid> = found
        .iter()
        .map(|(id, slug)| (slug.as_str(), *id))
        .collect();
    let list: Vec<(Uuid, i32)> = req
        .items
        .iter()
        .map(|item| (ids[item.slug.as_str()], item.rank))
        .collect();
    ranks::replace_all(&state.db, &list).await?;
    if let Err(error) = sync::apply_ranks(&state).await {
        // Список в БД уже новый: слой Б подхватит его опросом.
        tracing::warn!(%error, "book ranks saved but not applied to the sidecar");
    }
    tracing::info!(admin_id = %admin.id, books = list.len(), "book ranks replaced");
    Ok(StatusCode::NO_CONTENT)
}

/// Пересобрать слой Б книжного поиска (фасеты, сортировки, места) из БД — в фоне.
#[utoipa::path(
    post, operation_id = "reload_books_sidecar", path = "/admin/books/sidecar", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 202, description = "Пересборка запущена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Уже идёт другая операция", body = ErrorBody),
        (status = 503, description = "Книжный поиск выключен", body = ErrorBody),
    )
)]
pub async fn reload_sidecar(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> AppResult<StatusCode> {
    let index = enabled_index(&state)?;
    spawn_operation(&index, "sidecar", admin.id, async move {
        sync::reload_sidecar(&state)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
}

// ------------------------------------------------------------ состояние и управление индексом

/// Место одного поля Tantivy на диске, байты (сумма по сегментам).
#[derive(Debug, Serialize, ToSchema)]
pub struct IndexFieldUsage {
    #[schema(example = "title")]
    pub field: String,
    pub total: u64,
    /// Словарь термов.
    pub termdict: u64,
    /// Списки документов по термам.
    pub postings: u64,
    /// Позиции слов (поиск фраз).
    pub positions: u64,
    /// FAST-колонки.
    pub fast: u64,
    /// Нормы длины поля.
    pub fieldnorms: u64,
}

/// Слой А — Tantivy (текст и карточки).
#[derive(Debug, Serialize, ToSchema)]
pub struct TextIndexStatus {
    #[schema(example = "books-4")]
    pub schema_version: String,
    pub docs: u64,
    /// Удалённые документы, ещё лежащие в сегментах (уйдут при слиянии `merge`).
    pub deleted_docs: u64,
    pub segments: u64,
    /// Всё, что занимает индекс по данным Tantivy.
    pub index_bytes: u64,
    /// Хранилище документов (поле `card`; сжато целиком, по полям не делится).
    pub store_bytes: u64,
    pub deletes_bytes: u64,
    /// Все файлы каталога индекса (с ещё не убранными старыми сегментами и `sidecar.bin`).
    pub disk_bytes: u64,
    /// Снимок слоя Б на диске (`sidecar.bin`).
    pub sidecar_file_bytes: u64,
    /// Поля по убыванию места.
    pub fields: Vec<IndexFieldUsage>,
    /// Ответов в кэше выдачи и готовых рядов.
    pub cache_entries: u64,
    pub cache_ranked: u64,
}

/// Один вид фасетов слоя Б.
#[derive(Debug, Serialize, ToSchema)]
pub struct FacetGroupStatus {
    /// Префикс ключа: `tag`, `person`, `role`, `pub`, `lang`, `form`, `year`.
    #[schema(example = "tag")]
    pub kind: String,
    /// Сколько разных значений.
    pub values: u64,
    /// Битмапы в сжатом виде, байты.
    pub bytes: u64,
}

/// Слой Б — RAM (фасеты, сортировки, ручной порядок).
#[derive(Debug, Serialize, ToSchema)]
pub struct SidecarStatus {
    /// Растёт с каждой подменой слоя (ключ кэша выдачи).
    pub generation: u64,
    /// Видимые книги.
    pub books: u64,
    /// Длина массивов по `ord` (с дырами от удалённых книг).
    pub slots: u64,
    pub with_cover: u64,
    pub manual_ranks: u64,
    /// Книг по виду текста: `book`, `collection`, `article`, `document`.
    pub forms: BTreeMap<String, u64>,
    pub facets: Vec<FacetGroupStatus>,
    /// Оценка памяти: `SortMeta` (8 байт на книгу), готовые порядки, битмапы.
    pub meta_bytes: u64,
    pub orders_bytes: u64,
    pub facets_bytes: u64,
    pub total_bytes: u64,
}

/// Сборка в этом процессе.
#[derive(Debug, Serialize, ToSchema)]
pub struct BuildStatus {
    pub at: DateTime<Utc>,
    pub took_ms: u64,
    pub books: u64,
}

/// Операция админки над индексом.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IndexOperation {
    /// `rebuild`, `sidecar`, `merge`.
    #[schema(example = "rebuild")]
    pub op: String,
    pub started_at: DateTime<Utc>,
    /// `null` — ещё идёт.
    pub finished_at: Option<DateTime<Utc>>,
    /// `null` — ещё идёт.
    pub ok: Option<bool>,
    pub error: Option<String>,
}

/// Книжный индекс целиком: `GET /admin/books/index`.
#[derive(Debug, Serialize, ToSchema)]
pub struct BooksIndexStatus {
    /// Видимых книг в БД — столько должно быть в индексе.
    pub db_books: i64,
    /// Выданных номеров книг (`catalog_book_ords`).
    pub db_ords: i64,
    pub text: TextIndexStatus,
    pub sidecar: SidecarStatus,
    /// Последние сборки в этом процессе (после рестарта — `null`).
    pub last_index_build: Option<BuildStatus>,
    pub last_sidecar_build: Option<BuildStatus>,
    /// Текущая или последняя операция админки.
    pub operation: Option<IndexOperation>,
}

/// Операция админки: одна за раз на каталог индекса (в тестах у каждого свой).
static OPERATIONS: Mutex<BTreeMap<PathBuf, IndexOperation>> = Mutex::new(BTreeMap::new());

fn operation(dir: &Path) -> Option<IndexOperation> {
    OPERATIONS.lock().unwrap().get(dir).cloned()
}

/// Начать операцию `op`; другая ещё идёт — 409.
fn begin(dir: &Path, op: &str) -> AppResult<()> {
    let mut all = OPERATIONS.lock().unwrap();
    if let Some(running) = all.get(dir).filter(|o| o.finished_at.is_none()) {
        return Err(AppError::Conflict(format!(
            "books index: {} is running",
            running.op
        )));
    }
    all.insert(
        dir.to_path_buf(),
        IndexOperation {
            op: op.to_string(),
            started_at: Utc::now(),
            finished_at: None,
            ok: None,
            error: None,
        },
    );
    Ok(())
}

fn finish(dir: &Path, result: Result<(), String>) {
    if let Some(op) = OPERATIONS.lock().unwrap().get_mut(dir) {
        op.finished_at = Some(Utc::now());
        op.ok = Some(result.is_ok());
        op.error = result.err();
    }
}

/// Запустить операцию в фоне.
fn spawn_operation<F>(
    index: &BookIndex,
    op: &'static str,
    admin: Uuid,
    work: F,
) -> AppResult<StatusCode>
where
    F: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let dir = index.dir().to_path_buf();
    begin(&dir, op)?;
    tracing::info!(admin_id = %admin, op, "books index operation started");
    tokio::spawn(async move {
        let result = work.await;
        if let Err(error) = &result {
            tracing::warn!(op, %error, "books index operation failed");
        }
        finish(&dir, result);
    });
    Ok(StatusCode::ACCEPTED)
}

fn enabled_index(state: &AppState) -> AppResult<Arc<BookIndex>> {
    match sync::open(state) {
        None => Err(AppError::Unavailable("books search is not enabled".into())),
        Some(Err(error)) => Err(AppError::Internal(format!("books index: {error}"))),
        Some(Ok(index)) => Ok(index),
    }
}

fn build_status(info: Option<sync::BuildInfo>) -> Option<BuildStatus> {
    info.map(|b| BuildStatus {
        at: b.at,
        took_ms: b.took_ms,
        books: b.books,
    })
}

/// Состояние книжного индекса: оба слоя, место по полям, фасеты, последние сборки.
#[utoipa::path(
    get, operation_id = "books_index_status", path = "/admin/books/index", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Состояние индекса", body = BooksIndexStatus),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 503, description = "Книжный поиск выключен", body = ErrorBody),
    )
)]
pub async fn index_status(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> AppResult<axum::Json<BooksIndexStatus>> {
    let index = enabled_index(&state)?;
    let current = operation(index.dir());
    let (db_books, db_ords) = sync::db_counts(&state.db).await?;
    let (text, side) = tokio::task::spawn_blocking(move || {
        let text = index.stats();
        (text, index.sidecar().stats())
    })
    .await
    .map_err(|e| AppError::Internal(format!("books index task failed: {e}")))?;
    let text = text.map_err(|e| AppError::Internal(format!("books index: {e}")))?;
    let mut fields: Vec<IndexFieldUsage> = text
        .fields
        .into_iter()
        .map(|(field, b)| IndexFieldUsage {
            field,
            total: b.total(),
            termdict: b.termdict,
            postings: b.postings,
            positions: b.positions,
            fast: b.fast,
            fieldnorms: b.fieldnorms,
        })
        .collect();
    fields.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.field.cmp(&b.field)));
    let (last_sidecar, last_index) = sync::last_builds();
    Ok(axum::Json(BooksIndexStatus {
        db_books,
        db_ords,
        text: TextIndexStatus {
            schema_version: text.schema_version.to_string(),
            docs: text.docs,
            deleted_docs: text.deleted_docs,
            segments: text.segments as u64,
            index_bytes: text.index_bytes,
            store_bytes: text.store_bytes,
            deletes_bytes: text.deletes_bytes,
            disk_bytes: text.disk_bytes,
            sidecar_file_bytes: text.sidecar_file_bytes,
            fields,
            cache_entries: text.cache_entries as u64,
            cache_ranked: text.cache_ranked as u64,
        },
        sidecar: SidecarStatus {
            generation: side.generation,
            books: side.books,
            slots: side.slots,
            with_cover: side.with_cover,
            manual_ranks: side.manual_ranks,
            forms: side
                .forms
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            facets: side
                .facets
                .into_iter()
                .map(|(kind, g)| FacetGroupStatus {
                    kind,
                    values: g.values,
                    bytes: g.bytes,
                })
                .collect(),
            meta_bytes: side.meta_bytes,
            orders_bytes: side.orders_bytes,
            facets_bytes: side.facets_bytes,
            total_bytes: side.meta_bytes + side.orders_bytes + side.facets_bytes,
        },
        last_index_build: build_status(last_index),
        last_sidecar_build: build_status(last_sidecar),
        operation: current,
    }))
}

/// Полная перестройка: Tantivy, слой Б и отпечатки текста — из БД, в фоне. До коммита поиск
/// читает старый индекс целиком. Ход — в `operation` у `GET /admin/books/index`.
#[utoipa::path(
    post, operation_id = "rebuild_books_index", path = "/admin/books/index/rebuild", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 202, description = "Перестройка запущена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Уже идёт другая операция", body = ErrorBody),
        (status = 503, description = "Книжный поиск выключен", body = ErrorBody),
    )
)]
pub async fn rebuild_index(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> AppResult<StatusCode> {
    let index = enabled_index(&state)?;
    spawn_operation(&index, "rebuild", admin.id, async move {
        sync::rebuild(&state)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
}

/// Слить сегменты Tantivy в один и убрать старые файлы — в фоне: освобождает место удалённых
/// документов (`deleted_docs`) и ускоряет поиск. Данные не меняются.
#[utoipa::path(
    post, operation_id = "merge_books_index", path = "/admin/books/index/merge", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 202, description = "Слияние запущено"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Уже идёт другая операция", body = ErrorBody),
        (status = 503, description = "Книжный поиск выключен", body = ErrorBody),
    )
)]
pub async fn merge_index(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> AppResult<StatusCode> {
    let index = enabled_index(&state)?;
    spawn_operation(&index.clone(), "merge", admin.id, async move {
        tokio::task::spawn_blocking(move || index.merge())
            .await
            .map_err(|e| e.to_string())?
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
}

/// Сбросить кэш выдачи книг и заново прогреть списки раздела (сразу).
#[utoipa::path(
    post, operation_id = "clear_books_cache", path = "/admin/books/index/cache", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 204, description = "Кэш сброшен"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 503, description = "Книжный поиск выключен", body = ErrorBody),
    )
)]
pub async fn clear_cache(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> AppResult<StatusCode> {
    let index = enabled_index(&state)?;
    tokio::task::spawn_blocking(move || index.clear_cache())
        .await
        .map_err(|e| AppError::Internal(format!("books index task failed: {e}")))?
        .map_err(|e| AppError::Internal(format!("books index: {e}")))?;
    tracing::info!(admin_id = %admin.id, "books search cache cleared");
    Ok(StatusCode::NO_CONTENT)
}
