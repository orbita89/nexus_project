//! Замена обложки книги файлом: тот же путь на диске, тот же адрес.
//!
//! Обложки книг лежат в `BOOK_COVERS_DIR/<пачка>/<номер>.avif` и раздаются gateway по
//! `/covers/books/<пачка>/<номер>.avif` (`books::cassandra`). Новый файл ложится **на место
//! старого**: через временный файл в той же папке и `rename` — gateway не отдаст недописанную
//! картинку, а старый файл исчезает в момент подмены (копии не остаётся). `cover_url` не меняется;
//! браузеры подхватят новый файл, когда истечёт `Cache-Control` gateway (сутки).
//!
//! Куда кладётся файл:
//! 1. у книги уже есть обложка в `/covers/books/…` — ровно по этому пути;
//! 2. нет — `<пачка из metadata.file.archive>/<external_ids.cassandra>.avif`, как у импорта;
//! 3. книга не из библиотеки — `manual/<id сущности>.avif`.
//!
//! Файл не перекодируется: принимается только AVIF (бокс `ftyp` с брендом `avif`/`avis`),
//! размеры читаются из `ispe` и пишутся в `cover_width`/`cover_height`.
//!
//! Ответ — сразу после записи файла и строки; публикация (индекс книг, ISR) идёт в фоне.

use crate::books::cassandra::{avif_size, safe_cover_path};
use crate::entities::detail_by_id;
use crate::jobs::Reporter;
use crate::models::{EntityDetail, EntityKind};
use crate::publish::{self, Touched};
use axum::extract::{Multipart, State};
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::Path;
use shared::{AdminUser, AppError, AppResult, AppState};
use std::path::PathBuf;
use uuid::Uuid;

/// Предел файла обложки.
pub const MAX_COVER_BYTES: usize = 10 * 1024 * 1024;
/// Префикс адреса обложек книг на gateway.
const URL_PREFIX: &str = "/covers/books/";

/// Путь обложки относительно `BOOK_COVERS_DIR` (`<папка>/<файл>.avif`).
pub fn cover_path(
    id: Uuid,
    cover_url: Option<&str>,
    cassandra: Option<i64>,
    archive: Option<&str>,
) -> String {
    let existing = cover_url
        .and_then(|url| url.split_once(URL_PREFIX).map(|(_, rest)| rest))
        .map(|rest| rest.split(['?', '#']).next().unwrap_or(rest))
        .filter(|rest| rest.ends_with(".avif") && safe_cover_path(rest));
    if let Some(rest) = existing {
        return rest.to_string();
    }
    let batch = archive.map(|a| a.strip_suffix(".arx").unwrap_or(a));
    if let (Some(number), Some(batch)) = (cassandra, batch) {
        let path = format!("{batch}/{number}.avif");
        if safe_cover_path(&path) {
            return path;
        }
    }
    format!("manual/{}.avif", id.simple())
}

/// Это AVIF: `ftyp` в начале и бренд `avif`/`avis` среди брендов.
pub fn is_avif(data: &[u8]) -> bool {
    let Some(size) = data.get(0..4) else {
        return false;
    };
    let size = u32::from_be_bytes([size[0], size[1], size[2], size[3]]) as usize;
    if data.get(4..8) != Some(b"ftyp") || size < 16 {
        return false;
    }
    let Some(boxed) = data.get(8..size.min(data.len())) else {
        return false;
    };
    boxed
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .any(|(i, brand)| i != 1 && (brand == b"avif" || brand == b"avis"))
}

/// Первое файловое поле формы.
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
        if data.len() > MAX_COVER_BYTES {
            return Err(AppError::BadRequest("file is larger than 10 MB".into())
                .with_code("file_too_large"));
        }
        if data.is_empty() {
            return Err(AppError::BadRequest("file is empty".into()));
        }
        return Ok(data.to_vec());
    }
    Err(AppError::BadRequest("no file in upload".into()))
}

