//! `nexus books load [--seconds N] [--threads 1,4,16] [--limit N]` — нагрузка на индекс книг
//! (Tantivy) прямо в процессе, без роутера и сети: отчёт в консоль.
//!
//! Что меряется:
//! 1. Индекс: книг, размер на диске, память процесса до/после открытия и прогрева.
//! 2. Холодный первый запрос каждого сценария и задержки по сценариям в один поток
//!    (без кэша ответов — цена самого поиска): p50 / p95 / p99 / max.
//! 3. Нагрузка: смесь всех сценариев в N потоков на `--seconds` секунд, без кэша и с кэшем:
//!    QPS, p50 / p95 / p99 / max, ошибки, CPU-время, рост памяти.
//!
//! Память берётся из `/proc/self/status` (Linux): `RssAnon` — куча (кэши, буферы поиска),
//! `RssFile` — страницы индекса через mmap (page cache ОС), `VmHWM` — пик RSS.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use catalog::books::index::{BookIndex, BookQuery, BookSort};

pub struct Options {
    pub seconds: u64,
    pub threads: Vec<usize>,
    pub limit: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            seconds: 10,
            threads: vec![1, 4, 16],
            limit: 24,
        }
    }
}

pub const USAGE: &str = "nexus books load [--seconds N] [--threads 1,4,16] [--limit N]";

pub fn parse(args: &[&str]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag}: no value"))?;
        match *flag {
            "--seconds" => options.seconds = value.parse().map_err(|_| "--seconds: number")?,
            "--limit" => options.limit = value.parse().map_err(|_| "--limit: number")?,
            "--threads" => {
                options.threads = value
                    .split(',')
                    .map(|n| n.trim().parse::<usize>())
                    .collect::<Result<_, _>>()
                    .map_err(|_| "--threads: list like 1,4,16")?;
            }
            _ => return Err(format!("unknown flag {flag}\nusage: {USAGE}")),
        }
    }
    if options.seconds == 0 || options.limit == 0 || options.threads.contains(&0) {
        return Err("--seconds, --limit and --threads must be > 0".into());
    }
    Ok(options)
}

/// Сценарий: название и запросы к индексу.
struct Scenario {
    title: &'static str,
    queries: Vec<(BookQuery, usize)>,
}

fn text(q: &str) -> (BookQuery, usize) {
    (
        BookQuery {
            q: q.into(),
            ..BookQuery::default()
        },
        0,
    )
}

fn listing(sort: BookSort, offset: usize) -> (BookQuery, usize) {
    (
        BookQuery {
            sort,
            ..BookQuery::default()
        },
        offset,
    )
}

fn scenarios() -> Vec<Scenario> {
    let texts = |qs: &[&str]| qs.iter().map(|q| text(q)).collect();
    vec![
        Scenario {
            title: "Название целиком",
            queries: texts(&[
                "собачье сердце",
                "мастер и маргарита",
                "война и мир",
                "преступление и наказание",
                "гарри поттер",
                "анна каренина",
                "три мушкетёра",
                "основание",
            ]),
        },
        Scenario {
            title: "Недописанное слово",
            queries: texts(&[
                "собач",
                "прест",
                "мастер и марг",
                "война и ми",
                "три мушк",
                "каренин",
            ]),
        },
        Scenario {
            title: "Опечатка",
            queries: texts(&[
                "приступление",
                "собачие сердце",
                "мастер и маргорита",
                "гари потер",
            ]),
        },
        Scenario {
            title: "Автор",
            queries: texts(&["булгаков", "азимов", "достоевский", "стругацкие"]),
        },
        Scenario {
            title: "Одна буква / короткое",
            queries: texts(&["а", "ма", "the"]),
        },
        Scenario {
            title: "Раздел: рекомендуемые / новинки / популярные",
            queries: vec![
                listing(BookSort::Recommended, 0),
                listing(BookSort::New, 0),
                listing(BookSort::Popular, 0),
                listing(BookSort::Title, 0),
            ],
        },
        Scenario {
            title: "Фильтры: годы, жанр, обложка",
            queries: vec![
                (
                    BookQuery {
                        sort: BookSort::Recommended,
                        year_from: Some(2000),
                        year_to: Some(2010),
                        ..BookQuery::default()
                    },
                    0,
                ),
                (
                    BookQuery {
                        sort: BookSort::Recommended,
                        tag: Some("fantasy".into()),
                        ..BookQuery::default()
                    },
                    0,
                ),
                (
                    BookQuery {
                        q: "война".into(),
                        has_cover: Some(true),
                        language: Some("ru".into()),
                        ..BookQuery::default()
                    },
                    0,
                ),
            ],
        },
        Scenario {
            title: "Глубокая страница (offset 2400)",
            queries: vec![
                listing(BookSort::Recommended, 2400),
                listing(BookSort::New, 2400),
            ],
        },
    ]
}

