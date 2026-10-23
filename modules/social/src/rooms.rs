//! Комнаты чата со slug: системные, заведённые админом и свои комнаты пользователей.
//!
//! Вступление — как интерес: комната попадает в «мои чаты» (`GET /chats`), канал `room:<slug>`
//! подписывается сам (`interest.added` / `interest.removed` с `room_id`). Писать в комнату, как и в
//! остальные, может любой вошедший; сообщения и история — общие (`chat`, `/chat/room/{slug}/...`).
//!
//! Системная комната (`system`) — вкладка панели чата: участник в ней каждый пользователь (строк
//! в `chat_room_members` нет, см. [`chat::fill_rooms`]), её нельзя создать, покинуть, изменить и
//! удалить — заводится только миграцией.
//!
//! Свою комнату заводит любой вошедший (`POST /chat/rooms`): он становится `owner_id` и
//! участником, правит и удаляет её сам. У комнат админа `owner_id` пуст — их ведёт только админ.

use crate::chat::{custom_room, custom_room_id, CustomRoom, CUSTOM_COLUMNS};
use crate::events;
use crate::models::PageQuery;
use crate::validate::{self, MAX_REPORT_REASON, MAX_ROOM_DESCRIPTION, MAX_ROOM_TITLE};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AdminUser, AppError, AppResult, AppState, AuthUser};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Сколько своих комнат разрешено держать одновременно.
pub const MAX_OWNED_ROOMS: i64 = 5;
/// Сколько своих комнат разрешено завести за сутки.
pub const MAX_ROOMS_PER_DAY: i64 = 3;

/// Слаги, которые нельзя занять своей комнатой: системные и служебные адреса фронтенда.
/// Слаги уже заведённых комнат закрывает UNIQUE — перечислять их здесь не нужно.
const RESERVED_SLUGS: &[&str] = &[
    "general", "movies", "series", "books", "games", "admin", "api", "new", "search", "chat",
];

/// Порядок каталога комнат. Системные всегда первыми — это вкладки панели.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RoomSort {
    /// Живые сейчас: `messages_24h` убыв., при равенстве — свежее сообщение выше.
    #[default]
    Active,
    /// Больше участников — выше.
    Members,
    /// Новые — выше.
    New,
}

impl RoomSort {
    /// Хвост `ORDER BY` после системных. `messages_24h` — алиас из `CUSTOM_COLUMNS`.
    fn order_by(&self) -> &'static str {
        match self {
            Self::Active => "messages_24h DESC, r.last_message_at DESC NULLS LAST",
            Self::Members => "members_count DESC",
            Self::New => "r.created_at DESC",
        }
    }
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListRoomsQuery {
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
    /// `active` (по умолчанию), `members` или `new`. При `q` не действует: там порядок по
    /// близости к запросу.
    pub sort: Option<RoomSort>,
    /// Поиск по названию и описанию, до 100 символов. Пустой — как без него.
    pub q: Option<String>,
}

/// Длина поискового запроса по комнатам.
pub const MAX_ROOM_QUERY: usize = 100;

