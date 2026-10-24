//! Фото профиля: загрузка файла, раздача и удаление.
//!
//! **Файл не доверяем** — как и вложениям чата: присланное декодируем ([`image`]), ограничиваем
//! сторону [`MAX_SIDE`] и кодируем в WebP. Так подтверждается, что это картинка, пропадают EXIF
//! и GPS, а на диск ложится файл в десятки раз меньше присланного. Исходник не сохраняется.
//!
//! Ссылка на готовый файл кладётся в `users.avatar_url` — тот же столбец, что у аватара от
//! провайдера. Поэтому `UserRef` и все модули, которые его показывают, не меняются совсем.
//! Сам файл называется `avatar_id`: новая загрузка — новый id, старый файл стирается.

use crate::models::{UserView, USER_COLUMNS};
use axum::extract::{Multipart, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::Path;
use shared::{AppError, AppResult, AppState, AuthUser};
use uuid::Uuid;

/// Предел на вход. После пережатия на диск ложится обычно 20–60 КБ.
pub const MAX_UPLOAD_BYTES: usize = 2 * 1024 * 1024;
/// Сторона готового аватара: больше нигде не показывается.
pub const MAX_SIDE: u32 = 512;
/// Защита от «архивных бомб»: 30000×30000 весит килобайты, а в памяти — гигабайты.
pub const MAX_SOURCE_SIDE: u32 = 10_000;
/// Не больше стольких загрузок за [`RATE_WINDOW_SECS`] на пользователя.
pub const RATE_UPLOADS: i64 = 10;
pub const RATE_WINDOW_SECS: u64 = 600;

/// Ссылка, которая попадает в `users.avatar_url`.
fn url_of(id: Uuid) -> String {
    format!("/api/v1/auth/avatars/{id}")
}

/// Путь файла: `<dir>/<первые 2 символа id>/<id>.webp` — чтобы в одной директории не оказались
/// сотни тысяч файлов.
fn path_of(state: &AppState, id: Uuid) -> std::path::PathBuf {
    let name = id.simple().to_string();
    std::path::Path::new(&state.config.avatars_dir)
        .join(&name[..2])
        .join(format!("{name}.webp"))
}

/// Картинка из `multipart`: первое файловое поле. Имя файла от клиента не используется нигде.
async fn read_upload(mut form: Multipart) -> AppResult<Vec<u8>> {
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("malformed upload: {e}")))?
    {
        if field.file_name().is_none() {
            continue;
        }
        let data = field
            .bytes()
            .await
            .map_err(|e| AppError::BadRequest(format!("upload failed: {e}")))?;
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

/// Присланные байты → наш WebP. Размеры читаем из заголовка — до декодирования.
fn reencode(data: &[u8]) -> AppResult<(Vec<u8>, u32, u32)> {
    let bad = |message: &str| AppError::BadRequest(message.into()).with_code("not_an_image");
    let probe = |data: &[u8]| {
        image::ImageReader::new(std::io::Cursor::new(data.to_vec()))
            .with_guessed_format()
            .map_err(|_| bad("unsupported file"))
    };
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
    Ok((out, width, height))
}

/// Загрузить фото профиля: заменяет прежнее (и загруженное, и адрес от провайдера).
#[utoipa::path(
    post, operation_id = "upload_avatar", path = "/me/avatar", tag = "profile",
    security(("bearer" = [])),
    request_body(content = String, description = "multipart/form-data с одним файловым полем", content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Профиль с новым `avatar_url`", body = UserView),
        (status = 400, description = "Не картинка (`not_an_image`), больше 2 МБ (`file_too_large`), пустой запрос", body = ErrorBody),
        (status = 401, description = "Нет токена или аккаунт заблокирован", body = ErrorBody),
        (status = 429, description = "Больше 10 загрузок за 10 минут (`Retry-After`)", body = ErrorBody),
    )
)]
pub async fn upload(
    State(state): State<AppState>,
    auth: AuthUser,
    form: Multipart,
) -> AppResult<Json<UserView>> {
    let recent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM avatar_uploads
         WHERE user_id = $1 AND created_at > now() - make_interval(secs => $2)",
    )
    .bind(auth.id)
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
    let (webp, _, _) = tokio::task::spawn_blocking(move || reencode(&data))
        .await
        .map_err(|e| AppError::Internal(format!("reencode join: {e}")))??;

    let mut tx = state.db.begin().await?;
    let previous: Option<(Option<Uuid>,)> =
        sqlx::query_as("SELECT avatar_id FROM users WHERE id = $1 AND is_active FOR UPDATE")
            .bind(auth.id)
            .fetch_optional(&mut *tx)
            .await?;
    // Аккаунт заблокировали между проверкой токена и записью.
    let (previous,) = previous.ok_or(AppError::Unauthorized)?;
    // id файла даёт база (uuidv7) — он же становится именем файла на диске.
    let id: Uuid =
        sqlx::query_scalar("INSERT INTO avatar_uploads (user_id) VALUES ($1) RETURNING id")
            .bind(auth.id)
            .fetch_one(&mut *tx)
            .await?;

    // Файл пишем до коммита: не записался — транзакция откатится и ссылки на него не будет.
    let path = path_of(&state, id);
    let write = async {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(&path, &webp).await
    };
    write
        .await
        .map_err(|e| AppError::Internal(format!("store avatar: {e}")))?;

    let user: UserView = sqlx::query_as(&format!(
        "UPDATE users SET avatar_id = $2, avatar_url = $3 WHERE id = $1 RETURNING {USER_COLUMNS}"
    ))
    .bind(auth.id)
    .bind(id)
    .bind(url_of(id))
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    remove_file(&state, previous).await;
    Ok(Json(user))
}

