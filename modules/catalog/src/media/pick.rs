//! Чистые функции выбора: какой результат поиска — наша сущность, какой постер лучше, какой
//! ролик похож на настоящий трейлер. Без сети — поэтому всё здесь покрыто тестами.

use serde::Deserialize;

/// Год из даты `YYYY-MM-DD` (TMDB, IGDB после перевода).
pub fn year_of(date: &str) -> Option<i32> {
    date.get(..4)?.parse().ok()
}

/// Совпадает ли год с допуском ±1: даты премьер в разных странах расходятся.
pub fn year_matches(found: Option<i32>, expected: Option<i32>) -> bool {
    match (found, expected) {
        (Some(found), Some(expected)) => (found - expected).abs() <= 1,
        // Год неизвестен с одной из сторон — не отбрасываем, решает порядок выдачи.
        _ => true,
    }
}

/// Строка для сравнения названий: нижний регистр, только буквы и цифры, ё → е.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = true;
    for c in text.chars().flat_map(char::to_lowercase) {
        let c = if c == 'ё' { 'е' } else { c };
        if c.is_alphanumeric() {
            out.push(c);
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim_end().to_string()
}

// ------------------------------------------------------------------ TMDB

#[derive(Debug, Deserialize)]
pub struct TmdbSearchResult {
    pub id: u64,
    /// Фильм.
    pub release_date: Option<String>,
    /// Сериал.
    pub first_air_date: Option<String>,
}

/// Первый результат поиска TMDB с подходящим годом (выдача уже отсортирована по релевантности).
pub fn tmdb_match(results: &[TmdbSearchResult], year: Option<i32>) -> Option<u64> {
    results
        .iter()
        .find(|r| {
            let date = r.release_date.as_deref().or(r.first_air_date.as_deref());
            year_matches(date.and_then(year_of), year)
        })
        .map(|r| r.id)
}

#[derive(Debug, Deserialize)]
pub struct TmdbImage {
    pub file_path: String,
    pub iso_639_1: Option<String>,
    #[serde(default)]
    pub vote_average: f64,
    #[serde(default)]
    pub vote_count: u64,
}

/// Лучший постер: русский, затем английский, затем без текста; внутри — по голосам TMDB.
pub fn best_poster(posters: &[TmdbImage]) -> Option<&str> {
    let lang_rank = |lang: Option<&str>| match lang {
        Some("ru") => 0,
        Some("en") => 1,
        None => 2,
        Some(_) => 3,
    };
    posters
        .iter()
        .filter(|p| lang_rank(p.iso_639_1.as_deref()) < 3)
        .min_by(|a, b| {
            lang_rank(a.iso_639_1.as_deref())
                .cmp(&lang_rank(b.iso_639_1.as_deref()))
                .then(b.vote_average.total_cmp(&a.vote_average))
                .then(b.vote_count.cmp(&a.vote_count))
        })
        .map(|p| p.file_path.as_str())
}

#[derive(Debug, Deserialize)]
pub struct TmdbVideo {
    pub key: String,
    pub site: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub official: bool,
    pub iso_639_1: Option<String>,
    pub published_at: Option<String>,
}

/// Официальный трейлер на YouTube: русский, затем английский; из нескольких — самый ранний
/// (обычно это основной трейлер, а не поздние ролики к выходу на дисках).
pub fn official_youtube_trailer(videos: &[TmdbVideo]) -> Option<&str> {
    let lang_rank = |lang: Option<&str>| match lang {
        Some("ru") => 0,
        Some("en") => 1,
        _ => 2,
    };
    videos
        .iter()
        .filter(|v| v.site == "YouTube" && v.kind == "Trailer" && v.official)
        .min_by(|a, b| {
            lang_rank(a.iso_639_1.as_deref())
                .cmp(&lang_rank(b.iso_639_1.as_deref()))
                .then(a.published_at.cmp(&b.published_at))
        })
        .map(|v| v.key.as_str())
}

// ------------------------------------------------------------------ IGDB

#[derive(Debug, Deserialize)]
pub struct IgdbGame {
    /// Unix-время первого релиза.
    pub first_release_date: Option<i64>,
    pub cover: Option<IgdbCover>,
    #[serde(default)]
    pub videos: Vec<IgdbVideo>,
}

#[derive(Debug, Deserialize)]
pub struct IgdbCover {
    pub image_id: String,
}

#[derive(Debug, Deserialize)]
pub struct IgdbVideo {
    #[serde(default)]
    pub name: String,
    /// id видео YouTube.
    pub video_id: String,
}

pub fn igdb_year(game: &IgdbGame) -> Option<i32> {
    let ts = game.first_release_date?;
    chrono::DateTime::from_timestamp(ts, 0).map(|d| chrono::Datelike::year(&d))
}

/// Первая игра выдачи с подходящим годом.
pub fn igdb_match(games: &[IgdbGame], year: Option<i32>) -> Option<&IgdbGame> {
    games.iter().find(|g| year_matches(igdb_year(g), year))
}

/// Трейлер игры: сначала релизный и кинематографический, затем любой «trailer», иначе первый.
pub fn igdb_trailer(game: &IgdbGame) -> Option<&str> {
    let rank = |name: &str| {
        let name = name.to_lowercase();
        if name.contains("launch") || name.contains("cinematic") {
            0
        } else if name.contains("trailer") {
            1
        } else {
            2
        }
    };
    game.videos
        .iter()
        .min_by_key(|v| rank(&v.name))
        .map(|v| v.video_id.as_str())
}

// ------------------------------------------------------------------ Rutube

#[derive(Debug, Deserialize)]
pub struct RutubeVideo {
    pub id: String,
    pub title: String,
    /// Секунды.
    pub duration: Option<u64>,
    #[serde(default)]
    pub is_hidden: bool,
    #[serde(default)]
    pub is_adult: bool,
    #[serde(default)]
    pub is_deleted: bool,
}

/// Слова, которые могут стоять в названии трейлера рядом с названием произведения. Всё прочее —
/// признак другого ролика: игра по фильму («Game Trailer»), дополнение («Awakening»), ремастер
/// («Legendary Edition»), мод, обзор, целая серия. Строгость сознательная: лучше без запасного
/// трейлера, чем с чужим видео.
const TRAILER_WORDS: &[&str] = &[
    "трейлер",
    "тизер",
    "trailer",
    "teaser",
    "official",
    "официальный",
    "официальном",
    "русский",
    "русском",
    "русская",
    "на",
    "языке",
    "в",
    "с",
    "дублированный",
    "дубляж",
    "дубляжом",
    "озвучка",
    "субтитры",
    "субтитрами",
    "hd",
    "4k",
    "uhd",
    "1080p",
    "720p",
    "финальный",
    "основной",
    "новый",
    "final",
    "main",
    "rus",
];
const MOVIE_WORDS: &[&str] = &["фильм", "фильма", "movie", "film", "кино"];
const SERIES_WORDS: &[&str] = &[
    "сериал",
    "сериала",
    "series",
    "сезон",
    "season",
    "netflix",
    "hbo",
];
const GAME_WORDS: &[&str] = &[
    "игра",
    "игры",
    "game",
    "gameplay",
    "геймплейный",
    "релизный",
    "launch",
    "анонсирующий",
    "анонс",
    "cinematic",
    "pc",
    "ps3",
    "ps4",
    "ps5",
    "xbox",
    "one",
    "switch",
];

/// Тип произведения — от него зависит, какие слова допустимы в названии ролика.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    Movie,
    Series,
    Game,
}