// ---------- память и CPU процесса (Linux) ----------

#[derive(Clone, Copy, Default)]
struct Memory {
    rss: u64,
    anon: u64,
    file: u64,
    peak: u64,
}

/// Значения `/proc/self/status` в КиБ; не Linux — нули.
fn memory() -> Memory {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    parse_status(&status)
}

fn parse_status(status: &str) -> Memory {
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap_or(0)
    };
    Memory {
        rss: field("VmRSS:"),
        anon: field("RssAnon:"),
        file: field("RssFile:"),
        peak: field("VmHWM:"),
    }
}

/// CPU-время процесса (user + system) из `/proc/self/stat`; не Linux — `None`.
fn cpu_time() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Поля после имени в скобках: utime — 14-е, stime — 15-е (с 1), т.е. 12-е и 13-е после ')'.
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    // USER_HZ на Linux практически всегда 100.
    Some(Duration::from_millis(ticks * 10))
}

fn mib(kib: u64) -> String {
    format!("{:.1} MiB", kib as f64 / 1024.0)
}

fn mib_delta(after: u64, before: u64) -> String {
    let delta = after as f64 - before as f64;
    format!("{:+.1} MiB", delta / 1024.0)
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

// ---------- статистика ----------

#[derive(Debug, PartialEq)]
struct Stats {
    count: usize,
    p50: Duration,
    p95: Duration,
    p99: Duration,
    max: Duration,
}

/// Перцентили по ближайшему рангу; `times` сортируется на месте.
fn stats(times: &mut [Duration]) -> Option<Stats> {
    if times.is_empty() {
        return None;
    }
    times.sort_unstable();
    let at = |share: f64| {
        times[((times.len() as f64 * share).ceil() as usize).clamp(1, times.len()) - 1]
    };
    Some(Stats {
        count: times.len(),
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
        max: times[times.len() - 1],
    })
}

fn ms(d: Duration) -> String {
    format!("{:.2}", d.as_secs_f64() * 1000.0)
}

fn row(title: &str, s: &Stats) -> String {
    format!(
        "{title:<46} {:>8} {:>8} {:>8} {:>9} {:>8}",
        ms(s.p50),
        ms(s.p95),
        ms(s.p99),
        ms(s.max),
        s.count
    )
}

fn header(first: &str) -> String {
    format!(
        "{first:<46} {:>8} {:>8} {:>8} {:>9} {:>8}",
        "p50 мс", "p95", "p99", "max", "запросов"
    )
}

// ---------- прогон ----------

struct LoadResult {
    stats: Option<Stats>,
    qps: f64,
    errors: u64,
    cpu: Option<Duration>,
    wall: Duration,
}

/// `threads` потоков крутят смесь запросов `seconds` секунд; каждый поток начинает со своего
/// места в списке, чтобы потоки не шли строем.
fn load(
    index: &BookIndex,
    mix: &[(BookQuery, usize)],
    threads: usize,
    seconds: u64,
    limit: usize,
) -> LoadResult {
    let stop = AtomicBool::new(false);
    let errors = AtomicU64::new(0);
    let cpu_before = cpu_time();
    let started = Instant::now();
    let mut times: Vec<Duration> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|worker| {
                let (stop, errors) = (&stop, &errors);
                scope.spawn(move || {
                    let mut times = Vec::new();
                    let mut i = worker * 7;
                    while !stop.load(Ordering::Relaxed) {
                        let (query, offset) = &mix[i % mix.len()];
                        i += 1;
                        let t = Instant::now();
                        match index.search(query, limit, *offset) {
                            Ok(_) => times.push(t.elapsed()),
                            Err(_) => {
                                errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    times
                })
            })
            .collect();
        std::thread::sleep(Duration::from_secs(seconds));
        stop.store(true, Ordering::Relaxed);
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_default())
            .collect()
    });
    let wall = started.elapsed();
    let cpu = cpu_before
        .zip(cpu_time())
        .map(|(before, after)| after.saturating_sub(before));
    let qps = times.len() as f64 / wall.as_secs_f64();
    LoadResult {
        stats: stats(&mut times),
        qps,
        errors: errors.into_inner(),
        cpu,
        wall,
    }
}