/// Убрать фото профиля: и загруженное, и адрес от провайдера.
#[utoipa::path(
    delete, operation_id = "delete_avatar", path = "/me/avatar", tag = "profile",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Профиль без аватара", body = UserView),
        (status = 401, description = "Нет токена или аккаунт заблокирован", body = ErrorBody),
    )
)]
pub async fn delete(State(state): State<AppState>, auth: AuthUser) -> AppResult<Json<UserView>> {
    let mut tx = state.db.begin().await?;
    let previous: Option<(Option<Uuid>,)> =
        sqlx::query_as("SELECT avatar_id FROM users WHERE id = $1 AND is_active FOR UPDATE")
            .bind(auth.id)
            .fetch_optional(&mut *tx)
            .await?;
    let (previous,) = previous.ok_or(AppError::Unauthorized)?;
    let user: UserView = sqlx::query_as(&format!(
        "UPDATE users SET avatar_id = NULL, avatar_url = NULL WHERE id = $1
         RETURNING {USER_COLUMNS}"
    ))
    .bind(auth.id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    remove_file(&state, previous).await;
    Ok(Json(user))
}

/// Отдать фото. Аватар виден всем: он стоит рядом с каждым сообщением и рецензией.
#[utoipa::path(
    get, operation_id = "get_avatar", path = "/avatars/{id}", tag = "profile",
    params(("id" = Uuid, Path, description = "id файла из `avatar_url`")),
    responses(
        (status = 200, description = "Картинка WebP", content_type = "image/webp"),
        (status = 404, description = "Нет такого файла", body = ErrorBody),
    )
)]
pub async fn get(State(state): State<AppState>, Path(id): Path<Uuid>) -> AppResult<Response> {
    let bytes = tokio::fs::read(path_of(&state, id))
        .await
        .map_err(|_| AppError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/webp"),
            // Браузер не должен угадывать тип и предлагать «скачать как .html».
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_DISPOSITION, "inline"),
            // Файл неизменяем: id новый на каждую загрузку.
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response())
}

/// Стереть файл прежнего аватара. Неудачу не считаем фатальной: ссылки на него уже нет.
pub(crate) async fn remove_file(state: &AppState, id: Option<Uuid>) {
    if let Some(id) = id {
        let _ = tokio::fs::remove_file(path_of(state, id)).await;
    }
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
        let (webp, width, height) = reencode(&png(64, 48)).unwrap();
        assert_eq!((width, height), (64, 48));
        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
    }

    #[test]
    fn downscales_long_side() {
        let (_, width, height) = reencode(&png(MAX_SIDE * 4, MAX_SIDE * 2)).unwrap();
        assert_eq!(width, MAX_SIDE);
        assert_eq!(height, MAX_SIDE / 2);
    }

    #[test]
    fn rejects_everything_that_is_not_an_image() {
        for data in [b"not an image at all".to_vec(), vec![0u8; 64]] {
            assert_eq!(reencode(&data).unwrap_err().code(), "not_an_image");
        }
    }
}
