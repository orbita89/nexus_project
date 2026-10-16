//! Импорт книг из дампа LibGen (`catalog::books`): книги и файлы в `entities`, авторы в `people`, повторный
//! запуск; карточка книги с автором — через публичный API.

use axum::http::StatusCode;
use catalog::books::{self, ImportOptions, ImportReport};
use sqlx::PgPool;
use std::io::Write;
use std::path::PathBuf;

/// Дамп в стиле `mysqldump`: схема, две вставки, комментарии и экранирование.
const DUMP: &str = r#"-- MySQL dump 10.13
/*!40101 SET NAMES utf8 */;
DROP TABLE IF EXISTS `updated`;
CREATE TABLE `updated` (
  `ID` int(15) unsigned NOT NULL AUTO_INCREMENT,
  `Title` varchar(2000) DEFAULT '',
  `Author` varchar(1000) DEFAULT '',
  `Year` varchar(14) DEFAULT '',
  `Publisher` varchar(400) DEFAULT '',
  `Pages` varchar(100) DEFAULT '',
  `Language` varchar(150) DEFAULT '',
  `Identifier` varchar(300) DEFAULT '',
  `Extension` varchar(50) DEFAULT '',
  `MD5` char(32) DEFAULT NULL,
  `Visible` char(3) DEFAULT '',
  `Coverurl` varchar(200) DEFAULT '',
  PRIMARY KEY (`ID`)
) ENGINE=MyISAM DEFAULT CHARSET=utf8;
INSERT INTO `updated` VALUES (1,'Мастер и Маргарита','Булгаков, Михаил Афанасьевич','1990','Художественная литература','480','Russian','','fb2','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','','1000/a.jpg'),(2,'Мастер и Маргарита','Булгаков, Михаил Афанасьевич','2005','АСТ','512','Russian','','pdf','bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb','',''),(3,'Dune','Frank Herbert','1965','','','English','','epub','cccccccccccccccccccccccccccccccc','','');
INSERT INTO `updated` VALUES (4,'Белая гвардия; роман \'1925\'','Булгаков, Михаил Афанасьевич','',NULL,NULL,'rus',NULL,'djvu','dddddddddddddddddddddddddddddddd','',NULL),(5,'Двенадцать стульев','Ильф И.; Петров Е.','1928','','','Russian','','epub','eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee','',''),(6,'Удалённая','Кто-то','2000','','','Russian','','pdf','ffffffffffffffffffffffffffffffff','del','');
"#;

struct TempFile(PathBuf);

impl TempFile {
    fn new(content: &str) -> Self {
        let path = std::env::temp_dir().join(format!("nexus-libgen-{}.sql", uuid::Uuid::new_v4()));
        std::fs::File::create(&path)
            .unwrap()
            .write_all(content.as_bytes())
            .unwrap();
        Self(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn imports_russian_books_with_authors_and_is_idempotent(pool: PgPool) {
    let (state, _) = test_utils::state(pool.clone());
    let file = TempFile::new(DUMP);
    let mut options = ImportOptions::new(file.0.clone());
    // По одной строке: второй файл «Мастера и Маргариты» дописывается к уже созданной книге.
    options.batch_size = 1;

    let report = books::import(&state, &options).await.unwrap();
    assert_eq!(
        report,
        ImportReport {
            rows: 6,
            russian: 4,
            known: 0,
            books_created: 3,
            files_linked: 4,
            people_created: 3,
            credits_created: 4,
        }
    );

    // Два файла «Мастера и Маргариты» — одна книга; данные — от первого файла.
    let (slug, release, cover, metadata): (
        String,
        Option<String>,
        Option<String>,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT e.slug, e.release_date::text, e.cover_url, e.metadata FROM entities e
             WHERE e.kind = 'book' AND e.title = 'Мастер и Маргарита'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(slug, "master-i-margarita");
    assert_eq!(release.as_deref(), Some("1990-01-01"));
    assert_eq!(
        cover.as_deref(),
        Some("https://libgen.is/covers/1000/a.jpg")
    );
    assert_eq!(
        metadata,
        serde_json::json!({
            "pages": 480,
            "publisher": "Художественная литература",
            "libgen": {
                "work": "мастер и маргарита|булгаков михаил афанасьевич",
                "files": [
                    { "md5": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "extension": "fb2", "id": 1 },
                    { "md5": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "extension": "pdf", "id": 2 },
                ],
            },
        })
    );
    // Админка сохраняет такую книгу без изменений.
    assert_eq!(
        catalog::metadata::validate(catalog::models::EntityKind::Book, metadata.clone()).unwrap(),
        metadata
    );

    // Булгаков — один человек на две книги; у «Двенадцати стульев» два автора по порядку.
    let bulgakov: Vec<String> = sqlx::query_scalar(
        "SELECT e.title FROM entity_credits c JOIN people p ON p.id = c.person_id
         JOIN entities e ON e.id = c.entity_id
         WHERE p.full_name = 'Булгаков Михаил Афанасьевич' AND c.role = 'author' ORDER BY e.title",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        bulgakov,
        ["Белая гвардия; роман '1925'", "Мастер и Маргарита"]
    );
    let authors: Vec<String> = sqlx::query_scalar(
        "SELECT p.full_name FROM entity_credits c JOIN people p ON p.id = c.person_id
         JOIN entities e ON e.id = c.entity_id
         WHERE e.title = 'Двенадцать стульев' ORDER BY c.position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(authors, ["Ильф И.", "Петров Е."]);

    // Карточка доступна через API вместе с автором.
    let app = nexus::build_app(state.clone());
    let card = test_utils::get(app, "/api/v1/catalog/entities/master-i-margarita").await;
    assert_eq!(card.status, StatusCode::OK);
    assert!(card
        .json()
        .to_string()
        .contains("Булгаков Михаил Афанасьевич"));

    // Повторный запуск ничего не дублирует.
    let again = books::import(&state, &options).await.unwrap();
    assert_eq!(again.known, 4);
    assert_eq!(
        (
            again.books_created,
            again.people_created,
            again.files_linked
        ),
        (0, 0, 0)
    );
    let books: i64 = sqlx::query_scalar("SELECT count(*) FROM entities WHERE kind = 'book'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(books, 3);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn limit_and_dump_without_schema(pool: PgPool) {
    let (state, _) = test_utils::state(pool.clone());
    // Без CREATE TABLE — порядок колонок таблицы `updated` LibGen.
    let mut row: Vec<String> = vec!["''".into(); books::UPDATED_COLUMNS.len()];
    fn set(row: &mut [String], name: &str, value: &str) {
        let i = books::UPDATED_COLUMNS
            .iter()
            .position(|c| *c == name)
            .unwrap();
        row[i] = format!("'{value}'");
    }
    set(&mut row, "Language", "Russian");
    set(&mut row, "Author", "Пушкин А.С.");
    let mut tuples = Vec::new();
    for (n, title) in ["Евгений Онегин", "Капитанская дочка", "Дубровский"]
        .iter()
        .enumerate()
    {
        set(&mut row, "Title", title);
        set(&mut row, "MD5", &format!("{n:032x}"));
        tuples.push(format!("({})", row.join(",")));
    }
    let file = TempFile::new(&format!(
        "INSERT INTO `updated` VALUES {};\n",
        tuples.join(",")
    ));
    let mut options = ImportOptions::new(file.0.clone());
    options.limit = Some(2);

    let report = books::import(&state, &options).await.unwrap();
    assert_eq!(
        (report.russian, report.books_created, report.people_created),
        (2, 2, 1)
    );
}
