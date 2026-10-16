//! Потоковое чтение SQL-дампа MySQL (`mysqldump` LibGen): операторы по одному, без загрузки файла
//! в память. Нужны только `CREATE TABLE` (порядок колонок) и `INSERT INTO ... VALUES (...),(...)`.

use std::io::{self, BufRead};

/// Операторы дампа, разделённые `;` вне строк и комментариев. Память — один оператор
/// (`mysqldump --extended-insert`: обычно до нескольких МБ).
pub struct Statements<R> {
    input: R,
    buf: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Normal,
    Quoted(u8),
    Escape(u8),
    LineComment,
    BlockComment,
}

impl<R: BufRead> Statements<R> {
    pub fn new(input: R) -> Self {
        Self {
            input,
            buf: Vec::new(),
        }
    }

    /// Следующий непустой оператор без `;`; `None` — конец файла. Невалидный UTF-8 заменяется.
    pub fn next_statement(&mut self) -> io::Result<Option<String>> {
        self.buf.clear();
        let mut state = State::Normal;
        // Предыдущий байт в Normal: `--` и `/*` распознаются по паре.
        let mut prev = 0u8;
        loop {
            let chunk = self.input.fill_buf()?;
            if chunk.is_empty() {
                let rest = String::from_utf8_lossy(&self.buf).trim().to_string();
                self.buf.clear();
                return Ok((!rest.is_empty()).then_some(rest));
            }
            let mut used = 0;
            let mut done = false;
            for &b in chunk {
                used += 1;
                match state {
                    State::Normal => match b {
                        b';' => {
                            done = true;
                            break;
                        }
                        b'\'' | b'"' | b'`' => {
                            state = State::Quoted(b);
                            self.buf.push(b);
                        }
                        b'-' if prev == b'-' => {
                            self.buf.pop();
                            state = State::LineComment;
                        }
                        b'#' => state = State::LineComment,
                        b'*' if prev == b'/' => {
                            self.buf.pop();
                            state = State::BlockComment;
                        }
                        _ => self.buf.push(b),
                    },
                    State::Quoted(q) => {
                        self.buf.push(b);
                        if b == b'\\' && q != b'`' {
                            state = State::Escape(q);
                        } else if b == q {
                            state = State::Normal;
                        }
                    }
                    State::Escape(q) => {
                        self.buf.push(b);
                        state = State::Quoted(q);
                    }
                    State::LineComment => {
                        if b == b'\n' {
                            state = State::Normal;
                            self.buf.push(b'\n');
                        }
                    }
                    State::BlockComment => {
                        if b == b'/' && prev == b'*' {
                            state = State::Normal;
                            self.buf.push(b' ');
                        }
                    }
                }
                // В строке пара `--`/`/*` не считается; после закрытия комментария — с чистого листа.
                prev = match state {
                    State::Normal if prev == b'*' && b == b'/' => 0,
                    State::Normal | State::BlockComment => b,
                    _ => 0,
                };
            }
            self.input.consume(used);
            if done {
                let statement = String::from_utf8_lossy(&self.buf).trim().to_string();
                self.buf.clear();
                if !statement.is_empty() {
                    return Ok(Some(statement));
                }
                state = State::Normal;
                prev = 0;
            }
        }
    }
}

/// Имя таблицы без обратных кавычек и схемы: `` `libgen`.`updated` `` → `updated`.
fn table_name(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    let end = s
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(s.len());
    let (name, rest) = s.split_at(end);
    let name = name.rsplit('.').next().unwrap_or(name).trim_matches('`');
    (name, rest)
}

/// Отрезает ключевые слова в начале без учёта регистра: `strip_words(s, &["insert", "into"])`.
fn strip_words<'a>(s: &'a str, words: &[&str]) -> Option<&'a str> {
    let mut s = s.trim_start();
    for word in words {
        let head = s.get(..word.len())?;
        if !head.eq_ignore_ascii_case(word) {
            return None;
        }
        s = s[word.len()..].trim_start();
    }
    Some(s)
}

/// `CREATE TABLE t (...)` → (t, колонки по порядку). Колонка — строка определения, начинающаяся с
/// `` `имя` ``; ключи и ограничения пропускаются.
pub fn create_table(statement: &str) -> Option<(String, Vec<String>)> {
    let rest = strip_words(statement, &["create", "table"])?;
    let rest = strip_words(rest, &["if", "not", "exists"]).unwrap_or(rest);
    let (name, body) = table_name(rest);
    let columns = body
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches('(').trim_start();
            let inner = line.strip_prefix('`')?;
            Some(inner[..inner.find('`')?].to_string())
        })
        .collect();
    Some((name.to_string(), columns))
}

