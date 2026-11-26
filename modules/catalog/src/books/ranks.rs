//! Ручной порядок книг (`catalog_manual_ranks`): импорт списка `Название — Автор` из файла
//! (`nexus books ranks FILE`) и замена всего списка (админка `PUT /admin/books/ranks`).
//!
//! Место — номер строки списка (первая книга — место 1). Строка находит книгу по точному
//! названию (ключ без регистра, знаков и `ё`, как `title_key` индекса) и автору: совпасть
//! должна фамилия хотя бы одного автора («Аркадий и Борис Стругацкие» ~ «Аркадий Стругацкий»,
//! «Конан Дойль» ~ «Артур Конан Дойл»). Книга с тем же названием, но другим автором места не
//! получает: лучше пропуск, чем чужая книга на первом месте. Пропуски — в отчёте.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use shared::AppState;
use uuid::Uuid;

use super::BoxError;
use crate::models::VISIBLE;
use shared::tantivy::normalize_key;

/// Самое дальнее место: `SortMeta::manual_rank` — `u16`.
pub const MAX_RANK: usize = 65_535;

/// Строка списка: название и авторы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListLine {
    pub title: String,
    pub authors: Vec<String>,
}

/// Книги списка по порядку: строки `Название — Автор1, Автор2`. Заголовок, пустые строки и
/// строки без « — » пропускаются.
pub fn parse_list(text: &str) -> Vec<ListLine> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("Формат:"))
        .filter_map(|line| {
            // Тире бывает в названии («Свои люди — сочтёмся»), в имени автора — нет: делим по последнему.
            let (title, authors) = line.rsplit_once(" — ")?;
            let title = title.trim();
            if title.is_empty() {
                return None;
            }
            Some(ListLine {
                title: title.to_string(),
                authors: authors
                    .split(',')
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .map(String::from)
                    .collect(),
            })
        })
        .collect()
}

/// Латиница → кириллица по звучанию (грубо, только для сравнения имён: «Umberto Eco» ~
/// «Умберто Эко»).
fn cyrillic(text: &str) -> String {
    const PAIRS: [(&str, &str); 9] = [
        ("sch", "щ"),
        ("sh", "ш"),
        ("ch", "ч"),
        ("zh", "ж"),
        ("kh", "х"),
        ("ts", "ц"),
        ("yu", "ю"),
        ("ya", "я"),
        ("ph", "ф"),
    ];
    let lower = text.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut rest = lower.as_str();
    'outer: while let Some(c) = rest.chars().next() {
        for (latin, cyr) in PAIRS {
            if rest.starts_with(latin) {
                out.push_str(cyr);
                rest = &rest[latin.len()..];
                continue 'outer;
            }
        }
        let mapped = match c {
            'a' => "а",
            'b' => "б",
            'c' | 'k' | 'q' => "к",
            'd' => "д",
            'e' => "е",
            'f' => "ф",
            'g' => "г",
            'h' => "х",
            'i' | 'y' => "и",
            'j' => "дж",
            'l' => "л",
            'm' => "м",
            'n' => "н",
            'o' => "о",
            'p' => "п",
            'r' => "р",
            's' => "с",
            't' => "т",
            'u' => "у",
            'v' | 'w' => "в",
            'x' => "кс",
            'z' => "з",
            _ => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
                continue;
            }
        };
        out.push_str(mapped);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Слова имени в одном виде: кириллица, нижний регистр, без знаков, `ё`/`э` → `е`, `й` → `и`;
