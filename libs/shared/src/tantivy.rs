//! Общие помощники поисковых индексов Tantivy: анализаторы, нормализация ключей, разбор
//! запроса, открытие индекса с версией схемы, поколения каталогов. Без знания предметной
//! области: схемы и запросы живут в модулях.

use std::path::{Path, PathBuf};

use std::collections::HashMap;
use tantivy::directory::MmapDirectory;

use tantivy::query::{BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema};
use tantivy::tokenizer::{
    Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, TextAnalyzer, Token,
    TokenFilter, TokenStream, Tokenizer,
};
use tantivy::{Index, IndexSettings, Score, Searcher, TantivyError, Term};

/// Максимум слов запроса: лишние отбрасываются.
pub const MAX_QUERY_TERMS: usize = 16;
/// Длина запроса в символах: дальше обрезается.
pub const MAX_QUERY_CHARS: usize = 200;
/// Опечатка — только для слов не короче.
pub const FUZZY_MIN_TERM_LEN: usize = 4;
/// Автодополнение — только для слов не короче.
pub const PREFIX_MIN_LEN: usize = 3;
/// Токены длиннее (в байтах) — мусор: в индекс не идут.
pub const MAX_TOKEN_BYTES: usize = 80;

pub const RU_STEM: &str = "ru_stem";
pub const RU_RAW: &str = "ru_raw";
/// Файл с версией схемы рядом с индексом.
pub const SCHEMA_FILE: &str = "nexus-schema";

/// Служебные слова: опечатки и автодополнение по ним — чистый шум.
pub const STOP_WORDS_RU: &[&str] = &[
    "и",
    "в",
    "во",
    "на",
    "не",
    "что",
    "с",
    "со",
    "а",
    "по",
    "за",
    "из",
    "к",
    "ко",
    "о",
    "об",
    "от",
    "для",
    "же",
    "бы",
    "ли",
    "или",
    "но",
    "да",
    "ни",
    "у",
    "при",
    "про",
    "без",
    "над",
    "под",
    "это",
    "как",
    "то",
    "все",
    "чтобы",
    "который",
    "когда",
    "если",
    "этот",
    "весь",
];

/// Анализатор названий, авторов и серий: слова, нижний регистр, `ё` → `е`, стеммер.
pub fn ru_stem_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
        .filter(LowerCaser)
        .filter(YoFilter)
        .filter(Stemmer::new(Language::Russian))
        .build()
}

/// То же без стеммера. Фильтры до стеммера у обоих одинаковые — токены запроса выравниваются
/// один к одному ([`BookIndex::query_terms`]).
pub fn ru_raw_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
        .filter(LowerCaser)
        .filter(YoFilter)
        .build()
}

/// Фильтр токенов «ё → е» (после `LowerCaser`): «ёжик»/«ежик» совпадают как точные термы и не
/// тратят единственную разрешённую опечатку.
#[derive(Clone)]
pub struct YoFilter;

impl TokenFilter for YoFilter {
    type Tokenizer<T: Tokenizer> = YoFilterWrapper<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> Self::Tokenizer<T> {
        YoFilterWrapper(tokenizer)
    }
}

#[derive(Clone)]
pub struct YoFilterWrapper<T>(T);

impl<T: Tokenizer> Tokenizer for YoFilterWrapper<T> {
    type TokenStream<'a> = YoFilterStream<T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        YoFilterStream(self.0.token_stream(text))
    }
}

pub struct YoFilterStream<T>(T);

impl<T: TokenStream> TokenStream for YoFilterStream<T> {
    fn advance(&mut self) -> bool {
        if !self.0.advance() {
            return false;
        }
        let text = &mut self.0.token_mut().text;
        if !text.is_ascii() && text.contains('ё') {
            *text = text.replace('ё', "е");
        }
        true
    }

    fn token(&self) -> &Token {
        self.0.token()
    }

    fn token_mut(&mut self) -> &mut Token {
        self.0.token_mut()
    }
}

/// Ключ названия, имени и издательства: нижний регистр, любая последовательность не букв и не
/// цифр — один пробел, `ё` → `е`. Одна функция при индексации и при запросе.
pub fn normalize_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut gap = false;
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            out.push(if c == 'ё' { 'е' } else { c });
        } else {
            gap = true;
        }
    }
    out
}

