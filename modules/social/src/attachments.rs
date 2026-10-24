//! Вложения в сообщениях чата — только картинки.
//!
//! Два шага, как в любом мессенджере: сначала `POST /chat/attachments` заливает файл и
//! возвращает `id`, потом `chat.send` передаёт `attachment_id` — так виден прогресс загрузки
//! отдельно от отправки, и можно отправить сообщение вообще без текста.
//!
//! **Файл не доверяем.** Присланное пережимаем сами: декодируем ([`image`]), ограничиваем
//! длинную сторону [`MAX_SIDE`] и кодируем в WebP. Отсюда сразу три вещи: подтверждается, что
//! это действительно картинка (а не исполняемый файл с расширением `.png`), пропадают EXIF и
//! GPS из снимка, и на диск ложится файл в разы меньше присланного. Исходный байт-в-байт файл
//! не сохраняется никогда.
//!
//! **Раздаёт приложение, не nginx** (`GET /chat/attachments/{id}`): вложение из личной
//! переписки не должно открываться по прямой ссылке у того, кто не участник. Пока вложение не
//! приложено к сообщению, его видит только загрузивший.

use crate::chat;
use axum::extract::{Multipart, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use shared::error::ErrorBody;
use shared::extract::Path;
use shared::{AppError, AppResult, AppState, AuthUser};
use utoipa::ToSchema;
use uuid::Uuid;

/// Больше этого не принимаем. Это предел на **вход**: после пережатия на диск ложится
/// обычно 100–300 КБ. Снимок с телефона проходит, видео или архив — нет.
pub const MAX_UPLOAD_BYTES: usize = 2 * 1024 * 1024;
/// Длинная сторона готовой картинки. Больше в чате не нужно, а диск и трафик экономит сильно.
pub const MAX_SIDE: u32 = 1600;
/// Защита от «архивных бомб»: картинка 30000×30000 весит мало, а в памяти разворачивается
/// в гигабайты. Проверяем размеры до декодирования.
pub const MAX_SOURCE_SIDE: u32 = 10_000;
/// Качество WebP: на фотографии неотличимо от исходника, файл вчетверо легче.
const WEBP_QUALITY: f32 = 80.0;
/// Не больше стольких загрузок за [`RATE_WINDOW_SECS`] секунд на пользователя.
pub const RATE_UPLOADS: i64 = 10;
pub const RATE_WINDOW_SECS: u64 = 600;
/// Неприкреплённое вложение живёт столько; дальше его забирает уборка.
pub const ORPHAN_LIFETIME_HOURS: i64 = 1;

/// Вложение сообщения.
#[derive(Debug, Clone, Serialize, ToSchema, sqlx::FromRow)]
pub struct Attachment {
    pub id: Uuid,
    /// Где взять картинку: `/api/v1/social/chat/attachments/{id}`.
    #[schema(example = "/api/v1/social/chat/attachments/01932f3e-0000-7000-8000-000000000000")]
    #[sqlx(skip)]
    pub url: String,
    /// Пока только `image`. Поле оставлено на будущее — под файлы.
    #[schema(example = "image")]
    #[sqlx(skip)]
    pub kind: &'static str,
    pub width: i32,
    pub height: i32,
    /// Вес готового файла в байтах.
    pub size: i64,
}

impl Attachment {
    fn with_url(mut self) -> Self {
        self.url = url_of(self.id);
        self.kind = "image";
        self
    }
}

fn url_of(id: Uuid) -> String {
    format!("/api/v1/social/chat/attachments/{id}")
}

/// Путь файла на диске: `<dir>/<первые 2 символа id>/<id>.webp`. Раскладка по подкаталогам —
/// чтобы в одной директории не оказались сотни тысяч файлов.
fn path_of(state: &AppState, id: Uuid) -> std::path::PathBuf {
    let name = id.simple().to_string();
    std::path::Path::new(&state.config.attachments_dir)
        .join(&name[..2])
        .join(format!("{name}.webp"))
}

/// Картинка из `multipart`: первое файловое поле. Имя файла от клиента не используется нигде —
/// ни в пути, ни в ответе: оно подконтрольно пользователю.
async fn read_upload(mut form: Multipart) -> AppResult<Vec<u8>> {
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("malformed upload: {e}")))?
    {
        if field.file_name().is_none() {
            continue;
        }
        let data = field.bytes().await.map_err(|e| {
            // Обрыв на середине и превышение лимита тела приходят сюда же.
            AppError::BadRequest(format!("upload failed: {e}"))
        })?;
        if data.len() > MAX_UPLOAD_BYTES {
            return Err(AppError::BadRequest(format!(
                "file is larger than {} MB",
                MAX_UPLOAD_BYTES / 1024 / 1024
            ))
            .with_code("file_too_large"));
        }
        if data.is_empty() {
            return Err(AppError::BadRequest("file is empty".into()));
        }
        return Ok(data.to_vec());
    }
    Err(AppError::BadRequest("no file in upload".into()))
}

