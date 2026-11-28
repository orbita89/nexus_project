//! Скрипты из админки (`/admin/scripts`): реестр служебных скриптов с описанием и флагами,
//! запуск фоновой задачей, лог по строкам, отмена, отчёт.
//!
//! - Команда собирается **только из реестра** ([`SCRIPTS`]): файл и подкоманда зашиты, флаги
//!   проверяются по типу (число в диапазоне, да/нет, выбор, строка по шаблону) — произвольную
//!   команду из админки не запустить, shell не участвует.
//! - Python — `SCRIPTS_PYTHON` (dev-образ: `/opt/pyenv/bin/python` с зависимостями
//!   `scripts/catalog-import/requirements.txt`). Не задан — запуск выключен (503).
//! - Скрипту передаются `DATABASE_URL` и `AI_SECRET_KEY` приложения: провайдер ИИ он берёт из
//!   `catalog_ai_providers` (`crate::ai`). stdin пуст: вопрос «Записать?» = «нет» — запись
//!   только явным флагом `--yes` / `--apply`.
//! - Одновременно — одна задача (скрипты делят кеш `cache/books_llm`). Задачи живут в памяти
//!   процесса (последние [`KEEP_JOBS`], лог — последние [`KEEP_LINES`] строк): после рестарта
//!   их нет, на другой реплике их не видно.
//! - [`After::BooksReindex`]: только с флагом `--reindex` — после успешной записи перестройка
//!   книжного индекса и ручного порядка в процессе сервера (как `nexus books index` +
//!   `nexus books ranks`); `--reindex` скрипту не передаётся, один он — только перестройка.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};

use chrono::{DateTime, Utc};
use serde::Serialize;
use shared::{AppError, AppState};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Notify;
use utoipa::ToSchema;

/// Сколько последних задач помнить.
pub const KEEP_JOBS: usize = 20;
/// Сколько последних строк лога держать у задачи.
pub const KEEP_LINES: usize = 5000;
/// Отчёт отдаётся не длиннее (байт).
const REPORT_MAX: usize = 512 * 1024;
/// Список ручного порядка книг (`nexus books ranks`), от корня проекта.
const RANKS_FILE: &str = "knigi_10_000_progress.txt";

/// Тип значения флага.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FlagKind {
    /// Флаг без значения: `--yes`.
    Bool,
    /// Целое `min..=max`.
    Int { min: i64, max: i64 },
    /// Одно из значений.
    Choice {
        #[schema(value_type = Vec<String>)]
        options: &'static [&'static str],
    },
    /// Провайдер ИИ из `catalog_ai_providers` (slug) — в форме выбор из списка.
    AiProvider,
    /// Строка по шаблону (regex, как в `pattern` HTML-поля).
    Text {
        #[schema(value_type = String)]
        pattern: &'static str,
    },
}

/// Флаг скрипта.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
pub struct Flag {
    /// Имя без `--`: `limit`.
    #[schema(value_type = String, example = "limit")]
    pub name: &'static str,
    #[serde(flatten)]
    pub kind: FlagKind,
    /// Значение скрипта по умолчанию (подсказка в форме); `null` — не задано.
    #[schema(value_type = Option<String>)]
    pub default: Option<&'static str>,
    /// Что делает флаг.
    #[schema(value_type = String)]
    pub description: &'static str,
}

/// Что сделать после успешного (код 0) выполнения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum After {
    /// Перестроить книжный индекс Tantivy и ручной порядок книг.
    BooksReindex,
}

/// Чем выполняется скрипт.
#[derive(Debug, Clone, Copy)]
enum Program {
    /// `SCRIPTS_PYTHON <file> <args...> <флаги>`.
    Python {
        file: &'static str,
        args: &'static [&'static str],
    },
}

/// Скрипт реестра.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
pub struct Script {
    #[schema(value_type = String, example = "books-llm")]
    pub id: &'static str,
    /// Группа в меню.
    #[schema(value_type = String, example = "Книги")]
    pub group: &'static str,
    #[schema(value_type = String)]
    pub title: &'static str,
    /// Что делает, по шагам.
    #[schema(value_type = String)]
    pub description: &'static str,
    /// Команда для терминала — то же самое без админки.
    #[schema(value_type = String)]
    pub command: &'static str,
    #[schema(value_type = Vec<Flag>)]
    pub flags: &'static [Flag],
    pub after: Option<After>,
    /// Есть отчёт (`GET /admin/scripts/{id}/report`).
    pub has_report: bool,
    #[serde(skip)]
    program: Program,
    #[serde(skip)]
    report: Option<&'static str>,
    /// Есть прогресс (`GET /admin/scripts/{id}/progress`).
    pub has_progress: bool,
    #[serde(skip)]
    progress: Option<&'static str>,
}