pub fn run(index_dir: &str, options: &Options) -> anyhow::Result<()> {
    let build = if cfg!(debug_assertions) {
        "debug (цифры завышены, меряйте --release)"
    } else {
        "release"
    };
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());
    let mem_start = memory();

    let opened = Instant::now();
    let index = BookIndex::open(Path::new(index_dir)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let open_time = opened.elapsed();
    let mem_open = memory();
    let warmed = Instant::now();
    index.warm().map_err(|e| anyhow::anyhow!("{e}"))?;
    let warm_time = warmed.elapsed();
    let mem_warm = memory();

    println!("══ Tantivy: нагрузка на индекс книг ══");
    println!(
        "сборка: {build}; CPU: {cpus}; лимит страницы: {}",
        options.limit
    );
    println!(
        "индекс: {index_dir}; книг: {}; на диске: {}",
        index.len(),
        mib(dir_size(Path::new(index_dir)) / 1024)
    );
    println!("открытие: {open_time:.1?}; прогрев разделов: {warm_time:.1?}");
    println!(
        "память: старт RSS {} → после открытия {} ({}) → после прогрева {} ({})",
        mib(mem_start.rss),
        mib(mem_open.rss),
        mib_delta(mem_open.rss, mem_start.rss),
        mib(mem_warm.rss),
        mib_delta(mem_warm.rss, mem_open.rss),
    );
    println!(
        "         куча (RssAnon) {}, индекс через mmap (RssFile) {}",
        mib(mem_warm.anon),
        mib(mem_warm.file)
    );

    // 1. Холодные запросы и задержки по сценариям, один поток, без кэша ответов.
    index.set_cached(false);
    let scenarios = scenarios();
    println!("\n── 1. Один поток, без кэша ответов (5 кругов на сценарий) ──");
    println!("{}  холодный мс  найдено", header("сценарий"));
    let mut all = Vec::new();
    for scenario in &scenarios {
        let mut cold = Duration::ZERO;
        let mut found = 0usize;
        for (query, offset) in &scenario.queries {
            let t = Instant::now();
            let page = index
                .search(query, options.limit, *offset)
                .map_err(|e| anyhow::anyhow!("{}: {e}", scenario.title))?;
            cold = cold.max(t.elapsed());
            found += page.total;
        }
        let mut times = Vec::new();
        for _ in 0..5 {
            for (query, offset) in &scenario.queries {
                let t = Instant::now();
                index
                    .search(query, options.limit, *offset)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                times.push(t.elapsed());
            }
        }
        all.extend_from_slice(&times);
        if let Some(s) = stats(&mut times) {
            println!("{}  {:>10}  {found}", row(scenario.title, &s), ms(cold));
        }
    }
    if let Some(s) = stats(&mut all) {
        println!("{}", row("ВСЕГО", &s));
    }

    // 2. Нагрузка смесью сценариев.
    let mix: Vec<(BookQuery, usize)> = scenarios.into_iter().flat_map(|s| s.queries).collect();
    println!(
        "\n── 2. Нагрузка: смесь из {} запросов, {} с на прогон ──",
        mix.len(),
        options.seconds
    );
    println!(
        "{}  {:>9} {:>6} {:>8} {:>11} {:>10}",
        header("кэш / потоки"),
        "QPS",
        "ошибок",
        "CPU, ядер",
        "RSS",
        "пик RSS"
    );
    for cached in [false, true] {
        index.set_cached(cached);
        for &threads in &options.threads {
            let before = memory();
            let result = load(&index, &mix, threads, options.seconds, options.limit);
            let after = memory();
            let cores = result.cpu.map_or("n/a".into(), |cpu| {
                format!("{:.2}", cpu.as_secs_f64() / result.wall.as_secs_f64())
            });
            let title = format!(
                "{} × {threads}",
                if cached {
                    "с кэшем"
                } else {
                    "без кэша"
                }
            );
            let Some(s) = &result.stats else {
                println!(
                    "{title:<46} нет успешных запросов, ошибок: {}",
                    result.errors
                );
                continue;
            };
            println!(
                "{}  {:>9.0} {:>6} {:>8} {:>11} {:>10}",
                row(&title, s),
                result.qps,
                result.errors,
                cores,
                mib_delta(after.rss, before.rss),
                mib(after.peak)
            );
        }
    }

    let end = memory();
    println!(
        "\nпамять в конце: RSS {} (куча {}, mmap индекса {}), пик за прогон {}",
        mib(end.rss),
        mib(end.anon),
        mib(end.file),
        mib(end.peak)
    );
    println!(
        "подсказка: RssFile — страницы индекса в page cache ОС, их можно вытеснить; рост RssAnon\n\
         между прогонами при одинаковой нагрузке — признак утечки или неограниченного кэша."
    );
    index.set_cached(true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn stats_nearest_rank() {
        let mut times: Vec<Duration> = (1..=100).rev().map(d).collect();
        let s = stats(&mut times).unwrap();
        assert_eq!(
            (s.p50, s.p95, s.p99, s.max, s.count),
            (d(50), d(95), d(99), d(100), 100)
        );
    }

    #[test]
    fn stats_single_and_empty() {
        assert!(stats(&mut []).is_none());
        let s = stats(&mut [d(7)]).unwrap();
        assert_eq!((s.p50, s.p99, s.max), (d(7), d(7), d(7)));
    }

    #[test]
    fn parses_proc_status() {
        let m = parse_status("Name:\tnexus\nVmHWM:\t  2048 kB\nVmRSS:\t  1024 kB\nRssAnon:\t 512 kB\nRssFile:\t 256 kB\n");
        assert_eq!((m.rss, m.anon, m.file, m.peak), (1024, 512, 256, 2048));
        assert_eq!(parse_status("").rss, 0);
    }

    #[test]
    fn parses_options() {
        let o = parse(&["--seconds", "3", "--threads", "2, 8", "--limit", "10"]).unwrap();
        assert_eq!((o.seconds, o.threads, o.limit), (3, vec![2, 8], 10));
        let o = parse(&[]).unwrap();
        assert_eq!((o.seconds, o.threads, o.limit), (10, vec![1, 4, 16], 24));
        assert!(parse(&["--threads", "0"]).is_err());
        assert!(parse(&["--seconds"]).is_err());
        assert!(parse(&["--bogus", "1"]).is_err());
    }

    #[test]
    fn scenarios_are_not_empty() {
        assert!(scenarios().iter().all(|s| !s.queries.is_empty()));
    }
}
