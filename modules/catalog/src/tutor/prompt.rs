//! Промпты ИИ-преподавателя: как можно меньше токенов при сохранённом контексте.
//!
//! В модель уходит не вся переписка, а:
//! - короткий системный промпт (~80 токенов): роль, книга (название, автор, год — их модель знает
//!   и так, текст книги не передаём), докуда дочитал читатель — чтобы не спойлерить;
//! - `summary` — сжатое содержание ранних сообщений (≤ [`SUMMARY_CHARS`]);
//! - последние [`WINDOW`] реплик, ответы ИИ — урезанными до [`ANSWER_IN_HISTORY`];
//! - вопрос и, если он из читалки, выделенный фрагмент (≤ [`MAX_QUOTE`]) с главой.
//!
//! Тексты — шаблоны из [`super::templates`], их правят в админке.
//!
//! Так запрос в длинном чате стоит столько же, сколько в коротком: ~1–2 тыс. токенов на входе.
//! Когда вне окна копится [`SUMMARIZE_AFTER`] реплик, их сжимает отдельный дешёвый запрос.

use super::llm::Turn;
use super::templates::{self, Templates};

/// Последних реплик в запросе (3 пары вопрос–ответ).
pub const WINDOW: usize = 6;
/// Сколько реплик вне окна копится до пересжатия summary.
pub const SUMMARIZE_AFTER: usize = 6;
/// Ответ ИИ в истории — не длиннее (полный — в базе и на экране).
pub const ANSWER_IN_HISTORY: usize = 1_200;
pub const MAX_QUESTION: usize = 2_000;
pub const MAX_QUOTE: usize = 1_500;
pub const SUMMARY_CHARS: usize = 900;
/// Предел длины ответа, токенов: ответы короткие — так и дешевле, и полезнее.
pub const ANSWER_TOKENS: u32 = 700;
pub const SUMMARY_TOKENS: u32 = 300;

/// Книга для промпта.
pub struct Book {
    pub title: String,
    pub authors: Vec<String>,
    pub year: Option<i32>,
}

/// Системный промпт: роль, книга, прогресс читателя (тексты — из [`Templates`]).
pub fn system(t: &Templates, book: &Book, progress: Option<f32>, summary: &str) -> String {
    let mut about = format!("«{}»", book.title);
    if !book.authors.is_empty() {
        about.push_str(&format!(", {}", book.authors.join(", ")));
    }
    if let Some(year) = book.year {
        about.push_str(&format!(", {year}"));
    }
    let mut s = t.render(templates::SYSTEM, &[("about", &about)]);
    match progress {
        Some(p) if p < 0.98 => {
            let percent = ((p * 100.0).round() as i32).to_string();
            s.push_str(&t.render(templates::SPOILER, &[("percent", &percent)]));
        }
        _ => {}
    }
    if !summary.is_empty() {
        s.push_str(&t.render(templates::SUMMARY, &[("summary", summary)]));
    }
    s
}

/// Вопрос читателя; из читалки — с фрагментом и главой.
pub fn question(t: &Templates, text: &str, quote: Option<&str>, chapter: Option<&str>) -> String {
    match quote {
        Some(q) => {
            let place = chapter
                .map(|c| format!(" (глава «{c}»)"))
                .unwrap_or_default();
            let quote = clip(q, MAX_QUOTE);
            t.render(
                templates::QUOTE,
                &[("place", &place), ("quote", &quote), ("question", text)],
            )
        }
        None => text.to_string(),
    }
}

/// Сохранённая реплика (из базы).
pub struct Stored {
    pub role: String,
    pub content: String,
    pub quote: Option<String>,
    pub chapter: Option<String>,
}

/// Реплики окна для модели (по порядку); ответы ИИ урезаны.
pub fn window(t: &Templates, history: &[Stored]) -> Vec<Turn> {
    let start = history.len().saturating_sub(WINDOW);
    let mut turns: Vec<Turn> = history[start..]
        .iter()
        .map(|m| {
            if m.role == "assistant" {
                Turn {
                    role: "assistant",
                    content: clip(&m.content, ANSWER_IN_HISTORY),
                }
            } else {
                Turn {
                    role: "user",
                    content: question(t, &m.content, m.quote.as_deref(), m.chapter.as_deref()),
                }
            }
        })
        .collect();
    // Окно начинается с ответа ИИ (обрезали посередине пары) — у Anthropic первое сообщение
    // обязано быть от пользователя.
    while turns.first().is_some_and(|t| t.role == "assistant") {
        turns.remove(0);
    }
    turns
}

/// Запрос на пересжатие: старое summary + выпавшие из окна реплики → новое summary.
pub fn summarize(t: &Templates, old: &str, dropped: &[Stored]) -> (String, Vec<Turn>) {
    let mut text = String::new();
    if !old.is_empty() {
        text.push_str(&format!("Прежнее резюме: {old}\n\n"));
    }
    for m in dropped {
        let who = if m.role == "assistant" {
            "ИИ"
        } else {
            "Читатель"
        };
        text.push_str(&format!("{who}: {}\n", clip(&m.content, 600)));
    }
    (
        t.render(
            templates::SUMMARIZE,
            &[("chars", &SUMMARY_CHARS.to_string())],
        ),
        vec![Turn {
            role: "user",
            content: text,
        }],
    )
}

/// Обрезать по символам (не по байтам) с многоточием.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Название обсуждения по первому вопросу.
pub fn title(question: &str) -> String {
    let line = question.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        "Обсуждение".into()
    } else {
        clip(line, 80)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(role: &str, content: &str) -> Stored {
        Stored {
            role: role.into(),
            content: content.into(),
            quote: None,
            chapter: None,
        }
    }

    #[test]
    fn system_mentions_book_progress_and_summary() {
        let book = Book {
            title: "Мастер и Маргарита".into(),
            authors: vec!["Михаил Булгаков".into()],
            year: Some(1967),
        };
        let t = Templates::default();
        let s = system(&t, &book, Some(0.3), "спорили о Воланде");
        assert!(s.contains("«Мастер и Маргарита», Михаил Булгаков, 1967"));
        assert!(s.contains("прочитал 30%"));
        assert!(s.contains("спорили о Воланде"));
        assert!(!system(&t, &book, Some(1.0), "").contains("прочитал"));
    }

    #[test]
    fn window_keeps_last_turns_and_starts_with_user() {
        let history: Vec<Stored> = (0..9)
            .map(|i| {
                m(
                    if i % 2 == 0 { "user" } else { "assistant" },
                    &format!("{i}"),
                )
            })
            .collect();
        let w = window(&Templates::default(), &history);
        // Последние 6 — с 3-го (ассистент) по 8-й; первый ассистент отброшен.
        assert_eq!(w.first().unwrap().content, "4");
        assert_eq!(w.first().unwrap().role, "user");
        assert_eq!(w.len(), 5);
    }

    #[test]
    fn long_answers_are_clipped_in_history() {
        let long = "а".repeat(5_000);
        let w = window(
            &Templates::default(),
            &[m("user", "?"), m("assistant", &long)],
        );
        assert_eq!(w[1].content.chars().count(), ANSWER_IN_HISTORY + 1);
    }

    #[test]
    fn question_with_quote_and_chapter() {
        let q = question(
            &Templates::default(),
            "Почему?",
            Some("Рукописи не горят"),
            Some("Глава 24"),
        );
        assert!(q.contains("глава «Глава 24»"));
        assert!(q.contains("«Рукописи не горят»"));
        assert!(q.ends_with("Вопрос: Почему?"));
    }
}
