//! Книги библиотеки Cassandra (`catalog::books::cassandra`) и книжный поиск на Tantivy
//! (`catalog::books::{index, sync, search}`): импорт каталога библиотеки, повторный запуск,
//! карточка книги, выдача `GET /search?kind=book` из индекса и подсказки фильтров.

use axum::http::StatusCode;
use catalog::books::cassandra::{self, Options, Report};
use shared::Config;
use sqlx::PgPool;
use std::path::PathBuf;

/// Каталог библиотеки во временной папке: `metadata/*.jsonl`, `covers/`, а рядом — куда
/// копировать обложки и где держать индекс.
struct Library(PathBuf);

impl Library {
    fn new(lines: &[serde_json::Value]) -> Self {
        let dir = std::env::temp_dir().join(format!("nexus-cassandra-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("metadata")).unwrap();
        std::fs::create_dir_all(dir.join("covers/1-9")).unwrap();
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        // Битая строка теряет одну книгу, а не файл.
        std::fs::write(dir.join("metadata/1-9.jsonl"), format!("{text}{{broken\n")).unwrap();
        // Минимальный «AVIF»: импорту нужен только бокс `ispe` с размерами.
        let mut cover = vec![0u8; 16];
        cover.extend_from_slice(b"ispe\0\0\0\0");
        cover.extend_from_slice(&600u32.to_be_bytes());
        cover.extend_from_slice(&900u32.to_be_bytes());
        std::fs::write(dir.join("covers/1-9/1.avif"), cover).unwrap();
        Self(dir)
    }

    fn config(&self) -> Config {
        let mut config = Config::from_env();
        config.book_covers_dir = Some(self.0.join("out-covers").to_string_lossy().into_owned());
        config.books_index_dir = Some(self.0.join("index").to_string_lossy().into_owned());
        config.public_url = "http://nexus.test".into();
        config
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn library() -> Library {
    Library::new(&[
        serde_json::json!({
            "id": 1, "title": "Собачье сердце", "author": "Михаил Булгаков",
            "genre_codes": ["prose_rus_classic", "unknown_code"],
            "archive": "1-9.arx", "internal_path": "1.fb2", "cover": "1-9/1.avif",
            "details": {
                "authors": ["Михаил Булгаков"], "annotation": "Повесть о профессоре и псе.",
                "lang": "ru", "series": "Московские повести", "series_number": "2",
                "publisher": "АСТ", "year": "1987", "date": "1925",
                "chapters": ["Глава 1"], "doc_id": "X", "city": "Москва", "src_ocr": "ocr"
            },
            "size": 1000, "words": 30000
        }),
        serde_json::json!({
            "id": 2, "title": "Мастер и Маргарита", "author": "Михаил Булгаков",
            "genre_codes": ["prose_rus_classic"],
            "details": {
                "authors": ["Михаил Булгаков"], "translators": ["Иван Переводчиков"],
                "publisher": "Эксмо", "year": "1967"
            }
        }),
        serde_json::json!({
            "id": 3, "title": "Ёжик в тумане", "author": "Сергей Козлов",
            "genre_codes": ["child_tale"],
            "details": { "authors": ["Сергей Козлов"] }
        }),
        serde_json::json!({ "id": 4, "title": "   " }),
    ])
}

#[allow(clippy::type_complexity)]
#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn imports_library_and_is_idempotent(pool: PgPool) {
    let library = library();
    let (state, _) = test_utils::state_with_config(pool.clone(), library.config());
    let options = Options::new(library.0.clone());

    let report = cassandra::import(&state, &options).await.unwrap();
    assert_eq!(
        report,
        Report {
            rows: 5,
            skipped: 2,
            known: 0,
            books_created: 3,
            people_created: 3,
            credits_created: 4,
            genres_linked: 3,
            covers_copied: 1,
            covers_missing: 0,
        }
    );
    assert!(library.0.join("out-covers/1-9/1.avif").is_file());

    let (slug, description, release, cover, width, height, metadata, external): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i32>,
        Option<i32>,
        serde_json::Value,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT slug, description, release_date::text, cover_url, cover_width, cover_height,
                metadata, external_ids
         FROM entities WHERE kind = 'book' AND title = 'Собачье сердце'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(slug, "sobache-serdtse");
    assert_eq!(description.as_deref(), Some("Повесть о профессоре и псе."));
    assert_eq!(release.as_deref(), Some("1987-01-01"));
    assert_eq!(
        cover.as_deref(),
        Some("http://nexus.test/covers/books/1-9/1.avif")
    );
    assert_eq!((width, height), (Some(600), Some(900)));
    assert_eq!(external, serde_json::json!({ "cassandra": 1 }));
    assert_eq!(
        metadata,
        serde_json::json!({
            "publisher": "АСТ",
            "series": "Московские повести", "series_number": "2",
            "language": "ru", "written": "1925", "words": 30000,
            "file": { "archive": "1-9.arx", "path": "1.fb2", "size": 1000 },
        })
    );
    // Админка сохраняет такую книгу без изменений.
    assert_eq!(
        catalog::metadata::validate(catalog::models::EntityKind::Book, metadata.clone()).unwrap(),
        metadata
    );

    // Один Булгаков на две книги; переводчик — отдельной ролью.
    let roles: Vec<(String, String)> = sqlx::query_as(
        "SELECT p.full_name, ec.role FROM entity_credits ec
         JOIN people p ON p.id = ec.person_id JOIN entities e ON e.id = ec.entity_id
         WHERE e.title = 'Мастер и Маргарита' ORDER BY ec.position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        roles,
        [
            ("Михаил Булгаков".to_string(), "author".to_string()),
            ("Иван Переводчиков".to_string(), "translator".to_string()),
        ]
    );
    // Жанр из таблицы FB2 — тег-жанр; неизвестный код пропущен.
    let genres: Vec<(String, String)> = sqlx::query_as(
        "SELECT t.name, t.kind FROM entity_tags et JOIN tags t ON t.id = et.tag_id
         JOIN entities e ON e.id = et.entity_id WHERE e.title = 'Собачье сердце'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(genres.len(), 1);
    assert_eq!(genres[0].1, "genre");

    // Повторный запуск ничего не создаёт.
    let again = cassandra::import(&state, &options).await.unwrap();
    assert_eq!(
        (again.known, again.books_created, again.people_created),
        (3, 0, 0)
    );
    let books: i64 = sqlx::query_scalar("SELECT count(*) FROM entities WHERE kind = 'book'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(books, 3);

    // Карточка книги: аннотация и автор — из БД.
    let app = nexus::build_app(state);
    let card = test_utils::get(app, "/api/v1/catalog/entities/sobache-serdtse").await;
    assert_eq!(card.status, StatusCode::OK);
    let card = card.json();
    assert_eq!(card["description"], "Повесть о профессоре и псе.");
    assert_eq!(card["credits"][0]["person"]["full_name"], "Михаил Булгаков");
    assert_eq!(card["metadata"]["series"], "Московские повести");
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn book_search_is_answered_by_the_books_index(pool: PgPool) {
    let library = library();
    let (state, _) = test_utils::state_with_config(pool.clone(), library.config());
    cassandra::import(&state, &Options::new(library.0.clone()))
        .await
        .unwrap();
    assert_eq!(catalog::books::sync::rebuild(&state).await.unwrap(), 3);
    let slugs = |body: &serde_json::Value| -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["slug"].as_str().unwrap().to_string())
            .collect()
    };
    let get = |uri: &'static str| {
        let app = nexus::build_app(state.clone());
        async move {
            let response = test_utils::get(app, uri).await;
            assert_eq!(response.status, StatusCode::OK, "{uri}");
            response.json()
        }
    };

    // Недописанное слово, автор в строке выдачи, точный total.
    let found = get("/api/v1/catalog/search?kind=book&q=%D1%81%D0%BE%D0%B1%D0%B0%D1%87").await;
    assert_eq!(slugs(&found), ["sobache-serdtse"]);
    assert_eq!(found["total"], 1);
    assert_eq!(found["items"][0]["kind"], "book");
    assert_eq!(found["items"][0]["authors"][0]["name"], "Михаил Булгаков");
    assert_eq!(found["items"][0]["cover_width"], 600);

    // Автор, фильтр по человеку и роли, обложка.
    let by_author =
        get("/api/v1/catalog/search?kind=book&q=%D0%B1%D1%83%D0%BB%D0%B3%D0%B0%D0%BA%D0%BE%D0%B2")
            .await;
    assert_eq!(by_author["total"], 2);
    let person = get("/api/v1/catalog/search?kind=book&person=mikhail-bulgakov&role=author").await;
    assert_eq!(person["total"], 2);
    let translated =
        get("/api/v1/catalog/search?kind=book&person=ivan-perevodchikov&role=translator").await;
    assert_eq!(slugs(&translated), ["master-i-margarita"]);
    let covers = get("/api/v1/catalog/search?kind=book&has_cover=true").await;
    assert_eq!(slugs(&covers), ["sobache-serdtse"]);
    // «ё» = «е».
    let yo = get("/api/v1/catalog/search?kind=book&q=%D0%B5%D0%B6%D0%B8%D0%BA").await;
    assert_eq!(slugs(&yo), ["ezhik-v-tumane"]);
    // Фильтр, которого у книг не бывает, — пусто, а не ошибка.
    let studio = get("/api/v1/catalog/search?kind=book&studio=Pixar").await;
    assert_eq!(studio["total"], 0);

    // Подсказки фильтров раздела — из индекса.
    let tags = get("/api/v1/catalog/search/tags?kind=book").await;
    assert_eq!(tags.as_array().unwrap().len(), 2);
    assert_eq!(tags[0]["count"], 2);
    assert_eq!(tags[0]["kind"], "genre");
    let publishers = get("/api/v1/catalog/search/publishers?kind=book&q=%D1%8D%D0%BA").await;
    assert_eq!(publishers[0]["name"], "Эксмо");

    // Соседи в ряду автора и похожие по жанру.
    let neighbors = get(
        "/api/v1/catalog/entities/sobache-serdtse/neighbors?kind=book&person=mikhail-bulgakov&sort=title",
    )
    .await;
    assert_eq!(neighbors["prev"]["slug"], "master-i-margarita");
    assert!(neighbors["next"].is_null());
    let similar = get("/api/v1/catalog/entities/sobache-serdtse/similar").await;
    assert_eq!(slugs(&similar), ["master-i-margarita"]);
}
