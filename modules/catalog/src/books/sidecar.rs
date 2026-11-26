//! Слой Б книжного поиска: фасеты, ручной порядок и сортировки — в RAM, рядом с Tantivy.
//!
//! Tantivy (слой А, [`super::index`]) хранит только то, что меняется редко: текст для поиска
//! (название, авторы, серия), карточку выдачи и плотный номер книги `ord`. Всё, что меняется
//! ежедневно — ручное место в выдаче, оценки, теги и жанры, люди, издательство, язык, год,
//! обложка, видимость, — живёт здесь: [`Sidecar`] целиком собирается из PostgreSQL
//! ([`super::sync::reload_sidecar`]) и атомарно подменяется (`ArcSwap`) без записи в индекс.
//!
//! Книга здесь — номер `ord: u32` (таблица `catalog_entity_ords`): индекс в плоских массивах и
//! бит в `RoaringBitmap`. Номер стабилен: перестройка Tantivy и слияние сегментов его не меняют.
//!
//! **Ручной порядок** ([`SortMeta::manual_rank`]): `1..=65535` — место в общем списке книг
//! (`catalog_manual_ranks`), `0` — нет места. Ранг глобальный: в фасете книги сохраняют
//! относительный порядок (третья книга списка — первая в жанре, если первые две не из него).
//! В тексте ручное место поднимает только сильные совпадения ([`STRONG_SHARE`]), а точное
//! название запроса стоит выше любого ручного места ([`Sidecar::rank_hits`]).

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{Datelike, NaiveDate};
use roaring::RoaringBitmap;

use super::index::{day_of, normalize_key, title_sort_key, BookDoc, BookForm, BookQuery, BookSort};

/// Флаги книги в [`SortMeta::flags`].
pub const FLAG_COVER: u16 = 1;
pub const FLAG_DESCRIPTION: u16 = 2;
pub const FLAG_AUTHOR: u16 = 4;

/// Доля лучшей оценки текста, с которой совпадение считается сильным: только такие книги
/// ручное место поднимает над остальными. Слабое совпадение (опечатка, слово в серии) с ручным
/// местом не обгоняет хорошее совпадение без него.
pub const STRONG_SHARE: f32 = 0.5;

/// Делитель логарифма известности автора в множителе релевантности: автор с 500 работами
/// получает ×1,4, с пятью — ×1,1. Точное название (вес 12) таким множителем не перебить.
pub const POPULARITY_DAMPING: f32 = 15.0;

/// Фильтр уже узкого раздела сортируется по позициям ([`Order::pos`]), широкий — проходом по
/// готовому порядку: при `|фильтр| * NARROW < книг` сортировка выгоднее прохода.
const NARROW: u64 = 16;

/// Ключи фасетов (`Facets::values`).
pub fn tag_key(slug: &str) -> String {
    format!("tag:{slug}")
}
pub fn person_key(slug: &str) -> String {
    format!("person:{slug}")
}
pub fn role_key(role: &str, slug: &str) -> String {
    format!("role:{role}:{slug}")
}
pub fn publisher_key(name: &str) -> String {
    format!("pub:{}", normalize_key(name))
}
pub fn form_key(form: BookForm) -> String {
    format!("form:{}", form.as_str())
}
pub fn language_key(code: &str) -> String {
    format!("lang:{}", code.trim().to_lowercase())
}

/// Сортировочные данные книги: 8 байт, индекс в массиве — `ord`. На 300 000 книг — 2,4 МБ.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct SortMeta {
    /// `0` — нет ручного места; `1..=65535` — место в общем списке книг.
    pub manual_rank: u16,
    /// [`FLAG_COVER`] | [`FLAG_DESCRIPTION`] | [`FLAG_AUTHOR`].
    pub flags: u16,
    /// Известность автора (сколько у него работ): множитель релевантности.
    pub popularity: u32,
}

const _: () = assert!(std::mem::size_of::<SortMeta>() == 8);

