//! Small, durable job registry. HTTP handlers only enqueue; no broker is needed.
use anyhow::Result;
use sqlx::SqlitePool;
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing::Instrument;

use crate::sync_diagnostics::{self as diagnostics, SyncFailure};

const DEADLINE: Duration = Duration::from_secs(180);
static WORKERS: Semaphore = Semaphore::const_new(2);

struct Context {
    pool: SqlitePool,
    source_id: String,
    sync_id: String,
}
tokio::task_local! { static JOB: Context; }

/// Called by diagnostics: only constant stage names are persisted, never errors/bodies.
pub(crate) async fn stage(stage: &'static str) {
    if matches!(stage, "response_headers" | "response_body" | "sync") {
        return;
    }
    let context = JOB.try_with(|c| (c.pool.clone(), c.source_id.clone(), c.sync_id.clone()));
    if let Ok((pool, source_id, sync_id)) = context {
        let _ =
            sqlx::query("UPDATE caldav_sources SET sync_stage = ? WHERE id = ? AND sync_id = ?")
                .bind(stage)
                .bind(source_id)
                .bind(sync_id)
                .execute(&pool)
                .await;
    }
}

/// Also recovers jobs abandoned by a process restart, without touching the cache.
pub(crate) async fn expire(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "UPDATE caldav_sources SET sync_status = 'failed', sync_error = 'interrupted',
                   sync_finished_at = datetime('now')
                 WHERE sync_status IN ('queued', 'running')
                   AND sync_started_at < datetime('now', '-190 seconds')",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Atomic claim works across concurrent requests and processes using the same DB.
/// Automatic retries back off for a minute; a manual retry is immediate.
pub(crate) async fn enqueue(
    pool: &SqlitePool,
    key: &[u8; 32],
    source_id: &str,
    force: bool,
    trigger: &'static str,
) -> Result<String> {
    enqueue_with_deadline(pool, key, source_id, force, trigger, DEADLINE).await
}

async fn enqueue_with_deadline(
    pool: &SqlitePool,
    key: &[u8; 32],
    source_id: &str,
    force: bool,
    trigger: &'static str,
    deadline: Duration,
) -> Result<String> {
    expire(pool).await?;
    let sync_id = uuid::Uuid::new_v4().to_string();
    let automatic = matches!(trigger, "background" | "on_demand");
    let claimed = sqlx::query(
        "UPDATE caldav_sources SET sync_status = 'queued', sync_stage = 'queued', sync_id = ?,
           sync_started_at = datetime('now'), sync_finished_at = NULL
         WHERE id = ? AND enabled = 1 AND sync_status NOT IN ('queued', 'running')
           AND (? = 0 OR sync_finished_at IS NULL OR sync_finished_at < datetime('now', '-60 seconds'))"
    ).bind(&sync_id).bind(source_id).bind(automatic).execute(pool).await?.rows_affected();
    if claimed == 0 {
        return Ok(sqlx::query_scalar::<_, Option<String>>(
            "SELECT sync_id FROM caldav_sources WHERE id = ?",
        )
        .bind(source_id)
        .fetch_one(pool)
        .await?
        .unwrap_or_default());
    }

    let context = Context {
        pool: pool.clone(),
        source_id: source_id.to_owned(),
        sync_id: sync_id.clone(),
    };
    let pool = pool.clone();
    let key = *key;
    let source_id = source_id.to_owned();
    let span = tracing::info_span!("calendar_sync", %sync_id, %source_id, trigger);
    let response_id = sync_id.clone();
    tokio::spawn(JOB.scope(context, async move {
        let work = async {
            let _permit = WORKERS.acquire().await?;
            sqlx::query("UPDATE caldav_sources SET sync_status = 'running' WHERE id = ? AND sync_id = ?")
                .bind(&source_id).bind(&sync_id).execute(&pool).await?;
            crate::commands::sync::sync_source_by_id(&pool, &key, &source_id, force).await
        };
        let result = match tokio::time::timeout(deadline, work).await {
            Ok(result) => result,
            Err(_) => Err(SyncFailure::new("deadline").into()),
        };
        let (status, error, http_status) = match &result {
            Ok(()) => ("ok", None, None),
            Err(error) => ("failed", Some(diagnostics::error_kind(error)), diagnostics::http_status(error)),
        };
        tracing::info!(target: diagnostics::TARGET, outcome = status, error_kind = error,
            http_status, "background sync finished");
        if sqlx::query("UPDATE caldav_sources SET sync_status = ?, sync_error = ?, sync_http_status = ?,
                          sync_finished_at = datetime('now') WHERE id = ? AND sync_id = ?")
            .bind(status).bind(error).bind(http_status.map(i64::from))
            .bind(&source_id).bind(&sync_id).execute(&pool).await.is_err() {
            tracing::error!(target: diagnostics::TARGET, error_kind = "database", "could not record sync result");
        }
    }.instrument(span)));
    Ok(response_id)
}

/// Fail closed only for calendars contributing to this event type. No calendar
/// source is a valid configuration, but an unverified new source is not.
pub(crate) async fn available(
    pool: &SqlitePool,
    user_id: &str,
    event_type_id: Option<&str>,
    window_start_utc: &str,
    window_end_utc: &str,
) -> bool {
    available_for(
        pool,
        user_id,
        "",
        event_type_id,
        window_start_utc,
        window_end_utc,
    )
    .await
}

pub(crate) async fn available_for(
    pool: &SqlitePool,
    user_id: &str,
    account_id: &str,
    event_type_id: Option<&str>,
    window_start_utc: &str,
    window_end_utc: &str,
) -> bool {
    let result: Result<i64, _> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM caldav_sources cs JOIN accounts a ON a.id = cs.account_id
         WHERE (a.user_id = ? OR a.id = ?) AND cs.enabled = 1
           AND (EXISTS (SELECT 1 FROM calendars c WHERE c.source_id = cs.id AND c.is_busy = 1
                  AND (NOT EXISTS (SELECT 1 FROM event_type_calendars WHERE event_type_id = ?)
                       OR c.id IN (SELECT calendar_id FROM event_type_calendars WHERE event_type_id = ?)))
                OR (NOT EXISTS (SELECT 1 FROM calendars WHERE source_id = cs.id)
                    AND NOT EXISTS (SELECT 1 FROM event_type_calendars WHERE event_type_id = ?)))
           AND (cs.sync_verified_at IS NULL OR cs.sync_verified_at < datetime('now', '-300 seconds')
                OR cs.sync_error IS NOT NULL OR cs.sync_status = 'failed' OR cs.sync_window_start IS NULL OR cs.sync_window_start > ?
                OR (cs.sync_window_end IS NOT NULL AND cs.sync_window_end < ?))"
    ).bind(user_id).bind(account_id).bind(event_type_id.unwrap_or("")).bind(event_type_id.unwrap_or(""))
        .bind(event_type_id.unwrap_or("")).bind(window_start_utc).bind(window_end_utc).fetch_one(pool).await;
    matches!(result, Ok(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::sync::snapshot::tests::fixture;
    use std::sync::atomic::Ordering;

    async fn finished(pool: &SqlitePool) -> (String, Option<String>, Option<i64>, Option<String>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let row: (String, Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
                    "SELECT sync_status, sync_error, sync_http_status, sync_stage FROM caldav_sources WHERE id = 's'"
                ).fetch_one(pool).await.unwrap();
                if matches!(row.0.as_str(), "ok" | "failed") { return row; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap()
    }

    #[tokio::test]
    async fn failed_http_job_is_private_retryable_and_not_a_success() {
        let (pool, _, mock, server) = fixture().await;
        mock.mode.store(1, Ordering::SeqCst);
        let id = enqueue(&pool, &[0; 32], "s", false, "dashboard")
            .await
            .unwrap();
        let row = finished(&pool).await;
        assert_eq!(
            row,
            (
                "failed".into(),
                Some("http_status".into()),
                Some(504),
                Some("download_events".into())
            )
        );
        // Automatic requests back off; manual retry gets a new attempt.
        assert_eq!(
            enqueue(&pool, &[0; 32], "s", false, "on_demand")
                .await
                .unwrap(),
            id
        );
        mock.mode.store(0, Ordering::SeqCst);
        assert_ne!(
            enqueue(&pool, &[0; 32], "s", false, "dashboard")
                .await
                .unwrap(),
            id
        );
        assert_eq!(finished(&pool).await.0, "ok");
        assert!(available(&pool, "host", None, "20300101T000000Z", "20310101T000000Z").await);
        server.abort();
    }

    #[tokio::test]
    async fn hung_server_hits_job_deadline_and_can_be_retried() {
        let (pool, _, mock, server) = fixture().await;
        mock.mode.store(5, Ordering::SeqCst);
        enqueue_with_deadline(
            &pool,
            &[0; 32],
            "s",
            false,
            "dashboard",
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        assert_eq!(finished(&pool).await.1.as_deref(), Some("deadline"));
        assert!(!available(&pool, "host", None, "20300101T000000Z", "20310101T000000Z").await);
        mock.mode.store(0, Ordering::SeqCst);
        enqueue(&pool, &[0; 32], "s", false, "dashboard")
            .await
            .unwrap();
        assert_eq!(finished(&pool).await.0, "ok");
        server.abort();
    }

    #[tokio::test]
    async fn unknown_stale_failed_and_out_of_coverage_are_not_free() {
        let (pool, _, _, server) = fixture().await;
        let check = || available(&pool, "host", None, "20300101T000000Z", "20300102T000000Z");
        assert!(!check().await); // Including new sources with no discovered calendars.
        sqlx::query("UPDATE caldav_sources SET sync_verified_at = datetime('now'), sync_window_start = '20200101T000000Z'").execute(&pool).await.unwrap();
        assert!(check().await);
        sqlx::query("UPDATE caldav_sources SET sync_status = 'running'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(check().await); // A fresh complete cache is usable while refreshing.
        for update in [
            "sync_status = 'failed'",
            "sync_verified_at = '2000-01-01 00:00:00'",
            "sync_window_start = '20300102T000000Z'",
            "sync_window_end = '20291231T000000Z'",
            "sync_status = 'queued', sync_error = 'timeout'",
        ] {
            sqlx::query(&format!("UPDATE caldav_sources SET {update}"))
                .execute(&pool)
                .await
                .unwrap();
            assert!(!check().await);
            sqlx::query("UPDATE caldav_sources SET sync_status = 'ok', sync_error = NULL, sync_verified_at = datetime('now'),
                sync_window_start = '20200101T000000Z', sync_window_end = NULL").execute(&pool).await.unwrap();
        }
        assert!(
            available(
                &pool,
                "unrelated-user",
                None,
                "20300101T000000Z",
                "20300102T000000Z"
            )
            .await
        );
        server.abort();
    }

    #[tokio::test]
    async fn abandoned_job_becomes_an_explicit_failure() {
        let (pool, _, _, server) = fixture().await;
        sqlx::query("UPDATE caldav_sources SET sync_status = 'running', sync_started_at = '2000-01-01 00:00:00'").execute(&pool).await.unwrap();
        expire(&pool).await.unwrap();
        assert_eq!(finished(&pool).await.1.as_deref(), Some("interrupted"));
        server.abort();
    }
}
