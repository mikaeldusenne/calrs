//! Stage the whole source, then publish one SQLite transaction. No network in it.
use anyhow::Result;
use chrono::{Duration, Utc};
use sqlx::{SqliteConnection, SqlitePool};
use std::collections::HashMap;

use crate::caldav::{CaldavClient, CalendarInfo};
use crate::sync_diagnostics::{self as diagnostics, trace, SyncFailure};
use crate::utils::{
    extract_vevent_field as field, extract_vevent_tzid, parse_ical_datetime, split_vevents,
};

struct CalendarSnapshot {
    id: String,
    info: CalendarInfo,
    // None means the already verified ctag has not changed.
    objects: Option<HashMap<String, (String, String)>>,
}

pub(super) async fn sync(
    pool: &SqlitePool,
    client: &CaldavClient,
    source_id: &str,
    force: bool,
    revision: i64,
) -> Result<()> {
    let principal = trace("discover_principal", client.discover_principal()).await?;
    let home = trace(
        "discover_calendar_home",
        client.discover_calendar_home(&principal),
    )
    .await?;
    let calendars = trace("list_calendars", client.list_calendars(&home)).await?;
    if calendars.is_empty() {
        return Err(SyncFailure::new("no_calendars").into());
    }
    let since = (Utc::now() - Duration::days(super::full_fetch_lookback_days()))
        .format("%Y%m%dT000000Z")
        .to_string();
    let verified: bool = sqlx::query_scalar(
        "SELECT sync_verified_at IS NOT NULL AND sync_window_start <= ? FROM caldav_sources WHERE id = ?"
    ).bind(&since).bind(source_id).fetch_one(pool).await?;
    let mut snapshots = Vec::new();
    let mut total_bytes = 0;
    for info in calendars {
        let existing: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT id, ctag FROM calendars WHERE source_id = ? AND href = ?")
                .bind(source_id)
                .bind(&info.href)
                .fetch_optional(pool)
                .await?;
        let (id, ctag) = existing.unwrap_or_else(|| (uuid::Uuid::new_v4().to_string(), None));
        if !force && verified && ctag.is_some() && ctag == info.ctag {
            snapshots.push(CalendarSnapshot {
                id,
                info,
                objects: None,
            });
            continue;
        }
        let inventory = trace("inventory", client.inventory(&info.href, &since)).await?;
        let cached: Vec<(String, String, String)> =
            sqlx::query_as("SELECT href, etag, ical FROM caldav_objects WHERE calendar_id = ?")
                .bind(&id)
                .fetch_all(pool)
                .await?;
        let mut objects: HashMap<_, _> = cached
            .into_iter()
            .filter(|(href, etag, _)| !force && verified && inventory.get(href) == Some(etag))
            .map(|(href, etag, ical)| (href, (etag, ical)))
            .collect();
        total_bytes += objects.values().map(|(_, ical)| ical.len()).sum::<usize>();
        if total_bytes > 64 * 1024 * 1024 {
            return Err(SyncFailure::new("snapshot_too_large").into());
        }
        let changed: Vec<_> = inventory
            .iter()
            .filter(|(href, _)| !objects.contains_key(*href))
            .collect();
        for batch in changed.chunks(50) {
            let events = trace("download_events", client.multiget(&info.href, batch)).await?;
            diagnostics::event_count(events.len());
            for event in events {
                total_bytes += event.ical_data.len();
                if total_bytes > 64 * 1024 * 1024 {
                    return Err(SyncFailure::new("snapshot_too_large").into());
                }
                objects.insert(
                    event.href.clone(),
                    (inventory[&event.href].clone(), event.ical_data),
                );
            }
        }
        snapshots.push(CalendarSnapshot {
            id,
            info,
            objects: Some(objects),
        });
    }
    // A changing collection must not be published under a stale ctag. A later
    // automatic/manual retry will acquire a coherent snapshot.
    let current = trace("verify_calendar_state", client.list_calendars(&home)).await?;
    if current.len() != snapshots.len()
        || snapshots.iter().any(|s| {
            !current
                .iter()
                .any(|c| c.href == s.info.href && c.ctag == s.info.ctag)
        })
    {
        return Err(SyncFailure::new("remote_changed").into());
    }
    trace(
        "commit_snapshot",
        publish(pool, source_id, &since, None, &snapshots, revision),
    )
    .await
}

