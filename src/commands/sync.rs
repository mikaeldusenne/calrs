use crate::caldav::CaldavClient;
use crate::providers::factory::kinds;
use crate::sync_diagnostics::{self as diagnostics, trace};
use anyhow::Result;
use colored::Colorize;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;
#[cfg(test)]
use uuid::Uuid;

/// Default look-back window for full-fetch syncs.
const DEFAULT_FULL_FETCH_LOOKBACK_DAYS: i64 = 7;

/// Parse the optional full-fetch look-back override.
fn parse_full_fetch_lookback_days(value: Option<&str>) -> i64 {
    value
        .map(str::trim)
        .and_then(|days| days.parse::<u32>().ok())
        .filter(|days| *days <= 36_500)
        .map(i64::from)
        .unwrap_or(DEFAULT_FULL_FETCH_LOOKBACK_DAYS)
}

fn full_fetch_lookback_days() -> i64 {
    parse_full_fetch_lookback_days(std::env::var("CALRS_SYNC_LOOKBACK_DAYS").ok().as_deref())
}

/// Per-source async mutexes used by `sync_if_stale` to dedupe in-flight
/// syncs. Without this, concurrent on-demand calls (e.g. several booking
/// pages loading at once, each fanning out over team members) could stack
/// multiple full CalDAV fetches for the same source, which each hold the
/// server's full iCal response in memory until parsing completes.
static SOURCE_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();

fn source_locks() -> &'static Mutex<HashMap<String, Arc<Mutex<()>>>> {
    SOURCE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get (or create) the dedup mutex for a given source.
