//! Тексты промптов ИИ, настраиваемые из админки (`/admin/ai/prompts`).
//!
//! Каждый промпт — шаблон с подстановками `{имя}`. Встроенный текст — здесь, в [`DEFS`];
//! в таблице `ai_prompts` — только переопределения: для всех сущностей (`default`) или для одного
//! типа (`book`, `movie`, …). Поиск: тип → `default` → встроенный.

use std::collections::HashMap;

use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;

/// Область «для всех сущностей».
pub const DEFAULT_SCOPE: &str = "default";
pub const MAX_TEMPLATE: usize = 6_000;

/// Описание промпта: ключ, назначение, подстановки и встроенный текст.
pub struct Def {
    pub key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Подстановки: имя → что подставится.
    pub placeholders: &'static [(&'static str, &'static str)],
    pub builtin: &'static str,
}

pub const SYSTEM: &str = "tutor.system";
pub const SPOILER: &str = "tutor.spoiler";
pub const SUMMARY: &str = "tutor.summary";
pub const QUOTE: &str = "tutor.quote";
pub const SUMMARIZE: &str = "tutor.summarize";

pub const DEFS: &[Def] = &[
    Def {
        key: SYSTEM,
        title: "Системный промпт",
        description: "Роль ИИ и правила ответа. Уходит в каждый запрос — чем короче, тем дешевле.",
        placeholders: &[("about", "произведение: «Название», авторы, год")],
        builtin: "Ты — преподаватель литературы, обсуждаешь с читателем книгу {about}. \
Отвечай по-русски, по существу и коротко (до 150 слов), без вступлений; \
объясняй смысл, контекст эпохи, мотивы героев, задавай встречный вопрос, если это помогает \
думать. Не выдумывай: если не знаешь — так и скажи.",
    },
    Def {
        key: SPOILER,
        title: "Без спойлеров",
        description: "Добавляется к системному промпту, если читатель ещё не дочитал (< 98%).",
        placeholders: &[("percent", "сколько процентов прочитано, число")],
        builtin: " Читатель прочитал {percent}% — не раскрывай, что будет дальше, \
если он сам прямо не попросит спойлер.",
    },
    Def {
        key: SUMMARY,
        title: "Резюме разговора",
        description: "Добавляется к системному промпту, когда ранние реплики уже сжаты в резюме.",
        placeholders: &[("summary", "сжатое содержание ранних реплик")],
        builtin: "\nРанее в разговоре: {summary}",
    },
    Def {
        key: QUOTE,
        title: "Вопрос по фрагменту",
        description: "Как оформляется вопрос, заданный из читалки по выделенному тексту.",
        placeholders: &[
            ("place", "« (глава «…»)» или пусто"),
            ("quote", "выделенный фрагмент"),
            ("question", "вопрос читателя"),
        ],
        builtin: "Фрагмент{place}:\n«{quote}»\n\nВопрос: {question}",
    },
    Def {
        key: SUMMARIZE,
        title: "Сжатие разговора",
        description: "Отдельный дешёвый запрос: старые реплики → короткое резюме.",
        placeholders: &[("chars", "предел длины резюме, символов")],
        builtin: "Сожми разговор о книге в резюме до {chars} символов: о чём спрашивал читатель, \
к каким выводам пришли, что осталось открытым. Только факты, без вступлений.",
    },
];

pub fn def(key: &str) -> Option<&'static Def> {
    DEFS.iter().find(|d| d.key == key)
}

/// Действующие тексты промптов для одного типа сущности.
#[derive(Debug, Clone)]
pub struct Templates(HashMap<&'static str, String>);

impl Default for Templates {
    /// Встроенные тексты.
    fn default() -> Self {
        Self(
            DEFS.iter()
                .map(|d| (d.key, d.builtin.to_string()))
                .collect(),
        )
    }
}

impl Templates {
    /// Тексты для типа `kind`: переопределение типа → `default` → встроенный.
    pub async fn load(db: &PgPool, kind: &str) -> sqlx::Result<Self> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT key, scope, template FROM ai_prompts WHERE scope IN ($1, 'default')",
        )
        .bind(kind)
        .fetch_all(db)
        .await?;
        Ok(Self::resolve(kind, rows))
    }

    fn resolve(kind: &str, rows: Vec<(String, String, String)>) -> Self {
        let mut out = Self::default();
        // Сначала default, потом тип — тип перекрывает.
        for wanted in [DEFAULT_SCOPE, kind] {
            for (key, scope, template) in &rows {
                if scope == wanted {
                    if let Some(d) = def(key) {
                        out.0.insert(d.key, template.clone());
                    }
                }
            }
        }
        out
    }

    pub fn get(&self, key: &str) -> &str {
        self.0.get(key).map(String::as_str).unwrap_or("")
    }

    /// Шаблон `key` с подстановками.
    pub fn render(&self, key: &str, vars: &[(&str, &str)]) -> String {
        render(self.get(key), vars)
    }
}

/// Подставить `{имя}`; неизвестные `{…}` остаются как есть.
pub fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, value) in vars {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// Подстановки `{имя}` в шаблоне, которых нет у промпта (опечатки).
pub fn unknown_placeholders(d: &Def, template: &str) -> Vec<String> {
    let mut unknown = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        let name = &after[..end];
        let is_name = !name.is_empty()
            && name.len() <= 30
            && name.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        if is_name
            && !d.placeholders.iter().any(|(p, _)| *p == name)
            && !unknown.iter().any(|u| u == name)
        {
            unknown.push(name.to_string());
        }
        rest = &after[end + 1..];
    }
    unknown
}

/// Подстановка промпта — для админки.
#[derive(Debug, Serialize, ToSchema)]
pub struct Placeholder {
    pub name: String,
    pub description: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, scope: &str, t: &str) -> (String, String, String) {
        (key.into(), scope.into(), t.into())
    }

    #[test]
    fn kind_overrides_default_which_overrides_builtin() {
        let rows = vec![
            row(SYSTEM, "book", "книга {about}"),
            row(SYSTEM, "default", "общий {about}"),
            row(SUMMARIZE, "default", "сожми до {chars}"),
            row("unknown.key", "default", "x"),
        ];
        let t = Templates::resolve("book", rows.clone());
        assert_eq!(t.get(SYSTEM), "книга {about}");
        assert_eq!(t.get(SUMMARIZE), "сожми до {chars}");
        assert_eq!(t.get(QUOTE), def(QUOTE).unwrap().builtin);
        let movie = Templates::resolve("movie", rows);
        assert_eq!(movie.get(SYSTEM), "общий {about}");
    }

    #[test]
    fn render_and_unknown_placeholders() {
        assert_eq!(render("{a} и {a}, {b}", &[("a", "1")]), "1 и 1, {b}");
        let d = def(SPOILER).unwrap();
        assert_eq!(
            unknown_placeholders(d, "{percent}% {procent} {procent} {Не имя}"),
            vec!["procent".to_string()]
        );
        for d in DEFS {
            assert!(unknown_placeholders(d, d.builtin).is_empty(), "{}", d.key);
        }
    }
}