pub(super) async fn sync_provider(
    pool: &SqlitePool,
    provider: &dyn crate::providers::CalendarProvider,
    source_id: &str,
    revision: i64,
) -> Result<()> {
    let since = Utc::now() - Duration::days(super::full_fetch_lookback_days());
    let calendars = trace("list_calendars", provider.list_calendars()).await?;
    if calendars.is_empty() {
        return Err(SyncFailure::new("no_calendars").into());
    }
    let mut snapshots = Vec::new();
    for cal in calendars {
        let id: Option<String> =
            sqlx::query_scalar("SELECT id FROM calendars WHERE source_id = ? AND href = ?")
                .bind(source_id)
                .bind(&cal.id)
                .fetch_optional(pool)
                .await?;
        let events = trace(
            "download_events",
            provider.fetch_events_since(&cal.id, &since.to_rfc3339()),
        )
        .await?;
        let objects = events
            .into_iter()
            .map(|e| (e.remote_id, (String::new(), e.ical)))
            .collect();
        snapshots.push(CalendarSnapshot {
            id: id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            info: CalendarInfo {
                href: cal.id,
                display_name: cal.display_name,
                color: cal.color,
                ctag: cal.change_marker,
                sync_token: None,
            },
            objects: Some(objects),
        });
    }
    // Native EWS expands only a two-year window; never claim infinite coverage.
    let end = (since + Duration::days(730))
        .format("%Y%m%dT%H%M%SZ")
        .to_string();
    trace(
        "commit_snapshot",
        publish(
            pool,
            source_id,
            &since.format("%Y%m%dT%H%M%SZ").to_string(),
            Some(&end),
            &snapshots,
            revision,
        ),
    )
    .await
}

