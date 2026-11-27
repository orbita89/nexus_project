//! Слой Б людей: видимые, «известен по…», порядки по известности и по имени — в RAM. Номера —
//! `catalog_person_ords`; в Tantivy у человека `ord` несёт `PERSON_FLAG`.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use roaring::RoaringBitmap;

use super::columns::{self, Column, Columns};
use super::sidecar::{
    get_bits, get_str, get_u32, next_generation, put_bits, put_str, put_u32, Order,
};
use super::types::title_sort_key;

/// Порядок выдачи людей.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeopleSort {
    /// Релевантность, при равной — известность; без запроса — известность.
    Relevance,
    Fame,
    Name,
}

/// Человек для слоя Б (текст и карточка — в [`super::types::TextDoc`]).
#[derive(Debug, Clone)]
pub struct PersonFacts {
    pub ord: u32,
    pub name: String,
    pub known_for: Option<String>,
    pub fame: f64,
}

#[derive(Debug, Default)]
pub struct PeopleSidecar {
    generation: u64,
    visible: RoaringBitmap,
    /// Основная роль → люди.
    known_for: HashMap<String, RoaringBitmap>,
    /// Известность по номерам (мера для порядка совпадений одного качества).
    fame: Column<f32>,
    by_fame: Order,
    by_name: Order,
}

impl PeopleSidecar {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn len(&self) -> u64 {
        self.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    pub fn visible(&self) -> &RoaringBitmap {
        &self.visible
    }

    pub fn fame(&self, ord: u32) -> f32 {
        self.fame.get(ord as usize).copied().unwrap_or(0.0)
    }

    /// Люди под фильтр роли (`None` — все).
    pub fn filter(&self, role: Option<&str>) -> RoaringBitmap {
        match role {
            None => self.visible.clone(),
            Some(role) => self
                .known_for
                .get(role)
                .map(|bits| bits & &self.visible)
                .unwrap_or_default(),
        }
    }

    /// Первые `len` людей из `filter` в порядке `sort` (без текста) и сколько их всего.
    pub fn page(&self, sort: PeopleSort, filter: &RoaringBitmap, len: usize) -> (Vec<u32>, usize) {
        let order = match sort {
            PeopleSort::Name => &self.by_name,
            PeopleSort::Fame | PeopleSort::Relevance => &self.by_fame,
        };
        order.take(filter, len, |_| true, Some(filter.len()))
    }

    /// Совпадения текста по релевантности: оценка (округлена — почти равные по качеству идут по
    /// известности), затем известность. Первые `len` номеров.
    pub fn rank_hits(&self, hits: &[(u32, f32)], len: usize) -> Vec<u32> {
        // Итог = текст (доля лучшей оценки) + вес × известность (логарифм, доля от самого
        // известного среди найденных): «брэд» — Брэд Питт, а не первый попавшийся тёзка.
        let weight = crate::index::weights::current().search("person");
        let top = hits.iter().map(|hit| hit.1).fold(0.0f32, f32::max);
        let best = hits
            .iter()
            .map(|&(ord, _)| self.fame(ord).max(0.0).ln_1p())
            .fold(0.0f32, f32::max);
        let mut keyed: Vec<((std::cmp::Reverse<i64>, u32), u32)> = hits
            .iter()
            .map(|&(ord, score)| {
                let text = if top > 0.0 { (score / top).sqrt() } else { 0.0 };
                let fame = if best > 0.0 {
                    self.fame(ord).max(0.0).ln_1p() / best
                } else {
                    0.0
                };
                let total = (1.0 - weight) * text + weight * fame;
                ((std::cmp::Reverse((total * 1_000_000.0) as i64), ord), ord)
            })
            .collect();
        if keyed.len() > len {
            keyed.select_nth_unstable_by(len, |a, b| a.0.cmp(&b.0));
            keyed.truncate(len);
        }
        keyed.sort_unstable_by_key(|a| a.0);
        keyed.into_iter().map(|(_, ord)| ord).collect()
    }

    /// Людей по основным ролям среди `found` (роли без людей нет).
    pub fn role_counts(&self, found: &RoaringBitmap) -> BTreeMap<String, i64> {
        self.known_for
            .iter()
            .filter_map(|(role, bits)| {
                let n = bits.intersection_len(found);
                (n > 0).then(|| (role.clone(), n as i64))
            })
            .collect()
    }