/// Кандидат в трейлер на Rutube. Официальных каналов там нет, поэтому фильтры строгие: от
/// 30 секунд до 6 минут, в названии «трейлер»/«тизер», название сущности (не сиквела) и больше
/// ничего лишнего ([`TRAILER_WORDS`]), год — если указан — совпадает. Первый подходящий по выдаче.
pub fn rutube_trailer<'a>(
    videos: &'a [RutubeVideo],
    titles: &[&str],
    year: Option<i32>,
    work: Work,
    require_year: bool,
) -> Option<&'a str> {
    let titles: Vec<String> = titles
        .iter()
        .map(|t| normalize(t))
        .filter(|t| !t.is_empty())
        .collect();
    let kind_words = match work {
        Work::Movie => MOVIE_WORDS,
        Work::Series => SERIES_WORDS,
        Work::Game => GAME_WORDS,
    };
    let allowed = |word: &str| {
        TRAILER_WORDS.contains(&word)
            || kind_words.contains(&word)
            || (word.len() == 1 && word.chars().all(|c| c.is_ascii_digit()))
            || (word.len() == 4
                && word
                    .parse::<i32>()
                    .is_ok_and(|y| (1900..=2099).contains(&y)))
            || titles.iter().any(|t| t.split(' ').any(|w| w == word))
    };
    videos
        .iter()
        .find(|v| {
            let title = normalize(&v.title);
            let duration_ok = v.duration.is_some_and(|d| (30..=360).contains(&d));
            let is_trailer = ["трейлер", "тизер", "trailer", "teaser"]
                .iter()
                .any(|w| title.split(' ').any(|t| t == *w));
            duration_ok
                && !v.is_hidden
                && !v.is_adult
                && !v.is_deleted
                && is_trailer
                && titles.iter().any(|t| names_entity(&title, t))
                && title.split(' ').all(allowed)
                && years_ok(&years_in(&title), year, require_year)
        })
        .map(|v| v.id.as_str())
}

