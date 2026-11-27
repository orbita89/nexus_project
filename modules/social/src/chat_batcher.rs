//! Пакетная запись чата: сообщение уходит подписчикам сразу (см. `chat::PgChat::send`), а в
//! PostgreSQL попадает пачкой — одна транзакция на [`BATCH_SIZE`] сообщений или на
//! [`FLUSH_EVERY`], что наступит раньше. Так же копятся «прочитано» (`chat.read`).
//!
//! Цена: при падении процесса теряются сообщения последней ~1 с (решение 2026-11-26: на
//! слабом сервере важнее нагрузка). Штатная остановка (SIGTERM) дописывает всё —
//! [`Batcher::flush`] из `main`.
//!
//! Чтение своих записей: REST `social` перед обработкой зовёт `state.chat.flush()` — если в
//! очереди что-то есть, пачка пишется сразу и запрос ждёт её; пустая очередь — ничего не стоит.
//!
//! Одна пачка — один запрос на таблицу:
//! - `chat_messages`: `INSERT … SELECT FROM UNNEST(…)`; `reply_to` на сообщение, которого уже
//!   нет, становится `NULL` (одна битая ссылка не валит пачку);
//! - `chat_rooms`: счётчик и последнее сообщение — агрегат по комнатам, `GREATEST` (прямая
//!   запись сообщения с вложением могла уйти раньше);
//! - `chat_reads`: upsert с `GREATEST` (автор прочитал своё, плюс `chat.read`);
//! - `chat_room_hidden`: новое сообщение в личке возвращает её собеседнику.
//!
//! Ошибка пачки — повтор по одному сообщению; не записавшееся — в лог и отбрасывается.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep_until, Instant};
use uuid::Uuid;

/// Сообщений в пачке: набралось — пишем сразу.
pub const BATCH_SIZE: usize = 50;
/// Не дольше этого сообщение ждёт записи.
pub const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// Очередь команд. Полная — `send` ждёт (обратное давление), а не копит память.
const QUEUE: usize = 1024;

/// Сообщение, ещё не записанное в БД.
#[derive(Debug, Clone)]
pub struct PendingMessage {
    pub id: Uuid,
    pub room_id: Uuid,
    pub author_id: Uuid,
    pub reply_to: Option<Uuid>,
    pub body: String,
    pub created_at: DateTime<Utc>,
    /// Личная переписка: собеседник (ему вернуть скрытую переписку).
    pub dm_peer: Option<Uuid>,
}

enum Cmd {
    Message(PendingMessage),
    Read {
        user_id: Uuid,
        room_id: Uuid,
        message_id: Uuid,
    },
    Flush(oneshot::Sender<()>),
}

/// Хэндл батчера. Задача записи запускается при создании и живёт, пока жив хэндл.
pub struct Batcher {
    tx: mpsc::Sender<Cmd>,
    /// Команд в очереди и в текущей пачке (не записано).
    pending: Arc<AtomicUsize>,
}

impl Batcher {
    pub fn spawn(pool: PgPool) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE);
        let pending = Arc::new(AtomicUsize::new(0));
        tokio::spawn(run(pool, rx, pending.clone()));
        Self { tx, pending }
    }

    pub async fn push(&self, message: PendingMessage) {
        self.enqueue(Cmd::Message(message)).await;
    }

    pub async fn read(&self, user_id: Uuid, room_id: Uuid, message_id: Uuid) {
        self.enqueue(Cmd::Read {
            user_id,
            room_id,
            message_id,
        })
        .await;
    }

    async fn enqueue(&self, cmd: Cmd) {
        self.pending.fetch_add(1, Ordering::AcqRel);
        if self.tx.send(cmd).await.is_err() {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            tracing::error!("chat batcher is gone, message dropped");
        }
    }

    /// Записать всё, что поставлено в очередь до вызова, и дождаться записи. Пусто — сразу.
    pub async fn flush(&self) {
        if self.pending.load(Ordering::Acquire) == 0 {
            return;
        }
        let (done, wait) = oneshot::channel();
        if self.tx.send(Cmd::Flush(done)).await.is_ok() {
            let _ = wait.await;
        }
    }

    /// Не записано (для тестов и метрик).
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }
}

