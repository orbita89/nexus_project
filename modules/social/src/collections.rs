//! Коллекции: подборки произведений. Публичные видны всем, приватные — только владельцу
//! (для остальных их как будто нет: 404). Менять коллекцию может только владелец.

use crate::models::{
    Collection, CollectionDetail, CollectionItem, CollectionItemRow, CollectionRow,
    CreateCollection, PageQuery, PutCollectionItem, ReorderItems, UpdateCollection,
    COLLECTION_COLUMNS, ITEM_COLUMNS,
};
use crate::refs;
use crate::validate::{
    self, MAX_COLLECTION_DESCRIPTION, MAX_COLLECTION_ITEMS, MAX_COLLECTION_TITLE, MAX_NOTE,
};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser};
use sqlx::PgExecutor;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Новые публичные коллекции.
#[utoipa::path(
    get, path = "/collections", tag = "collections",
    params(PageQuery),
    responses((status = 200, description = "Страница коллекций", body = Page<Collection>))
)]
pub async fn list_public(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<Collection>>> {
    page(&state, "c.is_public", None, query).await
}

/// Коллекции пользователя, новые сверху. Владелец видит и свои приватные.
#[utoipa::path(
    get, operation_id = "list_user_collections", path = "/users/{username}/collections", tag = "collections",
    security((), ("bearer" = [])),
    params(("username" = String, Path, description = "username", example = "user"), PageQuery),
    responses(
        (status = 200, description = "Страница коллекций", body = Page<Collection>),
        (status = 401, description = "Токен передан, но недействителен", body = ErrorBody),
        (status = 404, description = "Пользователь не найден", body = ErrorBody),
    )
)]
pub async fn list_for_user(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(username): Path<String>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<Collection>>> {
    let owner = refs::user(&state, &username).await?;
    let filter = if viewer.is_some_and(|viewer| viewer.id == owner.id) {
        "c.user_id = $1"
    } else {
        "c.user_id = $1 AND c.is_public"
    };
    page(&state, filter, Some(owner.id), query).await
}

/// Публичные коллекции, в которых есть сущность.
#[utoipa::path(
    get, operation_id = "list_entity_collections", path = "/entities/{slug}/collections", tag = "collections",
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021"), PageQuery),
    responses(
        (status = 200, description = "Страница коллекций", body = Page<Collection>),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn list_for_entity(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<Collection>>> {
    let entity = refs::entity(&state, &slug).await?;
    let filter = "c.is_public AND EXISTS (SELECT 1 FROM collection_items i
                  WHERE i.collection_id = c.id AND i.entity_id = $1)";
    page(&state, filter, Some(entity.id), query).await
}

/// Страница коллекций под фильтром `filter` (`$1` — `arg`, если есть).
async fn page(
    state: &AppState,
    filter: &str,
    arg: Option<Uuid>,
    query: PageQuery,
) -> AppResult<Json<Page<Collection>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    // $1 привязывается всегда (без аргумента — NULL), чтобы номера $2/$3 не зависели от фильтра.
    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM collections c WHERE {filter}"
    ))
    .bind(arg)
    .fetch_one(&state.db)
    .await?;
    let rows: Vec<CollectionRow> = sqlx::query_as(&format!(
        "SELECT {COLLECTION_COLUMNS} FROM collections c WHERE {filter}
         ORDER BY c.created_at DESC, c.id LIMIT $2 OFFSET $3"
    ))
    .bind(arg)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page {
        items: refs::collections(state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Коллекция с содержимым по порядку.
#[utoipa::path(
    get, operation_id = "get_collection", path = "/collections/{id}", tag = "collections",
    security((), ("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    responses(
        (status = 200, description = "Коллекция", body = CollectionDetail),
        (status = 401, description = "Токен передан, но недействителен", body = ErrorBody),
        (status = 404, description = "Нет или приватная чужая", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<CollectionDetail>> {
    let row = load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    if !row.is_public && viewer.is_none_or(|viewer| viewer.id != row.user_id) {
        return Err(AppError::NotFound);
    }
    Ok(Json(detail(&state, row).await?))
}

/// Создать коллекцию.
#[utoipa::path(
    post, operation_id = "create_collection", path = "/collections", tag = "collections",
    security(("bearer" = [])),
    request_body = CreateCollection,
    responses(
        (status = 201, description = "Создана", body = CollectionDetail),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(req): JsonBody<CreateCollection>,
) -> AppResult<(StatusCode, Json<CollectionDetail>)> {
    let title = validate::required("title", &req.title, MAX_COLLECTION_TITLE)?;
    let description =
        validate::optional("description", req.description, MAX_COLLECTION_DESCRIPTION)?;

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO collections (user_id, title, description, is_public)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(user.id)
    .bind(title)
    .bind(description)
    .bind(req.is_public.unwrap_or(true))
    .fetch_one(&state.db)
    .await
    .map_err(validate::missing_reference)?;

    tracing::info!(user_id = %user.id, collection_id = %id, "collection created");
    let row = load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    Ok((StatusCode::CREATED, Json(detail(&state, row).await?)))
}

/// Изменить название, описание или видимость своей коллекции.
#[utoipa::path(
    patch, operation_id = "update_collection", path = "/collections/{id}", tag = "collections",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    request_body = UpdateCollection,
    responses(
        (status = 200, description = "Изменена", body = CollectionDetail),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужая публичная коллекция", body = ErrorBody),
        (status = 404, description = "Нет или приватная чужая", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<UpdateCollection>,
) -> AppResult<Json<CollectionDetail>> {
    let title = req
        .title
        .map(|title| validate::required("title", &title, MAX_COLLECTION_TITLE))
        .transpose()?;
    let description = req
        .description
        .map(|value| validate::optional("description", value, MAX_COLLECTION_DESCRIPTION))
        .transpose()?;
    owned(&state.db, id, &user).await?;

    sqlx::query(
        "UPDATE collections SET
            title = COALESCE($2, title),
            description = CASE WHEN $3 THEN $4 ELSE description END,
            is_public = COALESCE($5, is_public)
         WHERE id = $1",
    )
    .bind(id)
    .bind(title)
    .bind(description.is_some())
    .bind(description.flatten())
    .bind(req.is_public)
    .execute(&state.db)
    .await?;

    let row = load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    Ok(Json(detail(&state, row).await?))
}

/// Удалить свою коллекцию.
#[utoipa::path(
    delete, operation_id = "delete_collection", path = "/collections/{id}", tag = "collections",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужая публичная коллекция", body = ErrorBody),
        (status = 404, description = "Нет или приватная чужая", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    owned(&state.db, id, &user).await?;
    sqlx::query("DELETE FROM collections WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    tracing::info!(user_id = %user.id, collection_id = %id, "collection deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// Добавить произведение в свою коллекцию или изменить его позицию и заметку.
#[utoipa::path(
    put, path = "/collections/{id}/items/{slug}", tag = "collections",
    security(("bearer" = [])),
    params(
        ("id" = Uuid, Path, description = "id коллекции"),
        ("slug" = String, Path, description = "slug сущности", example = "dune-2021"),
    ),
    request_body(content = PutCollectionItem, description = "Можно не передавать тело: добавить в конец без заметки"),
    responses(
        (status = 201, description = "Добавлено", body = CollectionItem),
        (status = 200, description = "Изменено", body = CollectionItem),
        (status = 400, description = "Неверные поля или в коллекции уже 500 произведений", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужая публичная коллекция", body = ErrorBody),
        (status = 404, description = "Нет коллекции или сущности", body = ErrorBody),
    )
)]
pub async fn put_item(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, slug)): Path<(Uuid, String)>,
    body: Option<JsonBody<PutCollectionItem>>,
) -> AppResult<(StatusCode, Json<CollectionItem>)> {
    let req = body.map(|JsonBody(req)| req).unwrap_or_default();
    let note = req
        .note
        .map(|value| validate::optional("note", value, MAX_NOTE))
        .transpose()?;
    let entity = refs::entity(&state, &slug).await?;

    let mut tx = state.db.begin().await?;
    // Блокировка коллекции: параллельные добавления не превысят лимит и не займут одну позицию.
    owned_for_update(&mut *tx, id, &user).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM collection_items WHERE collection_id = $1 AND entity_id = $2)",
    )
    .bind(id)
    .bind(entity.id)
    .fetch_one(&mut *tx)
    .await?;

    let (row, status): (CollectionItemRow, _) = if exists {
        let row = sqlx::query_as(&format!(
            "UPDATE collection_items SET
                position = COALESCE($3, position),
                note = CASE WHEN $4 THEN $5 ELSE note END
             WHERE collection_id = $1 AND entity_id = $2 RETURNING {ITEM_COLUMNS}"
        ))
        .bind(id)
        .bind(entity.id)
        .bind(req.position)
        .bind(note.is_some())
        .bind(note.flatten())
        .fetch_one(&mut *tx)
        .await?;
        (row, StatusCode::OK)
    } else {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM collection_items WHERE collection_id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if count >= MAX_COLLECTION_ITEMS {
            return Err(AppError::BadRequest(format!(
                "collection already has {MAX_COLLECTION_ITEMS} items"
            )));
        }
        let row = sqlx::query_as(&format!(
            "INSERT INTO collection_items (collection_id, entity_id, position, note)
             VALUES ($1, $2, COALESCE($3, (SELECT COALESCE(max(position) + 1, 0)
                                           FROM collection_items WHERE collection_id = $1)), $4)
             RETURNING {ITEM_COLUMNS}"
        ))
        .bind(id)
        .bind(entity.id)
        .bind(req.position)
        .bind(note.flatten())
        .fetch_one(&mut *tx)
        .await
        .map_err(validate::missing_reference)?;
        (row, StatusCode::CREATED)
    };
    touch(&mut *tx, id).await?;
    tx.commit().await?;

    Ok((
        status,
        Json(CollectionItem {
            entity,
            position: row.position,
            note: row.note,
            added_at: row.added_at,
        }),
    ))
}

/// Убрать произведение из своей коллекции.
#[utoipa::path(
    delete, path = "/collections/{id}/items/{slug}", tag = "collections",
    security(("bearer" = [])),
    params(
        ("id" = Uuid, Path, description = "id коллекции"),
        ("slug" = String, Path, description = "slug сущности", example = "dune-2021"),
    ),
    responses(
        (status = 204, description = "Убрано"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужая публичная коллекция", body = ErrorBody),
        (status = 404, description = "Нет коллекции, сущности или её нет в коллекции", body = ErrorBody),
    )
)]
pub async fn delete_item(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, slug)): Path<(Uuid, String)>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    let mut tx = state.db.begin().await?;
    owned_for_update(&mut *tx, id, &user).await?;
    let deleted =
        sqlx::query("DELETE FROM collection_items WHERE collection_id = $1 AND entity_id = $2")
            .bind(id)
            .bind(entity.id)
            .execute(&mut *tx)
            .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    touch(&mut *tx, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Задать порядок всех произведений своей коллекции: позиции становятся 0, 1, 2, ...
#[utoipa::path(
    put, path = "/collections/{id}/items", tag = "collections",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    request_body = ReorderItems,
    responses(
        (status = 200, description = "Коллекция в новом порядке", body = CollectionDetail),
        (status = 400, description = "Список не совпадает с содержимым коллекции", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужая публичная коллекция", body = ErrorBody),
        (status = 404, description = "Нет или приватная чужая", body = ErrorBody),
    )
)]
pub async fn reorder(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<ReorderItems>,
) -> AppResult<Json<CollectionDetail>> {
    let mut tx = state.db.begin().await?;
    owned_for_update(&mut *tx, id, &user).await?;
    let current: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM collection_items WHERE collection_id = $1")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    // slug → id только среди того, что лежит в коллекции: чужие slug'и дадут 400 ниже.
    let by_slug: HashMap<String, Uuid> = refs::entities(&state, current.clone())
        .await?
        .into_values()
        .map(|entity| (entity.slug, entity.id))
        .collect();
    let order = new_order(&req.entities, &by_slug, current.len())?;

    sqlx::query(
        "UPDATE collection_items ci SET position = (v.n - 1)::int
         FROM unnest($2::uuid[]) WITH ORDINALITY AS v(entity_id, n)
         WHERE ci.collection_id = $1 AND ci.entity_id = v.entity_id",
    )
    .bind(id)
    .bind(&order)
    .execute(&mut *tx)
    .await?;
    touch(&mut *tx, id).await?;
    tx.commit().await?;

    let row = load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    Ok(Json(detail(&state, row).await?))
}

/// id в новом порядке. Список должен содержать каждое произведение коллекции ровно один раз.
fn new_order(
    slugs: &[String],
    by_slug: &HashMap<String, Uuid>,
    total: usize,
) -> AppResult<Vec<Uuid>> {
    let mismatch = || {
        AppError::BadRequest("entities must list every item of the collection exactly once".into())
    };
    let mut seen = HashSet::new();
    let order = slugs
        .iter()
        .map(|slug| by_slug.get(slug).copied().filter(|id| seen.insert(*id)))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(mismatch)?;
    if order.len() != total {
        return Err(mismatch());
    }
    Ok(order)
}

pub(crate) async fn load(db: impl PgExecutor<'_>, id: Uuid) -> AppResult<Option<CollectionRow>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {COLLECTION_COLUMNS} FROM collections c WHERE c.id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Коллекция пользователя `user`. Чужая приватная — 404 (её существование не раскрываем),
/// чужая публичная — 403.
async fn owned(db: impl PgExecutor<'_>, id: Uuid, user: &AuthUser) -> AppResult<()> {
    owner_check(db, id, user, "").await
}

/// То же, с блокировкой строки коллекции до конца транзакции.
async fn owned_for_update(db: impl PgExecutor<'_>, id: Uuid, user: &AuthUser) -> AppResult<()> {
    owner_check(db, id, user, "FOR UPDATE").await
}

async fn owner_check(
    db: impl PgExecutor<'_>,
    id: Uuid,
    user: &AuthUser,
    lock: &str,
) -> AppResult<()> {
    let row: Option<(Uuid, bool)> = sqlx::query_as(&format!(
        "SELECT user_id, is_public FROM collections WHERE id = $1 {lock}"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?;
    match row {
        Some((owner, _)) if owner == user.id => Ok(()),
        Some((_, true)) => Err(AppError::Forbidden),
        _ => Err(AppError::NotFound),
    }
}

/// Изменение содержимого — это изменение коллекции: `updated_at` обновит триггер.
async fn touch(db: impl PgExecutor<'_>, id: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE collections SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

async fn detail(state: &AppState, row: CollectionRow) -> AppResult<CollectionDetail> {
    let items: Vec<CollectionItemRow> = sqlx::query_as(&format!(
        "SELECT {ITEM_COLUMNS} FROM collection_items WHERE collection_id = $1
         ORDER BY position, added_at, entity_id"
    ))
    .bind(row.id)
    .fetch_all(&state.db)
    .await?;
    Ok(CollectionDetail {
        collection: refs::collection(state, row).await?,
        items: refs::items(state, items).await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorder_requires_every_item_once() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let by_slug = HashMap::from([("a".to_string(), a), ("b".to_string(), b)]);
        let slugs = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert_eq!(new_order(&slugs(&["b", "a"]), &by_slug, 2).unwrap(), [b, a]);
        assert!(new_order(&slugs(&["a"]), &by_slug, 2).is_err());
        assert!(new_order(&slugs(&["a", "a"]), &by_slug, 2).is_err());
        assert!(new_order(&slugs(&["a", "b", "c"]), &by_slug, 2).is_err());
        assert!(new_order(&slugs(&[]), &HashMap::new(), 0)
            .unwrap()
            .is_empty());
    }
}