/// Сколько слов словаря максимум подставляется вместо префикса (самые частые).
pub const PREFIX_MAX_TERMS: usize = 64;
/// Сколько слов словаря просматривать на сегмент в поисках частых.
pub const PREFIX_SCAN_LIMIT: usize = 4_000;
pub const BOOST_PREFIX: Score = 1.5;

/// Слово запроса: исходная форма и стем (выравниваются один к одному).
#[derive(Debug, Clone)]
pub struct QueryTerm {
    pub raw: String,
    pub stem: String,
}

/// Какие поля участвуют в текстовом запросе. `stem` — поля `ru_stem`, `raw` — `ru_raw`,
/// `key` — STRING-поле с `normalize_key` целиком (точное совпадение всей строки).
#[derive(Clone, Copy)]
pub struct TextSpec<'a> {
    pub stem: &'a [(Field, Score)],
    pub raw: &'a [(Field, Score)],
    pub key: Option<(Field, Score)>,
}

/// Слова запроса: стемы — анализатором `ru_stem`, исходные — `ru_raw`.
pub fn analyze(index: &Index, query: &str) -> Vec<QueryTerm> {
    let query: String = query.chars().take(MAX_QUERY_CHARS).collect();
    let tokens = |name: &str| -> Vec<String> {
        let Some(mut analyzer) = index.tokenizers().get(name) else {
            return Vec::new();
        };
        let mut stream = analyzer.token_stream(&query);
        let mut out = Vec::new();
        while let Some(token) = stream.next() {
            if out.len() == MAX_QUERY_TERMS {
                break;
            }
            out.push(token.text.clone());
        }
        out
    };
    let raws = tokens(RU_RAW);
    tokens(RU_STEM)
        .into_iter()
        .enumerate()
        .map(|(i, stem)| QueryTerm {
            raw: raws.get(i).cloned().unwrap_or_else(|| stem.clone()),
            stem,
        })
        .collect()
}

fn term_q(field: Field, text: &str, boost: Score) -> Box<dyn Query> {
    let q = TermQuery::new(
        Term::from_field_text(field, text),
        IndexRecordOption::WithFreqs,
    );
    Box::new(BoostQuery::new(Box::new(q), boost))
}

/// Автодополнение: ИЛИ самых частых слов словаря `fields`, начинающихся с `prefix`.
pub fn prefix_query(
    searcher: &Searcher,
    fields: &[(Field, Score)],
    prefix: &str,
) -> Result<Option<Box<dyn Query>>, TantivyError> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for (field, boost) in fields {
        let mut freq: HashMap<Vec<u8>, u64> = HashMap::new();
        for segment in searcher.segment_readers() {
            let inverted = segment.inverted_index(*field)?;
            let mut stream = inverted
                .terms()
                .range()
                .ge(prefix.as_bytes())
                .into_stream()
                .map_err(TantivyError::from)?;
            let mut scanned = 0;
            while scanned < PREFIX_SCAN_LIMIT && stream.advance() {
                let key = stream.key();
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                scanned += 1;
                *freq.entry(key.to_vec()).or_default() += u64::from(stream.value().doc_freq);
            }
        }
        let mut words: Vec<(Vec<u8>, u64)> = freq.into_iter().collect();
        words.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        words.truncate(PREFIX_MAX_TERMS);
        for (word, _) in words {
            if let Ok(text) = String::from_utf8(word) {
                clauses.push((Occur::Should, term_q(*field, &text, *boost * BOOST_PREFIX)));
            }
        }
    }
    Ok((!clauses.is_empty()).then(|| Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>))
}

/// Слова без служебных (если остались другие).
fn significant(terms: Vec<QueryTerm>) -> Vec<QueryTerm> {
    if terms.iter().all(|t| is_stop_word(&t.raw)) {
        return terms;
    }
    terms
        .into_iter()
        .filter(|t| !is_stop_word(&t.raw))
        .collect()
}