const BOOKS_LLM: &str = "scripts/catalog-import/books_llm.py";
const BOOKS_REPORT: &str = "scripts/catalog-import/cache/books_llm/apply_report.txt";
const BOOKS_PROGRESS: &str = "scripts/catalog-import/cache/books_llm/progress.json";

/// Реестр: всё, что можно запустить из админки. Цель одна — порядок в книгах, поэтому и
/// скрипт один: все шаги по порядку. Модель, ключи, лимиты и effort — в настройках провайдера
/// ИИ (`/admin/ai/providers`), здесь только выбор провайдера.
pub static SCRIPTS: &[Script] = &[Script {
    id: "books-llm",
    group: "Книги",
    title: "Чистка книг",
    description: "Приводит книги в порядок по шагам:\n\
        1. Берёт следующую порцию неразмеченных книг (или продолжает прерванную).\n\
        2. ИИ размечает каждую книгу: название без мусора, полные имена авторов, жанры, тип \
        (сборник, статья, документ, не книга).\n\
        3. Отчёт: что поменяется — итоги в логе, примеры в отчёте. На этом обработка \
        останавливается, в БД ничего не пишется. Обработанные книги повторно не обрабатываются.\n\
        4. Кнопка «Записать в БД» (в терминале YES=1) — запись: названия и жанры, слияние 100% дублей авторов, издания одной \
        книги — под основной, мусор скрыт; удаление слитых дублей людей.\n\
        5. С галочкой reindex — перестройка книжного поиска и ручного порядка \
        (knigi_10_000_progress.txt); без неё поиск не трогается.\n\
        Прерванный запуск продолжается с места остановки.",
    command: "make books-llm [LIMIT=5000] [ORDER=newest] [AI=имя] [YES=1] [REINDEX=1]",
    flags: &[
        Flag {
            name: "limit",
            kind: FlagKind::Int {
                min: 1,
                max: 500_000,
            },
            default: Some("5000"),
            description: "Сколько книг взять в новую порцию. Для пробы — 100.",
        },
        Flag {
            name: "order",
            kind: FlagKind::Choice {
                options: &["top", "newest", "collections"],
            },
            default: Some("top"),
            description: "Какие книги брать: top — сначала ручной список (knigi_10_000), потом \
                самые крупные; newest — последние добавленные в БД (например, limit 500 — 500 \
                новых); collections — перепроверить книги, помеченные сборником по названию \
                (ИИ решает заново: тома одного романа — не сборник). Уже нормализованные \
                пропускаются (кроме collections), повторов нет. Если есть незаконченная порция — \
                сначала доделывается она.",
        },
        Flag {
            name: "yes",
            kind: FlagKind::Bool,
            default: None,
            description:
                "Записать в БД после сухого прогона. Без галочки — только разметка и отчёт.",
        },
        Flag {
            name: "reindex",
            kind: FlagKind::Bool,
            default: None,
            description: "После записи перестроить книжный поиск и ручной порядок (~10 мин на \
                415 тыс. книг). Без галочки поиск не трогается — перестройте, когда удобно. Один \
                --reindex без других флагов — только перестройка, скрипт не запускается.",
        },
        Flag {
            name: "ai",
            kind: FlagKind::AiProvider,
            default: None,
            description: "Провайдер ИИ (модель, ключи, effort — в разделе «Провайдеры ИИ»). \
                Не выбран — провайдер по умолчанию (★).",
        },
    ],
    after: Some(After::BooksReindex),
    has_report: true,
    program: Program::Python {
        file: BOOKS_LLM,
        args: &["run"],
    },
    report: Some(BOOKS_REPORT),
    has_progress: true,
    progress: Some(BOOKS_PROGRESS),
}];

/// Имя провайдера ИИ: тот же формат, что `catalog_ai_providers.slug`.
fn valid_slug(text: &str) -> bool {
    crate::ai::valid_slug(text)
}

pub fn find(id: &str) -> Option<&'static Script> {
    SCRIPTS.iter().find(|s| s.id == id)
}

