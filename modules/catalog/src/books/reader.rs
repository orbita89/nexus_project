//! Читалка книг: `GET /catalog/books/{slug}/epub` отдаёт EPUB книги из архива `.arx`.
//!
//! Где лежит файл — `metadata.file` книги (`{archive, path}`, пишет `nexus books cassandra`).
//! Архивы — в `BOOKS_ARCHIVES_DIR` (локально `cassandra.books/data/archives`, на сервере — HDD).
//! Файл достаётся библиотекой `libarx` без внешней программы `arx`: открывается только нужная
//! запись, а не весь том. Извлечённый EPUB кладётся в кэш на диске (`BOOKS_EPUB_CACHE_DIR`), чтобы
//! повторное открытие не читало HDD; сверх `BOOKS_EPUB_CACHE_MB` удаляются давно не открытые.
//!
//! Доступ: `metadata.public_read = true` — всем, включая гостей; иначе только ролям `vip` и
//! `admin` (гость — 401, остальные — 403). HTML страницы книги один для всех и лежит на CDN:
//! кнопку «Читать» показывает JS по `metadata.file`/`public_read` и роли из токена, а защищает
//! файл только этот эндпоинт. Ответ `Cache-Control: private` — CDN закрытый файл не кэширует.
//!
//! Кэш в браузере (решение 2026-11-22): публичные книги фронт хранит в IndexedDB без ограничений;
//! закрытые — с проверкой `HEAD` этого же адреса перед каждым открытием (401/403 → удалить копию)
//! и автоудалением через 30 дней. `HEAD` проверяет только доступ и наличие `metadata.file`.

use std::path::{Path as FsPath, PathBuf};
use std::sync::Once;
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderValue, Method};
use axum::response::Response;
use serde_json::Value;
use shared::auth::AuthUser;
use shared::error::{AppError, AppResult};
use shared::extract::Path;
use shared::AppState;
use uuid::Uuid;

use crate::metadata::BookFile;

/// Отдать EPUB книги для читалки.
#[utoipa::path(
    get, operation_id = "read_book_epub", path = "/books/{slug}/epub", tag = "catalog",
    params(("slug" = String, Path, description = "Slug книги")),
    responses(
        (status = 200, description = "Файл книги. `HEAD` — только проверка доступа, без извлечения и тела", content_type = "application/epub+zip", body = Vec<u8>),
        (status = 401, description = "Закрытая книга, гость"),
        (status = 403, description = "Закрытая книга, роль не vip/admin"),
        (status = 404, description = "Нет книги или у неё нет файла"),
        (status = 503, description = "Хранилище архивов не настроено или недоступно"),
    ),
    security((), ("bearer" = []))
)]
pub async fn epub(
    State(state): State<AppState>,
    method: Method,
    user: Option<AuthUser>,
    Path(slug): Path<String>,
) -> AppResult<Response> {
    let metadata: Option<Value> =
        sqlx::query_scalar("SELECT metadata FROM entities WHERE slug = $1 AND kind = 'book'")
            .bind(&slug)
            .fetch_optional(&state.db)
            .await?;
    let metadata = metadata.ok_or(AppError::NotFound)?;
    let file: BookFile = metadata
        .get("file")
        .cloned()
        .and_then(|file| serde_json::from_value(file).ok())
        .ok_or(AppError::NotFound)?;
    let public = metadata.get("public_read").and_then(Value::as_bool) == Some(true);
    check_access(public, user.as_ref())?;
    // HEAD: читалка проверяет доступ перед открытием книги из кэша браузера — архив не трогаем.
    if method == Method::HEAD {
        let mut response = Response::new(Body::empty());
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return Ok(response);
    }

    let archives = state
        .config
        .books_archives_dir
        .clone()
        .ok_or_else(|| AppError::Unavailable("хранилище книг не настроено".into()))?;
    let store = Store {
        archives: PathBuf::from(archives),
        cache: PathBuf::from(&state.config.books_epub_cache_dir),
        limit_bytes: state.config.books_epub_cache_mb.saturating_mul(1024 * 1024),
    };
    // Первый запрос после старта: привести кэш в порядок (предел могли уменьшить, после падения
    // могли остаться недоизвлечённые .tmp-*). Дальше — после каждого нового извлечения.
    static STARTUP_PRUNE: Once = Once::new();
    STARTUP_PRUNE.call_once(|| {
        let store = store.clone();
        tokio::task::spawn_blocking(move || store.prune());
    });
    let path = tokio::task::spawn_blocking(move || store.get(&file))
        .await
        .map_err(|err| AppError::Unavailable(format!("извлечение книги: {err}")))??;
    // Потоком с диска, а не целиком в память: книга бывает до десятков МБ, и сотня одновременных
    // читателей иначе держала бы в RAM сотни мегабайт.
    let opened = tokio::fs::File::open(&path)
        .await
        .map_err(|err| AppError::Unavailable(format!("кэш книги: {err}")))?;
    let len = opened.metadata().await.map(|m| m.len()).ok();

    let mut response = Response::new(Body::from_stream(tokio_util::io::ReaderStream::new(opened)));
    let headers = response.headers_mut();
    if let Some(len) = len {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/epub+zip"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if public {
            "public, max-age=86400"
        } else {
            "private, max-age=3600"
        }),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("inline"),
    );
    Ok(response)
}