/// инициалы (короче 3 букв) отбрасываются.
fn words(name: &str) -> Vec<String> {
    normalize_key(&cyrillic(name))
        .replace(['э'], "е")
        .replace(['й'], "и")
        .split(' ')
        .filter(|w| w.chars().count() >= 3)
        .map(String::from)
        .collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(x != *y)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// Одна фамилия в разных формах: общее начало не короче `min(5, длина короче)` («стругацкие» ~
/// «стругацкий», «дойль» ~ «дойл») или одна правка у слов от 4 букв («остин» ~ «остен»).
fn same_surname(a: &str, b: &str) -> bool {
    let common = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    let shorter = a.chars().count().min(b.chars().count());
    common >= shorter.min(5) || (shorter >= 4 && edit_distance(a, b) <= 1)
}

/// Автор строки совпадает с автором книги: фамилия (последнее слово каждого автора строки) —
/// с любым словом имени автора в БД (порядок «Шолохов Михаил» и инициалы не мешают).
pub fn author_matches(line_authors: &[String], book_authors: &[String]) -> bool {
    let surnames: Vec<String> = line_authors.iter().filter_map(|a| words(a).pop()).collect();
    book_authors.iter().any(|author| {
        words(author)
            .iter()
            .any(|w| surnames.iter().any(|s| same_surname(s, w)))
    })
}

/// Короткая форма названия: до подзаголовка (`:`, `. `, `(`) или альтернативного названия
/// («, или …»). `None` — сокращать нечего.
pub fn short_title(title: &str) -> Option<String> {
    let lower = title.to_lowercase();
    let cut = [", или ", " или ", ": ", ". ", " ("]
        .iter()
        .filter_map(|sep| lower.find(sep))
        .min()?;
    let key = normalize_key(&title[..cut]);
    (!key.is_empty() && key != normalize_key(title)).then_some(key)
}

/// Ключи названия строки списка по убыванию точности: целиком; части «А, или Б»; без скобок;
/// том серии после двоеточия («Хроники Нарнии: Принц Каспиан» → «принц каспиан»).
pub fn line_title_keys(title: &str) -> Vec<String> {
    let mut keys = vec![normalize_key(title)];
    let lower = title.to_lowercase();
    for sep in [", или ", " или "] {
        if let Some(at) = lower.find(sep) {
            keys.push(normalize_key(&title[..at]));
            keys.push(normalize_key(&title[at + sep.len()..]));
        }
    }
    if let Some(at) = title.find(" (") {
        keys.push(normalize_key(&title[..at]));
    }
    for sep in [": ", ". "] {
        if let Some(at) = title.find(sep) {
            keys.push(normalize_key(&title[at + sep.len()..]));
        }
    }
    let mut seen = HashSet::new();
    keys.retain(|k| !k.is_empty() && seen.insert(k.clone()));
    keys
}

/// Книга-кандидат: id, есть ли обложка, авторы.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: Uuid,
    pub has_cover: bool,
    pub authors: Vec<String>,
}

/// Итог сопоставления списка с каталогом.
#[derive(Debug, Default)]
pub struct Matched {
    /// Книга и её место.
    pub ranks: Vec<(Uuid, i32)>,
    /// Названия нет в каталоге.
    pub not_found: Vec<String>,
    /// Название есть, автор другой — места нет.
    pub other_author: Vec<String>,
    /// Книга уже получила место выше по списку.
    pub repeated: Vec<String>,
    /// Строки дальше [`MAX_RANK`].
    pub beyond: usize,
}

/// Книги каталога по ключу названия: целиком (`full`) и короткой формой (`short`,
/// [`short_title`]: «Мастер и Маргарита: роман» → «мастер и маргарита»).
#[derive(Debug, Default)]
pub struct Titles {
    pub full: HashMap<String, Vec<Candidate>>,
    pub short: HashMap<String, Vec<Candidate>>,
}

impl Titles {
    pub fn add(&mut self, title: &str, candidate: &Candidate) {
        let key = normalize_key(title);
        if key.is_empty() {
            return;
        }
        if let Some(short) = short_title(title) {
            self.short.entry(short).or_default().push(candidate.clone());
        }
        self.full.entry(key).or_default().push(candidate.clone());
    }

    /// Кандидаты строки с подходящим автором: сначала по точному названию, затем по
    /// вариантам строки ([`line_title_keys`]), затем по короткой форме названия в каталоге.
    /// Второе значение — название вообще нашлось (для отчёта «другой автор»).
    fn find(&self, line: &ListLine) -> (Vec<&Candidate>, bool) {
        let keys = line_title_keys(&line.title);
        let full = keys.iter().filter_map(|k| self.full.get(k));
        let short = self.short.get(&keys[0]).into_iter();
        let mut seen_title = false;
        for candidates in full.chain(short) {
            seen_title = true;
            let fitting: Vec<&Candidate> = candidates
                .iter()
                .filter(|c| author_matches(&line.authors, &c.authors))
                .collect();
            if !fitting.is_empty() {
                return (fitting, true);
            }
        }
        (Vec::new(), seen_title)
    }
}