/// Аргументы командной строки из значений формы: только флаги реестра, значения проверены.
/// Пустая строка и `false` — флаг не передаётся.
pub fn build_args(
    script: &Script,
    values: &BTreeMap<String, serde_json::Value>,
) -> Result<Vec<String>, AppError> {
    let bad = |name: &str, why: String| AppError::BadRequest(why).on_field(name);
    for name in values.keys() {
        if !script.flags.iter().any(|f| f.name == name) {
            return Err(bad(name, format!("unknown flag {name}")));
        }
    }
    let mut out = Vec::new();
    for flag in script.flags {
        let Some(value) = values.get(flag.name) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let text = match value {
            serde_json::Value::String(s) => s.trim().to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => return Err(bad(flag.name, format!("{}: bad value", flag.name))),
        };
        match flag.kind {
            FlagKind::Bool => match value {
                serde_json::Value::Bool(true) => out.push(format!("--{}", flag.name)),
                serde_json::Value::Bool(false) => {}
                _ => return Err(bad(flag.name, format!("{}: true or false", flag.name))),
            },
            _ if text.is_empty() => {}
            FlagKind::Int { min, max } => {
                let n: i64 = text
                    .parse()
                    .ok()
                    .filter(|n| (min..=max).contains(n))
                    .ok_or_else(|| bad(flag.name, format!("{}: {min}..={max}", flag.name)))?;
                out.extend([format!("--{}", flag.name), n.to_string()]);
            }
            FlagKind::Choice { options } => {
                if !options.contains(&text.as_str()) {
                    return Err(bad(flag.name, format!("{}: one of {options:?}", flag.name)));
                }
                out.extend([format!("--{}", flag.name), text]);
            }
            FlagKind::AiProvider => {
                if !valid_slug(&text) {
                    return Err(bad(flag.name, format!("{}: provider slug", flag.name)));
                }
                out.extend([format!("--{}", flag.name), text]);
            }
            FlagKind::Text { pattern } => {
                let re = regex::Regex::new(pattern).expect("registry pattern");
                if !re.is_match(&text) {
                    return Err(bad(
                        flag.name,
                        format!("{}: does not match {pattern}", flag.name),
                    ));
                }
                out.extend([format!("--{}", flag.name), text]);
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    /// Код 0 (и шаг «после» прошёл).
    Succeeded,
    /// Код ≠ 0 или ошибка запуска / шага «после».
    Failed,
    /// Код 3 у `books-llm`: сухой прогон без записи — не ошибка.
    Stopped,
    Cancelled,
}

/// Задача без лога.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct JobInfo {
    pub id: u64,
    #[schema(example = "books-llm")]
    pub script: String,
    /// Аргументы, как в терминале: `--limit 5000 --yes`.
    pub args: Vec<String>,
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Всего строк лога с начала (включая отброшенные из памяти).
    pub lines_total: u64,
}

/// Задача и строки лога с номера `from`.
#[derive(Debug, Serialize, ToSchema)]
pub struct JobLog {
    #[serde(flatten)]
    pub job: JobInfo,
    /// Номер первой строки в `lines` (строки до него уже отброшены или не запрошены).
    pub from: u64,
    pub lines: Vec<String>,
}

struct Job {
    info: JobInfo,
    /// Последние строки; номер первой — `info.lines_total - lines.len()`.
    lines: VecDeque<String>,
    cancel: Arc<Notify>,
}

#[derive(Default)]
struct Jobs {
    next: u64,
    list: VecDeque<Job>,
}

static JOBS: LazyLock<Mutex<Jobs>> = LazyLock::new(Mutex::default);

fn jobs() -> std::sync::MutexGuard<'static, Jobs> {
    JOBS.lock().unwrap_or_else(|e| e.into_inner())
}

fn push_line(id: u64, line: String) {
    let mut jobs = jobs();
    if let Some(job) = jobs.list.iter_mut().find(|j| j.info.id == id) {
        job.info.lines_total += 1;
        job.lines.push_back(line);
        while job.lines.len() > KEEP_LINES {
            job.lines.pop_front();
        }
    }
}

fn finish(id: u64, status: JobStatus, code: Option<i32>) {
    let mut jobs = jobs();
    if let Some(job) = jobs.list.iter_mut().find(|j| j.info.id == id) {
        job.info.status = status;
        job.info.exit_code = code;
        job.info.finished_at = Some(Utc::now());
    }
}

/// Последние задачи, новые первыми.
pub fn list_jobs() -> Vec<JobInfo> {
    jobs().list.iter().rev().map(|j| j.info.clone()).collect()
}

/// Задача и лог со строки `after` (номер с 0).
pub fn job_log(id: u64, after: u64) -> Option<JobLog> {
    let jobs = jobs();
    let job = jobs.list.iter().find(|j| j.info.id == id)?;
    let first = job.info.lines_total - job.lines.len() as u64;
    let from = after.max(first);
    let lines = job
        .lines
        .iter()
        .skip((from - first) as usize)
        .cloned()
        .collect();
    Some(JobLog {
        job: job.info.clone(),
        from,
        lines,
    })
}

/// Остановить задачу. `false` — нет такой или уже завершена.
pub fn cancel(id: u64) -> bool {
    let jobs = jobs();
    match jobs.list.iter().find(|j| j.info.id == id) {
        Some(job) if job.info.status == JobStatus::Running => {
            job.cancel.notify_one();
            true
        }
        _ => false,
    }
}

/// Отчёт скрипта (хвост, если длиннее [`REPORT_MAX`]). `None` — у скрипта нет отчёта или файла.
pub fn report(script: &Script) -> Option<String> {
    let text = std::fs::read(script.report?).ok()?;
    let start = text.len().saturating_sub(REPORT_MAX);
    Some(String::from_utf8_lossy(&text[start..]).into_owned())
}

/// Прогресс скрипта (`progress.json`, его пишет сам скрипт в начале и в конце запуска).
/// `None` — у скрипта нет прогресса или скрипт ещё ни разу не запускался.
pub fn progress(script: &Script) -> Option<serde_json::Value> {
    let text = std::fs::read(script.progress?).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Запустить скрипт в фоне. 409 — уже идёт задача, 503 — Python не настроен.
pub fn start(
    state: &AppState,
    script: &'static Script,
    args: Vec<String>,
) -> Result<JobInfo, AppError> {
    let python = state
        .config
        .scripts_python
        .clone()
        .ok_or_else(|| AppError::Unavailable("SCRIPTS_PYTHON is not set".into()))?;
    let cancel = Arc::new(Notify::new());
    let info = {
        let mut jobs = jobs();
        if jobs
            .list
            .iter()
            .any(|j| j.info.status == JobStatus::Running)
        {
            return Err(AppError::Conflict("another script is running".into()));
        }
        jobs.next += 1;
        let info = JobInfo {
            id: jobs.next,
            script: script.id.to_string(),
            args: args.clone(),
            status: JobStatus::Running,
            exit_code: None,
            started_at: Utc::now(),
            finished_at: None,
            lines_total: 0,
        };
        jobs.list.push_back(Job {
            info: info.clone(),
            lines: VecDeque::new(),
            cancel: cancel.clone(),
        });
        while jobs.list.len() > KEEP_JOBS {
            let Some(pos) = jobs
                .list
                .iter()
                .position(|j| j.info.status != JobStatus::Running)
            else {
                break;
            };
            jobs.list.remove(pos);
        }
        info
    };
    let id = info.id;
    let state = state.clone();
    tokio::spawn(async move {
        let Program::Python { file, args: fixed } = script.program;
        // `--reindex` — флаг сервера, не скрипта; один он — только перестройка, без скрипта.
        let want_reindex = args.iter().any(|a| a == "--reindex");
        let py_args: Vec<String> = args.iter().filter(|a| *a != "--reindex").cloned().collect();
        let only_reindex = want_reindex && py_args.is_empty();
        let (status, code) = if only_reindex {
            push_line(
                id,
                "только перестройка поиска — скрипт не запускается".into(),
            );
            (JobStatus::Succeeded, Some(0))
        } else {
            run_python(&state, id, &python, file, fixed, &py_args, &cancel).await
        };
        let reindex = status == JobStatus::Succeeded
            && script.after == Some(After::BooksReindex)
            && want_reindex
            && (only_reindex || wrote(script, &py_args));
        if status == JobStatus::Succeeded && !want_reindex && script.after.is_some() {
            push_line(id, "поиск не перестроен (галочка reindex выключена)".into());
        }
        let status = if reindex {
            tokio::select! {
                result = books_reindex(&state, id) => match result {
                    Ok(()) => status,
                    Err(error) => {
                        push_line(id, format!("ошибка: {error}"));
                        JobStatus::Failed
                    }
                },
                _ = cancel.notified() => JobStatus::Cancelled,
            }
        } else {
            status
        };
        tracing::info!(
            job = id,
            script = script.id,
            ?status,
            ?code,
            "admin script finished"
        );
        finish(id, status, code);
    });
    Ok(info)
}

/// Шаг «после» нужен, только если скрипт писал в БД.
fn wrote(script: &Script, args: &[String]) -> bool {
    // Код 0 у `books_llm.py run` = записано (иначе 3); у других — явный `--apply`.
    script.id == "books-llm" || args.iter().any(|a| a == "--apply")
}

async fn run_python(
    state: &AppState,
    id: u64,
    python: &str,
    file: &str,
    fixed: &[&str],
    args: &[String],
    cancel: &Notify,
) -> (JobStatus, Option<i32>) {
    let dir = Path::new(file).parent().unwrap_or(Path::new("."));
    let name = Path::new(file).file_name().unwrap_or_default();
    push_line(
        id,
        format!("$ {} {} {}", file, fixed.join(" "), args.join(" ")),
    );
    let mut command = tokio::process::Command::new(python);
    command
        .arg(name)
        .args(fixed)
        .args(args)
        .current_dir(dir)
        .env("DATABASE_URL", &state.config.database_url)
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(secret) = &state.config.ai_secret_key {
        command.env("AI_SECRET_KEY", secret);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            push_line(id, format!("не удалось запустить {python}: {error}"));
            return (JobStatus::Failed, None);
        }
    };
    let pipes = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
    ];
    let readers: Vec<_> = pipes
        .into_iter()
        .flatten()
        .map(|pipe| {
            tokio::spawn(async move {
                let mut lines = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    push_line(id, line);
                }
            })
        })
        .collect();
    let result = tokio::select! {
        status = child.wait() => status.ok().and_then(|s| s.code()),
        _ = cancel.notified() => {
            let _ = child.kill().await;
            for reader in readers {
                let _ = reader.await;
            }
            push_line(id, "остановлено".into());
            return (JobStatus::Cancelled, None);
        }
    };
    for reader in readers {
        let _ = reader.await;
    }
    let status = match result {
        Some(0) => JobStatus::Succeeded,
        Some(3) => JobStatus::Stopped,
        _ => JobStatus::Failed,
    };
    (status, result)
}