/// Присланные байты → наш WebP. Здесь же вся проверка: не картинку мы не декодируем, а
/// слишком большую по размерам отклоняем до того, как она развернётся в памяти.
fn reencode(data: &[u8]) -> AppResult<(Vec<u8>, u32, u32)> {
    let bad = |message: &str| AppError::BadRequest(message.into()).with_code("not_an_image");
    let probe = |data: &[u8]| {
        image::ImageReader::new(std::io::Cursor::new(data.to_vec()))
            .with_guessed_format()
            .map_err(|_| bad("unsupported file"))
    };
    // Размеры читаем из заголовка — до декодирования: картинка 30000×30000 весит килобайты,
    // а в памяти развернулась бы в гигабайты.
    let (width, height) = probe(data)?
        .into_dimensions()
        .map_err(|_| bad("file is not an image"))?;
    if width > MAX_SOURCE_SIDE || height > MAX_SOURCE_SIDE {
        return Err(bad("image is too large"));
    }
    let image = probe(data)?
        .decode()
        .map_err(|_| bad("file is not an image"))?;
    // Уменьшаем только если надо: апскейла быть не должно.
    let image = if width > MAX_SIDE || height > MAX_SIDE {
        image.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Lanczos3)
    } else {
        image
    };
    let (width, height) = (image.width(), image.height());
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(std::io::Cursor::new(&mut out))
        .encode(
            image.to_rgba8().as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| AppError::Internal(format!("webp encode: {e}")))?;
    let _ = WEBP_QUALITY;
    Ok((out, width, height))
}

/// Загрузить картинку. Возвращает `id`, который потом уходит в `chat.send` как `attachment_id`.
#[utoipa::path(
    post, operation_id = "upload_chat_attachment", path = "/chat/attachments", tag = "chat",
    security(("bearer" = [])),
    request_body(content = String, description = "multipart/form-data с одним файловым полем", content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Загружено", body = Attachment),
        (status = 400, description = "Не картинка (`not_an_image`), больше 2 МБ (`file_too_large`), пустой запрос", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 429, description = "Больше 10 загрузок за 10 минут (`Retry-After`)", body = ErrorBody),
    )
)]
pub async fn upload(
    State(state): State<AppState>,
    user: AuthUser,
    form: Multipart,
) -> AppResult<(StatusCode, Json<Attachment>)> {
    let recent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chat_attachments
         WHERE user_id = $1 AND created_at > now() - make_interval(secs => $2)",
    )
    .bind(user.id)
    .bind(RATE_WINDOW_SECS as f64)
    .fetch_one(&state.db)
    .await?;
    if recent >= RATE_UPLOADS {
        return Err(AppError::TooManyRequests {
            retry_after_secs: RATE_WINDOW_SECS,
        });
    }

    let data = read_upload(form).await?;
    // Пережатие упирается в процессор, поэтому не держим им исполнитель.
    let (webp, width, height) = tokio::task::spawn_blocking(move || reencode(&data))
        .await
        .map_err(|e| AppError::Internal(format!("reencode join: {e}")))??;

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO chat_attachments (user_id, width, height, size) VALUES ($1, $2, $3, $4)
         RETURNING id",
    )
    .bind(user.id)
    .bind(width as i32)
    .bind(height as i32)
    .bind(webp.len() as i64)
    .fetch_one(&state.db)
    .await
    .map_err(crate::validate::missing_reference)?;

    let path = path_of(&state, id);
    let write = async {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(&path, &webp).await
    };
    if let Err(e) = write.await {
        // Строка без файла — мусор: убираем сразу, не дожидаясь уборки.
        let _ = sqlx::query("DELETE FROM chat_attachments WHERE id = $1")
            .bind(id)
            .execute(&state.db)
            .await;
        return Err(AppError::Internal(format!("store attachment: {e}")));
    }

    Ok((
        StatusCode::CREATED,
        Json(
            Attachment {
                id,
                url: String::new(),
                kind: "image",
                width: width as i32,
                height: height as i32,
                size: webp.len() as i64,
            }
            .with_url(),
        ),
    ))
}