/// Публичная книга — всем; закрытая — только `vip` и `admin`.
fn check_access(public: bool, user: Option<&AuthUser>) -> AppResult<()> {
    if public {
        return Ok(());
    }
    match user {
        None => Err(AppError::Unauthorized),
        Some(user) if user.role.can_read_books() => Ok(()),
        Some(_) => Err(AppError::Forbidden),
    }
}

/// Архивы и кэш извлечённых файлов.
#[derive(Clone)]
struct Store {
    archives: PathBuf,
    cache: PathBuf,
    limit_bytes: u64,
}

impl Store {
    /// Путь к EPUB в кэше; нет в кэше — извлечь из архива.
    fn get(&self, file: &BookFile) -> AppResult<PathBuf> {
        let (archive, entry) = safe_names(file).ok_or(AppError::NotFound)?;
        let stem = archive.trim_end_matches(".arx");
        let cached = self.cache.join(stem).join(entry);
        if cached.is_file() {
            // Отметка «недавно открыт» для вытеснения старых.
            if let Ok(f) = std::fs::File::options().append(true).open(&cached) {
                let _ = f.set_modified(SystemTime::now());
            }
            return Ok(cached);
        }
        let source = self.archives.join(archive);
        if !source.is_file() {
            tracing::warn!(archive = %source.display(), "book archive is missing");
            return Err(AppError::Unavailable("архив книги недоступен".into()));
        }
        let tmp = self.cache.join(format!(".tmp-{}", Uuid::new_v4()));
        let result = extract(&source, entry, &tmp).and_then(|extracted| {
            std::fs::create_dir_all(self.cache.join(stem))?;
            std::fs::rename(extracted, &cached)?;
            Ok(())
        });
        let _ = std::fs::remove_dir_all(&tmp);
        match result {
            Ok(()) => {
                self.prune();
                Ok(cached)
            }
            Err(err) => {
                tracing::error!(archive, entry, %err, "book extraction failed");
                Err(AppError::NotFound)
            }
        }
    }

    /// Привести кэш в порядок: удалить брошенные временные каталоги извлечения (`.tmp-*` старше
    /// [`STALE_TMP`] — бэкенд упал или перезапустился посреди извлечения), затем давно не
    /// открытые файлы, пока кэш больше предела, и опустевшие каталоги томов. Так папка кэша
    /// не растёт сверх `BOOKS_EPUB_CACHE_MB` ни при каких сбоях.
    fn prune(&self) {
        let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        let Ok(dirs) = std::fs::read_dir(&self.cache) else {
            return;
        };
        let now = SystemTime::now();
        let mut volumes = Vec::new();
        for dir in dirs.flatten() {
            let path = dir.path();
            let name = dir.file_name();
            if name.to_string_lossy().starts_with(".tmp-") {
                let old = dir
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| now.duration_since(t).unwrap_or_default() > STALE_TMP)
                    .unwrap_or(true);
                if old {
                    let _ = std::fs::remove_dir_all(&path);
                }
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            volumes.push(path);
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                        files.push((modified, meta.len(), entry.path()));
                    }
                }
            }
        }
        let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
        files.sort();
        for (_, len, path) in files {
            if total <= self.limit_bytes {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
        // Пустые каталоги томов: remove_dir удаляет только пустой — занятые останутся.
        for dir in volumes {
            let _ = std::fs::remove_dir(&dir);
        }
    }
}

/// Временный каталог извлечения старше этого — брошен (извлечение одной книги — секунды).
const STALE_TMP: Duration = Duration::from_secs(10 * 60);

/// Имя архива и записи без выхода за пределы каталогов: только имя файла `.arx` и
/// относительный путь без `..`.
fn safe_names(file: &BookFile) -> Option<(&str, &str)> {
    let archive = file.archive.as_str();
    let entry = file.path.as_str();
    let plain = |s: &str| {
        !s.is_empty()
            && !s.starts_with('/')
            && !s.contains('\\')
            && !s.contains('\0')
            && s.split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
    };
    (archive.ends_with(".arx") && !archive.contains('/') && plain(archive) && plain(entry))
        .then_some((archive, entry))
}

