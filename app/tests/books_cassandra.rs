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

/// Токен пользователя с ролью (dev-вход).
async fn token(state: &shared::AppState, login: &str, role: &str) -> String {
    let response = test_utils::request(
        nexus::build_app(state.clone()),
        axum::http::Method::POST,
        "/api/v1/auth/dev/login",
        Some(serde_json::json!({ "login": login, "role": role })),
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
    response.json()["access_token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn manual_ranks_order_book_listings_without_reindexing(pool: PgPool) {
    use axum::http::Method;
    let library = library();
    let (state, _) = test_utils::state_with_config(pool.clone(), library.config());
    cassandra::import(&state, &Options::new(library.0.clone()))
        .await
        .unwrap();
    catalog::books::sync::rebuild(&state).await.unwrap();
    let admin = token(&state, "admin@example.com", "admin").await;
    let user = token(&state, "user@example.com", "user").await;
    let send = |method: Method, uri: &'static str, body: Option<serde_json::Value>, token: &str| {
        let app = nexus::build_app(state.clone());
        let token = token.to_string();
        async move { test_utils::request(app, method, uri, body, Some(&token)).await }
    };
    let listing = || async {
        let response = test_utils::get(
            nexus::build_app(state.clone()),
            "/api/v1/catalog/search?kind=book&sort=recommended",
        )
        .await;
        response.json()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["slug"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    let ranks = serde_json::json!({ "items": [
        { "slug": "ezhik-v-tumane", "rank": 1 },
        { "slug": "master-i-margarita", "rank": 2 },
    ]});

    // Только админ; неизвестный slug — 404; повтор места — 400.
    let forbidden = send(
        Method::PUT,
        "/api/v1/catalog/admin/books/ranks",
        Some(ranks.clone()),
        &user,
    )
    .await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
    let unknown = send(
        Method::PUT,
        "/api/v1/catalog/admin/books/ranks",
        Some(serde_json::json!({ "items": [{ "slug": "net-takoy", "rank": 1 }] })),
        &admin,
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let repeated = send(
        Method::PUT,
        "/api/v1/catalog/admin/books/ranks",
        Some(serde_json::json!({ "items": [
            { "slug": "ezhik-v-tumane", "rank": 1 },
            { "slug": "master-i-margarita", "rank": 1 },
        ]})),
        &admin,
    )
    .await;
    assert_eq!(repeated.status, StatusCode::BAD_REQUEST);

    // Список действует сразу: слой Б в RAM, индекс не переписывается.
    let put = send(
        Method::PUT,
        "/api/v1/catalog/admin/books/ranks",
        Some(ranks),
        &admin,
    )
    .await;
    assert_eq!(put.status, StatusCode::NO_CONTENT);
    assert_eq!(
        listing().await[..2],
        ["ezhik-v-tumane", "master-i-margarita"]
    );
    let list = send(
        Method::GET,
        "/api/v1/catalog/admin/books/ranks",
        None,
        &admin,
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.json()["total"], 2);
    assert_eq!(list.json()["items"][0]["slug"], "ezhik-v-tumane");

    // В жанре места — относительные: «Ёжика» там нет, «Мастер» — первый.
    let genre = test_utils::get(
        nexus::build_app(state.clone()),
        "/api/v1/catalog/search?kind=book&person=mikhail-bulgakov&sort=recommended",
    )
    .await
    .json();
    assert_eq!(genre["items"][0]["slug"], "master-i-margarita");

    // Админский список книг — из индекса в том же ручном порядке, с флагами из БД; скрытые
    // (`active=false`) — из PostgreSQL.
    let admin_list = send(
        Method::GET,
        "/api/v1/catalog/admin/entities?kind=book",
        None,
        &admin,
    )
    .await;
    assert_eq!(admin_list.status, StatusCode::OK);
    let body = admin_list.json();
    assert_eq!(body["total"], 3);
    assert_eq!(body["items"][0]["slug"], "ezhik-v-tumane");
    assert_eq!(body["items"][1]["slug"], "master-i-margarita");
    assert_eq!(body["items"][0]["active"], true);
    assert_eq!(body["items"][0]["visible"], true);
    let by_title = send(
        Method::GET,
        "/api/v1/catalog/admin/entities?kind=book&q=%D0%BC%D0%B0%D1%81%D1%82%D0%B5%D1%80",
        None,
        &admin,
    )
    .await;
    assert_eq!(by_title.json()["items"][0]["slug"], "master-i-margarita");
    let hidden = send(
        Method::GET,
        "/api/v1/catalog/admin/entities?kind=book&active=false",
        None,
        &admin,
    )
    .await;
    assert_eq!(hidden.status, StatusCode::OK);
    assert_eq!(hidden.json()["total"], 0);

    // Пустой список — ручного порядка нет.
    let cleared = send(
        Method::PUT,
        "/api/v1/catalog/admin/books/ranks",
        Some(serde_json::json!({ "items": [] })),
        &admin,
    )
    .await;
    assert_eq!(cleared.status, StatusCode::NO_CONTENT);
    let reload = send(
        Method::POST,
        "/api/v1/catalog/admin/books/sidecar",
        None,
        &admin,
    )
    .await;
    assert_eq!(reload.status, StatusCode::ACCEPTED);

    // Состояние индекса — только админу: оба слоя, место по полям, фасеты, операция.
    let forbidden = send(
        Method::GET,
        "/api/v1/catalog/admin/books/index",
        None,
        &user,
    )
    .await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
    let status = send(
        Method::GET,
        "/api/v1/catalog/admin/books/index",
        None,
        &admin,
    )
    .await;
    assert_eq!(status.status, StatusCode::OK);
    let body = status.json();
    assert!(body["text"]["docs"].as_u64().unwrap() > 0);
    assert!(body["text"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["field"] == "title" && f["total"].as_u64().unwrap() > 0));
    assert!(body["sidecar"]["books"].as_u64().unwrap() > 0);
    assert_eq!(body["operation"]["op"], "sidecar");
    let cache = send(
        Method::POST,
        "/api/v1/catalog/admin/books/index/cache",
        None,
        &admin,
    )
    .await;
    assert_eq!(cache.status, StatusCode::NO_CONTENT);
}

#[test]
fn rank_list_file_is_parsed_like_the_progress_list() {
    let lines = catalog::books::ranks::parse_list(
        "ПОДБОРКА КНИГ НА РУССКОМ ЯЗЫКЕ\nВсего уникальных названий: 2\nФормат: Название — Автор\n\n\
         Мастер и Маргарита — Михаил Булгаков\nДвенадцать стульев — Илья Ильф, Евгений Петров\n",
    );
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].title, "Мастер и Маргарита");
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn book_reader_serves_private_books_only_to_vip_and_admin(pool: PgPool) {
    use axum::http::Method;
    let library = library();
    let mut config = library.config();
    config.books_archives_dir = Some(library.0.join("archives").to_string_lossy().into_owned());
    config.books_epub_cache_dir = library.0.join("epub-cache").to_string_lossy().into_owned();
    let (state, _) = test_utils::state_with_config(pool.clone(), config);
    cassandra::import(&state, &Options::new(library.0.clone()))
        .await
        .unwrap();
    let slug = |with_file: bool| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT slug FROM entities WHERE kind = 'book' AND (metadata ? 'file') = $1 LIMIT 1",
            )
            .bind(with_file)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (with_file, without_file) = (slug(true).await, slug(false).await);
    let user = token(&state, "user@example.com", "user").await;
    let author = token(&state, "author@example.com", "author").await;
    let vip = token(&state, "vip@example.com", "vip").await;
    let admin = token(&state, "admin@example.com", "admin").await;
    let read = |slug: &str, token: Option<&str>| {
        let app = nexus::build_app(state.clone());
        let uri = format!("/api/v1/catalog/books/{slug}/epub");
        let token = token.map(str::to_string);
        async move {
            test_utils::request(app, Method::GET, &uri, None, token.as_deref())
                .await
                .status
        }
    };

    // Закрытая книга: гость — 401, user/author — 403.
    assert_eq!(read(&with_file, None).await, StatusCode::UNAUTHORIZED);
    assert_eq!(read(&with_file, Some(&user)).await, StatusCode::FORBIDDEN);
    assert_eq!(read(&with_file, Some(&author)).await, StatusCode::FORBIDDEN);
    // vip/admin проходят проверку; архива `1-9.arx` в хранилище нет — 503.
    assert_eq!(
        read(&with_file, Some(&vip)).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        read(&with_file, Some(&admin)).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    // HEAD — только проверка доступа: архив не нужен, 200 vip и 403 user.
    let head = |token: &str| {
        let app = nexus::build_app(state.clone());
        let uri = format!("/api/v1/catalog/books/{with_file}/epub");
        let token = token.to_string();
        async move {
            test_utils::request(app, Method::HEAD, &uri, None, Some(&token))
                .await
                .status
        }
    };
    assert_eq!(head(&vip).await, StatusCode::OK);
    assert_eq!(head(&user).await, StatusCode::FORBIDDEN);
    // Нет файла или нет книги — 404.
    assert_eq!(read(&without_file, Some(&vip)).await, StatusCode::NOT_FOUND);
    assert_eq!(
        read("no-such-book", Some(&vip)).await,
        StatusCode::NOT_FOUND
    );

    // Публичная книга открыта гостю.
    sqlx::query(
        "UPDATE entities SET metadata = metadata || '{\"public_read\": true}' WHERE slug = $1",
    )
    .bind(&with_file)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        read(&with_file, None).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

/// AVIF для тестов: `ftyp` с брендом `avif` и бокс `ispe` с размерами.
fn avif(width: u32, height: u32) -> Vec<u8> {
    let mut data = 24u32.to_be_bytes().to_vec();
    data.extend_from_slice(b"ftypavif\0\0\0\0mif1avif");
    data.extend_from_slice(b"\0\0\0\x14ispe\0\0\0\0");
    data.extend_from_slice(&width.to_be_bytes());
    data.extend_from_slice(&height.to_be_bytes());
    data
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn book_cover_is_replaced_in_place(pool: PgPool) {
    let library = library();
    let (state, _) = test_utils::state_with_config(pool.clone(), library.config());
    cassandra::import(&state, &Options::new(library.0.clone()))
        .await
        .unwrap();
    let admin = token(&state, "admin@example.com", "admin").await;
    let user = token(&state, "user@example.com", "user").await;
    let id_of = |title: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM entities WHERE title = $1")
                .bind(title)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let upload = |id: uuid::Uuid, bytes: Vec<u8>, token: String| {
        let app = nexus::build_app(state.clone());
        async move {
            test_utils::upload(
                app,
                &format!("/api/v1/catalog/admin/entities/{id}/cover"),
                "cover.avif",
                bytes,
                Some(&token),
            )
            .await
        }
    };

    // Обложка есть: новый файл — по тому же пути, адрес прежний, размеры новые.
    let heart = id_of("Собачье сердце").await;
    let forbidden = upload(heart, avif(300, 450), user.clone()).await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
    let jpeg = upload(heart, b"\xff\xd8\xff\xe0 not avif".to_vec(), admin.clone()).await;
    assert_eq!(jpeg.status, StatusCode::BAD_REQUEST);
    assert_eq!(jpeg.json()["code"], "not_avif");
    let replaced = upload(heart, avif(300, 450), admin.clone()).await;
    assert_eq!(replaced.status, StatusCode::OK, "{:?}", replaced.json());
    let body = replaced.json();
    assert_eq!(
        body["cover_url"],
        "http://nexus.test/covers/books/1-9/1.avif"
    );
    assert_eq!(body["cover_width"], 300);
    assert_eq!(body["cover_height"], 450);
    let file = library.0.join("out-covers/1-9/1.avif");
    assert_eq!(std::fs::read(&file).unwrap(), avif(300, 450));
    // Временных файлов рядом не остаётся.
    assert_eq!(
        std::fs::read_dir(file.parent().unwrap()).unwrap().count(),
        1
    );

    // Обложки не было, книга не из архива — `manual/<id>.avif`.
    let master = id_of("Мастер и Маргарита").await;
    let added = upload(master, avif(600, 900), admin.clone()).await;
    assert_eq!(added.status, StatusCode::OK);
    let manual = format!("manual/{}.avif", master.simple());
    assert_eq!(
        added.json()["cover_url"],
        format!("http://nexus.test/covers/books/{manual}")
    );
    assert!(library.0.join("out-covers").join(manual).is_file());
}