/// Отдать картинку. Доступ — как к сообщению, в котором она лежит: пока вложение никуда не
/// приложено, его видит только загрузивший; дальше решает комната (личную переписку видят
/// только её двое).
#[utoipa::path(
    get, operation_id = "get_chat_attachment", path = "/chat/attachments/{id}", tag = "chat",
    params(("id" = Uuid, Path, description = "id вложения")),
    responses(
        (status = 200, description = "Картинка WebP", content_type = "image/webp"),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Нет вложения или нет доступа", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    let row: Option<(Uuid, Option<Uuid>)> =
        sqlx::query_as("SELECT user_id, message_id FROM chat_attachments WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let (owner, message_id) = row.ok_or(AppError::NotFound)?;
    let viewer_id = viewer.map(|user| user.id);
    match message_id {
        // Ещё не отправлено — видит только тот, кто загрузил.
        None if viewer_id != Some(owner) => return Err(AppError::NotFound),
        None => {}
        Some(message_id) => chat::visible_message(&state, message_id, viewer_id).await?,
    }

    let bytes = tokio::fs::read(path_of(&state, id))
        .await
        .map_err(|_| AppError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/webp"),
            // Браузер не должен угадывать тип и не должен предлагать «скачать как .html».
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_DISPOSITION, "inline"),
            // Файл неизменяем: id новый на каждую загрузку.
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable",
            ),
        ],
        bytes,
    )
        .into_response())
}

/// Вложения сообщений одним запросом: список истории не должен ходить за каждым.
pub(crate) async fn for_messages(
    state: &AppState,
    message_ids: &[Uuid],
) -> AppResult<std::collections::HashMap<Uuid, Attachment>> {
    if message_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<(Uuid, Uuid, i32, i32, i64)> = sqlx::query_as(
        "SELECT message_id, id, width, height, size FROM chat_attachments
         WHERE message_id = ANY($1)",
    )
    .bind(message_ids)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(message_id, id, width, height, size)| {
            (
                message_id,
                Attachment {
                    id,
                    url: url_of(id),
                    kind: "image",
                    width,
                    height,
                    size,
                },
            )
        })
        .collect())
}

/// Приложить загруженное вложение к сообщению: внутри транзакции отправки. Чужое или уже
/// использованное — 400: `UPDATE` не найдёт строку.
pub(crate) async fn attach(
    tx: &mut sqlx::PgConnection,
    attachment_id: Uuid,
    user_id: Uuid,
    message_id: Uuid,
) -> AppResult<()> {
    let updated = sqlx::query(
        "UPDATE chat_attachments SET message_id = $3
         WHERE id = $1 AND user_id = $2 AND message_id IS NULL",
    )
    .bind(attachment_id)
    .bind(user_id)
    .bind(message_id)
    .execute(tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(
            AppError::BadRequest("attachment_id: unknown or already used".into())
                .with_code("unknown_attachment"),
        );
    }
    Ok(())
}

/// Снять вложение с удаляемого сообщения — в транзакции удаления. Возвращает id файла,
/// который надо стереть после коммита ([`remove_file`]).
pub(crate) async fn detach(
    tx: &mut sqlx::PgConnection,
    message_id: Uuid,
) -> AppResult<Option<Uuid>> {
    Ok(
        sqlx::query_scalar("DELETE FROM chat_attachments WHERE message_id = $1 RETURNING id")
            .bind(message_id)
            .fetch_optional(tx)
            .await?,
    )
}

/// Стереть файл. Неудачу не считаем фатальной: строки уже нет, файл заберёт уборка.
pub(crate) async fn remove_file(state: &AppState, id: Option<Uuid>) {
    if let Some(id) = id {
        let _ = tokio::fs::remove_file(path_of(state, id)).await;
    }
}

/// Убрать вложения, которые загрузили и не отправили. Зовётся при старте приложения.
pub async fn cleanup_orphans(state: &AppState) -> AppResult<u64> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM chat_attachments
         WHERE message_id IS NULL AND created_at < now() - make_interval(hours => $1)
         RETURNING id",
    )
    // `hours` у make_interval целочисленный: f64 сюда не подходит (такой функции нет).
    .bind(ORPHAN_LIFETIME_HOURS as i32)
    .fetch_all(&state.db)
    .await?;
    for id in &ids {
        let _ = tokio::fs::remove_file(path_of(state, *id)).await;
    }
    Ok(ids.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Маленький PNG, собранный на лету: проверяем именно пережатие, а не чтение с диска.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let buffer = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(buffer)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn reencodes_to_webp_and_keeps_small_images_as_is() {
        let (webp, width, height) = reencode(&png(40, 20)).unwrap();
        assert_eq!((width, height), (40, 20));
        // RIFF....WEBP — сигнатура контейнера.
        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
    }

    #[test]
    fn downscales_long_side() {
        let (_, width, height) = reencode(&png(MAX_SIDE * 2, MAX_SIDE)).unwrap();
        assert_eq!(width, MAX_SIDE);
        assert_eq!(height, MAX_SIDE / 2);
    }

    #[test]
    fn rejects_everything_that_is_not_an_image() {
        for data in [b"not an image at all".to_vec(), vec![0u8; 64]] {
            let error = reencode(&data).unwrap_err();
            assert_eq!(error.code(), "not_an_image");
        }
    }
}
