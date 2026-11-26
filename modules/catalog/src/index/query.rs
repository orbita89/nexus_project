//! Разбор текстового запроса и построение клауз Tantivy — одна логика для всех видов (бывшая
//! `books/index.rs`): слова запроса объединяются через И, поля одного слова — через ИЛИ;
//! русский стеммер, `ё` = `е`; недописанное последнее слово дополняется по словарю (не более
//! [`PREFIX_MAX_TERMS`] самых частых слов); опечатки — только если строгий запрос ничего не нашёл;
//! точное название и точная фраза стоят выше совпадений по отдельным словам.
//!
//! Поля: `title` (стемы, позиции) — название; `people` и `extra` (сырые слова) — авторы и
//! серия/студии; `title_key` — название целиком.

use std::collections::HashMap;

use shared::tantivy::{normalize_key, RU_RAW, RU_STEM, STOP_WORDS_RU};
use tantivy::query::{
    BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{Index, Score, Searcher, TantivyError, Term};

use super::schema::Fields;

/// Максимум слов запроса: лишние отбрасываются.
const MAX_QUERY_TERMS: usize = 16;
/// Длина запроса в символах: дальше обрезается.
pub const MAX_QUERY_CHARS: usize = 200;
/// Опечатка — только для слов не короче (у трёхбуквенного слова соседом оказывается почти любое).
const FUZZY_MIN_TERM_LEN: usize = 4;
/// Запрос короче (в символах) не ищется вовсе: одна-две буквы совпадают с тысячами слов.
pub const MIN_QUERY_CHARS: usize = 3;
/// Автодополнение — только для слов не короче.
const PREFIX_MIN_LEN: usize = 3;
/// Сколько слов словаря максимум подставляется вместо префикса (самые частые).
const PREFIX_MAX_TERMS: usize = 64;
/// Сколько слов словаря просматривать на сегмент в поисках частых.
const PREFIX_SCAN_LIMIT: usize = 4_000;

// Веса клауз: точное название (несколько слов) > фраза > точное название (одно слово) > слово
// в названии > автодополнение > слово в имени > серия/студия > опечатка.
const BOOST_TITLE_KEY_MULTI: Score = 12.0;
const BOOST_TITLE_PHRASE: Score = 4.0;
const BOOST_TITLE_KEY_SINGLE: Score = 4.0;
const BOOST_FALLBACK_STRICT: Score = 4.0;
const BOOST_TITLE_TERM: Score = 2.0;
const BOOST_PREFIX: Score = 1.5;
const BOOST_PEOPLE_TERM: Score = 1.0;
const BOOST_EXTRA_TERM: Score = 0.8;
const BOOST_TITLE_FUZZY: Score = 0.5;
const BOOST_PEOPLE_FUZZY: Score = 0.3;

/// Слово запроса: как напечатано (нижний регистр, `ё` → `е`) и после стеммера.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTerm {
    pub raw: String,
    pub stem: String,
}

pub struct Parsed {
    pub terms: Vec<QueryTerm>,
    /// Автодополнение последнего слова — `None`, если слово дописано или коротко.
    pub prefix: Option<Box<dyn Query>>,
    /// То же автодополнение только по словам названия — для [`QueryBuilder::title_only`].
    pub title_prefix: Option<Box<dyn Query>>,
}

/// Строгий запрос по тексту и резервный — с одной опечаткой.
/// Строгий запрос, резервный (с опечатками) и «все слова в названии» (для релевантности).
type Clauses = Vec<(Occur, Box<dyn Query>)>;
/// Автодополнение по названию и людям и только по названию.
type PrefixQueries = (Option<Box<dyn Query>>, Option<Box<dyn Query>>);

pub type TextQueries = (Box<dyn Query>, Option<Box<dyn Query>>, Box<dyn Query>);

pub fn is_stop_word(raw: &str) -> bool {
    STOP_WORDS_RU.contains(&raw)
}

