//! Схема единого индекса `catalog-1`: один набор полей для книг, фильмов, сериалов, игр,
//! картин и людей. Только текст, плотный номер `ord`, вид `kind` и `card`; фасеты, сортировки и
//! ручные места — в слое Б ([`super::sidecar`]).
//!
//! Номера сущностей (`catalog_entity_ords`) и людей (`catalog_person_ords`) — два
//! пространства, поэтому в Tantivy у человека `ord` несёт [`PERSON_FLAG`] (бит 32): документ
//! однозначно определяется одним числом и удаляется одним термом.

use shared::tantivy::{RU_RAW, RU_STEM};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, INDEXED, STORED, STRING,
};
use tantivy::IndexSettings;

/// Версия схемы и анализаторов. Увеличить при любом их изменении: индекс на диске будет
/// пересоздан пустым и построен заново кнопкой в админке или `nexus search rebuild`.
pub const SCHEMA_VERSION: &str = "catalog-5";

/// Вид документа-человека (в `kind` и в выдаче).
pub const PERSON_KIND: &str = "person";

/// Бит «это человек» в поле `ord` Tantivy.
pub const PERSON_FLAG: u64 = 1 << 32;

/// Doc store: Zstd уровня 3, блок 64 КиБ — карточки сжимаются заметно лучше, чем LZ4.
pub fn settings() -> IndexSettings {
    IndexSettings {
        docstore_compression: tantivy::store::Compressor::Zstd(tantivy::store::ZstdCompressor {
            compression_level: Some(3),
        }),
        docstore_blocksize: 64 * 1024,
        ..IndexSettings::default()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Fields {
    /// Вид: `book`, `movie`, …, `person`. Нужен перестройке вида (`delete_term`).
    pub kind: Field,
    /// Номер в слое Б (у людей — с [`PERSON_FLAG`]): колонка и терм одновременно.
    pub ord: Field,
    /// Название, оригинальное название, синонимы (у людей — имя и оригинальное имя).
    pub title: Field,
    /// Нормализованное название целиком — 8-байтным хешем ([`title_key_term`]): точное
    /// совпадение одним термом. Строкой словарь занимал ~53 МБ (3,3 млн названий и имён).
    pub title_key: Field,
    /// Авторы, режиссёры, топ-актёры (без норм длины, слабее названия).
    pub people: Field,
    /// Серия книги, студии, «известен по…» (самый слабый вес).
    pub extra: Field,
    /// Фасеты с большим числом значений одним термом ([`super::sidecar::text_facets`]):
    /// `person:<slug>`, `role:<роль>:<slug>`, `studio:<ключ>`, `studio_slug:<slug>`,
    /// `publisher:<ключ>`, `museum:<ключ>`, `award:<slug>`, `award:<slug>:<год>`. Битовыми
    /// картами в RAM это были бы ~530 тыс. значений и ~100 МБ; здесь — короткий список номеров
    /// на значение с диска.
    pub facet: Field,
    /// Минимальный JSON выдачи (`EntitySummary` / `PersonHit`).
    pub card: Field,
}

fn text(tokenizer: &str, record: IndexRecordOption, norms: bool) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(record)
            .set_fieldnorms(norms),
    )
}

/// Терм `title_key`: FNV-1a 64 от нормализованного названия, 8 байт (little-endian). Стабилен
/// между версиями Rust; совпадение хешей на 3 млн названий — порядка 1 к 4 млн.
pub fn title_key_term(key: &str) -> [u8; 8] {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash.to_le_bytes()
}

pub fn build() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let fields = Fields {
        kind: b.add_text_field("kind", STRING),
        ord: b.add_u64_field("ord", INDEXED | FAST),
        // Позиции нужны только здесь: точная фраза по названию.
        title: b.add_text_field(
            "title",
            text(RU_STEM, IndexRecordOption::WithFreqsAndPositions, true),
        ),
        title_key: b.add_bytes_field("title_key", INDEXED),
        people: b.add_text_field("people", text(RU_RAW, IndexRecordOption::WithFreqs, false)),
        extra: b.add_text_field("extra", text(RU_RAW, IndexRecordOption::Basic, false)),
        facet: b.add_text_field("facet", STRING),
        card: b.add_text_field("card", STORED),
    };
    (b.build(), fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_key_term_is_stable_and_short() {
        assert_eq!(title_key_term("мастер"), title_key_term("мастер"));
        assert_ne!(
            title_key_term("мастер"),
            title_key_term("мастер и маргарита")
        );
        // FNV-1a 64 пустой строки — смещение: формат не должен меняться без смены схемы.
        assert_eq!(
            u64::from_le_bytes(title_key_term("")),
            0xcbf2_9ce4_8422_2325
        );
    }

    #[test]
    fn schema_has_only_the_agreed_fields() {
        let (schema, _) = build();
        let mut names: Vec<&str> = schema.fields().map(|(_, e)| e.name()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "card",
                "extra",
                "facet",
                "kind",
                "ord",
                "people",
                "title",
                "title_key"
            ]
        );
    }
}