/// Готовый порядок раздела по одной сортировке: книги по порядку и позиция каждой книги в нём
/// (`u32::MAX` — книги в порядке нет).
#[derive(Debug, Default, Clone)]
pub struct Order {
    ords: Vec<u32>,
    pos: Vec<u32>,
}

impl Order {
    fn new(ords: Vec<u32>, len: usize) -> Self {
        let mut pos = vec![u32::MAX; len];
        for (at, &ord) in ords.iter().enumerate() {
            pos[ord as usize] = at as u32;
        }
        Self { ords, pos }
    }

    /// Первые `len` книг порядка из `filter` (`accept` — доп. условие) и сколько их всего.
    fn take(
        &self,
        filter: &RoaringBitmap,
        len: usize,
        accept: impl Fn(u32) -> bool,
        exact_total: Option<u64>,
    ) -> (Vec<u32>, usize) {
        if filter.len() * NARROW < self.ords.len() as u64 {
            let mut picked: Vec<u32> = filter
                .iter()
                .filter(|&ord| self.pos.get(ord as usize).is_some_and(|&p| p != u32::MAX))
                .filter(|&ord| accept(ord))
                .collect();
            picked.sort_unstable_by_key(|&ord| self.pos[ord as usize]);
            let total = picked.len();
            picked.truncate(len);
            return (picked, total);
        }
        let mut out = Vec::with_capacity(len.min(filter.len() as usize));
        let mut total = 0usize;
        for &ord in &self.ords {
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

/// Фасеты: множества книг по значению. Собираются вместе с [`Sidecar`] и делятся между его
/// версиями, пока меняется только ручной порядок.
#[derive(Debug, Default)]
pub struct Facets {
    /// Все видимые книги.
    pub visible: RoaringBitmap,
    pub cover: RoaringBitmap,
    /// `tag:<slug>`, `person:<slug>`, `role:<role>:<slug>`, `pub:<ключ>`, `lang:<код>`.
    pub values: HashMap<String, RoaringBitmap>,
    pub years: BTreeMap<i32, RoaringBitmap>,
    /// Подсказки: теги и издательства (как написано) со счётчиками, частые сверху.
    pub tags: Vec<(String, u64)>,
    pub publishers: Vec<(String, u64)>,
}

/// Один вид фасетов (`tag`, `person`, `lang`…): сколько значений и сколько весят их битмапы.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FacetGroup {
    pub values: u64,
    pub bytes: u64,
}

/// Состояние слоя Б ([`Sidecar::stats`]). Байты — оценка памяти (битмапы — в сжатом виде).
#[derive(Debug, Clone)]
pub struct SidecarStats {
    pub generation: u64,
    /// Видимые книги.
    pub books: u64,
    /// Длина массивов по `ord` (с дырами от удалённых книг).
    pub slots: u64,
    pub with_cover: u64,
    pub manual_ranks: u64,
    /// Вид текста → книг.
    pub forms: Vec<(&'static str, u64)>,
    /// Префикс ключа → значения и байты.
    pub facets: Vec<(String, FacetGroup)>,
    pub meta_bytes: u64,
    pub orders_bytes: u64,
    pub facets_bytes: u64,
}

/// Порядки, не зависящие от ручных мест.
#[derive(Debug, Default)]
struct Orders {
    /// По дате выхода, новые сверху (только книги с датой).
    new: Order,
    title: Order,
    popular: Order,
    rating: Order,
    /// Дата выхода в днях от начала эры (`0` — нет даты): «вышла / не вышла» на сегодня.
    release_day: Vec<u32>,
    /// Балл «Рекомендуемые» без ручного места ([`BookDoc::rank`]).
    base_rank: Vec<u64>,
}

/// Поколение состояния: ключ кэша выдачи. Растёт с каждой подменой в процессе.
static GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Слой Б целиком: неизменяемый снимок, подменяется атомарно ([`super::index::BookIndex`]).
#[derive(Debug, Default)]
pub struct Sidecar {
    generation: u64,
    meta: Vec<SortMeta>,
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

impl Sidecar {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Сколько видимых книг.
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

    /// Новое состояние с другими ручными местами (`ord → место`; остальные книги — без места).
    /// Фасеты и прочие порядки не пересобираются: правка списка — миллисекунды.
    pub fn with_ranks(&self, ranks: &HashMap<u32, u16>) -> Self {
        let mut meta = self.meta.clone();
        for (ord, item) in meta.iter_mut().enumerate() {
            item.manual_rank = ranks.get(&(ord as u32)).copied().unwrap_or(0);
        }
        let recommended = recommended(&meta, &self.orders, &self.facets.visible);
        Self {
            generation: next_generation(),
            meta,
            recommended,
            facets: self.facets.clone(),
            orders: self.orders.clone(),
        }
    }

    /// Книги под фильтры запроса (текст не учитывается). `exclude` — номера книг «кроме».
    pub fn filter(&self, q: &BookQuery, exclude: &[u32]) -> RoaringBitmap {
        let facets = &self.facets;
        let mut out = facets.visible.clone();
        let value = |key: String| facets.values.get(&key).cloned().unwrap_or_default();
        if let Some(tag) = &q.tag {
            out &= value(tag_key(tag));
        }
        if !q.any_tags.is_empty() {
            let mut any = RoaringBitmap::new();
            for tag in &q.any_tags {
                if let Some(bits) = facets.values.get(&tag_key(tag)) {
                    any |= bits;
                }
            }
            out &= any;
        }
        if let Some(publisher) = &q.publisher {
            if normalize_key(publisher).is_empty() {
                return RoaringBitmap::new();
            }
            out &= value(publisher_key(publisher));
        }
        if let Some(person) = &q.person {
            out &= match &q.role {
                Some(role) => value(role_key(role, person)),
                None => value(person_key(person)),
            };
        }
        if let Some(language) = &q.language {
            out &= value(language_key(language));
        }
        let forms: &[BookForm] = if q.forms.is_empty() {
            &[BookForm::Book]
        } else {
            &q.forms
        };
        if !BookForm::ALL.iter().all(|f| forms.contains(f)) {
            let mut any = RoaringBitmap::new();
            for &form in forms {
                if let Some(bits) = facets.values.get(&form_key(form)) {
                    any |= bits;
                }
            }
            out &= any;
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
        for &ord in exclude {
            out.remove(ord);
        }
        out
    }

    /// Первые `len` книг из `filter` в порядке `sort` и сколько их всего. «Новые» — только
    /// вышедшие к `today`, «скоро» — только не вышедшие (книга без даты — ни там ни там).
    pub fn page(
        &self,
        sort: BookSort,
        filter: &RoaringBitmap,
        today: NaiveDate,
        len: usize,
    ) -> (Vec<u32>, usize) {
        let orders = &self.orders;
        let today = day_of(today) as u32;
        let day = |ord: u32| orders.release_day.get(ord as usize).copied().unwrap_or(0);
        let all = Some(filter.len());
        match sort {
            BookSort::Relevance | BookSort::Recommended => {
                self.recommended.take(filter, len, |_| true, all)
            }
            BookSort::Title => orders.title.take(filter, len, |_| true, all),
            BookSort::Popular => orders.popular.take(filter, len, |_| true, all),
            BookSort::Rating => orders.rating.take(filter, len, |_| true, all),
            BookSort::New => {
                orders
                    .new
                    .take(filter, len, |ord| (1..=today).contains(&day(ord)), None)
            }
            BookSort::Soon => {
                // Ближайшие сверху: тот же порядок по дате, пройденный с конца.
                let mut picked: Vec<u32> = Vec::new();
                let mut total = 0usize;
                for &ord in orders.new.ords.iter().rev() {
                    if day(ord) > today && filter.contains(ord) {
                        if picked.len() < len {
                            picked.push(ord);
                        }
                        total += 1;
                    }
                }
                (picked, total)
            }
        }
    }

    /// Порядок совпадений текста по релевантности (`hits` — номер и оценка Tantivy):
    /// 1. точное название запроса (`exact`): по ручному месту, затем по оценке;
    /// 2. сильные совпадения ([`STRONG_SHARE`]) с ручным местом — по месту;
    /// 3. остальные — по оценке × известность автора.
    ///
    /// Возвращает первые `len` номеров.
    pub fn rank_hits(&self, hits: &[(u32, f32)], exact: &RoaringBitmap, len: usize) -> Vec<u32> {
        let scored: Vec<(u32, f32)> = hits
            .iter()
            .map(|&(ord, score)| {
                let popularity = self.meta(ord).popularity as f32;
                (ord, score * (1.0 + popularity.ln_1p() / POPULARITY_DAMPING))
            })
            .collect();
        let top = scored.iter().map(|hit| hit.1).fold(0.0f32, f32::max);
        let strong = top * STRONG_SHARE;
        // Ключ: (группа, ручное место или 0, −оценка, номер).
        let key = |&(ord, score): &(u32, f32)| {
            let rank = self.meta(ord).manual_rank;
            let tier: u8 = if exact.contains(ord) {
                0
            } else if rank > 0 && score >= strong {
                1
            } else {
                2
            };
            let rank = match tier {
                0 if rank > 0 => u32::from(rank),
                0 => u32::from(u16::MAX) + 1,
                1 => u32::from(rank),
                _ => 0,
            };
            (tier, rank, std::cmp::Reverse(OrdF32(score)), ord)
        };
        let mut keyed: Vec<_> = scored.iter().map(|hit| (key(hit), hit.0)).collect();
        if keyed.len() > len {
            keyed.select_nth_unstable_by(len, |a, b| a.0.cmp(&b.0));
            keyed.truncate(len);
        }
        keyed.sort_unstable_by_key(|a| a.0);
        keyed.into_iter().map(|(_, ord)| ord).collect()
    }

    /// Состояние слоя Б для админки: книги, фасеты по видам ключей и оценка памяти.
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
        let years_bytes: u64 = f.years.values().map(|b| b.serialized_size() as u64).sum();
        groups.insert(
            "year".into(),
            FacetGroup {
                values: f.years.len() as u64,
                bytes: years_bytes,
            },
        );
        let forms = BookForm::ALL
            .iter()
            .map(|&form| {
                let n = f
                    .values
                    .get(&form_key(form))
                    .map_or(0, |b| b.intersection_len(&f.visible));
                (form.as_str(), n)
            })
            .collect();
        let order_bytes = |order: &Order| ((order.ords.len() + order.pos.len()) * 4) as u64;
        let orders_bytes = [&o.new, &o.title, &o.popular, &o.rating, &self.recommended]
            .into_iter()
            .map(order_bytes)
            .sum::<u64>()
            + (o.release_day.len() * 4 + o.base_rank.len() * 8) as u64;
        let facets_bytes = groups.values().map(|g| g.bytes).sum::<u64>()
            + (f.visible.serialized_size() + f.cover.serialized_size()) as u64;
        SidecarStats {
            generation: self.generation,
            books: f.visible.len(),
            slots: self.meta.len() as u64,
            with_cover: f.cover.len(),
            manual_ranks: self.meta.iter().filter(|m| m.manual_rank > 0).count() as u64,
            forms,
            facets: groups.into_iter().collect(),
            meta_bytes: (self.meta.len() * std::mem::size_of::<SortMeta>()) as u64,
            orders_bytes,
            facets_bytes,
        }
    }

    /// Счётчики значений фасета с префиксом `prefix` (`tag:`, `lang:`…) среди книг `filter`:
    /// для панели фильтров. Нули не возвращаются, частые сверху.
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

/// Сборка [`Sidecar`] из книг (порядок добавления не важен).
#[derive(Default)]
pub struct SidecarBuilder {
    meta: Vec<SortMeta>,
    facets: Facets,
    release_day: Vec<u32>,
    base_rank: Vec<u64>,
    title: Vec<(u64, u32)>,
    popular: Vec<(u64, u32)>,
    rating: Vec<(u64, u32)>,
    /// Ключ издательства → как написано у первой книги и сколько книг.
    publishers: HashMap<String, (String, u64)>,
}

impl SidecarBuilder {
    pub fn add(&mut self, doc: &BookDoc) {
        let ord = doc.ord;
        let at = ord as usize;
        if self.meta.len() <= at {
            self.meta.resize(at + 1, SortMeta::default());
            self.release_day.resize(at + 1, 0);
            self.base_rank.resize(at + 1, 0);
        }
        let flags = if doc.cover_url.is_some() {
            FLAG_COVER
        } else {
            0
        } | if doc.has_description {
            FLAG_DESCRIPTION
        } else {
            0
        } | if doc.authors.is_empty() {
            0
        } else {
            FLAG_AUTHOR
        };
        self.meta[at] = SortMeta {
            manual_rank: doc.manual_rank,
            flags,
            popularity: doc.author_works,
        };
        let day = doc.release_date.map_or(0, day_of);
        self.release_day[at] = day.min(u64::from(u32::MAX)) as u32;
        self.base_rank[at] = doc.rank();

        let facets = &mut self.facets;
        facets.visible.insert(ord);
        if doc.cover_url.is_some() {
            facets.cover.insert(ord);
        }
        let mut put = |key: String| {
            facets.values.entry(key).or_default().insert(ord);
        };
        put(form_key(doc.form));
        for tag in &doc.tags {
            put(tag_key(tag));
        }
        for (role, person) in &doc.credits {
            put(person_key(person));
            put(role_key(role, person));
        }
        if let Some(language) = doc.language.as_deref().filter(|l| !l.trim().is_empty()) {
            put(language_key(language));
        }
        if let Some(publisher) = doc.publisher.as_deref().map(str::trim) {
            let key = normalize_key(publisher);
            if !key.is_empty() {
                put(publisher_key(publisher));
                self.publishers
                    .entry(key)
                    .or_insert_with(|| (publisher.to_string(), 0))
                    .1 += 1;
            }
        }
        if let Some(date) = doc.release_date {
            facets.years.entry(date.year()).or_default().insert(ord);
        }

        let count = doc.ratings_count.clamp(0, (1 << 24) - 1) as u64;
        let average = (doc.rating_avg.unwrap_or(0.0).clamp(0.0, 100.0) * 1000.0) as u64;
        self.rating.push(((average << 24) | count, ord));
        self.popular.push(((count << 24) | (day & 0xFF_FFFF), ord));
        self.title.push((title_sort_key(&doc.title), ord));
    }

    pub fn finish(self) -> Sidecar {
        let Self {
            meta,
            mut facets,
            release_day,
            base_rank,
            mut title,
            mut popular,
            mut rating,
            publishers,
        } = self;
        let len = meta.len();
        // При равных ключах — по номеру: выдача не зависит от порядка чтения из БД.
        let desc = |v: &mut Vec<(u64, u32)>| {
            v.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            v.iter().map(|x| x.1).collect::<Vec<u32>>()
        };
        let popular = desc(&mut popular);
        let rating = desc(&mut rating);
        title.sort_unstable();
        let title: Vec<u32> = title.into_iter().map(|x| x.1).collect();
        let mut new: Vec<(u64, u32)> = facets
            .visible
            .iter()
            .filter(|&ord| release_day[ord as usize] > 0)
            .map(|ord| (u64::from(release_day[ord as usize]), ord))
            .collect();
        let new = desc(&mut new);

        let mut tags: Vec<(String, u64)> = facets
            .values
            .iter()
            .filter_map(|(key, bits)| {
                key.strip_prefix("tag:")
                    .map(|t| (t.to_string(), bits.len()))
            })
            .collect();
        tags.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        facets.tags = tags;
        let mut names: Vec<(String, u64)> = publishers.into_values().collect();
        names.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        facets.publishers = names;

        let orders = Orders {
            new: Order::new(new, len),
            title: Order::new(title, len),
            popular: Order::new(popular, len),
            rating: Order::new(rating, len),
            release_day,
            base_rank,
        };
        let recommended = recommended(&meta, &orders, &facets.visible);
        Sidecar {
            generation: next_generation(),
            meta,
            recommended,
            facets: Arc::new(facets),
            orders: Arc::new(orders),
        }
    }
}

// ------------------------------------------------------------- снимок на диске

/// Файл снимка рядом с индексом: сервер стартует и бенч (`nexus books load`) работает без
/// похода в БД. Сборка из БД его перезаписывает.
pub const SNAPSHOT_FILE: &str = "sidecar.bin";
/// `02` — фасеты `form:*`: старый снимок без них отдал бы пустую выдачу, его пересобирают из БД.
const MAGIC: &[u8; 8] = b"NXSIDE02";

fn put_u32(out: &mut impl Write, v: u32) -> std::io::Result<()> {
    out.write_all(&v.to_le_bytes())
}
fn put_u64(out: &mut impl Write, v: u64) -> std::io::Result<()> {
    out.write_all(&v.to_le_bytes())
}
fn put_str(out: &mut impl Write, s: &str) -> std::io::Result<()> {
    put_u32(out, s.len() as u32)?;
    out.write_all(s.as_bytes())
}
fn put_bits(out: &mut impl Write, bits: &RoaringBitmap) -> std::io::Result<()> {
    put_u32(out, bits.serialized_size() as u32)?;
    bits.serialize_into(out)
}
fn put_u32s(out: &mut impl Write, v: &[u32]) -> std::io::Result<()> {
    put_u32(out, v.len() as u32)?;
    for x in v {
        put_u32(out, *x)?;
    }
    Ok(())
}

fn get_u32(input: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    input.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn get_u64(input: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    input.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn get_str(input: &mut impl Read) -> std::io::Result<String> {
    let len = get_u32(input)? as usize;
    let mut b = vec![0u8; len];
    input.read_exact(&mut b)?;
    String::from_utf8(b).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
fn get_bits(input: &mut impl Read) -> std::io::Result<RoaringBitmap> {
    let len = get_u32(input)? as u64;
    RoaringBitmap::deserialize_from(input.take(len))
}
fn get_u32s(input: &mut impl Read) -> std::io::Result<Vec<u32>> {
    let len = get_u32(input)? as usize;
    (0..len).map(|_| get_u32(input)).collect()
}

impl Sidecar {
    /// Записать снимок атомарно (временный файл и переименование).
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let tmp = dir.join(format!("{SNAPSHOT_FILE}.tmp"));
        {
            let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            out.write_all(MAGIC)?;
            put_u32(&mut out, self.meta.len() as u32)?;
            for m in &self.meta {
                out.write_all(&m.manual_rank.to_le_bytes())?;
                out.write_all(&m.flags.to_le_bytes())?;
                put_u32(&mut out, m.popularity)?;
            }
            let o = &self.orders;
            put_u32s(&mut out, &o.release_day)?;
            put_u32(&mut out, o.base_rank.len() as u32)?;
            for v in &o.base_rank {
                put_u64(&mut out, *v)?;
            }
            for order in [&o.new, &o.title, &o.popular, &o.rating] {
                put_u32s(&mut out, &order.ords)?;
            }
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
            for list in [&f.tags, &f.publishers] {
                put_u32(&mut out, list.len() as u32)?;
                for (name, count) in list {
                    put_str(&mut out, name)?;
                    put_u64(&mut out, *count)?;
                }
            }
            out.flush()?;
        }
        std::fs::rename(&tmp, dir.join(SNAPSHOT_FILE))
    }

    /// Прочитать снимок. Нет файла или он другого формата — `None`.
    pub fn load(dir: &Path) -> Option<Self> {
        let file = std::fs::File::open(dir.join(SNAPSHOT_FILE)).ok()?;
        match Self::read(&mut std::io::BufReader::new(file)) {
            Ok(sidecar) => Some(sidecar),
            Err(error) => {
                tracing::warn!(%error, "books sidecar snapshot is unreadable, ignored");
                None
            }
        }
    }

    fn read(input: &mut impl Read) -> std::io::Result<Self> {
        let mut magic = [0u8; 8];
        input.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown snapshot format",
            ));
        }
        let len = get_u32(input)? as usize;
        let mut meta = Vec::with_capacity(len);
        for _ in 0..len {
            let mut b = [0u8; 4];
            input.read_exact(&mut b)?;
            meta.push(SortMeta {
                manual_rank: u16::from_le_bytes([b[0], b[1]]),
                flags: u16::from_le_bytes([b[2], b[3]]),
                popularity: get_u32(input)?,
            });
        }
        let release_day = get_u32s(input)?;
        let ranks = get_u32(input)? as usize;
        let base_rank = (0..ranks)
            .map(|_| get_u64(input))
            .collect::<std::io::Result<Vec<u64>>>()?;
        let mut order = || get_u32s(input).map(|ords| Order::new(ords, len));
        let (new, title, popular, rating) = (order()?, order()?, order()?, order()?);
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
        let mut list = || -> std::io::Result<Vec<(String, u64)>> {
            (0..get_u32(input)?)
                .map(|_| Ok((get_str(input)?, get_u64(input)?)))
                .collect()
        };
        let (tags, publishers) = (list()?, list()?);
        if release_day.len() != len || base_rank.len() != len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "snapshot arrays differ in length",
            ));
        }
        let facets = Facets {
            visible,
            cover,
            values,
            years,
            tags,
            publishers,
        };
        let orders = Orders {
            new,
            title,
            popular,
            rating,
            release_day,
            base_rank,
        };
        let recommended = recommended(&meta, &orders, &facets.visible);
        Ok(Self {
            generation: next_generation(),
            meta,
            recommended,
            facets: Arc::new(facets),
            orders: Arc::new(orders),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SummaryAuthor;

    fn book(ord: u32, title: &str, tags: &[&str], rank: u16) -> BookDoc {
        BookDoc {
            id: uuid::Uuid::new_v4(),
            ord,
            slug: format!("b{ord}"),
            title: title.into(),
            authors: vec![SummaryAuthor {
                slug: "a".into(),
                name: "A".into(),
            }],
            tags: tags.iter().map(|t| t.to_string()).collect(),
            manual_rank: rank,
            ..BookDoc::default()
        }
    }

    fn build(docs: &[BookDoc]) -> Sidecar {
        let mut builder = SidecarBuilder::default();
        for doc in docs {
            builder.add(doc);
        }
        builder.finish()
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
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
        let all = side.filter(&BookQuery::default(), &[]);
        let (page, total) = side.page(BookSort::Recommended, &all, today(), 10);
        assert_eq!(page, [1, 2, 3, 0]);
        assert_eq!(total, 4);
        // В жанре «roman» третья по списку — первая.
        let roman = side.filter(
            &BookQuery {
                tag: Some("roman".into()),
                ..BookQuery::default()
            },
            &[],
        );
        assert_eq!(
            side.page(BookSort::Relevance, &roman, today(), 10).0,
            [3, 0]
        );
    }

    #[test]
    fn only_books_by_default_other_forms_by_filter() {
        let mut collection = book(1, "Сборник", &[], 0);
        collection.form = BookForm::Collection;
        let mut article = book(2, "Статья", &[], 0);
        article.form = BookForm::Article;
        let side = build(&[book(0, "Книга", &[], 0), collection, article]);
        let with = |forms: Vec<BookForm>| {
            let bits = side.filter(
                &BookQuery {
                    forms,
                    ..BookQuery::default()
                },
                &[],
            );
            bits.iter().collect::<Vec<u32>>()
        };
        assert_eq!(with(vec![]), [0]);
        assert_eq!(with(vec![BookForm::Collection, BookForm::Article]), [1, 2]);
        assert_eq!(with(BookForm::ALL.to_vec()), [0, 1, 2]);
    }

    #[test]
    fn ranks_change_without_rebuilding_facets() {
        let side = build(&[book(0, "А", &[], 0), book(1, "Б", &[], 0)]);
        let ranked = side.with_ranks(&HashMap::from([(1, 1)]));
        assert_ne!(ranked.generation(), side.generation());
        assert!(Arc::ptr_eq(&ranked.facets, &side.facets));
        let all = ranked.filter(&BookQuery::default(), &[]);
        assert_eq!(
            ranked.page(BookSort::Recommended, &all, today(), 10).0,
            [1, 0]
        );
    }

    #[test]
    fn manual_rank_lifts_only_strong_text_matches_and_never_beats_exact_title() {
        let side = build(&[
            book(0, "Дюна", &[], 0),
            book(1, "Мастер и Маргарита", &[], 1),
            book(2, "Дети Дюны", &[], 0),
            book(3, "Дюна: сборник", &[], 2),
        ]);
        // 1 совпала по опечатке слабо — ручное место её не поднимает.
        let hits = [(0, 10.0), (2, 6.0), (1, 1.0), (3, 6.0)];
        let exact = RoaringBitmap::from_iter([0u32]);
        assert_eq!(side.rank_hits(&hits, &exact, 10), [0, 3, 2, 1]);
        assert_eq!(side.rank_hits(&hits, &exact, 2), [0, 3]);
    }

    #[test]
    fn facets_intersect_and_count() {
        let side = build(&[
            book(0, "А", &["roman", "klassika"], 0),
            book(1, "Б", &["roman"], 0),
            book(2, "В", &["klassika"], 0),
        ]);
        let classic = side.filter(
            &BookQuery {
                tag: Some("klassika".into()),
                ..BookQuery::default()
            },
            &[],
        );
        assert_eq!(classic.iter().collect::<Vec<_>>(), [0, 2]);
        assert_eq!(
            side.facet_counts(&classic, "tag:"),
            [("klassika".to_string(), 2), ("roman".to_string(), 1)]
        );
        assert_eq!(side.filter(&BookQuery::default(), &[1]).len(), 2);
        assert!(side
            .filter(
                &BookQuery {
                    tag: Some("net".into()),
                    ..BookQuery::default()
                },
                &[]
            )
            .is_empty());
    }

    #[test]
    fn narrow_and_wide_filters_give_the_same_order() {
        let docs: Vec<BookDoc> = (0..100)
            .map(|ord| {
                let tags: &[&str] = if ord % 25 == 0 { &["redkiy"] } else { &[] };
                let mut doc = book(ord, &format!("Книга {ord:03}"), tags, 0);
                doc.ratings_count = i64::from(ord % 7);
                doc
            })
            .collect();
        let side = build(&docs);
        let narrow = side.filter(
            &BookQuery {
                tag: Some("redkiy".into()),
                ..BookQuery::default()
            },
            &[],
        );
        let wide = side.filter(&BookQuery::default(), &[]);
        for sort in [BookSort::Title, BookSort::Popular, BookSort::Recommended] {
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
        first.publisher = Some("АСТ".into());
        first.release_date = NaiveDate::from_ymd_opt(1975, 1, 1);
        let side = build(&[first, book(3, "Б", &[], 1)]);
        let dir = std::env::temp_dir().join(format!("nexus-sidecar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        side.save(&dir).unwrap();
        let loaded = Sidecar::load(&dir).unwrap();
        assert_eq!(loaded.meta, side.meta);
        assert_eq!(loaded.facets.publishers, [("АСТ".to_string(), 1)]);
        let all = loaded.filter(&BookQuery::default(), &[]);
        assert_eq!(
            loaded.page(BookSort::Recommended, &all, today(), 10),
            side.page(BookSort::Recommended, &all, today(), 10)
        );
        assert_eq!(loaded.page(BookSort::New, &all, today(), 10).0, [0]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