/// Слова запроса: стемы — анализатором `title`, исходные — `ru_raw`. Стеммер не добавляет и не
/// удаляет токены, поэтому списки выравниваются один к одному.
pub fn query_terms(index: &Index, query: &str) -> Vec<QueryTerm> {
    let tokens = |name: &str| -> Vec<String> {
        let Some(mut analyzer) = index.tokenizers().get(name) else {
            return Vec::new();
        };
        let mut stream = analyzer.token_stream(query);
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

pub struct QueryBuilder<'a> {
    pub index: &'a Index,
    pub f: &'a Fields,
}

impl QueryBuilder<'_> {
    /// Строгий и резервный запросы; `None` — слов в запросе нет (пусто или одни знаки).
    pub fn build(
        &self,
        searcher: &Searcher,
        text: &str,
    ) -> Result<Option<TextQueries>, TantivyError> {
        let parsed = self.parse(searcher, text)?;
        let Some(strict) = self.strict(text, &parsed) else {
            return Ok(None);
        };
        let fallback = self.fallback(&parsed, strict.as_ref());
        let title = self.title_only(&parsed);
        Ok(Some((strict, fallback, title)))
    }

    fn parse(&self, searcher: &Searcher, query: &str) -> Result<Parsed, TantivyError> {
        let terms = query_terms(self.index, query);
        let (prefix, title_prefix) = match terms.last() {
            // Слово недописано, если запрос не кончается пробелом или знаком.
            Some(last)
                if query.chars().last().is_some_and(char::is_alphanumeric)
                    && !is_stop_word(&last.raw)
                    && last.raw.chars().count() >= PREFIX_MIN_LEN =>
            {
                self.prefix_query(searcher, last)?
            }
            _ => (None, None),
        };
        Ok(Parsed {
            terms,
            prefix,
            title_prefix,
        })
    }

    /// Автодополнение: слова словаря `title` (стемы) и `people` (сырые), начинающиеся с недописанного
    /// слова. Словарь просматривается диапазоном `[prefix, …)` не дальше [`PREFIX_SCAN_LIMIT`]
    /// слов на сегмент, в запрос идут [`PREFIX_MAX_TERMS`] самых частых. Не edge n-gram (раздувает
    /// индекс в разы) и не `RegexQuery`. Стем недописанного слова тоже берётся как префикс: «преступлени»
    /// длиннее основы «преступлен».
    fn prefix_query(
        &self,
        searcher: &Searcher,
        word: &QueryTerm,
    ) -> Result<PrefixQueries, TantivyError> {
        let mut prefixes = vec![word.raw.as_str()];
        if word.stem != word.raw {
            prefixes.push(word.stem.as_str());
        }
        let title = self.prefix_terms(searcher, self.f.title, &prefixes)?;
        let people = self.prefix_terms(searcher, self.f.people, &prefixes[..1])?;
        let boosted = |clauses: Vec<(Occur, Box<dyn Query>)>| -> Option<Box<dyn Query>> {
            (!clauses.is_empty()).then(|| {
                Box::new(BoostQuery::new(Box::new(BooleanQuery::new(clauses)), BOOST_PREFIX))
                    as Box<dyn Query>
            })
        };
        let title_only = boosted(title.iter().map(|(o, q)| (*o, q.box_clone())).collect());
        let both = boosted(title.into_iter().chain(people).collect());
        Ok((both, title_only))
    }

    /// Самые частые слова словаря `field`, начинающиеся с одного из `prefixes`, — термами ИЛИ.
    fn prefix_terms(
        &self,
        searcher: &Searcher,
        field: Field,
        prefixes: &[&str],
    ) -> Result<Clauses, TantivyError> {
        let mut freq: HashMap<Vec<u8>, u64> = HashMap::new();
        for prefix in prefixes {
            for segment in searcher.segment_readers() {
                let inverted = segment.inverted_index(field)?;
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
        }
        let mut words: Vec<(Vec<u8>, u64)> = freq.into_iter().collect();
        words.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        words.truncate(PREFIX_MAX_TERMS);
        Ok(words
            .into_iter()
            .filter_map(|(word, _)| {
                let word = std::str::from_utf8(&word).ok()?;
                let query: Box<dyn Query> = Box::new(TermQuery::new(
                    Term::from_field_text(field, word),
                    IndexRecordOption::Basic,
                ));
                Some((Occur::Should, query))
            })
            .collect())
    }

    /// Обязательная группа одного слова: ИЛИ совпадений по названию, людям и серии, опечатка в
    /// имени, автодополнение и — только в резервном запросе (`title_fuzzy`) — опечатка в названии.
    fn per_term(
        &self,
        term: &QueryTerm,
        title_fuzzy: bool,
        prefix: Option<&dyn Query>,
    ) -> (Occur, Box<dyn Query>) {
        let f = self.f;
        let boosted = |query: Box<dyn Query>, boost: Score| -> (Occur, Box<dyn Query>) {
            (Occur::Should, Box::new(BoostQuery::new(query, boost)))
        };
        let exact = |field: Field, text: &str, record: IndexRecordOption| -> Box<dyn Query> {
            Box::new(TermQuery::new(Term::from_field_text(field, text), record))
        };
        let mut clauses = vec![
            boosted(
                exact(
                    f.title,
                    &term.stem,
                    IndexRecordOption::WithFreqsAndPositions,
                ),
                BOOST_TITLE_TERM,
            ),
            boosted(
                exact(f.people, &term.raw, IndexRecordOption::WithFreqs),
                BOOST_PEOPLE_TERM,
            ),
            boosted(
                exact(f.extra, &term.raw, IndexRecordOption::Basic),
                BOOST_EXTRA_TERM,
            ),
        ];
        // Опечатки — только в резервном запросе: нечёткий поиск обходит весь словарь поля в
        // каждом сегменте (десятки мс на слово), а строгий запрос выполняется всегда.
        if title_fuzzy && !is_stop_word(&term.raw) && term.raw.chars().count() >= FUZZY_MIN_TERM_LEN {
            // `true` — перестановка двух соседних букв стоит одну правку.
            clauses.push(boosted(
                Box::new(FuzzyTermQuery::new(
                    Term::from_field_text(f.people, &term.raw),
                    1,
                    true,
                )),
                BOOST_PEOPLE_FUZZY,
            ));
            {
                // Название хранится стемами: опечатка ищется среди них.
                clauses.push(boosted(
                    Box::new(FuzzyTermQuery::new(
                        Term::from_field_text(f.title, &term.stem),
                        1,
                        true,
                    )),
                    BOOST_TITLE_FUZZY,
                ));
            }
        }
        if let Some(prefix) = prefix {
            clauses.push((Occur::Should, prefix.box_clone()));
        }
        (Occur::Must, Box::new(BooleanQuery::new(clauses)))
    }

    /// Строгий запрос: все слова обязательны; сверху — точная фраза и точный ключ названия.
    fn strict(&self, query: &str, parsed: &Parsed) -> Option<Box<dyn Query>> {
        let terms = &parsed.terms;
        if terms.is_empty() {
            return None;
        }
        let last = terms.len() - 1;
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = terms
            .iter()
            .enumerate()
            .map(|(index, term)| {
                let prefix = (index == last)
                    .then_some(parsed.prefix.as_deref())
                    .flatten();
                self.per_term(term, false, prefix)
            })
            .collect();
        // Фраза — только от двух слов: для одного она задвоила бы вклад терма.
        if terms.len() >= 2 {
            let phrase: Vec<Term> = terms
                .iter()
                .map(|term| Term::from_field_text(self.f.title, &term.stem))
                .collect();
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(PhraseQuery::new(phrase)),
                    BOOST_TITLE_PHRASE,
                )),
            ));
        }
        let key = normalize_key(query);
        if !key.is_empty() {
            // Однословному точному названию вес меньше: «Маргарита» не должна подавить
            // «Мастер и Маргарита».
            let boost = if terms.len() >= 2 {
                BOOST_TITLE_KEY_MULTI
            } else {
                BOOST_TITLE_KEY_SINGLE
            };
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(
                        Term::from_field_bytes(self.f.title_key, &crate::index::schema::title_key_term(&key)),
                        IndexRecordOption::Basic,
                    )),
                    boost,
                )),
            ));
        }
        Some(Box::new(BooleanQuery::new(clauses)))
    }

    /// Резервный запрос: тот же строгий, но одному слову названия разрешена опечатка. На каждое
    /// подходящее слово — свой вариант, варианты — через ИЛИ; сам строгий добавлен ради ранга.
    fn fallback(&self, parsed: &Parsed, strict: &dyn Query) -> Option<Box<dyn Query>> {
        let terms = &parsed.terms;
        let fuzzy: Vec<usize> = terms
            .iter()
            .enumerate()
            .filter(|(_, term)| {
                !is_stop_word(&term.raw) && term.raw.chars().count() >= FUZZY_MIN_TERM_LEN
            })
            .map(|(index, _)| index)
            .collect();
        if fuzzy.is_empty() {
            return None;
        }
        let last = terms.len() - 1;
        let mut variants: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(fuzzy.len() + 1);
        for &fixed in &fuzzy {
            let clauses: Vec<(Occur, Box<dyn Query>)> = terms
                .iter()
                .enumerate()
                .map(|(index, term)| {
                    let prefix = (index == last)
                        .then_some(parsed.prefix.as_deref())
                        .flatten();
                    self.per_term(term, index == fixed, prefix)
                })
                .collect();
            variants.push((Occur::Should, Box::new(BooleanQuery::new(clauses))));
        }
        variants.push((
            Occur::Should,
            Box::new(BoostQuery::new(strict.box_clone(), BOOST_FALLBACK_STRICT)),
        ));
        Some(Box::new(BooleanQuery::new(variants)))
    }

    /// Запрос «название равно ключу запроса» — для точного совпадения в релевантности.
    /// Совпадения только по названию: каждое слово — в названии (последнее недописанное — и
    /// автодополнением по названию). Делит выдачу на «нашлось в названии» и «только по людям,
    /// серии…»: популярность не поднимает второе над первым.
    fn title_only(&self, parsed: &Parsed) -> Box<dyn Query> {
        let n = parsed.terms.len();
        let clauses: Vec<(Occur, Box<dyn Query>)> = parsed
            .terms
            .iter()
            .enumerate()
            .map(|(i, term)| {
                let mut any: Vec<(Occur, Box<dyn Query>)> = vec![(
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.f.title, &term.stem),
                        IndexRecordOption::Basic,
                    )),
                )];
                if i + 1 == n {
                    if let Some(prefix) = &parsed.title_prefix {
                        any.push((Occur::Should, prefix.box_clone()));
                    }
                }
                (
                    Occur::Must,
                    Box::new(BooleanQuery::new(any)) as Box<dyn Query>,
                )
            })
            .collect();
        Box::new(BooleanQuery::new(clauses))
    }

    pub fn title_key(&self, key: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_bytes(self.f.title_key, &crate::index::schema::title_key_term(key)),
            IndexRecordOption::Basic,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema;

    #[test]
    fn terms_are_aligned_stems_and_raw_words() {
        let (schema, _) = schema::build();
        let index = Index::create_in_ram(schema);
        shared::tantivy::register_tokenizers(&index);
        let terms = query_terms(&index, "Ёжики в тумане");
        assert_eq!(terms.len(), 3);
        assert_eq!(terms[0].raw, "ежики");
        assert_ne!(terms[0].stem, terms[0].raw);
        assert!(is_stop_word("в"));
    }
}