async fn publish(
    pool: &SqlitePool,
    source_id: &str,
    since: &str,
    until: Option<&str>,
    snapshots: &[CalendarSnapshot],
    revision: i64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let current: i64 =
        sqlx::query_scalar("SELECT sync_revision FROM caldav_sources WHERE id = ? AND enabled = 1")
            .bind(source_id)
            .fetch_one(&mut *tx)
            .await?;
    if current != revision {
        return Err(SyncFailure::new("remote_changed").into());
    }
    for snapshot in snapshots {
        sqlx::query("INSERT INTO calendars (id, source_id, href, display_name, color, ctag)
                     VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET
                     display_name = excluded.display_name, color = excluded.color, ctag = excluded.ctag, sync_token = NULL")
            .bind(&snapshot.id).bind(source_id).bind(&snapshot.info.href)
            .bind(&snapshot.info.display_name).bind(&snapshot.info.color).bind(&snapshot.info.ctag)
            .execute(&mut *tx).await?;
        if let Some(objects) = &snapshot.objects {
            sqlx::query("DELETE FROM events WHERE calendar_id = ?")
                .bind(&snapshot.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM caldav_objects WHERE calendar_id = ?")
                .bind(&snapshot.id)
                .execute(&mut *tx)
                .await?;
            for (href, (etag, ical)) in objects {
                store_events(&mut tx, &snapshot.id, ical).await?;
                sqlx::query("INSERT INTO caldav_objects (calendar_id, href, etag, ical) VALUES (?, ?, ?, ?)")
                    .bind(&snapshot.id).bind(href).bind(etag).bind(ical).execute(&mut *tx).await?;
            }
        }
    }
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM calendars WHERE source_id = ?")
        .bind(source_id)
        .fetch_all(&mut *tx)
        .await?;
    for id in ids {
        if !snapshots.iter().any(|s| s.id == id) {
            sqlx::query("DELETE FROM calendars WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
    }
    sqlx::query("UPDATE caldav_sources SET last_synced = datetime('now'),
                 sync_verified_at = datetime('now'), sync_window_start = ?, sync_window_end = ?,
                 last_full_sync = CASE WHEN ? THEN datetime('now') ELSE last_full_sync END WHERE id = ?")
        .bind(since).bind(until).bind(snapshots.iter().any(|s| s.objects.is_some())).bind(source_id)
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Reject data we cannot safely use for availability instead of silently
/// inventing UIDs or losing busy periods. Errors contain only fixed codes.
async fn store_events(conn: &mut SqliteConnection, calendar_id: &str, ical: &str) -> Result<()> {
    let unfolded = crate::utils::unfold_ical(ical);
    let ical = unfolded.as_str();
    let blocks = split_vevents(ical);
    if !ical.contains("BEGIN:VEVENT")
        || !ical.contains("END:VCALENDAR")
        || ical.matches("BEGIN:VEVENT").count() != ical.matches("END:VEVENT").count()
    {
        return Err(SyncFailure::new("invalid_calendar").into());
    }
    let resource_uid = field(&blocks[0], "UID");
    let vtz = crate::timezone::VTimezones::parse(ical);
    // Use the same parser/resolver as availability; validate all overrides too.
    crate::rrule::extract_exdates_in_tz(ical, None)?;
    for event in blocks {
        let required = |name| {
            field(&event, name)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| SyncFailure::new("invalid_calendar"))
        };
        let uid = required("UID")?;
        if Some(&uid) != resource_uid.as_ref() {
            return Err(SyncFailure::new("invalid_calendar").into());
        }
        let start = field(&event, "DTSTART")
            .or_else(|| {
                (field(&event, "STATUS").as_deref() == Some("CANCELLED"))
                    .then(|| field(&event, "RECURRENCE-ID"))
                    .flatten()
            })
            .ok_or_else(|| SyncFailure::new("invalid_calendar"))?;
        let start_dt =
            parse_ical_datetime(&start).ok_or_else(|| SyncFailure::new("invalid_calendar"))?;
        let mut end = if let Some(end) = field(&event, "DTEND") {
            end
        } else if let Some(duration) = field(&event, "DURATION") {
            start_dt
                .checked_add_signed(parse_duration(&duration)?)
                .ok_or_else(|| SyncFailure::new("invalid_calendar"))?
                .format("%Y%m%dT%H%M%S")
                .to_string()
        } else if start.len() == 8 {
            (start_dt + Duration::days(1)).format("%Y%m%d").to_string()
        } else {
            start.clone() // RFC 5545: a timed event without DTEND/DURATION is instantaneous.
        };
        let tz =
            crate::timezone::accept_tzid(extract_vevent_tzid(&event, "DTSTART").as_deref(), &vtz)
                .map_err(SyncFailure::new)?;
        let end_tz =
            crate::timezone::accept_tzid(extract_vevent_tzid(&event, "DTEND").as_deref(), &vtz)
                .map_err(SyncFailure::new)?;
        let mut end_dt =
            parse_ical_datetime(&end).ok_or_else(|| SyncFailure::new("invalid_calendar"))?;
        if let (Some(start_tz), Some(end_tz)) = (&tz, &end_tz) {
            if start_tz != end_tz {
                let start_zone = crate::timezone::resolve_tzid(start_tz)
                    .ok_or_else(|| SyncFailure::new("unsupported_timezone"))?;
                end_dt = crate::utils::convert_event_to_tz(end_dt, Some(end_tz), start_zone);
                end = end_dt.format("%Y%m%dT%H%M%S").to_string();
            }
        }
        if end_dt < start_dt {
            return Err(SyncFailure::new("invalid_calendar").into());
        }
        // RANGE changes later instances, not just this one: don't misinterpret it.
        if event.contains("RANGE=THISANDFUTURE") || field(&event, "RDATE").is_some() {
            return Err(SyncFailure::new("unsupported_recurrence").into());
        }
        let rule = field(&event, "RRULE");
        if let Some(rule) = &rule {
            crate::rrule::validate(start_dt, rule, tz.as_deref())?;
        }
        sqlx::query(
            "INSERT INTO events (id, calendar_id, uid, summary, start_at, end_at,
            location, description, status, rrule, raw_ical, recurrence_id, timezone, transp)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(calendar_id)
        .bind(uid)
        .bind(field(&event, "SUMMARY"))
        .bind(start)
        .bind(end)
        .bind(field(&event, "LOCATION"))
        .bind(field(&event, "DESCRIPTION"))
        .bind(field(&event, "STATUS"))
        .bind(rule)
        .bind(ical)
        .bind(field(&event, "RECURRENCE-ID"))
        .bind(tz)
        .bind(field(&event, "TRANSP"))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

fn parse_duration(value: &str) -> Result<Duration> {
    let invalid = || SyncFailure::new("unsupported_duration");
    let rest = value
        .strip_prefix('+')
        .unwrap_or(value)
        .strip_prefix('P')
        .ok_or_else(invalid)?;
    let (mut number, mut seconds, mut time, mut last, mut digits) = (0_i64, 0_i64, false, 0, false);
    for c in rest.chars() {
        if c.is_ascii_digit() {
            number = number
                .checked_mul(10)
                .and_then(|n| n.checked_add(c.to_digit(10).unwrap() as i64))
                .ok_or_else(invalid)?;
            digits = true;
        } else if c == 'T' && !time && !digits && last != 1 {
            time = true;
        } else {
            let (factor, order) = match (time, c) {
                (false, 'W') => (604800, 1),
                (false, 'D') => (86400, 2),
                (true, 'H') => (3600, 3),
                (true, 'M') => (60, 4),
                (true, 'S') => (1, 5),
                _ => return Err(invalid().into()),
            };
            if !digits || order <= last || last == 1 {
                return Err(invalid().into());
            }
            seconds = number
                .checked_mul(factor)
                .and_then(|n| seconds.checked_add(n))
                .ok_or_else(invalid)?;
            number = 0;
            digits = false;
            last = order;
        }
    }
    if last == 0 || digits || rest.ends_with('T') {
        return Err(invalid().into());
    }
    Duration::try_seconds(seconds).ok_or_else(|| invalid().into())
}

#[cfg(test)]
pub(crate) mod tests;
