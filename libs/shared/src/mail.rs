//! Отправка писем. Один интерфейс, три способа доставки:
//! - `Smtp` — настоящая отправка (в dev — в Mailpit, письма видны на http://localhost:8025);
//! - `Log` — письмо пишется в лог, если SMTP не настроен (локальный `cargo run` без Docker);
//! - `Memory` — письма копятся в [`Outbox`], тесты читают из него ссылки.

use crate::{AppError, AppResult, Config};
use lettre::message::Mailbox;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Email {
    pub to: String,
    pub subject: String,
    pub text: String,
}

#[derive(Clone)]
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

#[derive(Clone)]
pub enum Mailer {
    Smtp(Box<SmtpMailer>),
    Log,
    Memory(Outbox),
}

impl Mailer {
    /// `SMTP_URL` не задан — письма только в лог.
    pub fn from_config(config: &Config) -> Result<Self, String> {
        let Some(url) = &config.smtp_url else {
            return Ok(Self::Log);
        };
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
            .map_err(|e| format!("invalid SMTP_URL: {e}"))?
            .build();
        let from = config
            .mail_from
            .parse()
            .map_err(|e| format!("invalid MAIL_FROM: {e}"))?;
        Ok(Self::Smtp(Box::new(SmtpMailer { transport, from })))
    }

    pub fn memory() -> (Self, Outbox) {
        let outbox = Outbox::default();
        (Self::Memory(outbox.clone()), outbox)
    }

    pub async fn send(&self, email: Email) -> AppResult<()> {
        match self {
            Self::Smtp(smtp) => {
                let to: Mailbox = email
                    .to
                    .parse()
                    .map_err(|e| AppError::Internal(format!("recipient address: {e}")))?;
                let message = Message::builder()
                    .from(smtp.from.clone())
                    .to(to)
                    .subject(email.subject)
                    .body(email.text)
                    .map_err(|e| AppError::Internal(format!("build email: {e}")))?;
                smtp.transport
                    .send(message)
                    .await
                    .map_err(|e| AppError::Internal(format!("smtp: {e}")))?;
            }
            // Только для локальной разработки: в письмах ссылки с токенами.
            Self::Log => {
                tracing::warn!(to = %email.to, subject = %email.subject, text = %email.text, "email (SMTP not configured)");
            }
            Self::Memory(outbox) => outbox.push(email),
        }
        Ok(())
    }

    /// Ошибка отправки не должна ломать запрос (пользователь может запросить письмо ещё раз),
    /// но должна попасть в лог.
    pub async fn send_or_log(&self, email: Email) {
        let to = email.to.clone();
        if let Err(e) = self.send(email).await {
            tracing::error!(%to, error = %e, "failed to send email");
        }
    }
}

/// Письма, «отправленные» через [`Mailer::Memory`].
#[derive(Clone, Default)]
pub struct Outbox(Arc<Mutex<Vec<Email>>>);

impl Outbox {
    fn push(&self, email: Email) {
        self.0.lock().expect("outbox lock").push(email);
    }

    pub fn all(&self) -> Vec<Email> {
        self.0.lock().expect("outbox lock").clone()
    }

    /// Последнее письмо на адрес (без учёта регистра).
    pub fn last_to(&self, to: &str) -> Option<Email> {
        self.all()
            .into_iter()
            .rev()
            .find(|email| email.to.eq_ignore_ascii_case(to))
    }
}