/// Строгий запрос: каждое слово обязано встретиться (стем или исходная форма), последнее,
/// если недописано, ещё и как префикс; точное совпадение `key` поднимает выше. `None` — слов нет.
pub fn strict_query(
    index: &Index,
    searcher: &Searcher,
    query: &str,
    spec: &TextSpec<'_>,
) -> Result<Option<Box<dyn Query>>, TantivyError> {
    let all = analyze(index, query);
    if all.is_empty() {
        return Ok(None);
    }
    let unfinished = query.chars().last().is_some_and(char::is_alphanumeric);
    let last_raw = all.last().map(|t| t.raw.clone());
    let terms = significant(all);
    let mut must: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for term in &terms {
        let mut group: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (f, b) in spec.stem {
            group.push((Occur::Should, term_q(*f, &term.stem, *b)));
        }
        for (f, b) in spec.raw {
            group.push((Occur::Should, term_q(*f, &term.raw, *b)));
        }
        if unfinished
            && last_raw.as_deref() == Some(term.raw.as_str())
            && term.raw.chars().count() >= PREFIX_MIN_LEN
            && !is_stop_word(&term.raw)
        {
            if let Some(prefix) = prefix_query(searcher, spec.raw, &term.raw)? {
                group.push((Occur::Should, prefix));
            }
        }
        must.push((Occur::Must, Box::new(BooleanQuery::new(group))));
    }
    if let Some((field, boost)) = spec.key {
        let key = normalize_key(query);
        if !key.is_empty() {
            must.push((Occur::Should, term_q(field, &key, boost)));
        }
    }
    Ok(Some(Box::new(BooleanQuery::new(must))))
}

/// Запрос с опечатками (расстояние 1 для слов не короче [`FUZZY_MIN_TERM_LEN`]); служебные слова
/// ищутся точно. Запасной вариант, когда строгий ничего не нашёл.
pub fn fuzzy_query(index: &Index, query: &str, spec: &TextSpec<'_>) -> Option<Box<dyn Query>> {
    let terms = significant(analyze(index, query));
    if terms.is_empty() {
        return None;
    }
    let mut must: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for term in &terms {
        let mut group: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (f, b) in spec.raw {
            let t = Term::from_field_text(*f, &term.raw);
            let q: Box<dyn Query> =
                if !is_stop_word(&term.raw) && term.raw.chars().count() >= FUZZY_MIN_TERM_LEN {
                    Box::new(FuzzyTermQuery::new(t, 1, true))
                } else {
                    Box::new(TermQuery::new(t, IndexRecordOption::WithFreqs))
                };
            group.push((Occur::Should, Box::new(BoostQuery::new(q, *b * 0.5))));
        }
        must.push((Occur::Must, Box::new(BooleanQuery::new(group))));
    }
    Some(Box::new(BooleanQuery::new(must)))
}

/// Регистрирует `ru_stem`/`ru_raw`: Tantivy хранит в meta.json только имя анализатора.
pub fn register_tokenizers(index: &Index) {
    index.tokenizers().register(RU_STEM, ru_stem_analyzer());
    index.tokenizers().register(RU_RAW, ru_raw_analyzer());
}

pub fn is_stop_word(raw: &str) -> bool {
    STOP_WORDS_RU.contains(&raw)
}

/// Слова запроса в нормализованном виде: запрос обрезан до `MAX_QUERY_CHARS`, слов не больше
/// `MAX_QUERY_TERMS`.
pub fn query_terms(q: &str) -> Vec<String> {
    let q: String = q.chars().take(MAX_QUERY_CHARS).collect();
    normalize_key(&q)
        .split(' ')
        .filter(|w| !w.is_empty())
        .take(MAX_QUERY_TERMS)
        .map(str::to_owned)
        .collect()
}