/// Сопоставить строки списка с книгами каталога.
pub fn match_list(lines: &[ListLine], titles: &Titles) -> Matched {
    let mut out = Matched::default();
    let mut taken: HashSet<Uuid> = HashSet::new();
    for (at, line) in lines.iter().enumerate() {
        let shown = format!("{} — {}", line.title, line.authors.join(", "));
        if at >= MAX_RANK {
            out.beyond += 1;
            continue;
        }
        if line_title_keys(&line.title).is_empty() {
            out.not_found.push(shown);
            continue;
        }
        let (mut fitting, seen_title) = titles.find(line);
        if fitting.is_empty() {
            if seen_title {
                out.other_author.push(shown);
            } else {
                out.not_found.push(shown);
            }
            continue;
        }
        // Изданий одной книги бывает несколько: сначала с обложкой, затем ещё не занятое.
        fitting.sort_by_key(|c| (!c.has_cover, taken.contains(&c.id), c.id));
        let Some(best) = fitting.into_iter().find(|c| !taken.contains(&c.id)) else {
            out.repeated.push(shown);
            continue;
        };
        taken.insert(best.id);
        out.ranks.push((best.id, at as i32 + 1));
    }
    out
}

/// Заменить весь ручной список одним коммитом.
pub async fn replace_all(db: &sqlx::PgPool, ranks: &[(Uuid, i32)]) -> Result<(), sqlx::Error> {
    let ids: Vec<Uuid> = ranks.iter().map(|r| r.0).collect();
    let values: Vec<i32> = ranks.iter().map(|r| r.1).collect();
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM catalog_manual_ranks")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO catalog_manual_ranks (entity_id, rank)
         SELECT * FROM unnest($1::uuid[], $2::int[])",
    )
    .bind(&ids)
    .bind(&values)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Видимые книги каталога по ключу названия (и оригинального названия).
async fn books_by_title(db: &sqlx::PgPool) -> Result<Titles, sqlx::Error> {
    let books: Vec<(Uuid, String, Option<String>, bool)> = sqlx::query_as(&format!(
        "SELECT e.id, e.title, e.original_title, e.cover_url IS NOT NULL
         FROM entities e WHERE e.kind = 'book' AND {VISIBLE} AND e.parent_id IS NULL"
    ))
    .fetch_all(db)
    .await?;
    let authors: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT ec.entity_id, p.full_name
         FROM entity_credits ec
         JOIN people p ON p.id = ec.person_id
         JOIN entities e ON e.id = ec.entity_id
         WHERE e.kind = 'book' AND ec.role = 'author'",
    )
    .fetch_all(db)
    .await?;
    let mut names: HashMap<Uuid, Vec<String>> = HashMap::new();
    for (id, name) in authors {
        names.entry(id).or_default().push(name);
    }
    let mut out = Titles::default();
    for (id, title, original, has_cover) in books {
        let candidate = Candidate {
            id,
            has_cover,
            authors: names.remove(&id).unwrap_or_default(),
        };
        out.add(&title, &candidate);
        if let Some(original) = original.filter(|o| normalize_key(o) != normalize_key(&title)) {
            out.add(&original, &candidate);
        }
    }
    Ok(out)
}

