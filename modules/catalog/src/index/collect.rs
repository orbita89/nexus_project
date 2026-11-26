//! Сборщик совпадений [`OrdHits`]: пары (номер, оценка) только для документов из фильтра —
//! общий для всех видов.

use std::sync::Arc;

use roaring::RoaringBitmap;
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::ColumnValues;
use tantivy::{DocId, Score, SegmentOrdinal, SegmentReader};

use super::schema::PERSON_FLAG;

/// Чьи номера собирает сборщик: сущностей или людей (у людей в колонке стоит [`PERSON_FLAG`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    Entity,
    Person,
}

impl Space {
    /// Номер из колонки в номер слоя Б; `None` — документ из другого пространства.
    pub fn local(self, raw: u64) -> Option<u32> {
        match self {
            Self::Entity if raw & PERSON_FLAG == 0 => u32::try_from(raw).ok(),
            Self::Person if raw & PERSON_FLAG != 0 => u32::try_from(raw & !PERSON_FLAG).ok(),
            _ => None,
        }
    }

    /// Значение колонки и терма `ord` для номера слоя Б.
    pub fn raw(self, ord: u32) -> u64 {
        match self {
            Self::Entity => u64::from(ord),
            Self::Person => u64::from(ord) | PERSON_FLAG,
        }
    }
}

/// Пары (номер, оценка) для документов из `filter` (фасеты слоя Б проверяются здесь же, а не
/// после «лучших N»: узкий фильтр не теряет совпадений). Номер берётся из колонки `ord`,
/// документы не читаются.
pub struct OrdHits {
    pub filter: Arc<RoaringBitmap>,
    pub space: Space,
}

pub struct OrdHitsSegment {
    ords: Arc<dyn ColumnValues<u64>>,
    filter: Arc<RoaringBitmap>,
    space: Space,
    hits: Vec<(u32, Score)>,
}

impl Collector for OrdHits {
    type Fruit = Vec<(u32, Score)>;
    type Child = OrdHitsSegment;

    fn for_segment(
        &self,
        _segment: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        Ok(OrdHitsSegment {
            ords: reader
                .fast_fields()
                .u64("ord")?
                .first_or_default_col(u64::MAX),
            filter: self.filter.clone(),
            space: self.space,
            hits: Vec::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        true
    }

    fn merge_fruits(&self, fruits: Vec<Vec<(u32, Score)>>) -> tantivy::Result<Self::Fruit> {
        Ok(fruits.into_iter().flatten().collect())
    }
}

impl SegmentCollector for OrdHitsSegment {
    type Fruit = Vec<(u32, Score)>;

    fn collect(&mut self, doc: DocId, score: Score) {
        if let Some(ord) = self.space.local(self.ords.get_val(doc)) {
            if self.filter.contains(ord) {
                self.hits.push((ord, score));
            }
        }
    }

    fn harvest(self) -> Self::Fruit {
        self.hits
    }
}

/// Только номера (без оценок) документов из `filter` — битовой картой. Tantivy не считает
/// BM25 (`requires_scoring = false`): для проверок «точное название» и «совпало в названии»
/// важен лишь факт совпадения.
pub struct OrdSet {
    pub filter: Arc<RoaringBitmap>,
    pub space: Space,
}

pub struct OrdSetSegment {
    ords: Arc<dyn ColumnValues<u64>>,
    filter: Arc<RoaringBitmap>,
    space: Space,
    set: RoaringBitmap,
}

impl Collector for OrdSet {
    type Fruit = RoaringBitmap;
    type Child = OrdSetSegment;

    fn for_segment(
        &self,
        _segment: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        Ok(OrdSetSegment {
            ords: reader
                .fast_fields()
                .u64("ord")?
                .first_or_default_col(u64::MAX),
            filter: self.filter.clone(),
            space: self.space,
            set: RoaringBitmap::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, fruits: Vec<RoaringBitmap>) -> tantivy::Result<Self::Fruit> {
        Ok(fruits.into_iter().fold(RoaringBitmap::new(), |a, b| a | b))
    }
}

impl SegmentCollector for OrdSetSegment {
    type Fruit = RoaringBitmap;

    fn collect(&mut self, doc: DocId, _score: Score) {
        if let Some(ord) = self.space.local(self.ords.get_val(doc)) {
            if self.filter.contains(ord) {
                self.set.insert(ord);
            }
        }
    }

    fn harvest(self) -> Self::Fruit {
        self.set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaces_do_not_overlap() {
        let person = Space::Person.raw(7);
        assert_eq!(Space::Person.local(person), Some(7));
        assert_eq!(Space::Entity.local(person), None);
        assert_eq!(Space::Entity.local(Space::Entity.raw(7)), Some(7));
        assert_eq!(Space::Person.local(7), None);
    }
}