/// Разобранный `INSERT`: таблица, явный список колонок (если был) и строки значений.
pub struct Insert {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// `INSERT [IGNORE] INTO t [(cols)] VALUES (...),(...)`; `REPLACE INTO` — так же. `NULL` → `None`.
pub fn insert(statement: &str) -> Option<Insert> {
    let rest =
        strip_words(statement, &["insert"]).or_else(|| strip_words(statement, &["replace"]))?;
    let rest = strip_words(rest, &["ignore"]).unwrap_or(rest);
    let rest = strip_words(rest, &["into"])?;
    let (name, mut rest) = table_name(rest);
    rest = rest.trim_start();
    let mut columns = None;
    if let Some(list) = rest.strip_prefix('(') {
        let end = list.find(')')?;
        columns = Some(
            list[..end]
                .split(',')
                .map(|c| c.trim().trim_matches('`').to_string())
                .collect(),
        );
        rest = &list[end + 1..];
    }
    let values = strip_words(rest, &["values"])?;
    Some(Insert {
        table: name.to_string(),
        columns,
        rows: tuples(values),
    })
}

/// `(1,'a\'b',NULL),(2,...)` → строки значений с раскрытыми экранированиями MySQL.
fn tuples(s: &str) -> Vec<Vec<Option<String>>> {
    let mut rows = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '(' {
            continue;
        }
        let mut row = Vec::new();
        loop {
            while chars.next_if(|c| c.is_whitespace()).is_some() {}
            match chars.peek() {
                Some('\'') | Some('"') => {
                    let quote = chars.next().unwrap_or('\'');
                    let mut value = String::new();
                    while let Some(c) = chars.next() {
                        match c {
                            '\\' => match chars.next() {
                                Some('0') => value.push('\0'),
                                Some('n') => value.push('\n'),
                                Some('r') => value.push('\r'),
                                Some('t') => value.push('\t'),
                                Some('b') => value.push('\u{8}'),
                                Some('Z') => value.push('\u{1a}'),
                                Some(other) => value.push(other),
                                None => break,
                            },
                            c if c == quote => {
                                if chars.next_if_eq(&quote).is_some() {
                                    value.push(quote);
                                } else {
                                    break;
                                }
                            }
                            c => value.push(c),
                        }
                    }
                    row.push(Some(value));
                }
                _ => {
                    let mut token = String::new();
                    while let Some(c) = chars.next_if(|c| *c != ',' && *c != ')') {
                        token.push(c);
                    }
                    let token = token.trim();
                    row.push((!token.eq_ignore_ascii_case("null")).then(|| token.to_string()));
                }
            }
            while chars.next_if(|c| c.is_whitespace()).is_some() {}
            match chars.next() {
                Some(',') => continue,
                _ => break,
            }
        }
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statements(dump: &str) -> Vec<String> {
        let mut reader = Statements::new(io::BufReader::with_capacity(7, dump.as_bytes()));
        let mut all = Vec::new();
        while let Some(s) = reader.next_statement().unwrap() {
            all.push(s);
        }
        all
    }

    #[test]
    fn splits_statements_outside_strings_and_comments() {
        let dump = "-- MySQL dump; comment\n/*!40101 SET NAMES utf8 */;\n\
             INSERT INTO `updated` VALUES (1,'a;b\\';c'),(2,'--x');\n# c;\nSELECT 1";
        let all = statements(dump);
        assert_eq!(all.len(), 2, "{all:?}");
        assert_eq!(
            all[0],
            "INSERT INTO `updated` VALUES (1,'a;b\\';c'),(2,'--x')"
        );
        assert_eq!(all[1], "SELECT 1");
    }

    #[test]
    fn reads_create_table_columns() {
        let sql = "CREATE TABLE `updated` (\n  `ID` int(15) unsigned NOT NULL AUTO_INCREMENT,\n  \
             `Title` varchar(2000) DEFAULT '',\n  `MD5` char(32),\n  PRIMARY KEY (`ID`),\n  \
             KEY `MD5` (`MD5`)\n) ENGINE=MyISAM";
        let (name, columns) = create_table(sql).unwrap();
        assert_eq!(name, "updated");
        assert_eq!(columns, ["ID", "Title", "MD5"]);
    }

    #[test]
    fn parses_insert_values_with_mysql_escapes() {
        let sql = "INSERT INTO `libgen`.`updated` (`ID`, `Title`) VALUES \
             (1,'Война и мир, \\'том\\' 1\\nконец'),( 2 , NULL ),(3,'it''s')";
        let insert = insert(sql).unwrap();
        assert_eq!(insert.table, "updated");
        assert_eq!(insert.columns.unwrap(), ["ID", "Title"]);
        assert_eq!(
            insert.rows,
            [
                vec![Some("1".into()), Some("Война и мир, 'том' 1\nконец".into())],
                vec![Some("2".into()), None],
                vec![Some("3".into()), Some("it's".into())],
            ]
        );
        assert!(super::insert("SET NAMES utf8").is_none());
    }
}