    /// Состояние для админки: люди, роли, оценка памяти.
    pub fn stats(&self) -> PeopleStats {
        let mut roles: Vec<(String, u64)> = self
            .known_for
            .iter()
            .map(|(role, bits)| (role.clone(), bits.len()))
            .collect();
        roles.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        PeopleStats {
            generation: self.generation,
            people: self.visible.len(),
            slots: self.fame.len() as u64,
            roles,
            bytes: (self.fame.len() * 4) as u64
                + self.by_fame.bytes()
                + self.by_name.bytes()
                + self.visible.serialized_size() as u64
                + self
                    .known_for
                    .values()
                    .map(|b| b.serialized_size() as u64)
                    .sum::<u64>(),
        }
    }
}

impl PeopleSidecar {
    /// Память слоя Б людей по частям.
    pub fn parts(&self) -> Vec<super::sidecar::MemPart> {
        let part = |name: &str, bytes: u64, items: u64| super::sidecar::MemPart {
            name: name.to_string(),
            bytes,
            items,
            mapped: false,
        };
        let filed = |mut p: super::sidecar::MemPart, mapped: bool| {
            p.mapped = mapped;
            p
        };
        let mut out = vec![
            filed(
                part(
                    "people:fame",
                    (self.fame.len() * 4) as u64,
                    self.fame.len() as u64,
                ),
                self.fame.is_mapped(),
            ),
            filed(
                part(
                    "people:order:fame",
                    self.by_fame.bytes(),
                    self.by_fame.ords.len() as u64,
                ),
                self.by_fame.heap_bytes() == 0,
            ),
            filed(
                part(
                    "people:order:name",
                    self.by_name.bytes(),
                    self.by_name.ords.len() as u64,
                ),
                self.by_name.heap_bytes() == 0,
            ),
            part(
                "people:visible",
                self.visible.serialized_size() as u64,
                self.visible.len(),
            ),
        ];
        for (role, bits) in &self.known_for {
            out.push(part(
                &format!("people:role:{role}"),
                bits.serialized_size() as u64,
                bits.len(),
            ));
        }
        out
    }

    /// Основные роли, у которых есть люди.
    pub fn roles(&self) -> Vec<String> {
        self.known_for.keys().cloned().collect()
    }

    /// Известность и позиции человека с номером `ord`: (видим, fame, место по известности).
    pub fn inspect(&self, ord: u32) -> Option<(bool, f32, Option<u32>, Option<String>)> {
        let fame = *self.fame.get(ord as usize)?;
        let pos = self
            .by_fame
            .ords
            .iter()
            .position(|&x| x == ord)
            .map(|p| p as u32);
        let role = self
            .known_for
            .iter()
            .find(|(_, bits)| bits.contains(ord))
            .map(|(r, _)| r.clone());
        Some((self.visible.contains(ord), fame, pos, role))
    }
}

#[derive(Debug, Clone)]
pub struct PeopleStats {
    pub generation: u64,
    pub people: u64,
    pub slots: u64,
    pub roles: Vec<(String, u64)>,
    pub bytes: u64,
}

/// Сборка [`PeopleSidecar`].
#[derive(Default)]
pub struct PeopleBuilder {
    visible: RoaringBitmap,
    known_for: HashMap<String, RoaringBitmap>,
    fame: Vec<f32>,
    by_fame: Vec<(u64, u32)>,
    by_name: Vec<(u64, u32)>,
}

impl PeopleBuilder {
    pub fn add(&mut self, person: &PersonFacts) {
        let at = person.ord as usize;
        if self.fame.len() <= at {
            self.fame.resize(at + 1, 0.0);
        }
        self.fame[at] = person.fame as f32;
        self.visible.insert(person.ord);
        if let Some(role) = &person.known_for {
            self.known_for
                .entry(role.clone())
                .or_default()
                .insert(person.ord);
        }
        // Известность — f64 ≥ 0: порядок битов неотрицательного числа совпадает с числовым.
        self.by_fame
            .push((person.fame.max(0.0).to_bits(), person.ord));
        self.by_name
            .push((title_sort_key(&person.name), person.ord));
    }