/// Запрос → шаблон `ILIKE`: `%`, `_` и `\` в нём — буквальные символы.
pub(crate) fn like_pattern(q: &str) -> String {
    let escaped = q
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateRoom {
    /// a-z, 0-9 и дефисы между ними, 1–80: адрес `room:<slug>`.
    #[schema(example = "cyberpunk-fans")]
    pub slug: String,
    /// 1–100 символов.
    #[schema(example = "Киберпанк")]
    pub title: String,
    /// До 500 символов.
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateRoom {
    /// 1–100 символов; не передан — не меняется.
    pub title: Option<String>,
    /// До 500 символов; пустая строка — убрать; не передан — не меняется.
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReportRoom {
    /// Что не так с комнатой, до 500 символов.
    #[schema(example = "Реклама")]
    pub reason: String,
}

/// Комната с жалобами — админский листинг.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReportedRoom {
    pub room: CustomRoom,
    pub reports_count: i64,
    pub last_report_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
struct ReportRow {
    room_id: Uuid,
    reports_count: i64,
    last_report_at: DateTime<Utc>,
}

async fn room(state: &AppState, id: Uuid) -> AppResult<CustomRoom> {
    custom_room(state, id).await
}

/// Строка комнаты для проверок прав: системная ли, чья.
#[derive(Debug, sqlx::FromRow)]
struct RoomOwner {
    id: Uuid,
    system: bool,
    owner_id: Option<Uuid>,
}

async fn room_owner(state: &AppState, slug: &str) -> AppResult<RoomOwner> {
    sqlx::query_as("SELECT id, system, owner_id FROM chat_rooms WHERE slug = $1")
        .bind(slug)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

impl RoomOwner {
    /// Комнату можно править и удалять: её владельцу или админу. Системную — никому.
    fn writable_by(&self, user: &AuthUser) -> AppResult<Uuid> {
        if self.system {
            return Err(AppError::Forbidden.with_code("room_is_system"));
        }
        let allowed = user.role >= shared::Role::Admin || self.owner_id == Some(user.id);
        if allowed {
            Ok(self.id)
        } else {
            Err(AppError::Forbidden)
        }
    }
}

/// `joined` для вошедшего: в какие из комнат он вступил (одним запросом). Гость — `null`.
/// В системной комнате состоят все.
async fn fill_joined(
    state: &AppState,
    viewer: Option<&AuthUser>,
    rooms: &mut [CustomRoom],
) -> AppResult<()> {
    let Some(user) = viewer else {
        return Ok(());
    };
    let ids: Vec<Uuid> = rooms.iter().map(|room| room.id).collect();
    let joined: Vec<Uuid> = sqlx::query_scalar(
        "SELECT room_id FROM chat_room_members WHERE user_id = $1 AND room_id = ANY($2)",
    )
    .bind(user.id)
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    let hidden: Vec<Uuid> = sqlx::query_scalar(
        "SELECT room_id FROM chat_room_hidden WHERE user_id = $1 AND room_id = ANY($2)",
    )
    .bind(user.id)
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    for room in rooms {
        room.joined = Some(room.system || joined.contains(&room.id));
        room.hidden = Some(hidden.contains(&room.id));
    }
    Ok(())
}

/// Комнаты: системные сверху, дальше — по `?sort`. Вошедшему — `joined` (вступил ли).
#[utoipa::path(
    get, operation_id = "list_chat_rooms", path = "/chat/rooms", tag = "chat",
    params(ListRoomsQuery),
    responses(
        (status = 200, description = "Страница комнат", body = Page<CustomRoom>),
        (status = 401, description = "Неверный токен", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Query(query): Query<ListRoomsQuery>,
) -> AppResult<Json<Page<CustomRoom>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let q = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    if q.is_some_and(|q| q.chars().count() > MAX_ROOM_QUERY) {
        return Err(
            AppError::BadRequest(format!("q must be at most {MAX_ROOM_QUERY} chars")).on_field("q"),
        );
    }
    let pattern = q.map(like_pattern);
    // Поиск по названию и описанию; без запроса фильтр пропускает всё. `{n}` — номер
    // параметра с шаблоном: в счёте он единственный, в выдаче идёт после limit и offset.
    let filter = |n: u8| {
        format!(
            "r.slug IS NOT NULL
             AND (${n}::text IS NULL OR r.title ILIKE ${n} OR r.description ILIKE ${n})"
        )
    };
    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM chat_rooms r WHERE {}",
        filter(1)
    ))
    .bind(&pattern)
    .fetch_one(&state.db)
    .await?;
    // С запросом порядок задаёт близость названия, иначе — выбранная сортировка.
    let order = match q {
        Some(_) => "similarity(r.title, $4) DESC".to_string(),
        None => format!(
            "r.system DESC, {}",
            query.sort.unwrap_or_default().order_by()
        ),
    };
    let mut items: Vec<CustomRoom> = sqlx::query_as(&format!(
        "SELECT {CUSTOM_COLUMNS} FROM chat_rooms r WHERE {}
         ORDER BY {order}, r.created_at, r.id LIMIT $1 OFFSET $2",
        filter(3)
    ))
    .bind(limit)
    .bind(offset)
    .bind(&pattern)
    .bind(q.unwrap_or_default())
    .fetch_all(&state.db)
    .await?;
    crate::chat::fill_rooms(&state, &mut items).await?;
    fill_joined(&state, viewer.as_ref(), &mut items).await?;
    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Комната по slug. Вошедшему — `joined` (вступил ли).
#[utoipa::path(
    get, operation_id = "get_chat_room", path = "/chat/rooms/{slug}", tag = "chat",
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    responses(
        (status = 200, description = "Комната", body = CustomRoom),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(slug): Path<String>,
) -> AppResult<Json<CustomRoom>> {
    let id = custom_room_id(&state.db, &slug).await?;
    let mut rooms = [room(&state, id).await?];
    fill_joined(&state, viewer.as_ref(), &mut rooms).await?;
    let [room] = rooms;
    Ok(Json(room))
}

/// Вступить в комнату → 204 (повторно — тоже 204): она в «моих чатах», канал подписан.
/// В системной все состоят и так — 204 без записи.
#[utoipa::path(
    put, operation_id = "join_chat_room", path = "/chat/rooms/{slug}/membership", tag = "chat",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    responses(
        (status = 204, description = "Участник"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn join(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let found = room_owner(&state, &slug).await?;
    if found.system {
        return Ok(StatusCode::NO_CONTENT);
    }
    let added = sqlx::query(
        "INSERT INTO chat_room_members (user_id, room_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(user.id)
    .bind(found.id)
    .execute(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    if added.rows_affected() > 0 {
        events::publish(
            &state,
            events::interest("interest.added", user.id, "room_id", found.id),
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Выйти из комнаты → 204 (даже если не был участником). Системную покинуть нельзя — 403.
#[utoipa::path(
    delete, operation_id = "leave_chat_room", path = "/chat/rooms/{slug}/membership", tag = "chat",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    responses(
        (status = 204, description = "Не участник"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Системную комнату нельзя покинуть", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn leave(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let found = room_owner(&state, &slug).await?;
    if found.system {
        return Err(AppError::Forbidden.with_code("room_is_system"));
    }
    let deleted = sqlx::query("DELETE FROM chat_room_members WHERE user_id = $1 AND room_id = $2")
        .bind(user.id)
        .bind(found.id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() > 0 {
        events::publish(
            &state,
            events::interest("interest.removed", user.id, "room_id", found.id),
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Убрать комнату из «Моих чатов» → 204 (повторно — тоже 204). Комната остаётся рабочей:
/// история, отправка и подписка на канал не меняются — иначе пропали бы упоминания.
/// Только для системных: из остальных можно просто выйти.
#[utoipa::path(
    put, operation_id = "hide_chat_room", path = "/chat/rooms/{slug}/hidden", tag = "chat",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "general")),
    responses(
        (status = 204, description = "Скрыта"),
        (status = 400, description = "Комната не системная — из неё можно выйти", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn hide(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let found = system_room(&state, &slug).await?;
    sqlx::query(
        "INSERT INTO chat_room_hidden (user_id, room_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(user.id)
    .bind(found)
    .execute(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Вернуть комнату в «Мои чаты» → 204 (даже если она не была скрыта).
#[utoipa::path(
    delete, operation_id = "unhide_chat_room", path = "/chat/rooms/{slug}/hidden", tag = "chat",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "general")),
    responses(
        (status = 204, description = "В списке"),
        (status = 400, description = "Комната не системная", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn unhide(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let found = system_room(&state, &slug).await?;
    sqlx::query("DELETE FROM chat_room_hidden WHERE user_id = $1 AND room_id = $2")
        .bind(user.id)
        .bind(found)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// id системной комнаты по slug. Несистемная — 400: её не скрывают, из неё выходят.
async fn system_room(state: &AppState, slug: &str) -> AppResult<Uuid> {
    let found = room_owner(state, slug).await?;
    if !found.system {
        return Err(AppError::BadRequest(
            "only system rooms can be hidden: leave the room instead".into(),
        ));
    }
    Ok(found.id)
}

/// Проверенные название, адрес и описание новой комнаты.
fn new_room(req: &CreateRoom, reserved: bool) -> AppResult<(String, String, Option<String>)> {
    let slug = validate::room_slug(&req.slug).map_err(|error| error.on_field("slug"))?;
    if reserved && RESERVED_SLUGS.contains(&slug.as_str()) {
        return Err(AppError::Conflict(format!("slug {slug} is reserved")).with_code("slug_taken"));
    }
    let title = validate::required("title", &req.title, MAX_ROOM_TITLE)?;
    let description =
        validate::optional("description", req.description.clone(), MAX_ROOM_DESCRIPTION)?;
    Ok((slug, title, description))
}

/// Запись комнаты; занятый slug → 409.
async fn insert_room(
    state: &AppState,
    slug: &str,
    title: &str,
    description: &Option<String>,
    created_by: Uuid,
    owner_id: Option<Uuid>,
) -> AppResult<Uuid> {
    sqlx::query_scalar(
        "INSERT INTO chat_rooms (slug, title, description, created_by, owner_id)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (slug) DO NOTHING RETURNING id",
    )
    .bind(slug)
    .bind(title)
    .bind(description)
    .bind(created_by)
    .bind(owner_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| {
        AppError::Conflict(format!("room {slug} already exists")).with_code("slug_taken")
    })
}

/// Лимиты владельца: всего живых комнат и заведённых за сутки. Исчерпан суточный — 429 с
/// `retry_after` до того, как самая старая из суточных выйдет из окна.
async fn check_limits(state: &AppState, user_id: Uuid) -> AppResult<()> {
    let (live, today, oldest): (i64, i64, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT count(*),
                count(*) FILTER (WHERE created_at > now() - interval '24 hours'),
                min(created_at) FILTER (WHERE created_at > now() - interval '24 hours')
         FROM chat_rooms WHERE owner_id = $1",
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;
    if today >= MAX_ROOMS_PER_DAY {
        let retry_after_secs = oldest
            .map(|at| (at + chrono::Duration::hours(24) - Utc::now()).num_seconds())
            .unwrap_or(0)
            .clamp(1, 24 * 3600) as u64;
        return Err(AppError::TooManyRequests { retry_after_secs });
    }
    if live >= MAX_OWNED_ROOMS {
        // Не 429: ждать бесполезно, нужно удалить свою комнату.
        return Err(AppError::Conflict(format!(
            "you already own {MAX_OWNED_ROOMS} chat rooms, delete one first"
        ))
        .with_code("room_limit"));
    }
    Ok(())
}

/// Завести свою комнату: автор становится владельцем и участником.
#[utoipa::path(
    post, operation_id = "create_chat_room", path = "/chat/rooms", tag = "chat",
    security(("bearer" = [])),
    request_body = CreateRoom,
    responses(
        (status = 201, description = "Создана", body = CustomRoom),
        (status = 400, description = "Неверный slug, название или описание", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 409, description = "slug занят или зарезервирован (`slug_taken`); уже 5 своих комнат (`room_limit`)", body = ErrorBody),
        (status = 429, description = "Больше 3 комнат в сутки (`Retry-After`)", body = ErrorBody),
    )
)]
pub async fn create_own(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(req): JsonBody<CreateRoom>,
) -> AppResult<(StatusCode, Json<CustomRoom>)> {
    let (slug, title, description) = new_room(&req, true)?;
    check_limits(&state, user.id).await?;
    let id = insert_room(&state, &slug, &title, &description, user.id, Some(user.id)).await?;
    sqlx::query("INSERT INTO chat_room_members (user_id, room_id) VALUES ($1, $2)")
        .bind(user.id)
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(validate::missing_reference)?;
    events::publish(
        &state,
        events::interest("interest.added", user.id, "room_id", id),
    );
    events::publish(&state, events::chat_room_created(id));
    Ok((StatusCode::CREATED, Json(room(&state, id).await?)))
}

/// Изменение названия или описания комнаты (slug не меняется: это адрес канала).
async fn apply_update(state: &AppState, id: Uuid, req: UpdateRoom) -> AppResult<Json<CustomRoom>> {
    let title = req
        .title
        .map(|title| validate::required("title", &title, MAX_ROOM_TITLE))
        .transpose()?;
    let change_description = req.description.is_some();
    let description = validate::optional("description", req.description, MAX_ROOM_DESCRIPTION)?;
    sqlx::query(
        "UPDATE chat_rooms SET title = coalesce($2, title),
             description = CASE WHEN $3 THEN $4 ELSE description END
         WHERE id = $1",
    )
    .bind(id)
    .bind(&title)
    .bind(change_description)
    .bind(&description)
    .execute(&state.db)
    .await?;
    events::publish(state, events::chat_room_changed("chat.room.updated", id));
    Ok(Json(room(state, id).await?))
}

async fn apply_delete(state: &AppState, id: Uuid) -> AppResult<StatusCode> {
    sqlx::query("DELETE FROM chat_rooms WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    events::publish(state, events::chat_room_changed("chat.room.deleted", id));
    Ok(StatusCode::NO_CONTENT)
}

/// Изменить свою комнату: владелец или админ.
#[utoipa::path(
    patch, operation_id = "update_chat_room", path = "/chat/rooms/{slug}", tag = "chat",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    request_body = UpdateRoom,
    responses(
        (status = 200, description = "Изменена", body = CustomRoom),
        (status = 400, description = "Неверное название или описание", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужая комната или системная (`room_is_system`)", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn update_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<UpdateRoom>,
) -> AppResult<Json<CustomRoom>> {
    let id = room_owner(&state, &slug).await?.writable_by(&user)?;
    apply_update(&state, id, req).await
}

/// Удалить свою комнату вместе с сообщениями и участниками → 204: владелец или админ.
#[utoipa::path(
    delete, operation_id = "delete_chat_room", path = "/chat/rooms/{slug}", tag = "chat",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужая комната или системная (`room_is_system`)", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn delete_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let id = room_owner(&state, &slug).await?.writable_by(&user)?;
    apply_delete(&state, id).await
}

/// Пожаловаться на комнату → 204. Повторная жалоба того же пользователя обновляет причину,
/// дубля не создаёт.
#[utoipa::path(
    post, operation_id = "report_chat_room", path = "/chat/rooms/{slug}/report", tag = "chat",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    request_body = ReportRoom,
    responses(
        (status = 204, description = "Жалоба принята"),
        (status = 400, description = "Пустая или слишком длинная причина", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "На системную комнату жаловаться нечего", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn report(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<ReportRoom>,
) -> AppResult<StatusCode> {
    let found = room_owner(&state, &slug).await?;
    if found.system {
        return Err(AppError::Forbidden.with_code("room_is_system"));
    }
    let reason = validate::required("reason", &req.reason, MAX_REPORT_REASON)?;
    sqlx::query(
        "INSERT INTO chat_room_reports (room_id, user_id, reason) VALUES ($1, $2, $3)
         ON CONFLICT (room_id, user_id) DO UPDATE SET reason = excluded.reason,
             created_at = now()",
    )
    .bind(found.id)
    .bind(user.id)
    .bind(&reason)
    .execute(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Комнаты с жалобами: больше жалоб — выше.
#[utoipa::path(
    get, operation_id = "admin_list_reported_chat_rooms", path = "/admin/chat/rooms/reports",
    tag = "social-admin",
    security(("bearer" = [])),
    params(PageQuery),
    responses(
        (status = 200, description = "Страница комнат с жалобами", body = Page<ReportedRoom>),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn list_reported(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<ReportedRoom>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let total: i64 = sqlx::query_scalar("SELECT count(DISTINCT room_id) FROM chat_room_reports")
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<ReportRow> = sqlx::query_as(
        "SELECT room_id, count(*) AS reports_count, max(created_at) AS last_report_at
         FROM chat_room_reports GROUP BY room_id
         ORDER BY count(*) DESC, max(created_at) DESC, room_id LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|row| row.room_id).collect();
    let mut found: Vec<CustomRoom> = sqlx::query_as(&format!(
        "SELECT {CUSTOM_COLUMNS} FROM chat_rooms r WHERE r.id = ANY($1)"
    ))
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    crate::chat::fill_rooms(&state, &mut found).await?;
    let mut by_id: std::collections::HashMap<Uuid, CustomRoom> =
        found.into_iter().map(|room| (room.id, room)).collect();
    let items = rows
        .into_iter()
        .filter_map(|row| {
            Some(ReportedRoom {
                room: by_id.remove(&row.room_id)?,
                reports_count: row.reports_count,
                last_report_at: row.last_report_at,
            })
        })
        .collect();
    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Завести комнату от имени площадки: `owner_id` пуст, правит её только админ.
#[utoipa::path(
    post, operation_id = "admin_create_chat_room", path = "/admin/chat/rooms", tag = "social-admin",
    security(("bearer" = [])),
    request_body = CreateRoom,
    responses(
        (status = 201, description = "Создана", body = CustomRoom),
        (status = 400, description = "Неверный slug, название или описание", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "slug занят", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(req): JsonBody<CreateRoom>,
) -> AppResult<(StatusCode, Json<CustomRoom>)> {
    // Админу зарезервированные слаги доступны: системные всё равно закрыты UNIQUE.
    let (slug, title, description) = new_room(&req, false)?;
    let id = insert_room(&state, &slug, &title, &description, admin.id, None).await?;
    events::publish(&state, events::chat_room_created(id));
    Ok((StatusCode::CREATED, Json(room(&state, id).await?)))
}

/// Изменить название или описание любой несистемной комнаты (slug не меняется).
#[utoipa::path(
    patch, operation_id = "admin_update_chat_room", path = "/admin/chat/rooms/{slug}", tag = "social-admin",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    request_body = UpdateRoom,
    responses(
        (status = 200, description = "Изменена", body = CustomRoom),
        (status = 400, description = "Неверное название или описание", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin или комната системная (`room_is_system`)", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<UpdateRoom>,
) -> AppResult<Json<CustomRoom>> {
    let id = room_owner(&state, &slug).await?.writable_by(&admin)?;
    apply_update(&state, id, req).await
}

/// Удалить любую несистемную комнату вместе с сообщениями и участниками → 204.
#[utoipa::path(
    delete, operation_id = "admin_delete_chat_room", path = "/admin/chat/rooms/{slug}", tag = "social-admin",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin или комната системная (`room_is_system`)", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let id = room_owner(&state, &slug).await?.writable_by(&admin)?;
    apply_delete(&state, id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_slugs_are_rejected_for_users_and_allowed_for_admin() {
        let req = CreateRoom {
            slug: "general".into(),
            title: "Свой общий".into(),
            description: None,
        };
        assert!(new_room(&req, true).is_err());
        assert!(new_room(&req, false).is_ok());
    }

    #[test]
    fn slug_and_title_are_validated() {
        let bad = CreateRoom {
            slug: "Общий Чат".into(),
            title: "Чат".into(),
            description: None,
        };
        assert!(new_room(&bad, true).is_err());
        let empty_title = CreateRoom {
            slug: "cyberpunk-fans".into(),
            title: "   ".into(),
            description: None,
        };
        assert!(new_room(&empty_title, true).is_err());
        let ok = CreateRoom {
            slug: "cyberpunk-fans".into(),
            title: "  Киберпанк ".into(),
            description: Some("  ".into()),
        };
        let (slug, title, description) = new_room(&ok, true).unwrap();
        assert_eq!(
            (slug.as_str(), title.as_str()),
            ("cyberpunk-fans", "Киберпанк")
        );
        assert_eq!(description, None);
    }
}