/// Заменить обложку книги файлом AVIF: тот же путь и тот же `cover_url` (или путь по правилам
/// выше, если обложки не было), старый файл заменяется.
#[utoipa::path(
    post, operation_id = "upload_book_cover", path = "/admin/entities/{id}/cover", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id книги")),
    request_body(content = String, description = "multipart/form-data с одним файловым полем (AVIF)", content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Карточка с обложкой", body = EntityDetail),
        (status = 400, description = "Не AVIF (`not_avif`), больше 10 МБ (`file_too_large`), не книга, пустой запрос", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет сущности", body = ErrorBody),
        (status = 503, description = "BOOK_COVERS_DIR не задан", body = ErrorBody),
    )
)]
pub async fn upload(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    form: Multipart,
) -> AppResult<Json<EntityDetail>> {
    let dir = state
        .config
        .book_covers_dir
        .clone()
        .map(PathBuf::from)
        .ok_or_else(|| AppError::Unavailable("BOOK_COVERS_DIR is not set".into()))?;
    // Тип, обложка, номер в библиотеке, архив книги.
    type Row = (EntityKind, Option<String>, Option<i64>, Option<String>);
    let row: Option<Row> = sqlx::query_as(
        "SELECT kind, cover_url, (external_ids ->> 'cassandra')::bigint,
                metadata -> 'file' ->> 'archive'
         FROM entities WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let (kind, cover_url, cassandra, archive) = row.ok_or(AppError::NotFound)?;
    if kind != EntityKind::Book {
        return Err(AppError::BadRequest("only books have file covers".into()));
    }
    let data = read_upload(form).await?;
    if !is_avif(&data) {
        return Err(AppError::BadRequest("cover must be an AVIF file".into()).with_code("not_avif"));
    }
    let (width, height) = avif_size(&data[..data.len().min(64 * 1024)]).ok_or_else(|| {
        AppError::BadRequest("AVIF without image size".into()).with_code("not_avif")
    })?;

    let relative = cover_path(id, cover_url.as_deref(), cassandra, archive.as_deref());
    let target = dir.join(&relative);
    tokio::task::spawn_blocking({
        let target = target.clone();
        move || -> std::io::Result<()> {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Временный файл рядом (та же ФС): rename атомарно заменяет старую обложку.
            let tmp = target.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
            std::fs::write(&tmp, &data)?;
            std::fs::rename(&tmp, &target).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
        }
    })
    .await
    .map_err(|e| AppError::Internal(format!("cover task failed: {e}")))?
    .map_err(|e| AppError::Internal(format!("cover write {}: {e}", target.display())))?;

    let url = format!("{}{URL_PREFIX}{relative}", state.config.public_url);
    sqlx::query(
        "UPDATE entities SET cover_url = $2, cover_width = $3, cover_height = $4 WHERE id = $1",
    )
    .bind(id)
    .bind(&url)
    .bind(width)
    .bind(height)
    .execute(&state.db)
    .await?;
    let touched = Touched::collect(&state.db, &[id], &[]).await?;
    // Публикация (индекс книг, ISR) — в фоне: файл и БД уже записаны, а писатель книжного
    // индекса может быть занят пересборкой минутами — ответ не должен его ждать.
    tokio::spawn({
        let state = state.clone();
        async move {
            publish::run(&state, touched, &Reporter::silent()).await;
        }
    });
    tracing::info!(admin_id = %admin.id, entity_id = %id, path = %relative, "book cover replaced");
    Ok(Json(detail_by_id(&state.db, id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_goes_to_the_existing_path_then_to_the_library_path() {
        let id = Uuid::nil();
        assert_eq!(
            cover_path(
                id,
                Some("http://localhost/covers/books/808072-809603/809016.avif?v=1"),
                Some(1),
                Some("x.arx")
            ),
            "808072-809603/809016.avif"
        );
        assert_eq!(
            cover_path(id, None, Some(809016), Some("808072-809603.arx")),
            "808072-809603/809016.avif"
        );
        // Чужая обложка (Open Library) не перезаписывается — файл по правилам библиотеки.
        assert_eq!(
            cover_path(
                id,
                Some("http://localhost/covers/ol/b/id/1-L.jpg"),
                Some(5),
                Some("1-9.arx")
            ),
            "1-9/5.avif"
        );
        assert_eq!(
            cover_path(
                id,
                Some("http://x/covers/books/../etc/passwd.avif"),
                None,
                None
            ),
            format!("manual/{}.avif", id.simple())
        );
    }

    #[test]
    fn only_avif_is_accepted() {
        let mut avif = 24u32.to_be_bytes().to_vec();
        avif.extend_from_slice(b"ftypavif\0\0\0\0mif1avif");
        assert!(is_avif(&avif));
        assert!(!is_avif(b"\xff\xd8\xff\xe0 jpeg"));
        let mut heic = 20u32.to_be_bytes().to_vec();
        heic.extend_from_slice(b"ftypheic\0\0\0\0mif1");
        assert!(!is_avif(&heic));
    }
}