async fn books_reindex(state: &AppState, id: u64) -> Result<(), String> {
    push_line(id, "== перестройка книжного поиска (Tantivy)".into());
    let started = std::time::Instant::now();
    let books = crate::index::sync::rebuild(
        state,
        crate::index::sync::Target::Kinds(vec![crate::models::EntityKind::Book]),
        &crate::jobs::Reporter::silent(),
        false,
    )
    .await
    .map_err(|e| e.to_string())?;
    push_line(
        id,
        format!("книг в индексе: {books} за {:.0?}", started.elapsed()),
    );
    if Path::new(RANKS_FILE).exists() {
        let (lines, matched) = crate::books::ranks::import(state, Path::new(RANKS_FILE))
            .await
            .map_err(|e| e.to_string())?;
        push_line(
            id,
            format!(
                "ручной порядок: строк {lines}, с местом {}, нет в каталоге {}, другой автор {}",
                matched.ranks.len(),
                matched.not_found.len(),
                matched.other_author.len()
            ),
        );
    } else {
        push_line(id, format!("{RANKS_FILE} нет — ручной порядок не тронут"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn values(v: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn registry_patterns_compile_and_ids_are_unique() {
        let mut ids = std::collections::HashSet::new();
        for script in SCRIPTS {
            assert!(ids.insert(script.id), "{}", script.id);
            for flag in script.flags {
                if let FlagKind::Text { pattern } = flag.kind {
                    regex::Regex::new(pattern).unwrap();
                }
            }
        }
    }

    #[test]
    fn args_are_built_only_from_valid_flags() {
        let run = find("books-llm").unwrap();
        let args = build_args(
            run,
            &values(json!({"limit": 100, "yes": true, "ai": "deepseek"})),
        )
        .unwrap();
        assert_eq!(args, ["--limit", "100", "--yes", "--ai", "deepseek"]);
        let only = build_args(run, &values(json!({"reindex": true}))).unwrap();
        assert_eq!(only, ["--reindex"]);
        assert!(build_args(run, &values(json!({"limit": 0}))).is_err());
        assert!(build_args(run, &values(json!({"ai": "x; rm -rf /"}))).is_err());
        assert!(build_args(run, &values(json!({"evil": "1"}))).is_err());
        assert!(build_args(run, &values(json!({"yes": "true"}))).is_err());
        assert!(build_args(run, &values(json!({"model": "x"}))).is_err());
        assert_eq!(SCRIPTS.len(), 1, "одна цель — один скрипт");
    }
}