    pub fn finish(self) -> PeopleSidecar {
        let Self {
            visible,
            known_for,
            fame,
            mut by_fame,
            mut by_name,
        } = self;
        let len = fame.len();
        by_fame.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        by_name.sort_unstable();
        PeopleSidecar {
            generation: next_generation(),
            visible,
            known_for,
            fame: fame.into(),
            by_fame: Order::new(by_fame.into_iter().map(|x| x.1).collect(), len),
            by_name: Order::new(by_name.into_iter().map(|x| x.1).collect(), len),
        }
    }
}

pub const SNAPSHOT_FILE: &str = "sidecar-people.bin";
/// Известность и порядки людей ([`columns`]).
pub const COLUMNS_FILE: &str = "sidecar-people.cols";
const MAGIC: &[u8; 8] = b"NXCATP02";

impl PeopleSidecar {
    /// Известность и порядки — файлом колонок (mmap), видимые и роли — в память.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        columns::write(
            &dir.join(COLUMNS_FILE),
            &[
                bytemuck::cast_slice(&self.fame),
                bytemuck::cast_slice(&self.by_fame.ords),
                bytemuck::cast_slice(self.by_fame.pos()),
                bytemuck::cast_slice(&self.by_name.ords),
                bytemuck::cast_slice(self.by_name.pos()),
            ],
        )?;
        let tmp = dir.join(format!("{SNAPSHOT_FILE}.tmp"));
        {
            let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            out.write_all(MAGIC)?;
            put_bits(&mut out, &self.visible)?;
            put_u32(&mut out, self.known_for.len() as u32)?;
            for (role, bits) in &self.known_for {
                put_str(&mut out, role)?;
                put_bits(&mut out, bits)?;
            }
            out.flush()?;
        }
        std::fs::rename(&tmp, dir.join(SNAPSHOT_FILE))
    }

    pub fn load(dir: &Path) -> Option<Self> {
        let result = (|| -> std::io::Result<Self> {
            let cols = Columns::open(dir, COLUMNS_FILE)?;
            let file = std::fs::File::open(dir.join(SNAPSHOT_FILE))?;
            Self::read(&cols, &mut std::io::BufReader::new(file))
        })();
        match result {
            Ok(sidecar) => Some(sidecar),
            Err(error) => {
                tracing::warn!(%error, "people sidecar snapshot is unreadable, ignored");
                None
            }
        }
    }

    fn read(cols: &Columns, input: &mut impl Read) -> std::io::Result<Self> {
        let invalid = |m: &'static str| std::io::Error::new(std::io::ErrorKind::InvalidData, m);
        let mut magic = [0u8; 8];
        input.read_exact(&mut magic)?;
        if &magic != MAGIC || cols.len() != 5 {
            return Err(invalid("unknown snapshot format"));
        }
        let fame: Column<f32> = cols.get(0)?;
        let by_fame = Order::mapped(cols.get(1)?, cols.get(2)?);
        let by_name = Order::mapped(cols.get(3)?, cols.get(4)?);
        if by_fame.pos().len() != fame.len() || by_name.pos().len() != fame.len() {
            return Err(invalid("people arrays differ in length"));
        }
        let visible = get_bits(input)?;
        let mut known_for = HashMap::new();
        for _ in 0..get_u32(input)? {
            let role = get_str(input)?;
            known_for.insert(role, get_bits(input)?);
        }
        Ok(Self {
            generation: next_generation(),
            visible,
            known_for,
            fame,
            by_fame,
            by_name,
        })
    }
}

/// Общий снимок можно делить между потоками.
pub type SharedPeople = Arc<PeopleSidecar>;

#[cfg(test)]
mod tests {
    use super::*;

    fn people() -> PeopleSidecar {
        let mut builder = PeopleBuilder::default();
        for (ord, name, role, fame) in [
            (0, "Брэд Питт", Some("actor"), 90.0),
            (1, "Брэд Бёрд", Some("director"), 40.0),
            (2, "Анна", None, 0.0),
        ] {
            builder.add(&PersonFacts {
                ord,
                name: name.into(),
                known_for: role.map(str::to_owned),
                fame,
            });
        }
        builder.finish()
    }

    #[test]
    fn orders_roles_and_text_ties_follow_fame() {
        let side = people();
        let all = side.filter(None);
        assert_eq!(side.page(PeopleSort::Fame, &all, 10), (vec![0, 1, 2], 3));
        assert_eq!(side.page(PeopleSort::Name, &all, 10).0, [2, 1, 0]);
        let actors = side.filter(Some("actor"));
        assert_eq!(actors.iter().collect::<Vec<_>>(), [0]);
        assert!(side.filter(Some("writer")).is_empty());
        // Равные по качеству совпадения — известный первым.
        assert_eq!(side.rank_hits(&[(1, 3.0), (0, 3.0)], 10), [0, 1]);
        let counts = side.role_counts(&all);
        assert_eq!(counts["actor"], 1);
        assert_eq!(counts["director"], 1);
        assert_eq!(counts.len(), 2);
    }

    #[test]
    fn snapshot_round_trips() {
        let side = people();
        let dir = std::env::temp_dir().join(format!("nexus-people-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        side.save(&dir).unwrap();
        let loaded = PeopleSidecar::load(&dir).unwrap();
        let all = loaded.filter(None);
        assert_eq!(
            loaded.page(PeopleSort::Fame, &all, 10),
            side.page(PeopleSort::Fame, &all, 10)
        );
        assert_eq!(loaded.role_counts(&all), side.role_counts(&all));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