/// Индекс в `dir`; нет или собран по другой версии схемы — создаётся пустой (старые файлы
/// удаляются). Анализаторы зарегистрированы.
pub fn open_versioned(dir: &Path, schema: Schema, version: &str) -> Result<Index, TantivyError> {
    std::fs::create_dir_all(dir)?;
    let marker = dir.join(SCHEMA_FILE);
    let current = std::fs::read_to_string(&marker).is_ok_and(|v| v.trim() == version);
    if !current {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_file() {
                std::fs::remove_file(&path)?;
            }
        }
    }
    let directory = MmapDirectory::open(dir).map_err(TantivyError::from)?;
    let index = if Index::exists(&directory)? {
        Index::open(directory)?
    } else {
        Index::create(directory, schema, IndexSettings::default())?
    };
    if !current {
        std::fs::write(&marker, version)?;
    }
    register_tokenizers(&index);
    Ok(index)
}

/// Сливает все сегменты индекса каталога `dir` в один и удаляет лишние файлы.
pub fn merge_all(dir: &Path) -> Result<(), TantivyError> {
    let index = Index::open_in_dir(dir)?;
    let mut writer: tantivy::IndexWriter = index.writer_with_num_threads(1, 50_000_000)?;
    let ids = index.searchable_segment_ids()?;
    if ids.len() > 1 {
        writer.merge(&ids).wait()?;
    }
    writer.garbage_collect_files().wait()?;
    Ok(())
}

/// Тяжёлая синхронная работа индекса вне async-потока.
pub async fn blocking<T, E, F>(f: F) -> Result<T, E>
where
    F: FnOnce() -> Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static + From<TantivyError>,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => Err(TantivyError::SystemError(format!("blocking task: {e}")).into()),
    }
}

/// Поколения каталогов индекса: `<root>/<name>.v<ms>` и указатель `<root>/<name>.current`.
pub mod generations {
    use super::*;

    fn pointer(root: &Path, name: &str) -> PathBuf {
        root.join(format!("{name}.current"))
    }

    /// Каталог текущего поколения; указателя нет — `<root>/<name>.v0` (создаётся при открытии).
    pub fn current(root: &Path, name: &str) -> PathBuf {
        match std::fs::read_to_string(pointer(root, name)) {
            Ok(dir) if !dir.trim().is_empty() => root.join(dir.trim()),
            _ => root.join(format!("{name}.v0")),
        }
    }

    /// Новый (пустой, ещё не текущий) каталог поколения.
    pub fn fresh(root: &Path, name: &str) -> PathBuf {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        root.join(format!("{name}.v{ms}"))
    }

    /// Атомарно делает `new_dir` текущим и удаляет поколения старше двух (текущее + прежнее).
    pub fn switch(root: &Path, name: &str, new_dir: &Path) -> std::io::Result<()> {
        let dir_name = new_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| std::io::Error::other("bad generation dir"))?
            .to_owned();
        let ptr = pointer(root, name);
        let tmp = root.join(format!("{name}.current.tmp"));
        std::fs::write(&tmp, &dir_name)?;
        std::fs::rename(&tmp, &ptr)?;
        let prefix = format!("{name}.v");
        let mut gens: Vec<(u128, PathBuf)> = std::fs::read_dir(root)?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let n = e.file_name().into_string().ok()?;
                let ms = n.strip_prefix(&prefix)?.parse().ok()?;
                Some((ms, e.path()))
            })
            .collect();
        gens.sort_by_key(|(ms, _)| std::cmp::Reverse(*ms));
        for (_, path) in gens.into_iter().skip(2) {
            let _ = std::fs::remove_dir_all(path);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_keys() {
        assert_eq!(normalize_key("  «Ёжик» — в тумане!  "), "ежик в тумане");
    }

    #[test]
    fn stop_words_and_terms() {
        assert!(is_stop_word("на"));
        assert!(!is_stop_word("матрица"));
        let long = "слово ".repeat(100);
        assert!(query_terms(&long).len() <= MAX_QUERY_TERMS);
    }

    #[test]
    fn switch_keeps_two_generations() {
        let root = std::env::temp_dir().join(format!("nexus-gen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let r = root.as_path();
        for ms in [1u32, 2, 3] {
            let dir = r.join(format!("works.v{ms}"));
            std::fs::create_dir(&dir).unwrap();
            generations::switch(r, "works", &dir).unwrap();
        }
        assert_eq!(generations::current(r, "works"), r.join("works.v3"));
        assert!(!r.join("works.v1").exists());
        assert!(r.join("works.v2").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
