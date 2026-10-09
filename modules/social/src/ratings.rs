//! Сводки оценок для каталога (`shared::directory::EntityRatings`): по ним `catalog` сортирует
//! список (`popular`, `rating`), не читая `reviews`. Отправляются после каждой записи рецензии и
//! полностью при старте приложения — так расхождение (сбой отправки) не живёт дольше перезапуска.

use shared::directory::RatingStats;
use shared::AppState;
use sqlx::PgExecutor;
use uuid::Uuid;

/// Сводки по рецензиям: только оценки, текст без оценки не считается. Без `ids` — по всем.
async fn load(
    db: impl PgExecutor<'_>,
    ids: Option<&[Uuid]>,
) -> sqlx::Result<Vec<(Uuid, RatingStats)>> {
    let rows: Vec<(Uuid, Option<f64>, i64)> = sqlx::query_as(
        "SELECT entity_id, avg(rating)::float8, count(rating)
         FROM reviews
         WHERE $1::uuid[] IS NULL OR entity_id = ANY($1)
         GROUP BY entity_id",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, average, count)| (id, RatingStats { average, count }))
        .collect())
}

/// Сообщить каталогу сводку сущности после записи рецензии. Ошибка — warning: рецензия уже
/// записана, сводку поправит следующая запись или старт.
pub(crate) async fn sync(state: &AppState, entity_id: Uuid) {
    let result = async {
        let mut stats = load(&state.db, Some(&[entity_id])).await?;
        if stats.is_empty() {
            // Рецензий не осталось.
            stats.push((
                entity_id,
                RatingStats {
                    average: None,
                    count: 0,
                },
            ));
        }
        state.ratings.set(&stats).await
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, %entity_id, "rating stats sync failed");
    }
}

/// Все сводки заново: при старте приложения.
pub async fn sync_all(state: &AppState) -> shared::AppResult<usize> {
    let stats = load(&state.db, None).await?;
    state.ratings.replace_all(&stats).await?;
    Ok(stats.len())
}

/// [`sync_all`] в фоне.
pub fn spawn_sync_all(state: AppState) {
    tokio::spawn(async move {
        match sync_all(&state).await {
            Ok(count) => tracing::info!(entities = count, "rating stats synced to catalog"),
            Err(error) => tracing::warn!(%error, "rating stats sync failed"),
        }
    });
}