/// Название ролика содержит название сущности как отдельные слова и сразу за ним не идёт номер
/// части («Оно 2», «Дюна 2», «часть вторая» — это уже другое произведение).
fn names_entity(video: &str, entity: &str) -> bool {
    let haystack = format!(" {video} ");
    let needle = format!(" {entity} ");
    let Some(pos) = haystack.find(&needle) else {
        return false;
    };
    let after = &haystack[pos + needle.len()..];
    let next = after.split_whitespace().next().unwrap_or("");
    let sequel_marks = [
        "2",
        "3",
        "4",
        "5",
        "6",
        "7",
        "8",
        "9",
        "ii",
        "iii",
        "iv",
        "часть",
        "part",
    ];
    !sequel_marks.contains(&next)
}

/// Годы в названии ролика не противоречат году произведения. `require_year` — в каталоге есть
/// одноимённое произведение того же типа (ремейк): тогда без года в названии не различить.
fn years_ok(found: &[i32], year: Option<i32>, require_year: bool) -> bool {
    if require_year && found.is_empty() {
        return false;
    }
    found.iter().all(|y| year.is_none_or(|e| *y == e))
}

/// Четырёхзначные годы (1900–2099) в названии ролика.
fn years_in(title: &str) -> Vec<i32> {
    title
        .split_whitespace()
        .filter(|w| w.len() == 4)
        .filter_map(|w| w.parse::<i32>().ok())
        .filter(|y| (1900..=2099).contains(y))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rutube(list: serde_json::Value) -> Vec<RutubeVideo> {
        serde_json::from_value(list).unwrap()
    }

    #[test]
    fn rutube_skips_full_movies_sequels_and_other_years() {
        // Настоящая выдача Rutube по запросу «Оно 2017 трейлер» (сокращено).
        let videos = rutube(json!([
            { "id": "full", "title": "Оно 1 (фильм, 2017)", "duration": 8082 },
            { "id": "sequel", "title": "Оно 2 - Русский трейлер (4К)", "duration": 153 },
            { "id": "reboot", "title": "Оно. Первое пришествие — Русский трейлер (2026)", "duration": 95 },
            { "id": "review", "title": "Оно (2017) — обзор и трейлер", "duration": 300 },
            { "id": "good", "title": "Оно (2017) — Русский трейлер", "duration": 161 }
        ]));
        assert_eq!(
            rutube_trailer(&videos, &["Оно", "It"], Some(2017), Work::Movie, false),
            Some("good")
        );
    }

    #[test]
    fn rutube_needs_trailer_word_and_entity_title() {
        let videos = rutube(json!([
            { "id": "a", "title": "Дюна — лучшие моменты", "duration": 120 },
            { "id": "b", "title": "Интерстеллар — трейлер", "duration": 120 },
            { "id": "c", "title": "Дюна: Часть вторая — трейлер", "duration": 150 },
            { "id": "d", "title": "Dune (2021) Official Trailer", "duration": 180, "is_hidden": true }
        ]));
        assert_eq!(
            rutube_trailer(&videos, &["Дюна", "Dune"], Some(2021), Work::Movie, false),
            None
        );
    }

    #[test]
    fn rutube_rejects_games_mods_and_remasters_seen_in_real_runs() {
        // Ошибки первой версии фильтра на настоящих данных: не трейлеры этих произведений.
        let pick = |title: &str, titles: &[&str], year, work| {
            let videos = rutube(json!([{ "id": "x", "title": title, "duration": 150 }]));
            rutube_trailer(&videos, titles, year, work, false).is_some()
        };
        assert!(!pick(
            "Dune Awakening | трейлер к выходу игры на консолях",
            &["Дюна", "Dune"],
            Some(1984),
            Work::Movie
        ));
        assert!(!pick(
            "The Expanse  Osiris Reborn | ТРЕЙЛЕР",
            &["Пространство", "The Expanse"],
            Some(2015),
            Work::Series
        ));
        assert!(!pick(
            "Сталкер фильм - Цена доверия - Трейлер",
            &["Сталкер"],
            Some(1979),
            Work::Movie
        ));
        assert!(!pick(
            "The Lord of the Rings: The Return of the King - Game Trailer HD",
            &[
                "Властелин колец: Возвращение короля",
                "The Lord of the Rings: The Return of the King"
            ],
            Some(2003),
            Work::Movie
        ));
        assert!(!pick(
            "Mass Effect™ Legendary Edition — официальный трейлер (4K)",
            &["Mass Effect"],
            Some(2007),
            Work::Game
        ));

        // А эти — настоящие трейлеры, их фильтр пропускает.
        assert!(pick(
            "Бегущий по лезвию — Русский трейлер (фильм 1982) / Blade Runner",
            &["Бегущий по лезвию", "Blade Runner"],
            Some(1982),
            Work::Movie
        ));
        assert!(pick(
            "Дом Дракона / House of the Dragon (2022) русский тизер-трейлер #2 (субтитры)",
            &["Дом Дракона", "House of the Dragon"],
            Some(2022),
            Work::Series
        ));
        assert!(pick(
            "The Last of Us Анонсирующий трейлер | PS3 (2013)",
            &["Одни из нас", "The Last of Us"],
            Some(2013),
            Work::Game
        ));
        assert!(pick(
            "Солярис (Solaris) - трейлер",
            &["Солярис", "Solaris"],
            Some(1972),
            Work::Movie
        ));
    }

    #[test]
    fn rutube_requires_year_when_there_is_a_remake() {
        let videos = rutube(json!([
            { "id": "no-year", "title": "Дюна (Dune) - трейлер", "duration": 206 },
            { "id": "1984", "title": "Дюна (1984) — трейлер", "duration": 140 }
        ]));
        let pick =
            |require| rutube_trailer(&videos, &["Дюна", "Dune"], Some(1984), Work::Movie, require);
        assert_eq!(pick(true), Some("1984"));
        assert_eq!(pick(false), Some("no-year"));
    }

    #[test]
    fn normalize_handles_case_punctuation_and_yo() {
        assert_eq!(
            normalize("Ёжик в тумане: ТРЕЙЛЕР!"),
            "ежик в тумане трейлер"
        );
    }

    #[test]
    fn poster_prefers_russian_then_votes() {
        let posters: Vec<TmdbImage> = serde_json::from_value(json!([
            { "file_path": "/en.jpg", "iso_639_1": "en", "vote_average": 9.0, "vote_count": 50 },
            { "file_path": "/ru-low.jpg", "iso_639_1": "ru", "vote_average": 5.0, "vote_count": 3 },
            { "file_path": "/ru-top.jpg", "iso_639_1": "ru", "vote_average": 5.5, "vote_count": 2 },
            { "file_path": "/fi.jpg", "iso_639_1": "fi", "vote_average": 10.0, "vote_count": 99 }
        ]))
        .unwrap();
        assert_eq!(best_poster(&posters), Some("/ru-top.jpg"));
        assert_eq!(best_poster(&posters[..1]), Some("/en.jpg"));
        assert_eq!(best_poster(&posters[3..]), None);
    }

    #[test]
    fn trailer_must_be_official_youtube_trailer() {
        let videos: Vec<TmdbVideo> = serde_json::from_value(json!([
            { "key": "fan", "site": "YouTube", "type": "Trailer", "official": false, "iso_639_1": "ru" },
            { "key": "clip", "site": "YouTube", "type": "Clip", "official": true, "iso_639_1": "en" },
            { "key": "late", "site": "YouTube", "type": "Trailer", "official": true, "iso_639_1": "en", "published_at": "2021-09-01" },
            { "key": "main", "site": "YouTube", "type": "Trailer", "official": true, "iso_639_1": "en", "published_at": "2021-07-22" },
            { "key": "vimeo", "site": "Vimeo", "type": "Trailer", "official": true, "iso_639_1": "ru" }
        ]))
        .unwrap();
        assert_eq!(official_youtube_trailer(&videos), Some("main"));
    }

    #[test]
    fn tmdb_match_checks_year() {
        let results: Vec<TmdbSearchResult> = serde_json::from_value(json!([
            { "id": 841, "release_date": "1984-12-14" },
            { "id": 438631, "release_date": "2021-09-15" }
        ]))
        .unwrap();
        assert_eq!(tmdb_match(&results, Some(2021)), Some(438631));
        assert_eq!(tmdb_match(&results, Some(1999)), None);
        assert_eq!(tmdb_match(&results, None), Some(841));
    }

    #[test]
    fn igdb_picks_launch_trailer_and_year() {
        let games: Vec<IgdbGame> = serde_json::from_value(json!([
            { "first_release_date": 1_431_993_600, "cover": { "image_id": "co1wyy" },
              "videos": [ { "name": "Gameplay", "video_id": "g" }, { "name": "Launch Cinematic", "video_id": "l" } ] }
        ]))
        .unwrap();
        let game = igdb_match(&games, Some(2015)).unwrap();
        assert_eq!(igdb_trailer(game), Some("l"));
        assert!(igdb_match(&games, Some(2020)).is_none());
    }
}
