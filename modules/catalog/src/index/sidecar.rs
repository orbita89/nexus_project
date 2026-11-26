//! Слой Б единого индекса: фасеты, ручной порядок и сортировки сущностей — в RAM, рядом с
//! Tantivy.
//!
//! Tantivy (слой А, [`super`]) хранит только текст, номер `ord`, вид и карточку. Всё, что меняется
//! ежедневно — ручное место в выдаче, оценки, теги, люди, издательство, язык, год, обложка,
//! видимость, — живёт здесь: [`EntitySidecar`] целиком собирается из PostgreSQL
//! ([`super::sync::reload_sidecar`]) и атомарно подменяется (`ArcSwap`) без записи в индекс.
//!
//! Сущность здесь — номер `ord: u32` (таблица `catalog_entity_ords`): индекс в плоских массивах
//! и бит в `RoaringBitmap`. Номер стабилен: перестройка Tantivy и слияние сегментов его не меняют.
//!
//! **Ключи фасетов** (`Facets::values`): `kind:<вид>`, `tag:<slug>`, `country:<код>`,
//! `studio:<ключ>`, `studio_slug:<slug>`, `publisher:<ключ>`, `museum:<ключ>`, `award:<slug>`,
//! `award:<slug>:<год>`, `awardyear:<год>`, `awarded` (фильтр по человеку — поле `credit` слоя А)
//! (последние два — только у книг), `lang:<код>`, `form:<вид текста>`; годы — отдельная карта.
//!
//! **Ручной порядок** ([`SortMeta::manual_rank`]): `1..=65535` — место в общем списке, `0` — нет.
//! Ранг глобальный: в фасете сущности сохраняют относительный порядок. В тексте ручное место
//! поднимает только сильные совпадения ([`STRONG_SHARE`]), а точное название запроса стоит выше
//! любого ручного места ([`EntitySidecar::rank_hits`]).

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use chrono::{Datelike, NaiveDate};
use roaring::RoaringBitmap;
use shared::tantivy::normalize_key;

use super::columns::{self, Column, Columns};
use super::types::{day_of, scaled, title_sort_key, BookForm, EntityDoc, Query, Sort};
use crate::models::EntityKind;

/// Флаги сущности в [`SortMeta::flags`].
pub const FLAG_COVER: u16 = 1;
pub const FLAG_DESCRIPTION: u16 = 2;
pub const FLAG_AUTHOR: u16 = 4;

/// Доля лучшей оценки текста, с которой совпадение считается сильным: только такие сущности
/// ручное место поднимает над остальными.
pub const STRONG_SHARE: f32 = 0.5;

/// Вес популярности в релевантности по умолчанию: итог = (1 − w)·текст + w·популярность, w — 0–1
/// (по видам — `catalog_rank_kinds.search_popularity`, [`super::weights`]); текст — доля лучшей
/// оценки среди найденных, популярность (доля от самой популярной найденной сущности того же вида,
/// 0–1; `rank_score` уже сбалансирован, логарифм сжал бы разницу до процентов). При 0,6 почти такое же совпадение популярной работы выше: «маст» —
/// «Мастер и Маргарита», а не «Масть». Нормировка — по найденным, без лишней памяти.
pub const POPULARITY_WEIGHT: f32 = 0.6;

/// Фильтр уже узкого раздела сортируется по позициям ([`Order::pos`]), широкий — проходом по
/// готовому порядку: при `|фильтр| * NARROW < сущностей` сортировка выгоднее прохода.
const NARROW: u64 = 16;

pub fn kind_key(kind: EntityKind) -> String {
    format!("kind:{}", kind.as_str())
}
pub fn tag_key(slug: &str) -> String {
    format!("tag:{slug}")
}
pub fn country_key(code: &str) -> String {
    format!("country:{}", code.trim().to_ascii_uppercase())
}
pub fn studio_key(name: &str) -> String {
    format!("studio:{}", normalize_key(name))
}
pub fn studio_slug_key(slug: &str) -> String {
    format!("studio_slug:{slug}")
}
pub fn publisher_key(name: &str) -> String {
    format!("publisher:{}", normalize_key(name))
}
pub fn museum_key(name: &str) -> String {
    format!("museum:{}", normalize_key(name))
}
pub fn award_key(slug: &str) -> String {
    format!("award:{slug}")
}
pub fn award_year_key(slug: &str, year: i32) -> String {
    format!("award:{slug}:{year}")
}
pub fn person_key(slug: &str) -> String {
    format!("person:{slug}")
}
pub fn role_key(role: &str, slug: &str) -> String {
    format!("role:{role}:{slug}")
}