#[derive(Default)]
struct Batch {
    messages: Vec<PendingMessage>,
    /// (пользователь, комната) → прочитано до.
    reads: HashMap<(Uuid, Uuid), Uuid>,
    /// Команд в пачке (для счётчика `pending`).
    commands: usize,
}

impl Batch {
    fn is_empty(&self) -> bool {
        self.commands == 0
    }

    fn read(&mut self, user_id: Uuid, room_id: Uuid, message_id: Uuid) {
        let last = self.reads.entry((user_id, room_id)).or_insert(message_id);
        if message_id > *last {
            *last = message_id;
        }
    }
}

async fn run(pool: PgPool, mut rx: mpsc::Receiver<Cmd>, pending: Arc<AtomicUsize>) {
    let mut batch = Batch::default();
    let mut deadline: Option<Instant> = None;
    loop {
        let cmd = tokio::select! {
            cmd = rx.recv() => cmd,
            _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                write_batch(&pool, std::mem::take(&mut batch), &pending).await;
                deadline = None;
                continue;
            }
        };
        let Some(cmd) = cmd else {
            // Все хэндлы удалены — дописать остаток и выйти.
            write_batch(&pool, std::mem::take(&mut batch), &pending).await;
            return;
        };
        match cmd {
            Cmd::Message(message) => {
                batch.read(message.author_id, message.room_id, message.id);
                batch.messages.push(message);
                batch.commands += 1;
            }
            Cmd::Read {
                user_id,
                room_id,
                message_id,
            } => {
                batch.read(user_id, room_id, message_id);
                batch.commands += 1;
            }
            Cmd::Flush(done) => {
                write_batch(&pool, std::mem::take(&mut batch), &pending).await;
                deadline = None;
                let _ = done.send(());
                continue;
            }
        }
        if batch.messages.len() >= BATCH_SIZE {
            write_batch(&pool, std::mem::take(&mut batch), &pending).await;
            deadline = None;
        } else if deadline.is_none() {
            deadline = Some(Instant::now() + FLUSH_EVERY);
        }
    }
}

async fn write_batch(pool: &PgPool, batch: Batch, pending: &AtomicUsize) {
    if batch.is_empty() {
        return;
    }
    let commands = batch.commands;
    if let Err(error) = write(pool, &batch.messages, &batch.reads).await {
        tracing::warn!(%error, messages = batch.messages.len(), "chat batch failed, retrying one by one");
        for message in &batch.messages {
            let mut reads = HashMap::new();
            reads.insert((message.author_id, message.room_id), message.id);
            if let Err(error) = write(pool, std::slice::from_ref(message), &reads).await {
                tracing::error!(%error, message_id = %message.id, room_id = %message.room_id,
                    "chat message dropped");
            }
        }
        if let Err(error) = write(pool, &[], &batch.reads).await {
            tracing::error!(%error, reads = batch.reads.len(), "chat reads dropped");
        }
    }
    pending.fetch_sub(commands, Ordering::AcqRel);
}

