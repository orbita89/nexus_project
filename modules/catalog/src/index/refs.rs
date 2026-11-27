//! Справочники для ответов поиска, которых нет в документах Tantivy: студии (slug и логотип по
//! названию) и теги (название, вид, описание по slug). Без них подсказки фильтров ходили бы в
//! БД на каждый запрос.
//!
//! Файл `sidecar-refs.cols` ([`super::columns`]): у каждой таблицы — отсортированные ключи и
//! значения строками (байты подряд + смещения). Читается через mmap, поиск — двоичный: в куче
//! ничего, кроме заголовка файла.

use std::path::Path;

use super::columns::{self, Column, Columns};

pub const REFS_FILE: &str = "sidecar-refs.cols";

/// Разделитель полей значения: в названиях, slug'ах и адресах его не бывает.
const SEP: char = '\u{1f}';

/// Таблица «строка → строка»: ключи отсортированы.
#[derive(Default)]
pub struct StrMap {
    keys: Column<u8>,
    key_ends: Column<u32>,
    values: Column<u8>,
    value_ends: Column<u32>,
}

impl StrMap {
    fn build(mut rows: Vec<(String, String)>) -> Self {
        rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        rows.dedup_by(|a, b| a.0 == b.0);
        let (mut keys, mut key_ends, mut values, mut value_ends) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (key, value) in rows {
            keys.extend_from_slice(key.as_bytes());
            key_ends.push(keys.len() as u32);
            values.extend_from_slice(value.as_bytes());
            value_ends.push(values.len() as u32);
        }
        Self {
            keys: keys.into(),
            key_ends: key_ends.into(),
            values: values.into(),
            value_ends: value_ends.into(),
        }
    }

    pub fn len(&self) -> usize {
        self.key_ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.key_ends.is_empty()
    }

    fn slice<'a>(bytes: &'a [u8], ends: &[u32], i: usize) -> &'a [u8] {
        let start = if i == 0 { 0 } else { ends[i - 1] as usize };
        &bytes[start..ends[i] as usize]
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match Self::slice(&self.keys, &self.key_ends, mid).cmp(key.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    return std::str::from_utf8(Self::slice(&self.values, &self.value_ends, mid))
                        .ok();
                }
            }
        }
        None
    }

    fn bytes(&self) -> u64 {
        (self.keys.len() + self.values.len() + 4 * (self.key_ends.len() + self.value_ends.len()))
            as u64
    }
}

/// Студия для подсказки фильтра.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StudioRef {
    pub slug: String,
    pub logo_url: Option<String>,
}

/// Тег для подсказки фильтра.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRef {
    pub name: String,
    pub kind: String,
    pub description: Option<String>,
}

#[derive(Default)]
pub struct Refs {
    studios: StrMap,
    tags: StrMap,
}

/// Строки студий: название, slug, логотип.
pub type StudioRow = (String, String, Option<String>);
/// Строки тегов: slug, название, вид, описание.
pub type TagRow = (String, String, String, Option<String>);

fn clean(s: &str) -> String {
    s.replace(SEP, " ")
}

impl Refs {
    pub fn build(studios: Vec<StudioRow>, tags: Vec<TagRow>) -> Self {
        let studios = studios
            .into_iter()
            .map(|(name, slug, logo)| {
                (
                    name,
                    format!(
                        "{}{SEP}{}",
                        clean(&slug),
                        clean(logo.as_deref().unwrap_or(""))
                    ),
                )
            })
            .collect();
        let tags = tags
            .into_iter()
            .map(|(slug, name, kind, description)| {
                (
                    slug,
                    format!(
                        "{}{SEP}{}{SEP}{}",
                        clean(&name),
                        clean(&kind),
                        clean(description.as_deref().unwrap_or(""))
                    ),
                )
            })
            .collect();
        Self {
            studios: StrMap::build(studios),
            tags: StrMap::build(tags),
        }
    }

    pub fn studio(&self, name: &str) -> Option<StudioRef> {
        let mut parts = self.studios.get(name)?.split(SEP);
        let slug = parts.next()?.to_owned();
        let logo = parts.next().filter(|l| !l.is_empty()).map(str::to_owned);
        Some(StudioRef {
            slug,
            logo_url: logo,
        })
    }

    pub fn tag(&self, slug: &str) -> Option<TagRef> {
        let mut parts = self.tags.get(slug)?.split(SEP);
        Some(TagRef {
            name: parts.next()?.to_owned(),
            kind: parts.next()?.to_owned(),
            description: parts.next().filter(|d| !d.is_empty()).map(str::to_owned),
        })
    }

    /// Студий и тегов, байт в файле.
    pub fn stats(&self) -> (usize, usize, u64) {
        (
            self.studios.len(),
            self.tags.len(),
            self.studios.bytes() + self.tags.bytes(),
        )
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let cols: [&[u8]; 8] = [
            &*self.studios.keys,
            bytemuck::cast_slice(&self.studios.key_ends),
            &*self.studios.values,
            bytemuck::cast_slice(&self.studios.value_ends),
            &*self.tags.keys,
            bytemuck::cast_slice(&self.tags.key_ends),
            &*self.tags.values,
            bytemuck::cast_slice(&self.tags.value_ends),
        ];
        columns::write(&dir.join(REFS_FILE), &cols)
    }

    /// Открыть файл; нет файла — пустые справочники (подсказки без slug'ов и описаний).
    pub fn load(dir: &Path) -> std::io::Result<Self> {
        let c = Columns::open(dir, REFS_FILE)?;
        let map = |at: usize| -> std::io::Result<StrMap> {
            Ok(StrMap {
                keys: c.get(at)?,
                key_ends: c.get(at + 1)?,
                values: c.get(at + 2)?,
                value_ends: c.get(at + 3)?,
            })
        };
        Ok(Self {
            studios: map(0)?,
            tags: map(4)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_round_trip_through_file() {
        let refs = Refs::build(
            vec![
                ("Pixar".into(), "pixar".into(), Some("/logo.png".into())),
                ("A24".into(), "a24".into(), None),
            ],
            vec![(
                "drama".into(),
                "Драма".into(),
                "genre".into(),
                Some("Про чувства".into()),
            )],
        );
        let dir = std::env::temp_dir().join(format!("nexus-refs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        refs.save(&dir).unwrap();
        let refs = Refs::load(&dir).unwrap();
        assert_eq!(
            refs.studio("Pixar"),
            Some(StudioRef {
                slug: "pixar".into(),
                logo_url: Some("/logo.png".into())
            })
        );
        assert_eq!(refs.studio("A24").unwrap().logo_url, None);
        assert_eq!(refs.studio("Nope"), None);
        assert_eq!(refs.tag("drama").unwrap().name, "Драма");
        assert_eq!(
            refs.tag("drama").unwrap().description.as_deref(),
            Some("Про чувства")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