/// Извлечь одну запись архива в пустой каталог `dir`; вернуть путь к файлу.
fn extract(archive: &FsPath, entry: &str, dir: &FsPath) -> std::io::Result<PathBuf> {
    let io = |err: &dyn std::fmt::Display| std::io::Error::other(err.to_string());
    std::fs::create_dir_all(dir)?;
    let arx = libarx::Arx::new(archive).map_err(|e| io(&e))?;
    let items = [relative_path::RelativePathBuf::from(entry)];
    libarx::ExtractBuilder::new(dir)
        .items(&items, false)
        .extract(&arx, None)
        .map_err(|e| io(&e))?;
    let out = dir.join(entry);
    if out.is_file() {
        Ok(out)
    } else {
        Err(std::io::Error::other("запись не извлечена"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::auth::Role;

    fn user(role: Role) -> AuthUser {
        AuthUser {
            id: Uuid::new_v4(),
            role,
            session_id: Uuid::new_v4(),
        }
    }

    fn file(archive: &str, path: &str) -> BookFile {
        BookFile {
            archive: archive.into(),
            path: path.into(),
            size: None,
        }
    }

    #[test]
    fn public_books_are_open_to_guests_private_only_to_vip_and_admin() {
        assert!(check_access(true, None).is_ok());
        assert!(matches!(
            check_access(false, None),
            Err(AppError::Unauthorized)
        ));
        for role in [Role::User, Role::Author] {
            assert!(matches!(
                check_access(false, Some(&user(role))),
                Err(AppError::Forbidden)
            ));
        }
        for role in [Role::Vip, Role::Admin] {
            assert!(check_access(false, Some(&user(role))).is_ok());
        }
    }

    #[test]
    fn archive_paths_cannot_escape_directories() {
        assert!(safe_names(&file("1-9.arx", "7.epub")).is_some());
        assert!(safe_names(&file("1-9.arx", "a/7.epub")).is_some());
        for (archive, path) in [
            ("../1-9.arx", "7.epub"),
            ("x/1-9.arx", "7.epub"),
            ("1-9.zip", "7.epub"),
            ("1-9.arx", "../7.epub"),
            ("1-9.arx", "/etc/passwd"),
            ("1-9.arx", "a//7.epub"),
            ("1-9.arx", ""),
        ] {
            assert!(
                safe_names(&file(archive, path)).is_none(),
                "{archive} {path}"
            );
        }
    }

    #[test]
    fn prune_drops_oldest_files_over_the_limit() {
        let root = std::env::temp_dir().join(format!("nexus-epub-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("1-9")).unwrap();
        let old = root.join("1-9/1.epub");
        let new = root.join("1-9/2.epub");
        std::fs::write(&old, vec![0u8; 600]).unwrap();
        std::fs::write(&new, vec![0u8; 600]).unwrap();
        let f = std::fs::File::options().append(true).open(&old).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH).unwrap();
        let store = Store {
            archives: root.clone(),
            cache: root.clone(),
            limit_bytes: 1000,
        };
        store.prune();
        assert!(!old.exists());
        assert!(new.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prune_removes_stale_tmp_and_empty_volume_dirs() {
        let root = std::env::temp_dir().join(format!("nexus-epub-test-{}", Uuid::new_v4()));
        let stale = root.join(".tmp-old");
        let fresh = root.join(".tmp-new");
        let empty = root.join("1-9");
        for dir in [&stale, &fresh, &empty] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(stale.join("x.epub"), b"x").unwrap();
        let old = SystemTime::now() - STALE_TMP - Duration::from_secs(60);
        std::fs::File::open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let store = Store {
            archives: root.clone(),
            cache: root.clone(),
            limit_bytes: 1 << 30,
        };
        store.prune();
        assert!(!stale.exists(), "брошенный .tmp удалён");
        assert!(fresh.exists(), "идущее извлечение не трогаем");
        assert!(!empty.exists(), "пустой каталог тома удалён");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Настоящий архив Cassandra, если он есть локально: `BOOKS_TEST_ARX=путь/к.arx`,
    /// `BOOKS_TEST_ENTRY=113037.epub`. В CI переменных нет — тест ничего не делает.
    #[test]
    fn extracts_epub_from_real_archive_when_available() {
        let (Ok(archive), Ok(entry)) = (
            std::env::var("BOOKS_TEST_ARX"),
            std::env::var("BOOKS_TEST_ENTRY"),
        ) else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("nexus-arx-test-{}", Uuid::new_v4()));
        let out = extract(FsPath::new(&archive), &entry, &dir).unwrap();
        let bytes = std::fs::read(out).unwrap();
        assert_eq!(&bytes[..2], b"PK");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