/// Одна транзакция: сообщения, комнаты, прочитанное, скрытые переписки.
async fn write(
    pool: &PgPool,
    messages: &[PendingMessage],
    reads: &HashMap<(Uuid, Uuid), Uuid>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !messages.is_empty() {
        let mut ids = Vec::with_capacity(messages.len());
        let mut rooms = Vec::with_capacity(messages.len());
        let mut authors = Vec::with_capacity(messages.len());
        let mut replies: Vec<Option<Uuid>> = Vec::with_capacity(messages.len());
        let mut bodies = Vec::with_capacity(messages.len());
        let mut times = Vec::with_capacity(messages.len());
        // Комната → (сколько, последнее id, последнее время).
        let mut per_room: HashMap<Uuid, (i32, Uuid, DateTime<Utc>)> = HashMap::new();
        let mut hidden_users = Vec::new();
        let mut hidden_rooms = Vec::new();
        for m in messages {
            ids.push(m.id);
            rooms.push(m.room_id);
            authors.push(m.author_id);
            replies.push(m.reply_to);
            bodies.push(m.body.as_str());
            times.push(m.created_at);
            let room = per_room.entry(m.room_id).or_insert((0, m.id, m.created_at));
            room.0 += 1;
            if m.id > room.1 {
                room.1 = m.id;
                room.2 = m.created_at;
            }
            if let Some(peer) = m.dm_peer {
                hidden_users.push(peer);
                hidden_rooms.push(m.room_id);
            }
        }
        sqlx::query(
            "INSERT INTO chat_messages (id, room_id, author_id, reply_to, body, created_at)
             SELECT u.id, u.room_id, u.author_id,
                    CASE WHEN u.reply_to = ANY($1)
                           OR EXISTS (SELECT 1 FROM chat_messages p WHERE p.id = u.reply_to)
                         THEN u.reply_to END,
                    u.body, u.created_at
             FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[], $4::uuid[], $5::text[],
                         $6::timestamptz[])
                  AS u(id, room_id, author_id, reply_to, body, created_at)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&ids)
        .bind(&rooms)
        .bind(&authors)
        .bind(&replies)
        .bind(&bodies)
        .bind(&times)
        .execute(&mut *tx)
        .await?;

        let mut room_ids = Vec::with_capacity(per_room.len());
        let mut counts = Vec::with_capacity(per_room.len());
        let mut last_ids = Vec::with_capacity(per_room.len());
        let mut last_ats = Vec::with_capacity(per_room.len());
        for (room_id, (count, last_id, last_at)) in per_room {
            room_ids.push(room_id);
            counts.push(count);
            last_ids.push(last_id);
            last_ats.push(last_at);
        }
        sqlx::query(
            "UPDATE chat_rooms r
             SET messages_count = r.messages_count + s.n,
                 last_message_id = GREATEST(r.last_message_id, s.last_id),
                 last_message_at = GREATEST(r.last_message_at, s.last_at)
             FROM UNNEST($1::uuid[], $2::int4[], $3::uuid[], $4::timestamptz[])
                  AS s(room_id, n, last_id, last_at)
             WHERE r.id = s.room_id",
        )
        .bind(&room_ids)
        .bind(&counts)
        .bind(&last_ids)
        .bind(&last_ats)
        .execute(&mut *tx)
        .await?;

        if !hidden_users.is_empty() {
            sqlx::query(
                "DELETE FROM chat_room_hidden h
                 USING UNNEST($1::uuid[], $2::uuid[]) AS u(user_id, room_id)
                 WHERE h.user_id = u.user_id AND h.room_id = u.room_id",
            )
            .bind(&hidden_users)
            .bind(&hidden_rooms)
            .execute(&mut *tx)
            .await?;
        }
    }
    if !reads.is_empty() {
        let mut users = Vec::with_capacity(reads.len());
        let mut rooms = Vec::with_capacity(reads.len());
        let mut last = Vec::with_capacity(reads.len());
        for ((user_id, room_id), message_id) in reads {
            users.push(*user_id);
            rooms.push(*room_id);
            last.push(*message_id);
        }
        sqlx::query(
            "INSERT INTO chat_reads (user_id, room_id, last_read_id)
             SELECT * FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[])
             ON CONFLICT (user_id, room_id) DO UPDATE
             SET last_read_id = GREATEST(chat_reads.last_read_id, EXCLUDED.last_read_id),
                 updated_at = now()",
        )
        .bind(&users)
        .bind(&rooms)
        .bind(&last)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_keeps_latest_read_per_user_and_room() {
        let mut batch = Batch::default();
        let (user, room) = (Uuid::new_v4(), Uuid::new_v4());
        let older = Uuid::now_v7();
        let newer = Uuid::now_v7();
        batch.read(user, room, newer);
        batch.read(user, room, older);
        assert_eq!(batch.reads[&(user, room)], newer);
        assert_eq!(batch.reads.len(), 1);
    }
}