pub(crate) async fn source_lock(source_id: &str) -> Arc<Mutex<()>> {
    let mut map = source_locks().lock().await;
    map.entry(source_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

pub(crate) mod snapshot;

pub async fn run(pool: &SqlitePool, key: &[u8; 32], full: bool) -> Result<()> {
    let sources: Vec<String> =
        sqlx::query_scalar("SELECT id FROM caldav_sources WHERE enabled = 1")
            .fetch_all(pool)
            .await?;
    for source_id in sources {
        let sync_id = crate::sync_jobs::enqueue(pool, key, &source_id, full, "cli").await?;
        loop {
            let (status, error): (String, Option<String>) = sqlx::query_as(
                "SELECT sync_status, sync_error FROM caldav_sources WHERE id = ? AND sync_id = ?",
            )
            .bind(&source_id)
            .bind(&sync_id)
            .fetch_one(pool)
            .await?;
            match status.as_str() {
                "ok" => break,
                "failed" => anyhow::bail!(
                    "Sync {sync_id} failed: {}",
                    error.as_deref().unwrap_or("application")
                ),
                _ => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
            }
            crate::sync_jobs::expire(pool).await?;
        }
    }
    println!("{} Sync complete.", "✓".green());
    Ok(())
}

/// Guest requests enqueue stale sources; they never wait for Exchange.
pub async fn sync_if_stale(pool: &SqlitePool, key: &[u8; 32], user_id: &str) {
    let sources: Vec<String> = sqlx::query_scalar(
        "SELECT cs.id FROM caldav_sources cs JOIN accounts a ON a.id = cs.account_id
         WHERE a.user_id = ? AND cs.enabled = 1
           AND (cs.sync_verified_at IS NULL OR cs.sync_verified_at < datetime('now', '-240 seconds')
                OR cs.sync_status = 'failed')",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    for source_id in sources {
        if crate::sync_jobs::enqueue(pool, key, &source_id, false, "on_demand")
            .await
            .is_err()
        {
            tracing::warn!(target: diagnostics::TARGET, error_kind = "database", "could not enqueue sync");
        }
    }
}

/// Worker-only entry point. Propagate every error to the job registry.
pub async fn sync_source_by_id(
    pool: &SqlitePool,
    key: &[u8; 32],
    source_id: &str,
    force: bool,
) -> Result<()> {
    let lock = source_lock(source_id).await;
    let _guard = lock.lock().await;
    let (url, username, password_enc, auth_type, access_token_enc, token_expires_at, provider_type, daily_full, revision):
        (String, String, Option<String>, String, Option<String>, Option<String>, String, bool, i64) = sqlx::query_as(
        "SELECT url, username, password_enc, auth_type, access_token_enc, token_expires_at, provider_type,
            last_full_sync IS NULL OR last_full_sync < datetime('now', '-1 day'), sync_revision
         FROM caldav_sources WHERE id = ? AND enabled = 1",
    ).bind(source_id).fetch_one(pool).await?;
    if provider_type == kinds::EWS {
        let password = crate::crypto::decrypt_password(key, password_enc.as_deref().unwrap_or(""))?;
        let provider =
            crate::providers::build_provider(&provider_type, &url, &username, &password)?;
        return snapshot::sync_provider(pool, provider.as_ref(), source_id, revision).await;
    }
    let client = trace(
        "credentials",
        crate::oauth2_caldav::build_client_for_source(
            pool,
            key,
            source_id,
            &url,
            &auth_type,
            &username,
            password_enc.as_deref(),
            access_token_enc.as_deref(),
            token_expires_at.as_deref(),
        ),
    )
    .await?;
    snapshot::sync(pool, &client, source_id, force || daily_full, revision).await?;
    // Reconciliation is best effort and outside the snapshot's success/deadline.
    // It still confirms remote absence before cancelling any booking.
    let pool = pool.clone();
    let key = *key;
    let source_id = source_id.to_owned();
    tokio::spawn(async move {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            cancel_orphaned_bookings(&pool, &key, Some(&client), &source_id),
        )
        .await;
    });
    Ok(())
}

/// Delete events by their CalDAV href (used for sync-collection 404 deletions).
/// Extracts UID from href pattern: /path/to/{uid}.ics
/// Also cancels any calrs bookings whose UID matches a deleted event and notifies the guest.
///
/// `client` is forwarded to `cancel_orphaned_booking` for confirm-before-cancel
/// verification. Tests pass `None` to bypass HTTP verification.
///
/// `source_id` scopes booking cancellations to event types owned by this source's
/// account (issue #106 defense-in-depth).
#[cfg(test)]
async fn delete_events_by_href(
    pool: &SqlitePool,
    key: &[u8; 32],
    client: Option<&CaldavClient>,
    source_id: &str,
    cal_id: &str,
    hrefs: &[String],
) -> u32 {
    let mut deleted = 0u32;
    for href in hrefs {
        // Extract UID from href: /calendars/alice/default/abc123.ics -> abc123
        let uid = href
            .rsplit('/')
            .next()
            .unwrap_or("")
            .trim_end_matches(".ics");
        if uid.is_empty() {
            continue;
        }
        let rows = sqlx::query("DELETE FROM events WHERE calendar_id = ? AND uid = ?")
            .bind(cal_id)
            .bind(uid)
            .execute(pool)
            .await
            .map(|r| r.rows_affected())
            .unwrap_or(0);
        if rows > 0 {
            deleted += rows as u32;
            // Cancel any matching booking that was deleted on the CalDAV server.
            // Gated on rows_affected > 0: if the server reports an href as deleted
            // but we never had a matching local event, that's a server-side quirk
            // (e.g. BlueMind sync-collection emitting spurious 404 propstats) — not
            // proof the host deleted the event. Cancelling on that signal alone has
            // wrongly cancelled live bookings in production.
            cancel_orphaned_booking(pool, key, client, source_id, uid).await;
        } else {
            tracing::warn!(
                uid = %uid,
                href = %href,
                calendar_id = %cal_id,
                "sync-collection reported href as deleted but no matching local event; \
                 skipping booking cancellation (likely server-side false positive)"
            );
        }
    }
    deleted
}

/// If a confirmed booking with this UID exists, mark it as cancelled — the event was
/// deleted on the CalDAV server side (host removed it directly in their calendar app).
/// Pending bookings are intentionally excluded: they haven't been pushed to CalDAV yet,
/// so "missing from server" is the normal state, not a cancellation signal.
/// Sends cancellation email to the guest (and host) if SMTP is configured.
///
/// `source_id` scopes the lookup: only bookings on event types whose account owns
/// `source_id` are eligible for cancellation. This is defense-in-depth — a sync
/// of source A must never be able to cancel a booking on source B (different
/// account, same UID by collision). See issue #106.
///
/// When `client` is `Some` and the booking has a stored `caldav_calendar_href`,
/// the resource is double-checked against the server via HEAD/PROPFIND before
/// the cancellation goes through. If the server says the event is still there
/// (or the verification can't conclude — network error, 5xx, auth failure),
/// the cancellation is skipped and a warning is logged. This is the safety net
/// against false positives in any orphan path: see issue #105.
///
/// Tests pass `None` for `client` to skip the HTTP verification and exercise the
/// DB-only cancellation behaviour directly.
async fn cancel_orphaned_booking(
    pool: &SqlitePool,
    key: &[u8; 32],
    client: Option<&CaldavClient>,
    source_id: &str,
    uid: &str,
) {
    // Fetch booking details before cancelling. Scoped by source_id via the
    // caldav_sources join: the booking must have been pushed to this
    // source's write calendar. The host contact is the assigned member
    // when set (#147), the event type owner otherwise.
    let booking: Option<(
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT b.id, b.guest_name, b.guest_email, COALESCE(b.guest_timezone, 'UTC'), b.start_at, b.end_at, b.uid,
                et.title, u.name, COALESCE(u.booking_email, u.email), b.caldav_calendar_href, u.timezone
         FROM bookings b
         JOIN event_types et ON et.id = b.event_type_id
         JOIN accounts a ON a.id = et.account_id
         JOIN users u ON u.id = COALESCE(b.assigned_user_id, a.user_id)
         JOIN caldav_sources cs ON cs.id = ?
         JOIN accounts sa ON sa.id = cs.account_id AND sa.user_id = u.id
         WHERE b.uid = ? AND b.status = 'confirmed'
           AND b.caldav_calendar_href = cs.write_calendar_href",
    )
    .bind(source_id)
    .bind(uid)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);

    let booking = match booking {
        Some(b) => b,
        None => return, // No confirmed booking with this UID
    };

    let (
        booking_id,
        guest_name,
        guest_email,
        guest_timezone,
        start_at,
        end_at,
        booking_uid,
        event_title,
        host_name,
        host_email,
        caldav_calendar_href,
        host_timezone,
    ) = booking;

    // Confirm-before-cancel: if we have a client and a stored calendar href for
    // this booking, verify that the resource is actually 404 on the server.
    // Any non-404 outcome (200 = still there, 5xx = server flake, network = down)
    // means we cannot prove the host deleted the event, so we skip the cancel.
    if let (Some(client), Some(cal_href)) = (client, caldav_calendar_href.as_deref()) {
        match client.event_exists(cal_href, uid).await {
            Ok(false) => {
                // Server confirms 404 — legitimate deletion, proceed.
            }
            Ok(true) => {
                tracing::warn!(
                    uid = %uid,
                    booking_id = %booking_id,
                    cal_href = %cal_href,
                    "skipping booking cancellation: CalDAV resource is still present on the server \
                     (a sync path reported it as deleted but verification disagrees)"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    uid = %uid,
                    booking_id = %booking_id,
                    cal_href = %cal_href,
                    error = %e,
                    "skipping booking cancellation: could not verify resource state on the server \
                     (treating inconclusive verification as not-deleted to avoid false positives)"
                );
                return;
            }
        }
    }

    // Cancel the booking
    let updated = sqlx::query(
        "UPDATE bookings SET status = 'cancelled' WHERE id = ? AND status = 'confirmed'",
    )
    .bind(&booking_id)
    .execute(pool)
    .await;

    let cancelled = matches!(updated, Ok(r) if r.rows_affected() > 0);
    if !cancelled {
        return;
    }

    tracing::info!(
        uid = %uid,
        booking_id = %booking_id,
        "booking cancelled: CalDAV event deleted externally"
    );

    // Send cancellation emails
    let smtp_config = match crate::email::load_smtp_config(pool, key).await {
        Ok(Some(cfg)) => cfg,
        _ => return, // No SMTP configured, skip email
    };

    let date = start_at.get(..10).unwrap_or(&start_at).to_string();
    let start_time = extract_time(&start_at);
    let end_time = extract_time(&end_at);

    let details = crate::email::CancellationDetails {
        event_title,
        date,
        start_time,
        end_time,
        guest_name,
        guest_email,
        guest_timezone,
        host_name,
        host_email,
        uid: booking_uid,
        reason: Some("The calendar event was deleted by the host.".to_string()),
        cancelled_by_host: true,
        host_timezone: host_timezone.unwrap_or_default(),
        ..Default::default()
    };

    if let Err(e) = crate::email::send_guest_cancellation(&smtp_config, &details).await {
        tracing::warn!(error = %e, "failed to send external cancellation email to guest");
    }
    if let Err(e) = crate::email::send_host_cancellation(&smtp_config, &details).await {
        tracing::warn!(error = %e, "failed to send external cancellation email to host");
    }
}