/// `nexus books ranks FILE`: список из файла — в `catalog_manual_ranks` (целиком заменяет
/// прежний) и в слой Б этого процесса; сервер подхватит правку опросом.
pub async fn import(state: &AppState, path: &Path) -> Result<(usize, Matched), BoxError> {
    let text = std::fs::read_to_string(path)?;
    let lines = parse_list(&text);
    let by_title = books_by_title(&state.db).await?;
    let matched = match_list(&lines, &by_title);
    replace_all(&state.db, &matched.ranks).await?;
    crate::index::sync::apply_ranks(state).await?;
    Ok((lines.len(), matched))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_lines_are_parsed_and_header_skipped() {
        let lines = parse_list(
            "ПОДБОРКА\nФормат: Название — Автор\n\nМастер и Маргарита — Михаил Булгаков\n\
             Двенадцать стульев — Илья Ильф, Евгений Петров\nбез тире\n",
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].authors, ["Илья Ильф", "Евгений Петров"]);
        let dashed = parse_list("Свои люди — сочтёмся — Александр Островский");
        assert_eq!(dashed[0].title, "Свои люди — сочтёмся");
        assert_eq!(dashed[0].authors, ["Александр Островский"]);
    }

    #[test]
    fn surnames_match_across_forms() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(author_matches(
            &s(&["Аркадий и Борис Стругацкие"]),
            &s(&["Аркадий Стругацкий"])
        ));
        assert!(author_matches(
            &s(&["Артур Конан Дойль"]),
            &s(&["Артур Конан Дойл"])
        ));
        assert!(author_matches(
            &s(&["Фёдор Достоевский"]),
            &s(&["Ф. М. Достоевский"])
        ));
        assert!(!author_matches(&s(&["Михаил Булгаков"]), &s(&["Критик"])));
        // Другой порядок, латиница, вариант транскрипции.
        assert!(author_matches(
            &s(&["Михаил Шолохов"]),
            &s(&["Шолохов Михаил Александрович"])
        ));
        assert!(author_matches(&s(&["Умберто Эко"]), &s(&["Umberto Eco"])));
        assert!(author_matches(&s(&["Джейн Остин"]), &s(&["Джейн Остен"])));
        // Однофамилец имени, а не фамилии, — не совпадение.
        assert!(!author_matches(
            &s(&["Михаил Булгаков"]),
            &s(&["Михаил Иванов"])
        ));
    }

    #[test]
    fn titles_match_short_and_alternative_forms() {
        let candidate = |author: &str| Candidate {
            id: Uuid::new_v4(),
            has_cover: true,
            authors: vec![author.to_string()],
        };
        let key = candidate("Алексей Толстой");
        let master = candidate("Михаил Булгаков");
        let caspian = candidate("Клайв Льюис");
        let mut titles = Titles::default();
        titles.add("Золотой ключик", &key);
        titles.add("Мастер и Маргарита: роман", &master);
        titles.add("Принц Каспиан", &caspian);
        let lines = parse_list(
            "Золотой ключик, или Приключения Буратино — Алексей Толстой\n\
             Мастер и Маргарита — Михаил Булгаков\n\
             Хроники Нарнии: Принц Каспиан — Клайв Льюис",
        );
        let matched = match_list(&lines, &titles);
        assert_eq!(
            matched.ranks,
            [(key.id, 1), (master.id, 2), (caspian.id, 3)]
        );
    }

    #[test]
    fn rank_is_the_line_number_and_wrong_author_gets_nothing() {
        let book = |cover: bool, author: &str| Candidate {
            id: Uuid::new_v4(),
            has_cover: cover,
            authors: vec![author.to_string()],
        };
        let master_plain = book(false, "Михаил Булгаков");
        let master_cover = book(true, "Михаил Булгаков");
        let mut by_title = Titles::default();
        by_title.add("Мастер и Маргарита", &master_plain);
        by_title.add("Мастер и Маргарита", &master_cover);
        by_title.add("Идиот", &book(true, "Другой Автор"));
        let lines = parse_list(
            "Нет такой — Кто-то\nМастер и Маргарита — Михаил Булгаков\nИдиот — Фёдор Достоевский\n\
             Мастер и Маргарита — Михаил Булгаков\nМастер и Маргарита — Михаил Булгаков",
        );
        let matched = match_list(&lines, &by_title);
        // Издание с обложкой — первым, второе издание — следующей строкой.
        assert_eq!(matched.ranks, [(master_cover.id, 2), (master_plain.id, 4)]);
        assert_eq!(matched.not_found.len(), 1);
        assert_eq!(matched.other_author.len(), 1);
        assert_eq!(matched.repeated.len(), 1);
    }
}