/// Термы поля `facet` слоя А: значения с большим числом вариантов (студии, издатели, музеи,
/// награды, люди). В RAM их нет — 74 тыс. студий или 180 тыс. людей битовыми картами стоили бы
/// десятки мегабайт.
pub fn text_facets(doc: &EntityDoc) -> Vec<String> {
    let mut out = Vec::new();
    for (names, key) in [
        (&doc.studios, studio_key as fn(&str) -> String),
        (&doc.publishers, publisher_key),
        (&doc.museums, museum_key),
    ] {
        for name in names {
            if !normalize_key(name).is_empty() {
                out.push(key(name.trim()));
            }
        }
    }
    out.extend(doc.studio_slugs.iter().map(|s| studio_slug_key(s)));
    for slug in &doc.award_slugs {
        out.push(award_key(slug));
        out.extend(doc.award_years.iter().map(|&y| award_year_key(slug, y)));
    }
    for (role, slug) in &doc.credits {
        out.push(person_key(slug));
        out.push(role_key(role, slug));
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Термы, которые должны быть у сущности под фильтры запроса (И между ними). `None` — фильтр
/// заведомо пуст (название из одних знаков).
pub fn text_facet_terms(q: &Query) -> Option<Vec<String>> {
    let mut terms = Vec::new();
    for (name, key) in [
        (&q.studio, studio_key as fn(&str) -> String),
        (&q.publisher, publisher_key),
        (&q.museum, museum_key),
    ] {
        if let Some(name) = name {
            if normalize_key(name).is_empty() {
                return None;
            }
            terms.push(key(name.trim()));
        }
    }
    if let Some(slug) = &q.studio_slug {
        terms.push(studio_slug_key(slug));
    }
    if let Some(slug) = &q.award {
        terms.push(match q.award_year {
            Some(year) => award_year_key(slug, year),
            None => award_key(slug),
        });
    }
    if let Some(person) = &q.person {
        terms.push(match &q.role {
            Some(role) => role_key(role, person),
            None => person_key(person),
        });
    }
    Some(terms)
}
pub fn awardyear_key(year: i32) -> String {
    format!("awardyear:{year}")
}
pub fn form_key(form: BookForm) -> String {
    format!("form:{}", form.as_str())
}
pub fn language_key(code: &str) -> String {
    format!("lang:{}", code.trim().to_lowercase())
}
const AWARDED: &str = "awarded";

/// Группы фасетов, по которым отдаются подсказки.
pub const HINT_GROUPS: [&str; 5] = ["studio", "publisher", "museum", "country", "tag"];

/// Сортировочные данные сущности: 8 байт, индекс в массиве — `ord`. Лежит в файле колонок как
/// есть (`repr(C)`, без дыр).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SortMeta {
    /// `0` — нет ручного места; `1..=65535` — место в общем списке.
    pub manual_rank: u16,
    /// [`FLAG_COVER`] | [`FLAG_DESCRIPTION`] | [`FLAG_AUTHOR`].
    pub flags: u16,
    /// Известность: множитель релевантности (у книг — работы автора, у остальных — балл).
    pub popularity: u32,
}

const _: () = assert!(std::mem::size_of::<SortMeta>() == 8);

/// Готовый порядок по одной сортировке: сущности по порядку и (лениво, только для узких
/// фильтров) позиция каждой.
#[derive(Debug, Default, Clone)]
pub struct Order {
    pub(super) ords: Column<u32>,
    len: usize,
    pos: OnceLock<Column<u32>>,
}

impl Order {
    pub(super) fn new(ords: Vec<u32>, len: usize) -> Self {
        Self {
            ords: ords.into(),
            len,
            pos: OnceLock::new(),
        }
    }

    /// Порядок из файла колонок: номера и готовые позиции (в куче ничего).
    pub(super) fn mapped(ords: Column<u32>, pos: Column<u32>) -> Self {
        let cell = OnceLock::new();
        let len = pos.len();
        let _ = cell.set(pos);
        Self {
            ords,
            len,
            pos: cell,
        }
    }

    /// Позиция каждого номера в порядке (`u32::MAX` — нет). У порядка из файла — готовая;
    /// у только что собранного строится при первом узком фильтре или записи файла.
    pub(super) fn pos(&self) -> &[u32] {
        self.pos.get_or_init(|| {
            let mut pos = vec![u32::MAX; self.len];
            for (at, &ord) in self.ords.iter().enumerate() {
                pos[ord as usize] = at as u32;
            }
            pos.into()
        })
    }

    /// Логический размер (номера и позиции), байт.
    pub(super) fn bytes(&self) -> u64 {
        (self.ords.len() * 4 + self.pos.get().map_or(0, |p| p.len() * 4)) as u64
    }

    /// Сколько из этого в куче (у порядка из файла — 0).
    pub(super) fn heap_bytes(&self) -> u64 {
        self.ords.heap_bytes() + self.pos.get().map_or(0, |p| p.heap_bytes())
    }

    /// Первые `len` номеров порядка из `filter` (`accept` — доп. условие) и сколько их всего.
    pub(super) fn take(
        &self,
        filter: &RoaringBitmap,
        len: usize,
        accept: impl Fn(u32) -> bool,
        exact_total: Option<u64>,
    ) -> (Vec<u32>, usize) {
        if filter.len() * NARROW < self.ords.len() as u64 {
            let pos = self.pos();
            let mut picked: Vec<u32> = filter
                .iter()
                .filter(|&ord| pos.get(ord as usize).is_some_and(|&p| p != u32::MAX))
                .filter(|&ord| accept(ord))
                .collect();
            picked.sort_unstable_by_key(|&ord| pos[ord as usize]);
            let total = picked.len();
            picked.truncate(len);
            return (picked, total);
        }
        let mut out = Vec::with_capacity(len.min(filter.len() as usize));
        let mut total = 0usize;
        for &ord in self.ords.iter() {
            if filter.contains(ord) && accept(ord) {
                if out.len() < len {
                    out.push(ord);
                } else if let Some(total) = exact_total {
                    // Число известно заранее — дальше идти незачем.
                    return (out, total as usize);
                }
                total += 1;
            }
        }
        (out, total)
    }
}

/// Видов в счётчиках подсказки: все вместе + по одному на [`EntityKind::ALL`].
const HINT_COUNTS: usize = 1 + EntityKind::ALL.len();

fn kind_slot(kind: EntityKind) -> usize {
    1 + EntityKind::ALL.iter().position(|&k| k == kind).unwrap_or(0)
}

/// Значение подсказки при сборке: ключ, название, счётчики.
type HintItem = (String, String, [u32; HINT_COUNTS]);

/// Строка таблицы подсказок: ключ, название и нормализованное название — срезы общего пула,
/// счётчики готовые (всего и по видам): подсказке не нужны битовые карты.
#[derive(Debug, Clone, Copy)]
struct HintRow {
    key: (u32, u16),
    label: (u32, u16),
    norm: (u32, u16),
    counts: [u32; HINT_COUNTS],
}

/// Подсказки одной группы фасетов (`studio`, `tag`…), частые сверху. Одна строка-пул и плоский
/// массив строк: ~50 байт на значение вместо строки-ключа, битовой карты и записи в хеш-таблице.
#[derive(Debug, Default, Clone)]
pub struct HintTable {
    pool: String,
    rows: Vec<HintRow>,
}

impl HintTable {
    /// Из (ключ, название, счётчики); сортирует по убыванию общего числа, затем по ключу.
    fn build(mut items: Vec<HintItem>) -> Self {
        items.sort_unstable_by(|a, b| b.2[0].cmp(&a.2[0]).then_with(|| a.0.cmp(&b.0)));
        let mut table = Self::default();
        for (key, label, counts) in items {
            let norm = normalize_key(&label);
            let key = table.push_str(&key);
            let label = table.push_str(&label);
            let norm = table.push_str(&norm);
            table.rows.push(HintRow {
                key,
                label,
                norm,
                counts,
            });
        }
        table.pool.shrink_to_fit();
        table.rows.shrink_to_fit();
        table
    }

    fn push_str(&mut self, text: &str) -> (u32, u16) {
        let text = &text[..text.len().min(u16::MAX as usize)];
        let at = self.pool.len() as u32;
        self.pool.push_str(text);
        (at, text.len() as u16)
    }

    fn str(&self, (at, len): (u32, u16)) -> &str {
        self.pool
            .get(at as usize..at as usize + len as usize)
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Байт в памяти.
    pub fn bytes(&self) -> u64 {
        (self.pool.capacity() + self.rows.capacity() * std::mem::size_of::<HintRow>()) as u64
    }

    /// Ключ, название и счётчики каждой строки (для записи снимка).
    fn iter(&self) -> impl Iterator<Item = (&str, &str, &[u32; HINT_COUNTS])> {
        self.rows
            .iter()
            .map(|row| (self.str(row.key), self.str(row.label), &row.counts))
    }
}

/// Фасеты: множества сущностей по значению.
#[derive(Debug, Default)]
pub struct Facets {
    /// Все видимые сущности.
    pub visible: RoaringBitmap,
    pub cover: RoaringBitmap,
    pub values: HashMap<String, RoaringBitmap>,
    pub years: BTreeMap<i32, RoaringBitmap>,
    /// Подсказки по группам ([`HINT_GROUPS`]), частые сверху.
    pub hints: HashMap<&'static str, HintTable>,
}

/// Группы подсказок, значения которых есть в памяти (`Facets::values`): счётчики считаются из
/// битовых карт. Остальные группы ([`TEXT_FACET_GROUPS`]) — только в Tantivy, их счётчики
/// набирает [`SidecarBuilder`].
const BITMAP_HINT_GROUPS: [&str; 2] = ["country", "tag"];

/// Фасеты с большим числом значений: не битовые карты в RAM, а термы поля `facet` слоя А
/// ([`text_facets`]). Фильтр по ним — запрос к Tantivy (короткий список номеров с диска).
pub const TEXT_FACET_GROUPS: [&str; 3] = ["studio", "publisher", "museum"];

impl Facets {
    /// Подсказки групп с битовыми картами (теги, страны): счётчики всего и по видам.
    fn build_bitmap_hints(&mut self) {
        let kinds: Vec<Option<&RoaringBitmap>> = EntityKind::ALL
            .iter()
            .map(|&kind| self.values.get(&kind_key(kind)))
            .collect();
        for group in BITMAP_HINT_GROUPS {
            let prefix = format!("{group}:");
            let items = self
                .values
                .iter()
                .filter_map(|(key, bits)| {
                    let rest = key.strip_prefix(&prefix)?;
                    let mut counts = [0u32; HINT_COUNTS];
                    counts[0] = bits.len() as u32;
                    for (slot, kind) in kinds.iter().enumerate() {
                        counts[slot + 1] = kind.map_or(0, |k| bits.intersection_len(k) as u32);
                    }
                    Some((rest.to_string(), rest.to_string(), counts))
                })
                .collect();
            self.hints.insert(group, HintTable::build(items));
        }
    }
}

/// Один вид фасетов (`tag`, `person`, `lang`…): сколько значений и сколько весят их битмапы.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FacetGroup {
    pub values: u64,
    pub bytes: u64,
}

/// Состояние слоя Б ([`EntitySidecar::stats`]). Байты — оценка памяти.
#[derive(Debug, Clone)]
pub struct SidecarStats {
    pub generation: u64,
    /// Видимые сущности.
    pub entities: u64,
    /// Длина массивов по `ord` (с дырами от удалённых).
    pub slots: u64,
    pub with_cover: u64,
    pub manual_ranks: u64,
    /// Вид → сущностей.
    pub kinds: Vec<(String, u64)>,
    /// Вид текста у книг → книг.
    pub forms: Vec<(&'static str, u64)>,
    /// Префикс ключа → значения и байты.
    pub facets: Vec<(String, FacetGroup)>,
    pub meta_bytes: u64,
    pub orders_bytes: u64,
    pub facets_bytes: u64,
}

/// Порядки, не зависящие от ручных мест. Все покрывают все видимые сущности.
#[derive(Debug, Default, Clone)]
struct Orders {
    /// По дате выхода, новые сверху (только с датой).
    new: Order,
    title: Order,
    popular: Order,
    rating: Order,
    trending: Order,
    top_rated: Order,
    awarded: Order,
    /// Дата выхода в днях от начала эры (`0` — нет даты): «вышла / не вышла» на сегодня.
    release_day: Column<u32>,
    /// Балл «Рекомендуемые» без ручного места ([`EntityDoc::base_rank`]).
    base_rank: Column<u64>,
}

impl Orders {
    fn all(&self) -> [&Order; 7] {
        [
            &self.new,
            &self.title,
            &self.popular,
            &self.rating,
            &self.trending,
            &self.top_rated,
            &self.awarded,
        ]
    }
}

/// Поколение состояния: ключ кэша выдачи. Растёт с каждой подменой в процессе.
static GENERATION: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Слой Б сущностей целиком: неизменяемый снимок, подменяется атомарно.
#[derive(Debug, Default)]
pub struct EntitySidecar {
    generation: u64,
    meta: Column<SortMeta>,
    /// «Рекомендуемые»: сначала ручные места по возрастанию, затем [`Orders::base_rank`].
    recommended: Order,
    facets: Arc<Facets>,
    orders: Arc<Orders>,
}

/// Порядок «Рекомендуемые» по ручным местам и баллу.
fn recommended(meta: &[SortMeta], orders: &Orders, visible: &RoaringBitmap) -> Order {
    let mut ords: Vec<u32> = visible.iter().collect();
    ords.sort_unstable_by(|&a, &b| {
        let (ma, mb) = (meta[a as usize].manual_rank, meta[b as usize].manual_rank);
        (ma == 0)
            .cmp(&(mb == 0))
            .then(ma.cmp(&mb))
            .then(orders.base_rank[b as usize].cmp(&orders.base_rank[a as usize]))
            .then(a.cmp(&b))
    });
    Order::new(ords, meta.len())
}

/// Пересечь `out` с множеством; нет такого значения — пусто.
fn narrow(out: &mut RoaringBitmap, bits: Option<&RoaringBitmap>) {
    match bits {
        Some(bits) => *out &= bits,
        None => out.clear(),
    }
}

impl EntitySidecar {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Сколько видимых сущностей.
    pub fn len(&self) -> u64 {
        self.facets.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn meta(&self, ord: u32) -> SortMeta {
        self.meta.get(ord as usize).copied().unwrap_or_default()
    }

    pub fn facets(&self) -> &Facets {
        &self.facets
    }

    /// Видимые сущности вида (пусто — вида нет).
    pub fn of_kind(&self, kind: EntityKind) -> RoaringBitmap {
        self.facets
            .values
            .get(&kind_key(kind))
            .cloned()
            .unwrap_or_default()
    }

    /// Новое состояние с другими ручными местами (`ord → место`; остальные — без места).
    /// Фасеты и прочие порядки не пересобираются: правка списка — миллисекунды.
    pub fn with_ranks(&self, ranks: &HashMap<u32, u16>) -> Self {
        let mut meta = self.meta.to_vec();
        for (ord, item) in meta.iter_mut().enumerate() {
            item.manual_rank = ranks.get(&(ord as u32)).copied().unwrap_or(0);
        }
        let recommended = recommended(&meta, &self.orders, &self.facets.visible);
        Self {
            generation: next_generation(),
            meta: meta.into(),
            recommended,
            facets: self.facets.clone(),
            orders: self.orders.clone(),
        }
    }

    /// Новые баллы «Рекомендуемые» (`ord → балл`, нет — 0) без прохода по БД и Tantivy:
    /// популярность в поиске, порядок «Рекомендуемые». Порядки, где балл — только второй ключ
    /// (`trending`, `top_rated`, `awarded`), догонит полная пересборка слоя Б.
    pub fn with_scores(&self, scores: &HashMap<u32, f64>) -> Self {
        let mut meta = self.meta.to_vec();
        let mut orders = (*self.orders).clone();
        let mut base_rank = orders.base_rank.to_vec();
        for (ord, item) in meta.iter_mut().enumerate() {
            let score = scores.get(&(ord as u32)).copied();
            item.popularity = super::types::popularity_meta(score);
            if let Some(rank) = base_rank.get_mut(ord) {
                let day = *rank & 0xFFFF_FFFF;
                *rank = super::types::base_rank(score, day);
            }
        }
        orders.base_rank = base_rank.into();
        let recommended = recommended(&meta, &orders, &self.facets.visible);
        Self {
            generation: next_generation(),
            meta: meta.into(),
            recommended,
            facets: self.facets.clone(),
            orders: Arc::new(orders),
        }
    }

    /// Сущности под фильтры запроса (текст не учитывается).
    pub fn filter(&self, q: &Query) -> RoaringBitmap {
        let facets = &self.facets;
        let values = &facets.values;
        let mut out = facets.visible.clone();
        let union = |keys: &mut dyn Iterator<Item = String>| {
            let mut any = RoaringBitmap::new();
            for key in keys {
                if let Some(bits) = values.get(&key) {
                    any |= bits;
                }
            }
            any
        };
        if !q.kinds.is_empty() {
            out &= union(&mut q.kinds.iter().map(|k| kind_key(*k)));
        }
        if !q.tags.is_empty() {
            out &= union(&mut q.tags.iter().map(|t| tag_key(t)));
        }
        if !q.countries.is_empty() {
            out &= union(&mut q.countries.iter().map(|c| country_key(c)));
        }
        // Студия, издатель, музей, награда, человек — термы слоя А ([`text_facet_terms`]).
        if let (None, Some(year)) = (&q.award, q.award_year) {
            narrow(&mut out, values.get(&awardyear_key(year)));
        }
        match q.awarded {
            Some(true) => narrow(&mut out, values.get(AWARDED)),
            Some(false) => {
                if let Some(awarded) = values.get(AWARDED) {
                    out -= awarded;
                }
            }
            None => {}
        }
        if let Some(language) = &q.language {
            narrow(&mut out, values.get(&language_key(language)));
        }
        // Виды текста — только у книг: остальные виды фильтр не трогает.
        let forms: &[BookForm] = if q.forms.is_empty() {
            &[BookForm::Book]
        } else {
            &q.forms
        };
        let books_in_scope = q.kinds.is_empty() || q.kinds.contains(&EntityKind::Book);
        if books_in_scope && !BookForm::ALL.iter().all(|f| forms.contains(f)) {
            if let Some(books) = values.get(&kind_key(EntityKind::Book)) {
                let keep = union(&mut forms.iter().map(|f| form_key(*f)));
                let mut hidden = books.clone();
                hidden -= keep;
                out -= hidden;
            }
        }
        match q.has_cover {
            Some(true) => out &= &facets.cover,
            Some(false) => out -= &facets.cover,
            None => {}
        }
        if q.year_from.is_some() || q.year_to.is_some() {
            let from = q.year_from.unwrap_or(i32::MIN);
            let to = q.year_to.unwrap_or(i32::MAX);
            if from > to {
                return RoaringBitmap::new();
            }
            let mut years = RoaringBitmap::new();
            for (_, bits) in facets.years.range(from..=to) {
                years |= bits;
            }
            out &= years;
        }
        if let Some(only) = &q.only_ords {
            out &= only.iter().copied().collect::<RoaringBitmap>();
        }
        for &ord in &q.exclude_ords {
            out.remove(ord);
        }
        out
    }

    /// Первые `len` сущностей из `filter` в порядке `sort` и сколько их всего. «Новые» — только
    /// вышедшие к `today`, «скоро» — только не вышедшие (без даты — ни там ни там).
    pub fn page(
        &self,
        sort: Sort,
        filter: &RoaringBitmap,
        today: NaiveDate,
        len: usize,
    ) -> (Vec<u32>, usize) {
        let orders = &self.orders;
        let today = day_of(today) as u32;
        let day = |ord: u32| orders.release_day.get(ord as usize).copied().unwrap_or(0);
        let all = Some(filter.len());
        match sort {
            Sort::Relevance | Sort::Recommended => {
                self.recommended.take(filter, len, |_| true, all)
            }
            Sort::Title => orders.title.take(filter, len, |_| true, all),
            Sort::Popular => orders.popular.take(filter, len, |_| true, all),
            Sort::Rating => orders.rating.take(filter, len, |_| true, all),
            Sort::Trending => orders.trending.take(filter, len, |_| true, all),
            Sort::TopRated => orders.top_rated.take(filter, len, |_| true, all),
            Sort::Awarded => orders.awarded.take(filter, len, |_| true, all),
            // Итог «вышло / не вышло» — проходом по фильтру (он обычно много меньше всех 2 млн),
            // порядок — только до набора страницы.
            Sort::New => {
                let released = |ord: u32| (1..=today).contains(&day(ord));
                let total = filter.iter().filter(|&ord| released(ord)).count() as u64;
                orders.new.take(filter, len, released, Some(total))
            }
            Sort::Soon => {
                // Ближайшие сверху: тот же порядок по дате, пройденный с конца.
                let total = filter.iter().filter(|&ord| day(ord) > today).count();
                let mut picked: Vec<u32> = Vec::with_capacity(len.min(total));
                if total > 0 {
                    for &ord in orders.new.ords.iter().rev() {
                        if day(ord) > today && filter.contains(ord) {
                            picked.push(ord);
                            if picked.len() >= len.min(total) {
                                break;
                            }
                        }
                    }
                }
                (picked, total)
            }
        }
    }

    /// Порядок совпадений текста по релевантности (`hits` — номер и оценка Tantivy):
    /// 1. точное название запроса (`exact`) с ручным местом — по месту;
    /// 2. сильные совпадения ([`STRONG_SHARE`]) с ручным местом — по месту;
    /// 3. совпадения в названии (`titled` и точные без места) — по «текст + популярность»;
    ///    точное название получает полную долю текста, но не безусловное первенство:
    ///    «мастер» — популярный «Мастер и Маргарита» выше малоизвестного фильма «Мастер»;
    /// 4. остальные (только люди, серия…) — так же.
    ///
    /// Возвращает первые `len` номеров.
    pub fn rank_hits(
        &self,
        hits: &[(u32, f32)],
        exact: &RoaringBitmap,
        titled: &RoaringBitmap,
        len: usize,
    ) -> Vec<u32> {
        self.ranked(hits, exact, titled, len)
            .into_iter()
            .map(|d| d.ord)
            .collect()
    }

    /// То же, что [`Self::rank_hits`], с разбором каждого места (для админки «Как ищет поиск»).
    pub fn ranked(
        &self,
        hits: &[(u32, f32)],
        exact: &RoaringBitmap,
        titled: &RoaringBitmap,
        len: usize,
    ) -> Vec<RankDetail> {
        let mut scored: Vec<RankDetail> = self
            .blend(hits, exact, titled)
            .into_iter()
            .map(|mut d| {
                let rank = self.meta(d.ord).manual_rank;
                let is_exact = exact.contains(d.ord);
                let in_title = titled.contains(d.ord);
                d.tier = if is_exact && rank > 0 {
                    0
                } else if rank > 0 && d.text >= STRONG_SHARE && in_title {
                    1
                } else if is_exact || in_title {
                    2
                } else {
                    3
                };
                d.manual_rank = if d.tier <= 1 { rank } else { 0 };
                d.exact = is_exact;
                d.in_title = in_title;
                d
            })
            .collect();
        // Ключ: (уровень, ручное место, −итог, номер).
        let key = |d: &RankDetail| (d.tier, d.manual_rank, std::cmp::Reverse(OrdF32(d.total)), d.ord);
        if scored.len() > len {
            scored.select_nth_unstable_by(len, |a, b| key(a).cmp(&key(b)));
            scored.truncate(len);
        }
        scored.sort_unstable_by(|a, b| key(a).cmp(&key(b)));
        scored
    }

    /// Оценка «текст + вес вида × популярность» каждого совпадения.
    /// Текст — доля лучшей оценки в своей группе (в названии вместе с точными — у точного 1;
    /// остальные), под корнем: длинное название «Властелин колец: Братство кольца» не проигрывает
    /// короткому только из-за длины. Популярность — доля от самой популярной найденной того же
    /// вида.
    fn blend(
        &self,
        hits: &[(u32, f32)],
        exact: &RoaringBitmap,
        titled: &RoaringBitmap,
    ) -> Vec<RankDetail> {
        // Точные и совпавшие в названии сравниваются между собой: доля — от лучшей из них.
        let group =
            |ord: u32| -> usize { usize::from(!(exact.contains(ord) || titled.contains(ord))) };
        let mut top = [0.0f32; 2];
        for &(ord, score) in hits {
            let g = group(ord);
            top[g] = top[g].max(score);
        }
        let kinds: Vec<(&str, &RoaringBitmap)> = self
            .facets
            .values
            .iter()
            .filter_map(|(key, bits)| key.strip_prefix("kind:").map(|k| (k, bits)))
            .collect();
        let weights = super::weights::current();
        // Вид и популярность каждого совпадения — один раз: вид ищется перебором битовых карт,
        // популярность читается из файла.
        let facts: Vec<(Option<usize>, f32)> = hits
            .iter()
            .map(|&(ord, _)| {
                (
                    kinds.iter().position(|(_, bits)| bits.contains(ord)),
                    self.meta(ord).popularity as f32,
                )
            })
            .collect();
        let mut best: HashMap<Option<usize>, f32> = HashMap::new();
        for &(kind, pop) in &facts {
            let slot = best.entry(kind).or_insert(0.0);
            *slot = slot.max(pop);
        }
        hits.iter()
            .zip(&facts)
            .map(|(&(ord, score), &(kind, popularity))| {
                let g = group(ord);
                // Корень: BM25 сильно штрафует длинные названия, без сжатия длина решала бы всё.
                let text = if exact.contains(ord) {
                    1.0
                } else if top[g] > 0.0 {
                    (score / top[g]).min(1.0).sqrt()
                } else {
                    0.0
                };
                let max = best.get(&kind).copied().unwrap_or(0.0);
                let pop = if max > 0.0 {
                    popularity / max
                } else {
                    0.0
                };
                let weight = kind.map_or(POPULARITY_WEIGHT, |k| weights.search(kinds[k].0));
                RankDetail {
                    ord,
                    bm25: score,
                    text,
                    popularity: pop,
                    weight,
                    total: (1.0 - weight) * text + weight * pop,
                    ..RankDetail::default()
                }
            })
            .collect()
    }

    /// Счётчики значений фасета с префиксом `prefix` (`tag:`, `lang:`…) среди `filter`: для
    /// панели фильтров. Нули не возвращаются, частые сверху.
    pub fn facet_counts(&self, filter: &RoaringBitmap, prefix: &str) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = self
            .facets
            .values
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, bits)| {
                (
                    key[prefix.len()..].to_string(),
                    bits.intersection_len(filter),
                )
            })
            .filter(|(_, count)| *count > 0)
            .collect();
        out.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// Подсказки значений группы ([`HINT_GROUPS`]): части названия (без регистра и `ё`)
    /// отвечает `needle`; сначала те, что с неё начинаются, затем по числу сущностей. `kind`
    /// считает число внутри вида. Возвращает (как показывать, число).
    pub fn hints(
        &self,
        group: &str,
        needle: &str,
        kind: Option<EntityKind>,
        limit: usize,
    ) -> Vec<(String, u64)> {
        let Some(table) = self.facets.hints.get(group) else {
            return Vec::new();
        };
        let needle = normalize_key(needle);
        let slot = kind.map_or(0, kind_slot);
        // Таблица по убыванию общего числа; внутри вида — по его счётчику, «с начала» выше.
        let mut hits: Vec<(bool, u32, &HintRow)> = table
            .rows
            .iter()
            .filter(|row| row.counts[slot] > 0)
            .filter_map(|row| {
                if needle.is_empty() {
                    return Some((true, row.counts[slot], row));
                }
                let norm = table.str(row.norm);
                norm.contains(&needle)
                    .then(|| (norm.starts_with(&needle), row.counts[slot], row))
            })
            .collect();
        let take = limit.min(hits.len());
        let key = |a: &(bool, u32, &HintRow)| (!a.0, std::cmp::Reverse(a.1));
        if take < hits.len() {
            hits.select_nth_unstable_by_key(take, key);
            hits.truncate(take);
        }
        hits.sort_by(|a, b| key(a).cmp(&key(b)).then_with(|| table.str(a.2.label).cmp(table.str(b.2.label))));
        hits.into_iter()
            .map(|(_, count, row)| (table.str(row.label).to_string(), u64::from(count)))
            .collect()
    }

    /// Состояние слоя Б для админки: сущности, фасеты по видам ключей и оценка памяти.
    pub fn stats(&self) -> SidecarStats {
        let f = &self.facets;
        let o = &self.orders;
        let mut groups: BTreeMap<String, FacetGroup> = BTreeMap::new();
        for (key, bits) in &f.values {
            let prefix = key.split(':').next().unwrap_or_default().to_string();
            let group = groups.entry(prefix).or_default();
            group.values += 1;
            group.bytes += bits.serialized_size() as u64;
        }
        groups.insert(
            "year".into(),
            FacetGroup {
                values: f.years.len() as u64,
                bytes: f.years.values().map(|b| b.serialized_size() as u64).sum(),
            },
        );
        let count = |key: String| {
            f.values
                .get(&key)
                .map_or(0, |bits| bits.intersection_len(&f.visible))
        };
        let kinds = EntityKind::ALL
            .iter()
            .map(|&kind| (kind.as_str().to_string(), count(kind_key(kind))))
            .filter(|(_, n)| *n > 0)
            .collect();
        let forms = BookForm::ALL
            .iter()
            .map(|&form| (form.as_str(), count(form_key(form))))
            .collect();
        let orders_bytes = o.all().iter().map(|order| order.bytes()).sum::<u64>()
            + self.recommended.bytes()
            + (o.release_day.len() * 4 + o.base_rank.len() * 8) as u64;
        let facets_bytes = groups.values().map(|g| g.bytes).sum::<u64>()
            + (f.visible.serialized_size() + f.cover.serialized_size()) as u64;
        SidecarStats {
            generation: self.generation,
            entities: f.visible.len(),
            slots: self.meta.len() as u64,
            with_cover: f.cover.len(),
            manual_ranks: self.meta.iter().filter(|m| m.manual_rank > 0).count() as u64,
            kinds,
            forms,
            facets: groups.into_iter().collect(),
            meta_bytes: (self.meta.len() * std::mem::size_of::<SortMeta>()) as u64,
            orders_bytes,
            facets_bytes,
        }
    }
}

/// Разбор места совпадения в выдаче по релевантности.
#[derive(Debug, Clone, Copy, Default)]
pub struct RankDetail {
    pub ord: u32,
    /// Уровень: 0 — точное название с ручным местом, 1 — сильное совпадение с ручным местом,
    /// 2 — совпало в названии (или точное без места), 3 — только люди, серия, студия.
    pub tier: u8,
    /// Ручное место, если оно учитывалось (уровни 0–1).
    pub manual_rank: u16,
    pub exact: bool,
    pub in_title: bool,
    /// Оценка Tantivy (BM25).
    pub bm25: f32,
    /// Доля текста: √(оценка / лучшая в группе), у точного названия — 1.
    pub text: f32,
    /// Доля популярности: балл / лучший балл вида среди найденных.
    pub popularity: f32,
    /// Вес популярности вида (`search_popularity`).
    pub weight: f32,
    /// Итог: (1 − вес) · текст + вес · популярность.
    pub total: f32,
}

/// Часть слоя Б в памяти (для админки): имя, байт, элементов.
#[derive(Debug, Clone)]
pub struct MemPart {
    pub name: String,
    pub bytes: u64,
    pub items: u64,
    /// В отображённом файле (не в куче).
    pub mapped: bool,
}

/// Что слой Б знает об одной сущности (для админки).
#[derive(Debug, Clone)]
pub struct EntityInspect {
    pub ord: u32,
    pub visible: bool,
    pub meta: SortMeta,
    /// Балл «Рекомендуемые» без даты (`base_rank >> 32`, ×1000).
    pub rank: u64,
    pub release_day: u32,
    /// Позиция в каждом готовом порядке (`None` — не входит).
    pub positions: Vec<(&'static str, Option<u32>, u64)>,
    pub facets: Vec<String>,
}

const ORDER_NAMES: [&str; 7] = [
    "new", "title", "popular", "rating", "trending", "top_rated", "awarded",
];

impl EntitySidecar {
    /// Память слоя Б по частям.
    pub fn parts(&self) -> Vec<MemPart> {
        let part = |name: &str, bytes: usize, items: usize| MemPart {
            name: name.to_string(),
            bytes: bytes as u64,
            items: items as u64,
            mapped: false,
        };
        let filed = |mut p: MemPart, mapped: bool| {
            p.mapped = mapped;
            p
        };
        let o = &self.orders;
        let mut out = vec![
            filed(
                part("meta", self.meta.len() * std::mem::size_of::<SortMeta>(), self.meta.len()),
                self.meta.is_mapped(),
            ),
            filed(
                part("base_rank", o.base_rank.len() * 8, o.base_rank.len()),
                o.base_rank.is_mapped(),
            ),
            filed(
                part("release_day", o.release_day.len() * 4, o.release_day.len()),
                o.release_day.is_mapped(),
            ),
            filed(
                part("order:recommended", self.recommended.bytes() as usize, self.recommended.ords.len()),
                self.recommended.heap_bytes() == 0,
            ),
        ];
        for (name, order) in ORDER_NAMES.iter().zip(o.all()) {
            out.push(filed(
                part(&format!("order:{name}"), order.bytes() as usize, order.ords.len()),
                order.heap_bytes() == 0,
            ));
        }
        let f = &self.facets;
        out.push(part("facet:visible", f.visible.serialized_size(), f.visible.len() as usize));
        out.push(part("facet:cover", f.cover.serialized_size(), f.cover.len() as usize));
        let mut groups: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for (key, bits) in &f.values {
            let g = groups
                .entry(key.split(':').next().unwrap_or_default().to_string())
                .or_default();
            // Ключ строкой + битовая карта.
            g.0 += bits.serialized_size() + key.len() + 100;
            g.1 += 1;
        }
        for (group, (bytes, n)) in groups {
            out.push(part(&format!("facet:{group}"), bytes, n));
        }
        out.push(part(
            "facet:year",
            f.years.values().map(|b| b.serialized_size()).sum(),
            f.years.len(),
        ));
        for (group, table) in &f.hints {
            out.push(part(&format!("hints:{group}"), table.bytes() as usize, table.len()));
        }
        out
    }

    /// Самые частые значения каждой группы фильтров в памяти: группа → (значение, сущностей).
    pub fn top_values(&self, n: usize) -> BTreeMap<String, Vec<(String, u64)>> {
        let mut out: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();
        for (key, bits) in &self.facets.values {
            let (group, value) = key.split_once(':').unwrap_or((key.as_str(), ""));
            out.entry(group.to_string())
                .or_default()
                .push((value.to_string(), bits.len()));
        }
        out.insert(
            "year".into(),
            self.facets
                .years
                .iter()
                .map(|(y, b)| (y.to_string(), b.len()))
                .collect(),
        );
        for list in out.values_mut() {
            list.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            list.truncate(n);
        }
        out
    }

    /// Первые `n` номеров каждого готового порядка: (имя, длина, номера).
    pub fn order_heads(&self, n: usize) -> Vec<(&'static str, u64, Vec<u32>)> {
        let mut out = vec![(
            "recommended",
            self.recommended.ords.len() as u64,
            self.recommended.ords.iter().take(n).copied().collect(),
        )];
        for (name, order) in ORDER_NAMES.iter().zip(self.orders.all()) {
            out.push((
                *name,
                order.ords.len() as u64,
                order.ords.iter().take(n).copied().collect(),
            ));
        }
        out
    }

    /// Всё, что слой Б хранит о сущности с номером `ord`.
    pub fn inspect(&self, ord: u32) -> Option<EntityInspect> {
        let i = ord as usize;
        if i >= self.meta.len() {
            return None;
        }
        let o = &self.orders;
        let find = |order: &Order| order.ords.iter().position(|&x| x == ord).map(|p| p as u32);
        let mut positions = vec![(
            "recommended",
            find(&self.recommended),
            self.recommended.ords.len() as u64,
        )];
        for (name, order) in ORDER_NAMES.iter().zip(o.all()) {
            positions.push((*name, find(order), order.ords.len() as u64));
        }
        let mut facets: Vec<String> = self
            .facets
            .values
            .iter()
            .filter(|(_, bits)| bits.contains(ord))
            .map(|(key, _)| key.clone())
            .collect();
        facets.extend(
            self.facets
                .years
                .iter()
                .filter(|(_, bits)| bits.contains(ord))
                .map(|(y, _)| format!("year:{y}")),
        );
        facets.sort();
        let base = o.base_rank.get(i).copied().unwrap_or(0);
        Some(EntityInspect {
            ord,
            visible: self.facets.visible.contains(ord),
            meta: self.meta[i],
            rank: base >> 32,
            release_day: o.release_day.get(i).copied().unwrap_or(0),
            positions,
            facets,
        })
    }
}

/// `f32` с полным порядком (NaN не бывает: оценки Tantivy конечны).
#[derive(Debug, Clone, Copy, PartialEq)]
struct OrdF32(f32);
impl Eq for OrdF32 {}
impl PartialOrd for OrdF32 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdF32 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Сборка [`EntitySidecar`] из сущностей (порядок добавления не важен).
#[derive(Default)]
pub struct SidecarBuilder {
    /// Счётчики подсказок групп [`TEXT_FACET_GROUPS`]: (группа, ключ) → (название, счётчики).
    tally: HashMap<(&'static str, String), (String, [u32; HINT_COUNTS])>,
    meta: Vec<SortMeta>,
    facets: Facets,
    release_day: Vec<u32>,
    base_rank: Vec<u64>,
    /// (ключ сортировки, номер) по порядкам.
    title: Vec<(u64, u32)>,
    popular: Vec<(u64, u32)>,
    rating: Vec<(u64, u32)>,
    trending: Vec<(u64, u32)>,
    top_rated: Vec<(u64, u32)>,
    awarded: Vec<(u64, u32)>,
}

impl SidecarBuilder {
    pub fn add(&mut self, doc: &EntityDoc) {
        let ord = doc.ord;
        let at = ord as usize;
        if self.meta.len() <= at {
            self.meta.resize(at + 1, SortMeta::default());
            self.release_day.resize(at + 1, 0);
            self.base_rank.resize(at + 1, 0);
        }
        let flag = |on: bool, bit: u16| if on { bit } else { 0 };
        self.meta[at] = SortMeta {
            manual_rank: doc.manual_rank,
            flags: flag(doc.cover_url.is_some(), FLAG_COVER)
                | flag(doc.has_description, FLAG_DESCRIPTION)
                | flag(!doc.authors.is_empty(), FLAG_AUTHOR),
            popularity: doc.popularity_meta(),
        };
        let day = doc.release_date.map_or(0, day_of);
        self.release_day[at] = day.min(u64::from(u32::MAX)) as u32;
        self.base_rank[at] = doc.base_rank();

        let facets = &mut self.facets;
        facets.visible.insert(ord);
        if doc.cover_url.is_some() {
            facets.cover.insert(ord);
        }
        let slot = kind_slot(doc.kind);
        for (group, names) in TEXT_FACET_GROUPS
            .iter()
            .zip([&doc.studios, &doc.publishers, &doc.museums])
        {
            let mut seen: Vec<String> = Vec::new();
            for name in names {
                let name = name.trim();
                let key = normalize_key(name);
                if key.is_empty() || seen.contains(&key) {
                    continue;
                }
                seen.push(key.clone());
                let entry = self
                    .tally
                    .entry((group, key))
                    .or_insert_with(|| (name.to_string(), [0; HINT_COUNTS]));
                entry.1[0] += 1;
                entry.1[slot] += 1;
            }
        }
        let facets = &mut self.facets;
        let mut put = |key: String| {
            facets.values.entry(key).or_default().insert(ord);
        };
        put(kind_key(doc.kind));
        if doc.kind == EntityKind::Book {
            put(form_key(doc.form));
        }
        for tag in &doc.tags {
            put(tag_key(tag));
        }
        for code in &doc.countries {
            put(country_key(code));
        }
        for &year in &doc.award_years {
            put(awardyear_key(year));
        }
        if doc.award_wins > 0 {
            put(AWARDED.to_string());
        }
        if let Some(language) = doc.language.as_deref().filter(|l| !l.trim().is_empty()) {
            put(language_key(language));
        }
        if let Some(date) = doc.release_date {
            facets.years.entry(date.year()).or_default().insert(ord);
        }

        let count = doc.ratings_count.clamp(0, (1 << 24) - 1) as u64;
        let average = (doc.rating_avg.unwrap_or(0.0).clamp(0.0, 100.0) * 1000.0) as u64;
        let rank_q = scaled(doc.rank_score, 1000.0);
        self.rating.push(((average << 24) | count, ord));
        self.popular.push(((count << 24) | (day & 0xFF_FFFF), ord));
        self.trending
            .push(((scaled(doc.popularity, 100.0) << 32) | rank_q, ord));
        self.top_rated
            .push(((scaled(doc.quality, 1000.0) << 32) | rank_q, ord));
        self.awarded
            .push(((doc.award_wins.clamp(0, 0xFFFF) as u64) << 32 | rank_q, ord));
        self.title.push((title_sort_key(&doc.title), ord));
    }

    pub fn finish(self) -> EntitySidecar {
        let Self {
            tally,
            meta,
            mut facets,
            release_day,
            base_rank,
            mut title,
            mut popular,
            mut rating,
            mut trending,
            mut top_rated,
            mut awarded,
        } = self;
        let len = meta.len();
        // При равных ключах — по номеру: выдача не зависит от порядка чтения из БД.
        let desc = |v: &mut Vec<(u64, u32)>| {
            v.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            v.iter().map(|x| x.1).collect::<Vec<u32>>()
        };
        let popular = desc(&mut popular);
        let rating = desc(&mut rating);
        let trending = desc(&mut trending);
        let top_rated = desc(&mut top_rated);
        let awarded = desc(&mut awarded);
        title.sort_unstable();
        let title: Vec<u32> = title.into_iter().map(|x| x.1).collect();
        let mut new: Vec<(u64, u32)> = facets
            .visible
            .iter()
            .filter(|&ord| release_day[ord as usize] > 0)
            .map(|ord| (u64::from(release_day[ord as usize]), ord))
            .collect();
        let new = desc(&mut new);
        facets.build_bitmap_hints();
        let mut groups: HashMap<&'static str, Vec<HintItem>> = HashMap::new();
        for ((group, key), (label, counts)) in tally {
            groups.entry(group).or_default().push((key, label, counts));
        }
        for group in TEXT_FACET_GROUPS {
            let items = groups.remove(group).unwrap_or_default();
            facets.hints.insert(group, HintTable::build(items));
        }
        let orders = Orders {
            new: Order::new(new, len),
            title: Order::new(title, len),
            popular: Order::new(popular, len),
            rating: Order::new(rating, len),
            trending: Order::new(trending, len),
            top_rated: Order::new(top_rated, len),
            awarded: Order::new(awarded, len),
            release_day: release_day.into(),
            base_rank: base_rank.into(),
        };
        let recommended = recommended(&meta, &orders, &facets.visible);
        EntitySidecar {
            generation: next_generation(),
            meta: meta.into(),
            recommended,
            facets: Arc::new(facets),
            orders: Arc::new(orders),
        }
    }
}

// ------------------------------------------------------------- снимок на диске

/// Файл снимка рядом с индексом: сервер стартует без похода в БД. Сборка из БД его
/// перезаписывает.
pub const SNAPSHOT_FILE: &str = "sidecar-entities.bin";
/// Массивы и порядки слоя Б сущностей ([`columns`]).
pub const COLUMNS_FILE: &str = "sidecar-entities.cols";
const MAGIC: &[u8; 8] = b"NXCATE03";

pub(super) fn put_u32(out: &mut impl Write, v: u32) -> std::io::Result<()> {
    out.write_all(&v.to_le_bytes())
}
pub(super) fn put_str(out: &mut impl Write, s: &str) -> std::io::Result<()> {
    put_u32(out, s.len() as u32)?;
    out.write_all(s.as_bytes())
}
pub(super) fn put_bits(out: &mut impl Write, bits: &RoaringBitmap) -> std::io::Result<()> {
    put_u32(out, bits.serialized_size() as u32)?;
    bits.serialize_into(out)
}
pub(super) fn get_u32(input: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    input.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
pub(super) fn get_str(input: &mut impl Read) -> std::io::Result<String> {
    let len = get_u32(input)? as usize;
    let mut b = vec![0u8; len];
    input.read_exact(&mut b)?;
    String::from_utf8(b).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
pub(super) fn get_bits(input: &mut impl Read) -> std::io::Result<RoaringBitmap> {
    let len = get_u32(input)? as u64;
    RoaringBitmap::deserialize_from(input.take(len))
}

impl EntitySidecar {
    /// Записать снимок атомарно: массивы и порядки — файлом колонок ([`COLUMNS_FILE`], читается
    /// через mmap), фасеты — компактным файлом ([`SNAPSHOT_FILE`], читается в память).
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let o = &self.orders;
        let orders: [&Order; 8] = [
            &self.recommended,
            &o.new,
            &o.title,
            &o.popular,
            &o.rating,
            &o.trending,
            &o.top_rated,
            &o.awarded,
        ];
        let mut cols: Vec<&[u8]> = vec![
            bytemuck::cast_slice(&self.meta),
            bytemuck::cast_slice(&o.release_day),
            bytemuck::cast_slice(&o.base_rank),
        ];
        for order in orders {
            cols.push(bytemuck::cast_slice(&order.ords));
            cols.push(bytemuck::cast_slice(order.pos()));
        }
        columns::write(&dir.join(COLUMNS_FILE), &cols)?;
        let tmp = dir.join(format!("{SNAPSHOT_FILE}.tmp"));
        {
            let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            out.write_all(MAGIC)?;
            let f = &self.facets;
            put_bits(&mut out, &f.visible)?;
            put_bits(&mut out, &f.cover)?;
            put_u32(&mut out, f.values.len() as u32)?;
            for (key, bits) in &f.values {
                put_str(&mut out, key)?;
                put_bits(&mut out, bits)?;
            }
            put_u32(&mut out, f.years.len() as u32)?;
            for (year, bits) in &f.years {
                put_u32(&mut out, *year as u32)?;
                put_bits(&mut out, bits)?;
            }
            put_u32(&mut out, f.hints.len() as u32)?;
            for (group, table) in &f.hints {
                put_str(&mut out, group)?;
                put_u32(&mut out, table.len() as u32)?;
                for (key, label, counts) in table.iter() {
                    put_str(&mut out, key)?;
                    put_str(&mut out, label)?;
                    for c in counts {
                        put_u32(&mut out, *c)?;
                    }
                }
            }
            out.flush()?;
        }
        std::fs::rename(&tmp, dir.join(SNAPSHOT_FILE))
    }

    /// Прочитать снимок: колонки отображаются (не копируются), фасеты читаются. Нет файлов или
    /// они другого формата — `None`.
    pub fn load(dir: &Path) -> Option<Self> {
        let result = (|| -> std::io::Result<Self> {
            let cols = Columns::open(dir, COLUMNS_FILE)?;
            let file = std::fs::File::open(dir.join(SNAPSHOT_FILE))?;
            Self::read(&cols, &mut std::io::BufReader::new(file))
        })();
        match result {
            Ok(sidecar) => Some(sidecar),
            Err(error) => {
                tracing::warn!(%error, "catalog sidecar snapshot is unreadable, ignored");
                None
            }
        }
    }

    fn read(cols: &Columns, input: &mut impl Read) -> std::io::Result<Self> {
        let invalid =
            |message: &'static str| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
        let mut magic = [0u8; 8];
        input.read_exact(&mut magic)?;
        if &magic != MAGIC || cols.len() != 3 + 8 * 2 {
            return Err(invalid("unknown snapshot format"));
        }
        let meta: Column<SortMeta> = cols.get(0)?;
        let release_day: Column<u32> = cols.get(1)?;
        let base_rank: Column<u64> = cols.get(2)?;
        let len = meta.len();
        let order = |i: usize| -> std::io::Result<Order> {
            let pos: Column<u32> = cols.get(3 + i * 2 + 1)?;
            if pos.len() != len {
                return Err(invalid("order positions differ in length"));
            }
            Ok(Order::mapped(cols.get(3 + i * 2)?, pos))
        };
        let recommended = order(0)?;
        let (new, title, popular, rating, trending, top_rated, awarded) = (
            order(1)?,
            order(2)?,
            order(3)?,
            order(4)?,
            order(5)?,
            order(6)?,
            order(7)?,
        );
        let visible = get_bits(input)?;
        let cover = get_bits(input)?;
        let mut values = HashMap::new();
        for _ in 0..get_u32(input)? {
            let key = get_str(input)?;
            values.insert(key, get_bits(input)?);
        }
        let mut years = BTreeMap::new();
        for _ in 0..get_u32(input)? {
            let year = get_u32(input)? as i32;
            years.insert(year, get_bits(input)?);
        }
        let mut hints = HashMap::new();
        for _ in 0..get_u32(input)? {
            let group = get_str(input)?;
            let Some(&group) = HINT_GROUPS.iter().find(|g| **g == group) else {
                return Err(invalid("unknown hint group"));
            };
            let n = get_u32(input)? as usize;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                let key = get_str(input)?;
                let label = get_str(input)?;
                let mut counts = [0u32; HINT_COUNTS];
                for c in &mut counts {
                    *c = get_u32(input)?;
                }
                items.push((key, label, counts));
            }
            hints.insert(group, HintTable::build(items));
        }
        if release_day.len() != len || base_rank.len() != len {
            return Err(invalid("snapshot arrays differ in length"));
        }
        let facets = Facets {
            visible,
            cover,
            values,
            years,
            hints,
        };
        Ok(Self {
            generation: next_generation(),
            meta,
            recommended,
            facets: Arc::new(facets),
            orders: Arc::new(Orders {
                new,
                title,
                popular,
                rating,
                trending,
                top_rated,
                awarded,
                release_day,
                base_rank,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SummaryAuthor;

    fn book(ord: u32, title: &str, tags: &[&str], rank: u16) -> EntityDoc {
        EntityDoc {
            id: uuid::Uuid::new_v4(),
            ord,
            kind: EntityKind::Book,
            slug: format!("b{ord}"),
            title: title.into(),
            authors: vec![SummaryAuthor {
                slug: "a".into(),
                name: "A".into(),
            }],
            tags: tags.iter().map(|t| t.to_string()).collect(),
            manual_rank: rank,
            ..EntityDoc::default()
        }
    }

    fn film(ord: u32, title: &str, kind: EntityKind, rank_score: f64) -> EntityDoc {
        EntityDoc {
            id: uuid::Uuid::new_v4(),
            ord,
            kind,
            slug: format!("f{ord}"),
            title: title.into(),
            rank_score: Some(rank_score),
            ..EntityDoc::default()
        }
    }

    fn build(docs: &[EntityDoc]) -> EntitySidecar {
        let mut builder = SidecarBuilder::default();
        for doc in docs {
            builder.add(doc);
        }
        builder.finish()
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
    }

    fn kinds(kinds: &[EntityKind]) -> Query {
        Query {
            kinds: kinds.to_vec(),
            ..Query::default()
        }
    }

    #[test]
    fn manual_rank_is_global_and_relative_inside_a_facet() {
        let mut rated = book(0, "Без места", &["roman"], 0);
        rated.ratings_count = 100;
        let side = build(&[
            rated,
            book(1, "Первая", &["fantastika"], 1),
            book(2, "Вторая", &["detektiv"], 2),
            book(3, "Третья", &["roman"], 3),
        ]);
        let all = side.filter(&Query::default());
        let (page, total) = side.page(Sort::Recommended, &all, today(), 10);
        assert_eq!(page, [1, 2, 3, 0]);
        assert_eq!(total, 4);
        let roman = side.filter(&Query {
            tags: vec!["roman".into()],
            ..Query::default()
        });
        assert_eq!(side.page(Sort::Relevance, &roman, today(), 10).0, [3, 0]);
    }

    #[test]
    fn kinds_split_the_one_index_and_book_forms_apply_to_books_only() {
        let mut collection = book(1, "Сборник", &[], 0);
        collection.form = BookForm::Collection;
        let side = build(&[
            book(0, "Книга", &[], 0),
            collection,
            film(2, "Фильм", EntityKind::Movie, 5.0),
            film(3, "Игра", EntityKind::Game, 1.0),
        ]);
        let ords = |q: Query| side.filter(&q).iter().collect::<Vec<u32>>();
        // Сборник не попадает в обычную выдачу, фильм и игра — попадают.
        assert_eq!(ords(Query::default()), [0, 2, 3]);
        assert_eq!(ords(kinds(&[EntityKind::Book])), [0]);
        assert_eq!(ords(kinds(&[EntityKind::Movie, EntityKind::Game])), [2, 3]);
        assert_eq!(
            ords(Query {
                forms: BookForm::ALL.to_vec(),
                kinds: vec![EntityKind::Book],
                ..Query::default()
            }),
            [0, 1]
        );
    }

    #[test]
    fn sorts_cover_all_visible_entities() {
        let mut matrix = film(0, "Матрица", EntityKind::Movie, 5.0);
        matrix.popularity = Some(1.0);
        matrix.quality = Some(8.0);
        matrix.award_wins = 4;
        matrix.award_slugs = vec!["oscar".into()];
        matrix.award_years = vec![2000];
        matrix.studios = vec!["Warner Bros.".into()];
        matrix.countries = vec!["US".into()];
        matrix.release_date = NaiveDate::from_ymd_opt(1999, 3, 31);
        let mut reload = film(1, "Перезагрузка", EntityKind::Movie, 3.0);
        reload.popularity = Some(9.0);
        reload.quality = Some(9.0);
        reload.release_date = NaiveDate::from_ymd_opt(2003, 5, 15);
        let witcher = film(2, "Ведьмак 3", EntityKind::Game, 9.0);
        let side = build(&[matrix, reload, witcher]);
        let all = side.filter(&Query::default());
        let sorted = |sort| side.page(sort, &all, today(), 10);
        assert_eq!(sorted(Sort::Recommended).0, [2, 0, 1]);
        assert_eq!(sorted(Sort::Trending).0[..2], [1, 0]);
        assert_eq!(sorted(Sort::TopRated).0[..2], [1, 0]);
        assert_eq!(sorted(Sort::Awarded).0[0], 0);
        assert_eq!(sorted(Sort::New), (vec![1, 0], 2));
        assert_eq!(sorted(Sort::Title).0[0], 2);
        let q = |change: fn(&mut Query)| {
            let mut q = Query::default();
            change(&mut q);
            side.filter(&q).iter().collect::<Vec<u32>>()
        };
        assert_eq!(q(|q| q.countries = vec!["us".into()]), [0]);
        assert_eq!(q(|q| q.award_year = Some(2000)), [0]);
        // Студия и награда — не в RAM: фильтр слоя Б их не трогает, это термы слоя А.
        assert_eq!(q(|q| q.studio = Some("warner bros".into())).len(), 3);
        assert_eq!(q(|q| q.awarded = Some(false)), [1, 2]);
        assert_eq!(q(|q| q.year_from = Some(2000)), [1]);
        assert_eq!(q(|q| q.only_ords = Some(vec![2])), [2]);
    }

    #[test]
    fn text_facets_match_query_terms() {
        let mut doc = film(0, "Матрица", EntityKind::Movie, 5.0);
        doc.studios = vec!["Warner Bros.".into(), " ".into()];
        doc.studio_slugs = vec!["warner-bros".into()];
        doc.award_slugs = vec!["oscar".into()];
        doc.award_years = vec![2000];
        doc.credits = vec![("director".into(), "wachowski".into())];
        let terms = text_facets(&doc);
        let wanted = |change: fn(&mut Query)| {
            let mut q = Query::default();
            change(&mut q);
            text_facet_terms(&q)
        };
        for q in [
            wanted(|q| q.studio = Some("WARNER  bros.".into())),
            wanted(|q| q.studio_slug = Some("warner-bros".into())),
            wanted(|q| q.award = Some("oscar".into())),
            wanted(|q| {
                q.award = Some("oscar".into());
                q.award_year = Some(2000);
            }),
            wanted(|q| q.person = Some("wachowski".into())),
            wanted(|q| {
                q.person = Some("wachowski".into());
                q.role = Some("director".into());
            }),
        ] {
            let q = q.unwrap();
            assert_eq!(q.len(), 1);
            assert!(terms.contains(&q[0]), "{q:?} not in {terms:?}");
        }
        assert!(!terms.iter().any(|t| t.starts_with("studio:") && t.len() == 7));
        assert_eq!(wanted(|q| q.studio = Some("!!!".into())), None);
        assert_eq!(wanted(|_| {}), Some(Vec::new()));
    }

    #[test]
    fn hints_count_per_kind_without_bitmaps() {
        let mut a = film(0, "А", EntityKind::Movie, 1.0);
        a.studios = vec!["Pixar".into(), "pixar".into()];
        let mut b = film(1, "Б", EntityKind::Game, 1.0);
        b.studios = vec!["Pixar".into()];
        let side = build(&[a, b]);
        assert_eq!(side.hints("studio", "pix", None, 5), [("Pixar".to_string(), 2)]);
        assert_eq!(
            side.hints("studio", "", Some(EntityKind::Game), 5),
            [("Pixar".to_string(), 1)]
        );
        assert!(side.hints("studio", "", Some(EntityKind::Series), 5).is_empty());
    }

    #[test]
    fn manual_rank_lifts_only_strong_text_matches() {
        let side = build(&[
            book(0, "Дюна", &[], 0),
            book(1, "Мастер и Маргарита", &[], 1),
            book(2, "Дети Дюны", &[], 0),
            book(3, "Дюна: сборник", &[], 2),
        ]);
        let hits = [(0, 10.0), (2, 6.0), (1, 1.0), (3, 6.0)];
        let exact = RoaringBitmap::from_iter([0u32]);
        let titled = RoaringBitmap::from_iter([0u32, 1, 2, 3]);
        // 3 — сильное совпадение с ручным местом; 1 — место есть, но совпадение слабое.
        assert_eq!(side.rank_hits(&hits, &exact, &titled, 10), [3, 0, 2, 1]);
        assert_eq!(side.rank_hits(&hits, &exact, &titled, 2), [3, 0]);
    }

    #[test]
    fn popular_title_match_beats_obscure_exact_title() {
        let side = build(&[
            film(0, "Мастер", EntityKind::Movie, 0.05),
            film(1, "Мастер и Маргарита", EntityKind::Movie, 0.9),
        ]);
        let hits = [(0, 10.0), (1, 7.0)];
        let exact = RoaringBitmap::from_iter([0u32]);
        let titled = RoaringBitmap::from_iter([0u32, 1]);
        assert_eq!(side.rank_hits(&hits, &exact, &titled, 10), [1, 0]);
    }

    #[test]
    fn title_match_beats_people_only_match() {
        let side = build(&[
            book(0, "Сияние", &[], 0),
            book(1, "Мастер и Маргарита", &[], 0),
        ]);
        // «маст»: у 0 совпало только имя в составе (оценка выше), у 1 — название.
        let hits = [(0, 10.0), (1, 5.0)];
        let titled = RoaringBitmap::from_iter([1u32]);
        assert_eq!(
            side.rank_hits(&hits, &RoaringBitmap::new(), &titled, 10),
            [1, 0]
        );
    }

    #[test]
    fn hints_count_inside_a_kind_and_prefer_prefix_matches() {
        let mut a = film(0, "А", EntityKind::Movie, 1.0);
        a.studios = vec!["Warner Bros.".into(), "Pixar".into()];
        let mut b = film(1, "Б", EntityKind::Game, 1.0);
        b.studios = vec!["Warner Bros.".into()];
        b.publishers = vec!["Valve".into()];
        let side = build(&[a, b]);
        assert_eq!(
            side.hints("studio", "", None, 10),
            [("Warner Bros.".to_string(), 2), ("Pixar".to_string(), 1)]
        );
        assert_eq!(
            side.hints("studio", "", Some(EntityKind::Movie), 10).len(),
            2
        );
        assert_eq!(
            side.hints("studio", "war", Some(EntityKind::Game), 10),
            [("Warner Bros.".to_string(), 1)]
        );
        assert_eq!(side.hints("publisher", "val", None, 10)[0].0, "Valve");
        assert!(side.hints("museum", "", None, 10).is_empty());
    }

    #[test]
    fn narrow_and_wide_filters_give_the_same_order() {
        let docs: Vec<EntityDoc> = (0..100)
            .map(|ord| {
                let tags: &[&str] = if ord % 25 == 0 { &["redkiy"] } else { &[] };
                let mut doc = book(ord, &format!("Книга {ord:03}"), tags, 0);
                doc.ratings_count = i64::from(ord % 7);
                doc
            })
            .collect();
        let side = build(&docs);
        let narrow = side.filter(&Query {
            tags: vec!["redkiy".into()],
            ..Query::default()
        });
        let wide = side.filter(&Query::default());
        for sort in [Sort::Title, Sort::Popular, Sort::Recommended] {
            let (narrow_page, total) = side.page(sort, &narrow, today(), 10);
            let wide_page: Vec<u32> = side
                .page(sort, &wide, today(), 100)
                .0
                .into_iter()
                .filter(|ord| narrow.contains(*ord))
                .collect();
            assert_eq!(narrow_page, wide_page, "{sort:?}");
            assert_eq!(total, 4);
        }
    }

    #[test]
    fn snapshot_round_trips() {
        let mut first = book(0, "Ёжик в тумане", &["skazka"], 2);
        first.publishers = vec!["АСТ".into()];
        first.release_date = NaiveDate::from_ymd_opt(1975, 1, 1);
        let side = build(&[first, book(3, "Б", &[], 1)]);
        let dir = std::env::temp_dir().join(format!("nexus-sidecar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        side.save(&dir).unwrap();
        let loaded = EntitySidecar::load(&dir).unwrap();
        assert_eq!(&loaded.meta[..], &side.meta[..]);
        assert!(loaded.meta.is_mapped());
        assert_eq!(
            loaded.hints("publisher", "", None, 10),
            [("АСТ".to_string(), 1)]
        );
        let all = loaded.filter(&Query::default());
        assert_eq!(
            loaded.page(Sort::Recommended, &all, today(), 10),
            side.page(Sort::Recommended, &all, today(), 10)
        );
        assert_eq!(loaded.page(Sort::New, &all, today(), 10).0, [0]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