/// Extract HH:MM time from a datetime string.
fn extract_time(dt_str: &str) -> String {
    // Try "YYYY-MM-DDTHH:MM:SS" or "YYYY-MM-DD HH:MM:SS"
    if dt_str.len() >= 16 {
        dt_str[11..16].to_string()
    } else {
        "00:00".to_string()
    }
}

/// Sweep for confirmed bookings whose CalDAV event no longer exists in the events table.
/// This catches bookings cancelled by the host deleting the event directly in their
/// calendar app. Pending bookings are excluded: they're awaiting host approval and must
/// not be auto-cancelled by sync — a guest-initiated reschedule that requires approval
/// deletes the prior CalDAV event on purpose, and the orphan sweep would otherwise race
/// the approval flow and cancel the booking before the host clicks approve.
async fn cancel_orphaned_bookings(
    pool: &SqlitePool,
    key: &[u8; 32],
    client: Option<&CaldavClient>,
    source_id: &str,
) {
    // Scope by the calendar the event was actually pushed to, not by the
    // event type owner's account: team bookings are written to the
    // assigned member's calendar (#147), and sweeping them from the
    // owner's source would auto-cancel bookings whose event simply hasn't
    // been synced into `events` by the member's source yet. Hrefs are not
    // globally unique across servers, so the source must also belong to
    // the booking's effective host (assigned member, else owner). The uid
    // lookup stays deliberately global: confirm-before-cancel is not
    // available on every path, and a write calendar that is not among the
    // synced ones must not read as "everything orphaned".
    let orphans: Vec<(String,)> = sqlx::query_as(
        "SELECT b.uid FROM bookings b
         JOIN event_types et ON et.id = b.event_type_id
         JOIN accounts a ON a.id = et.account_id
         JOIN caldav_sources cs ON cs.id = ?
         JOIN accounts sa ON sa.id = cs.account_id
                         AND sa.user_id = COALESCE(b.assigned_user_id, a.user_id)
         WHERE b.status = 'confirmed'
           AND b.caldav_calendar_href IS NOT NULL
           AND b.caldav_calendar_href = cs.write_calendar_href
           AND b.uid NOT IN (SELECT uid FROM events)",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    for (uid,) in &orphans {
        cancel_orphaned_booking(pool, key, client, source_id, uid).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    #[test]
    fn full_fetch_lookback_days_defaults_and_accepts_valid_overrides() {
        assert_eq!(parse_full_fetch_lookback_days(None), 7);
        assert_eq!(parse_full_fetch_lookback_days(Some("30")), 30);
        assert_eq!(parse_full_fetch_lookback_days(Some(" 0 ")), 0);
        assert_eq!(parse_full_fetch_lookback_days(Some("-1")), 7);
        assert_eq!(parse_full_fetch_lookback_days(Some("invalid")), 7);
        assert_eq!(parse_full_fetch_lookback_days(Some("4294967295")), 7);
    }

    async fn setup_test_db() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(
                SqliteConnectOptions::from_str("sqlite::memory:")
                    .unwrap()
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        crate::db::migrate(&pool).await.unwrap();
        pool
    }

    /// Seed the minimum fixtures needed to exercise the orphan sweep:
    /// one user + account + event type + caldav source. Returns the source id.
    async fn seed_fixtures(pool: &SqlitePool) -> (String, String) {
        let user_id = Uuid::new_v4().to_string();
        let account_id = Uuid::new_v4().to_string();
        let et_id = Uuid::new_v4().to_string();
        let source_id = Uuid::new_v4().to_string();

        sqlx::query("INSERT INTO users (id, email, name, role, auth_provider, username, enabled) VALUES (?, 'host@example.com', 'Host', 'admin', 'local', 'host', 1)")
            .bind(&user_id).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO accounts (id, name, email, timezone, user_id) VALUES (?, 'Host', 'host@example.com', 'UTC', ?)")
            .bind(&account_id).bind(&user_id).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO event_types (id, account_id, slug, title, duration_min) VALUES (?, ?, 'intro', 'Intro', 30)")
            .bind(&et_id).bind(&account_id).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO caldav_sources (id, account_id, name, url, username, write_calendar_href) VALUES (?, ?, 'test', 'https://dav.example.com/', 'user', '/calendars/host/default/')")
            .bind(&source_id).bind(&account_id).execute(pool).await.unwrap();

        (source_id, et_id)
    }

    /// Regression test for issue #44: a pending booking whose CalDAV event was deleted
    /// during a guest-initiated reschedule must NOT be cancelled by the orphan sweep.
    /// Before the fix, this scenario caused the reschedule request to be cancelled
    /// before the host could click approve — only the previous meeting was cancelled
    /// and no new one was created.
    #[tokio::test]
    async fn orphan_sweep_skips_pending_booking_awaiting_approval() {
        let pool = setup_test_db().await;
        let (source_id, et_id) = seed_fixtures(&pool).await;
        let key = [0u8; 32];

        // Booking in the exact state produced by guest_reschedule_booking's pending
        // branch: status='pending', caldav_calendar_href still set from the prior
        // confirmed push (fix A in web/mod.rs clears this — but the sweep must also
        // be safe even if a legacy booking row still has the href set).
        let booking_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO bookings (id, event_type_id, uid, guest_name, guest_email, guest_timezone,
                start_at, end_at, status, cancel_token, reschedule_token, caldav_calendar_href)
             VALUES (?, ?, 'orphaned-uid', 'Guest', 'guest@example.com', 'UTC',
                '2030-06-15T10:00:00', '2030-06-15T10:30:00', 'pending', 'ctok', 'rtok',
                '/calendars/host/default/')",
        )
        .bind(&booking_id)
        .bind(&et_id)
        .execute(&pool)
        .await
        .unwrap();
        // Note: no matching row in `events` — the CalDAV event was deleted during reschedule.

        cancel_orphaned_bookings(&pool, &key, None, &source_id).await;

        let status: String = sqlx::query_scalar("SELECT status FROM bookings WHERE id = ?")
            .bind(&booking_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            status, "pending",
            "pending bookings must not be auto-cancelled by the orphan sweep — \
             they're awaiting host approval, not tracking a CalDAV event"
        );
    }

    /// Regression test for production incident 2026-05-14: a sync-collection delta
    /// reported an href as deleted, but the local `events` table had no matching row
    /// (the event lived on a different calendar — the booking's write calendar).
    /// Before the fix, `delete_events_by_href` still called `cancel_orphaned_booking`,
    /// which scans bookings globally by UID and wrongly cancelled a live booking.
    /// The fix gates the cancellation on the local DELETE having matched a row.
    #[tokio::test]
    async fn delete_events_by_href_skips_cancellation_when_no_local_event() {
        let pool = setup_test_db().await;
        let (source_id, et_id) = seed_fixtures(&pool).await;
        let key = [0u8; 32];

        // Seed a calendar on this source (the one that supposedly reported a
        // deletion via sync-collection delta).
        let cal_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO calendars (id, source_id, href, display_name) \
             VALUES (?, ?, '/calendars/host/shared/', 'Shared')",
        )
        .bind(&cal_id)
        .bind(&source_id)
        .execute(&pool)
        .await
        .unwrap();

        // The confirmed booking. Note: NO matching row in `events` for this calendar
        // — the booking's CalDAV event lives elsewhere (or was never synced into
        // this particular calendar).
        let booking_uid = "live-booking-uid@calrs";
        let booking_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO bookings (id, event_type_id, uid, guest_name, guest_email, guest_timezone,
                start_at, end_at, status, cancel_token, reschedule_token, caldav_calendar_href)
             VALUES (?, ?, ?, 'Guest', 'guest@example.com', 'UTC',
                '2030-06-15T10:00:00', '2030-06-15T10:30:00', 'confirmed', 'ctok', 'rtok',
                '/calendars/host/default/')",
        )
        .bind(&booking_id)
        .bind(&et_id)
        .bind(booking_uid)
        .execute(&pool)
        .await
        .unwrap();

        // Simulate BlueMind's sync-collection reporting this href as deleted on
        // the Shared calendar (false positive — the event isn't there locally).
        let href = format!("/calendars/host/shared/{}.ics", booking_uid);
        let deleted = delete_events_by_href(&pool, &key, None, &source_id, &cal_id, &[href]).await;
        assert_eq!(deleted, 0, "no local row to delete");

        let status: String = sqlx::query_scalar("SELECT status FROM bookings WHERE id = ?")
            .bind(&booking_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            status, "confirmed",
            "a sync-collection 'deleted' href with no local event MUST NOT cancel the booking"
        );
    }

    /// Positive case for `delete_events_by_href`: when the server reports a deletion
    /// AND we had a matching local event for that calendar, we both remove the event
    /// and cancel any booking with that UID. This is the legitimate signal.
    #[tokio::test]
    async fn delete_events_by_href_cancels_when_local_event_existed() {
        let pool = setup_test_db().await;
        let (source_id, et_id) = seed_fixtures(&pool).await;
        let key = [0u8; 32];

        let cal_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO calendars (id, source_id, href, display_name) \
             VALUES (?, ?, '/calendars/host/default/', 'Default')",
        )
        .bind(&cal_id)
        .bind(&source_id)
        .execute(&pool)
        .await
        .unwrap();

        let booking_uid = "to-be-cancelled@calrs";

        // Seed the local event row (we knew about it before the server signaled deletion).
        sqlx::query(
            "INSERT INTO events (id, calendar_id, uid, summary, start_at, end_at) \
             VALUES (?, ?, ?, 'Demo', '2030-06-15T10:00:00', '2030-06-15T10:30:00')",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&cal_id)
        .bind(booking_uid)
        .execute(&pool)
        .await
        .unwrap();

        let booking_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO bookings (id, event_type_id, uid, guest_name, guest_email, guest_timezone,
                start_at, end_at, status, cancel_token, reschedule_token, caldav_calendar_href)
             VALUES (?, ?, ?, 'Guest', 'guest@example.com', 'UTC',
                '2030-06-15T10:00:00', '2030-06-15T10:30:00', 'confirmed', 'ctok', 'rtok',
                '/calendars/host/default/')",
        )
        .bind(&booking_id)
        .bind(&et_id)
        .bind(booking_uid)
        .execute(&pool)
        .await
        .unwrap();

        let href = format!("/calendars/host/default/{}.ics", booking_uid);
        let deleted = delete_events_by_href(&pool, &key, None, &source_id, &cal_id, &[href]).await;
        assert_eq!(deleted, 1, "local row should have been removed");

        let status: String = sqlx::query_scalar("SELECT status FROM bookings WHERE id = ?")
            .bind(&booking_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "cancelled");
    }

    /// Confirm-before-cancel (issue #105): when the verification HTTP call can't
    /// reach the server (unreachable host, timeout, 5xx), `cancel_orphaned_booking`
    /// must treat the result as inconclusive and SKIP the cancellation. The
    /// principle is "never cancel a customer booking unless the server confirms
    /// the event is gone." A flaky network minute is no basis for cancelling.
    ///
    /// This test points the CaldavClient at a closed port so the HEAD fails with
    /// a connection error; we set up a delta-path scenario where the booking
    /// would otherwise be cancelled (local event present, server reports deletion).
    #[tokio::test]
    async fn delete_events_by_href_skips_cancellation_when_verification_fails() {
        let pool = setup_test_db().await;
        let (source_id, et_id) = seed_fixtures(&pool).await;
        let key = [0u8; 32];

        let cal_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO calendars (id, source_id, href, display_name) \
             VALUES (?, ?, '/calendars/host/default/', 'Default')",
        )
        .bind(&cal_id)
        .bind(&source_id)
        .execute(&pool)
        .await
        .unwrap();

        let booking_uid = "ambiguous-deletion@calrs";

        // Local event row exists — so the DELETE will report rows_affected > 0
        // and the hotfix gate alone wouldn't save us. Only the verification
        // layer should keep this booking confirmed.
        sqlx::query(
            "INSERT INTO events (id, calendar_id, uid, summary, start_at, end_at) \
             VALUES (?, ?, ?, 'Demo', '2030-06-15T10:00:00', '2030-06-15T10:30:00')",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&cal_id)
        .bind(booking_uid)
        .execute(&pool)
        .await
        .unwrap();

        let booking_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO bookings (id, event_type_id, uid, guest_name, guest_email, guest_timezone,
                start_at, end_at, status, cancel_token, reschedule_token, caldav_calendar_href)
             VALUES (?, ?, ?, 'Guest', 'guest@example.com', 'UTC',
                '2030-06-15T10:00:00', '2030-06-15T10:30:00', 'confirmed', 'ctok', 'rtok',
                '/calendars/host/default/')",
        )
        .bind(&booking_id)
        .bind(&et_id)
        .bind(booking_uid)
        .execute(&pool)
        .await
        .unwrap();

        // Port 1 is reserved (tcpmux) and ~always closed on a dev box — HEAD will
        // fail with a connection refusal, exercising the Err(_) arm of the
        // verification gate.
        let client = CaldavClient::new("http://127.0.0.1:1", "u", "p");

        let href = format!("/calendars/host/default/{}.ics", booking_uid);
        let _ =
            delete_events_by_href(&pool, &key, Some(&client), &source_id, &cal_id, &[href]).await;

        let status: String = sqlx::query_scalar("SELECT status FROM bookings WHERE id = ?")
            .bind(&booking_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            status, "confirmed",
            "verification HTTP failure must NOT cancel the booking — \
             inconclusive evidence cannot justify a customer-visible cancellation"
        );
    }

    /// Positive case: the orphan sweep still cancels a confirmed booking whose CalDAV
    /// event has been deleted (host removed it directly in their calendar app). This
    /// is the original intent of the feature and must keep working.
    #[tokio::test]
    async fn orphan_sweep_cancels_confirmed_booking_with_missing_event() {
        let pool = setup_test_db().await;
        let (source_id, et_id) = seed_fixtures(&pool).await;
        let key = [0u8; 32];

        let booking_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO bookings (id, event_type_id, uid, guest_name, guest_email, guest_timezone,
                start_at, end_at, status, cancel_token, reschedule_token, caldav_calendar_href)
             VALUES (?, ?, 'deleted-uid', 'Guest', 'guest@example.com', 'UTC',
                '2030-06-15T10:00:00', '2030-06-15T10:30:00', 'confirmed', 'ctok', 'rtok',
                '/calendars/host/default/')",
        )
        .bind(&booking_id)
        .bind(&et_id)
        .execute(&pool)
        .await
        .unwrap();

        cancel_orphaned_bookings(&pool, &key, None, &source_id).await;

        let status: String = sqlx::query_scalar("SELECT status FROM bookings WHERE id = ?")
            .bind(&booking_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "cancelled");
    }

    /// Issue #106 (cross-source isolation defense-in-depth): a sync of source A
    /// must NEVER cancel a booking that belongs to source B's account, even if
    /// the UIDs collide. UUIDs make natural collisions vanishingly unlikely, but
    /// a single-tenant install where an admin imports an iCal feed (or a future
    /// codepath that generates UIDs from a non-UUID source) could produce one.
    /// The lookup in `cancel_orphaned_booking` is scoped via a join on
    /// `caldav_sources.account_id = event_types.account_id`, with the source_id
    /// filter pinning the side we're acting on. This test verifies that scoping.
    #[tokio::test]
    async fn cancel_does_not_cross_source_account_boundary() {
        let pool = setup_test_db().await;
        // Account A: gets seeded by the helper. Source A, event type A.
        // We don't act ON source A in this test — we only verify a sync of
        // source B can't reach across to cancel a booking on account A.
        let (_source_a_id, et_a_id) = seed_fixtures(&pool).await;

        // Account B: separate user, account, event type, source.
        let user_b_id = Uuid::new_v4().to_string();
        let account_b_id = Uuid::new_v4().to_string();
        let et_b_id = Uuid::new_v4().to_string();
        let source_b_id = Uuid::new_v4().to_string();
        let cal_b_id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO users (id, email, name, role, auth_provider, username, enabled) VALUES (?, 'host-b@example.com', 'Host B', 'user', 'local', 'hostb', 1)")
            .bind(&user_b_id).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO accounts (id, name, email, timezone, user_id) VALUES (?, 'Host B', 'host-b@example.com', 'UTC', ?)")
            .bind(&account_b_id).bind(&user_b_id).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO event_types (id, account_id, slug, title, duration_min) VALUES (?, ?, 'intro-b', 'Intro B', 30)")
            .bind(&et_b_id).bind(&account_b_id).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO caldav_sources (id, account_id, name, url, username, write_calendar_href) VALUES (?, ?, 'test-b', 'https://dav-b.example.com/', 'user-b', '/calendars/hostb/default/')")
            .bind(&source_b_id).bind(&account_b_id).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO calendars (id, source_id, href, display_name) VALUES (?, ?, '/calendars/hostb/default/', 'Default B')")
            .bind(&cal_b_id).bind(&source_b_id).execute(&pool).await.unwrap();

        let shared_uid = "colliding-uid@calrs";
        let key = [0u8; 32];

        // Confirmed booking on ACCOUNT A — this is what we're protecting.
        let booking_a_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO bookings (id, event_type_id, uid, guest_name, guest_email, guest_timezone,
                start_at, end_at, status, cancel_token, reschedule_token, caldav_calendar_href)
             VALUES (?, ?, ?, 'Guest', 'guest@example.com', 'UTC',
                '2030-06-15T10:00:00', '2030-06-15T10:30:00', 'confirmed', 'ctok', 'rtok',
                '/calendars/host/default/')",
        )
        .bind(&booking_a_id)
        .bind(&et_a_id)
        .bind(shared_uid)
        .execute(&pool)
        .await
        .unwrap();

        // Local event on SOURCE B's calendar with the same UID — so the
        // DELETE in `delete_events_by_href` will report rows_affected > 0
        // and the rows_affected gate alone can't save us. Only the
        // source-scoped lookup keeps booking A safe.
        sqlx::query(
            "INSERT INTO events (id, calendar_id, uid, summary, start_at, end_at) \
             VALUES (?, ?, ?, 'Colliding event on B', '2030-06-15T10:00:00', '2030-06-15T10:30:00')",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&cal_b_id)
        .bind(shared_uid)
        .execute(&pool)
        .await
        .unwrap();

        // Source B reports the colliding UID as deleted.
        let href = format!("/calendars/hostb/default/{}.ics", shared_uid);
        let deleted =
            delete_events_by_href(&pool, &key, None, &source_b_id, &cal_b_id, &[href]).await;
        assert_eq!(
            deleted, 1,
            "the local event row on source B should have been removed"
        );

        // Booking on ACCOUNT A must still be confirmed.
        let status: String = sqlx::query_scalar("SELECT status FROM bookings WHERE id = ?")
            .bind(&booking_a_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            status, "confirmed",
            "a sync of source B must not cancel a booking on account A — \
             source/account scoping is the defense-in-depth boundary"
        );
    }

    #[tokio::test]
    async fn source_lock_identity() {
        // Same id returns the same Arc so concurrent callers contend on one
        // mutex; different ids are independent so unrelated sources can sync
        // in parallel.
        let id_a = format!("lock-test-a-{}", Uuid::new_v4());
        let id_b = format!("lock-test-b-{}", Uuid::new_v4());

        let a1 = source_lock(&id_a).await;
        let a2 = source_lock(&id_a).await;
        let b1 = source_lock(&id_b).await;

        assert!(
            Arc::ptr_eq(&a1, &a2),
            "same id must resolve to the same mutex"
        );
        assert!(
            !Arc::ptr_eq(&a1, &b1),
            "different ids must resolve to different mutexes"
        );
    }

    #[tokio::test]
    async fn sync_if_stale_returns_immediately_and_deduplicates() {
        let pool = setup_test_db().await;
        let (source_id, _) = seed_fixtures(&pool).await;
        let user_id: String = sqlx::query_scalar(
            "SELECT a.user_id FROM caldav_sources cs JOIN accounts a ON a.id = cs.account_id WHERE cs.id = ?",
        ).bind(&source_id).fetch_one(&pool).await.unwrap();
        let lock = source_lock(&source_id).await;
        let guard = lock.lock().await;
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            sync_if_stale(&pool, &[0; 32], &user_id),
        )
        .await
        .expect("HTTP callers must not wait for the worker");
        let id: String = sqlx::query_scalar("SELECT sync_id FROM caldav_sources WHERE id = ?")
            .bind(&source_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            crate::sync_jobs::enqueue(&pool, &[0; 32], &source_id, true, "dashboard")
                .await
                .unwrap(),
            id
        );
        let verified: Option<String> =
            sqlx::query_scalar("SELECT sync_verified_at FROM caldav_sources WHERE id = ?")
                .bind(&source_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(verified.is_none());
        drop(guard);
    }
}
