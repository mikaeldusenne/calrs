use crate::sync_diagnostics::SyncFailure;
use chrono::{NaiveDateTime, TimeZone};

fn rule_set(
    start: NaiveDateTime,
    rule: &str,
    tz: chrono_tz::Tz,
) -> anyhow::Result<rrule::RRuleSet> {
    // Sub-daily recurrence from a decades-old DTSTART can consume unbounded CPU.
    // Fail explicitly, never interpret an unsupported rule as free time.
    if !rule.split(';').any(|p| {
        matches!(
            p,
            "FREQ=DAILY" | "FREQ=WEEKLY" | "FREQ=MONTHLY" | "FREQ=YEARLY"
        )
    }) {
        return Err(SyncFailure::new("unsupported_recurrence").into());
    }
    let rule = rule
        .split(';')
        .map(|part| {
            if let Some(until) = part.strip_prefix("UNTIL=").filter(|v| !v.ends_with('Z')) {
                let until = if until.len() == 8 {
                    format!("{until}T235959")
                } else {
                    until.to_owned()
                };
                crate::utils::parse_ical_datetime(&until)
                    .and_then(|d| tz.from_local_datetime(&d).latest())
                    .map(|d| {
                        format!(
                            "UNTIL={}",
                            d.with_timezone(&chrono::Utc).format("%Y%m%dT%H%M%SZ")
                        )
                    })
                    .unwrap_or_else(|| part.to_owned())
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(";");
    format!(
        "DTSTART;TZID={}:{}\nRRULE:{rule}",
        tz.name(),
        start.format("%Y%m%dT%H%M%S")
    )
    .parse()
    .map_err(|_| SyncFailure::new("invalid_recurrence").into())
}

pub(crate) fn validate(start: NaiveDateTime, rule: &str, tz: Option<&str>) -> anyhow::Result<()> {
    rule_set(
        start,
        rule,
        tz.and_then(crate::timezone::resolve_tzid)
            .unwrap_or(chrono_tz::UTC),
    )
    .map(|_| ())
}

/// Expand in event-local wall time; callers convert occurrences to the host TZ.
/// A safety limit/error blocks the requested window, NEVER silently truncates it.
pub fn expand_rrule(
    event_start: NaiveDateTime,
    event_end: NaiveDateTime,
    rrule_str: &str,
    exdates: &[NaiveDateTime],
    window_start: NaiveDateTime,
    window_end: NaiveDateTime,
) -> Vec<(NaiveDateTime, NaiveDateTime)> {
    expand_rrule_in_tz(
        event_start,
        event_end,
        rrule_str,
        exdates,
        window_start,
        window_end,
        chrono_tz::UTC,
    )
}

pub(crate) fn expand_rrule_in_tz(
    event_start: NaiveDateTime,
    event_end: NaiveDateTime,
    rrule_str: &str,
    exdates: &[NaiveDateTime],
    window_start: NaiveDateTime,
    window_end: NaiveDateTime,
    tz: chrono_tz::Tz,
) -> Vec<(NaiveDateTime, NaiveDateTime)> {
    if window_start >= window_end {
        return Vec::new();
    }
    let blocked = || vec![(window_start, window_end)];
    let duration = event_end - event_start;
    let Ok(set) = rule_set(event_start, rrule_str, tz) else {
        tracing::warn!(target: crate::sync_diagnostics::TARGET, error_kind = "invalid_recurrence", "availability blocked");
        return blocked();
    };
    let Some(after) = window_start.checked_sub_signed(duration) else {
        return blocked();
    };
    let zone = set.get_dt_start().timezone();
    let Some(after) = zone.from_local_datetime(&after).earliest() else {
        return blocked();
    };
    let Some(before) = zone.from_local_datetime(&window_end).latest() else {
        return blocked();
    };
    let result = set.after(after).before(before).all(10_000);
    if result.limited {
        tracing::warn!(target: crate::sync_diagnostics::TARGET, error_kind = "recurrence_limit", "availability blocked");
        return blocked();
    }
    result
        .dates
        .into_iter()
        .map(|d| d.naive_local())
        .filter(|d| *d < window_end && *d + duration > window_start)
        .filter(|d| !exdates.contains(d))
        .map(|d| (d, d + duration))
        .collect()
}

/// Parse EXDATE values and RECURRENCE-ID overrides from raw iCal.
/// Returns NaiveDateTimes that should be excluded from RRULE expansion.
/// Scans ALL VEVENTs in the resource: EXDATEs from the first (recurring) VEVENT,
/// and RECURRENCE-ID values from any override VEVENTs (modified instances).
pub fn extract_exdates(raw_ical: &str) -> Vec<NaiveDateTime> {
    extract_exdates_in_tz(raw_ical, None)
}

pub(crate) fn extract_exdates_in_tz(
    raw_ical: &str,
    event_zone: Option<chrono_tz::Tz>,
) -> Vec<NaiveDateTime> {
    let raw = crate::utils::unfold_ical(raw_ical);
    let mut dates = Vec::new();
    for line in raw.lines() {
        let property = if line.starts_with("EXDATE") {
            "EXDATE"
        } else if line.starts_with("RECURRENCE-ID") {
            "RECURRENCE-ID"
        } else {
            continue;
        };
        let rest = &line[property.len()..];
        if rest.is_empty() || !matches!(rest.as_bytes()[0], b';' | b':') {
            continue;
        }
        let Some((params, values)) = crate::timezone::params_and_value(rest) else {
            continue;
        };
        let source_zone = params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("TZID"))
            .map(|(_, v)| v.as_str())
            .map(str::to_string)
            .or_else(|| values.trim().ends_with('Z').then(|| "UTC".to_string()));
        for value in values.split(',') {
            if let Some(date) = crate::utils::parse_ical_datetime(value.trim()) {
                dates.push(match event_zone {
                    Some(zone) => {
                        crate::utils::convert_event_to_tz(date, source_zone.as_deref(), zone)
                    }
                    None => date,
                });
            }
        }
    }
    dates
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, NaiveDate};

    #[test]
    fn decades_old_daily_series_is_not_truncated() {
        let start = dt(2000, 1, 1, 9, 0);
        let from = dt(2026, 9, 7, 0, 0);
        let occurrences = expand_rrule(
            start,
            start + Duration::hours(1),
            "FREQ=DAILY",
            &[dt(2026, 9, 8, 9, 0)],
            from,
            from + Duration::days(3),
        );
        assert_eq!(
            occurrences,
            vec![
                (dt(2026, 9, 7, 9, 0), dt(2026, 9, 7, 10, 0)),
                (dt(2026, 9, 9, 9, 0), dt(2026, 9, 9, 10, 0))
            ]
        );
    }

    #[test]
    fn count_is_applied_before_exclusions() {
        let start = dt(2000, 1, 1, 9, 0);
        assert!(expand_rrule(
            start,
            start + Duration::hours(1),
            "FREQ=DAILY;COUNT=2",
            &[start],
            start + Duration::days(2),
            start + Duration::days(3)
        )
        .is_empty());
    }

    #[test]
    fn yearly_and_last_weekday_rules_are_supported() {
        let start = dt(2000, 1, 1, 9, 0);
        let from = dt(2026, 1, 1, 0, 0);
        assert_eq!(
            expand_rrule(
                start,
                start + Duration::hours(1),
                "FREQ=YEARLY",
                &[],
                from,
                from + Duration::days(1)
            ),
            vec![(dt(2026, 1, 1, 9, 0), dt(2026, 1, 1, 10, 0))]
        );
        assert_eq!(
            expand_rrule(
                start,
                start + Duration::hours(1),
                "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1",
                &[],
                from,
                dt(2026, 2, 1, 0, 0)
            )[0]
            .0,
            dt(2026, 1, 30, 9, 0)
        );
    }

    fn dt(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    #[test]
    fn test_weekly_monday() {
        let start = dt(2026, 2, 23, 16, 0);
        let end = dt(2026, 2, 23, 18, 0);
        let window_start = dt(2026, 3, 23, 0, 0);
        let window_end = dt(2026, 3, 24, 0, 0);

        let results = expand_rrule(
            start,
            end,
            "FREQ=WEEKLY;INTERVAL=1;BYDAY=MO",
            &[],
            window_start,
            window_end,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, dt(2026, 3, 23, 16, 0));
        assert_eq!(results[0].1, dt(2026, 3, 23, 18, 0));
    }

    #[test]
    fn test_weekly_with_until() {
        let start = dt(2026, 1, 5, 10, 0);
        let end = dt(2026, 1, 5, 11, 0);
        let window_start = dt(2026, 2, 1, 0, 0);
        let window_end = dt(2026, 3, 1, 0, 0);

        let results = expand_rrule(
            start,
            end,
            "FREQ=WEEKLY;UNTIL=20260209T100000;BYDAY=MO",
            &[],
            window_start,
            window_end,
        );
        assert_eq!(results.len(), 2); // Feb 2 and Feb 9
        assert_eq!(
            results[0].0.date(),
            NaiveDate::from_ymd_opt(2026, 2, 2).unwrap()
        );
        assert_eq!(
            results[1].0.date(),
            NaiveDate::from_ymd_opt(2026, 2, 9).unwrap()
        );
    }

    #[test]
    fn test_monthly_2nd_monday() {
        let start = dt(2026, 1, 12, 16, 0);
        let end = dt(2026, 1, 12, 17, 0);
        let window_start = dt(2026, 3, 1, 0, 0);
        let window_end = dt(2026, 4, 1, 0, 0);

        let results = expand_rrule(
            start,
            end,
            "FREQ=MONTHLY;INTERVAL=1;BYDAY=2MO",
            &[],
            window_start,
            window_end,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, dt(2026, 3, 9, 16, 0)); // 2nd Monday of March 2026
    }

    #[test]
    fn test_exdate_exclusion() {
        let start = dt(2026, 3, 2, 10, 0);
        let end = dt(2026, 3, 2, 11, 0);
        let window_start = dt(2026, 3, 1, 0, 0);
        let window_end = dt(2026, 3, 31, 0, 0);
        let exdates = vec![dt(2026, 3, 9, 10, 0)]; // exclude March 9

        let results = expand_rrule(
            start,
            end,
            "FREQ=WEEKLY;BYDAY=MO",
            &exdates,
            window_start,
            window_end,
        );
        let dates: Vec<_> = results.iter().map(|(s, _)| s.date()).collect();
        assert!(dates.contains(&NaiveDate::from_ymd_opt(2026, 3, 2).unwrap()));
        assert!(!dates.contains(&NaiveDate::from_ymd_opt(2026, 3, 9).unwrap())); // excluded
        assert!(dates.contains(&NaiveDate::from_ymd_opt(2026, 3, 16).unwrap()));
        assert!(dates.contains(&NaiveDate::from_ymd_opt(2026, 3, 23).unwrap()));
        assert!(dates.contains(&NaiveDate::from_ymd_opt(2026, 3, 30).unwrap()));
    }

    #[test]
    fn test_extract_exdates() {
        let ical = "BEGIN:VEVENT\nDTSTART:20260302T100000\nRRULE:FREQ=WEEKLY;BYDAY=MO\nEXDATE:20260309T100000\nEXDATE:20260316T100000\nEND:VEVENT";
        let exdates = extract_exdates(ical);
        assert_eq!(exdates.len(), 2);
        assert_eq!(exdates[0], dt(2026, 3, 9, 10, 0));
        assert_eq!(exdates[1], dt(2026, 3, 16, 10, 0));
    }

    #[test]
    fn test_weekly_count() {
        let start = dt(2026, 3, 2, 9, 0);
        let end = dt(2026, 3, 2, 10, 0);
        let window_start = dt(2026, 3, 1, 0, 0);
        let window_end = dt(2026, 12, 31, 0, 0);

        let results = expand_rrule(
            start,
            end,
            "FREQ=WEEKLY;COUNT=3;BYDAY=MO",
            &[],
            window_start,
            window_end,
        );
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_recurrence_id_exclusion() {
        // A recurring event with a modified instance (RECURRENCE-ID)
        let ical = "BEGIN:VCALENDAR\n\
            BEGIN:VEVENT\n\
            UID:abc\n\
            DTSTART:20260302T100000\n\
            DTEND:20260302T110000\n\
            RRULE:FREQ=WEEKLY;BYDAY=MO\n\
            END:VEVENT\n\
            BEGIN:VEVENT\n\
            UID:abc\n\
            RECURRENCE-ID:20260309T100000\n\
            DTSTART:20260309T140000\n\
            DTEND:20260309T150000\n\
            END:VEVENT\n\
            END:VCALENDAR";
        let exdates = extract_exdates(ical);
        // The RECURRENCE-ID should be treated as an exclusion
        assert_eq!(exdates.len(), 1);
        assert_eq!(exdates[0], dt(2026, 3, 9, 10, 0));

        // The original March 9 occurrence should be excluded from expansion
        let start = dt(2026, 3, 2, 10, 0);
        let end = dt(2026, 3, 2, 11, 0);
        let window_start = dt(2026, 3, 1, 0, 0);
        let window_end = dt(2026, 3, 31, 0, 0);
        let results = expand_rrule(
            start,
            end,
            "FREQ=WEEKLY;BYDAY=MO",
            &exdates,
            window_start,
            window_end,
        );
        let dates: Vec<_> = results.iter().map(|(s, _)| s.date()).collect();
        assert!(dates.contains(&NaiveDate::from_ymd_opt(2026, 3, 2).unwrap()));
        assert!(!dates.contains(&NaiveDate::from_ymd_opt(2026, 3, 9).unwrap())); // excluded by RECURRENCE-ID
        assert!(dates.contains(&NaiveDate::from_ymd_opt(2026, 3, 16).unwrap()));
    }

    #[test]
    fn test_weekly_multi_byday_devops() {
        // Real case: DevOps tools daily, WEEKLY on MO,TU,TH,FR
        // Starts 2025-10-20 (Monday), UNTIL 2026-03-15
        let start = dt(2025, 10, 20, 10, 30);
        let end = dt(2025, 10, 20, 11, 0);
        let window_start = dt(2026, 3, 10, 0, 0);
        let window_end = dt(2026, 3, 10, 23, 59);

        let results = expand_rrule(
            start,
            end,
            "FREQ=WEEKLY;UNTIL=20260315T093000;INTERVAL=1;BYDAY=MO,TU,TH,FR",
            &[],
            window_start,
            window_end,
        );
        // March 10 is a Tuesday — should have one occurrence at 10:30
        assert!(
            !results.is_empty(),
            "Expected occurrence on Tuesday March 10"
        );
        assert_eq!(results[0].0, dt(2026, 3, 10, 10, 30));
        assert_eq!(results[0].1, dt(2026, 3, 10, 11, 0));
    }

    #[test]
    fn test_daily_count_pre_window() {
        // Daily event with COUNT=3 starting before the window
        let start = dt(2026, 3, 1, 9, 0);
        let end = dt(2026, 3, 1, 10, 0);
        let window_start = dt(2026, 3, 3, 0, 0); // window starts after 2 occurrences
        let window_end = dt(2026, 3, 10, 0, 0);

        let results = expand_rrule(
            start,
            end,
            "FREQ=DAILY;COUNT=3",
            &[],
            window_start,
            window_end,
        );
        // COUNT=3: Mar 1, Mar 2, Mar 3 — only Mar 3 is in window
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, dt(2026, 3, 3, 9, 0));
    }

    // --- Additional edge case tests ---

    #[test]
    fn test_daily_interval_2() {
        let start = dt(2026, 3, 1, 9, 0);
        let end = dt(2026, 3, 1, 10, 0);
        let window_start = dt(2026, 3, 1, 0, 0);
        let window_end = dt(2026, 3, 8, 0, 0);

        let results = expand_rrule(
            start,
            end,
            "FREQ=DAILY;INTERVAL=2",
            &[],
            window_start,
            window_end,
        );
        // Mar 1, 3, 5, 7
        assert_eq!(results.len(), 4);
        assert_eq!(
            results[0].0.date(),
            NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()
        );
        assert_eq!(
            results[1].0.date(),
            NaiveDate::from_ymd_opt(2026, 3, 3).unwrap()
        );
        assert_eq!(
            results[2].0.date(),
            NaiveDate::from_ymd_opt(2026, 3, 5).unwrap()
        );
        assert_eq!(
            results[3].0.date(),
            NaiveDate::from_ymd_opt(2026, 3, 7).unwrap()
        );
    }

    #[test]
    fn test_weekly_interval_2() {
        let start = dt(2026, 3, 2, 10, 0); // Monday
        let end = dt(2026, 3, 2, 11, 0);
        let window_start = dt(2026, 3, 1, 0, 0);
        let window_end = dt(2026, 4, 1, 0, 0);

        let results = expand_rrule(
            start,
            end,
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO",
            &[],
            window_start,
            window_end,
        );
        // Mar 2, Mar 16, Mar 30
        assert_eq!(results.len(), 3);
        assert_eq!(
            results[0].0.date(),
            NaiveDate::from_ymd_opt(2026, 3, 2).unwrap()
        );
        assert_eq!(
            results[1].0.date(),
            NaiveDate::from_ymd_opt(2026, 3, 16).unwrap()
        );
        assert_eq!(
            results[2].0.date(),
            NaiveDate::from_ymd_opt(2026, 3, 30).unwrap()
        );
    }

    #[test]
    fn test_monthly_last_friday() {
        let start = dt(2026, 1, 30, 15, 0); // Last Friday of Jan
        let end = dt(2026, 1, 30, 16, 0);
        let window_start = dt(2026, 2, 1, 0, 0);
        let window_end = dt(2026, 5, 1, 0, 0);

        let results = expand_rrule(
            start,
            end,
            "FREQ=MONTHLY;BYDAY=-1FR",
            &[],
            window_start,
            window_end,
        );
        // Last Friday: Feb 27, Mar 27, Apr 24
        assert_eq!(results.len(), 3);
        assert_eq!(
            results[0].0.date(),
            NaiveDate::from_ymd_opt(2026, 2, 27).unwrap()
        );
        assert_eq!(
            results[1].0.date(),
            NaiveDate::from_ymd_opt(2026, 3, 27).unwrap()
        );
        assert_eq!(
            results[2].0.date(),
            NaiveDate::from_ymd_opt(2026, 4, 24).unwrap()
        );
    }

    #[test]
    fn test_monthly_same_day() {
        // Monthly on the 15th
        let start = dt(2026, 1, 15, 9, 0);
        let end = dt(2026, 1, 15, 10, 0);
        let window_start = dt(2026, 3, 1, 0, 0);
        let window_end = dt(2026, 6, 1, 0, 0);

        let results = expand_rrule(
            start,
            end,
            "FREQ=MONTHLY;INTERVAL=1",
            &[],
            window_start,
            window_end,
        );
        assert_eq!(results.len(), 3); // Mar 15, Apr 15, May 15
        assert_eq!(
            results[0].0.date(),
            NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()
        );
        assert_eq!(
            results[1].0.date(),
            NaiveDate::from_ymd_opt(2026, 4, 15).unwrap()
        );
        assert_eq!(
            results[2].0.date(),
            NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()
        );
    }

    #[test]
    fn test_monthly_feb_30_skipped() {
        // Monthly on the 31st — Feb has no 31st
        let start = dt(2026, 1, 31, 9, 0);
        let end = dt(2026, 1, 31, 10, 0);
        let window_start = dt(2026, 2, 1, 0, 0);
        let window_end = dt(2026, 4, 1, 0, 0);

        let results = expand_rrule(
            start,
            end,
            "FREQ=MONTHLY;INTERVAL=1",
            &[],
            window_start,
            window_end,
        );
        // Feb 31 doesn't exist → skipped; Mar 31 exists
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].0.date(),
            NaiveDate::from_ymd_opt(2026, 3, 31).unwrap()
        );
    }

    #[test]
    fn test_invalid_rrule() {
        let start = dt(2026, 3, 1, 9, 0);
        let end = dt(2026, 3, 1, 10, 0);
        let results = expand_rrule(start, end, "FREQ=INVALID", &[], start, end);
        assert_eq!(results, vec![(start, end)]); // Unknown is not free.
    }

    #[test]
    fn test_empty_rrule() {
        let start = dt(2026, 3, 1, 9, 0);
        let end = dt(2026, 3, 1, 10, 0);
        let results = expand_rrule(start, end, "", &[], start, end);
        assert_eq!(results, vec![(start, end)]);
    }

    // --- extract_exdates with multiple formats ---

    #[test]
    fn test_extract_exdates_with_timezone() {
        let ical = "BEGIN:VEVENT\nDTSTART:20260302T100000\nRRULE:FREQ=WEEKLY\nEXDATE;TZID=Europe/Paris:20260309T100000\nEND:VEVENT";
        let exdates = extract_exdates(ical);
        assert_eq!(exdates.len(), 1);
        assert_eq!(exdates[0], dt(2026, 3, 9, 10, 0));
    }

    #[test]
    fn test_extract_exdates_comma_separated() {
        let ical =
            "BEGIN:VEVENT\nEXDATE:20260309T100000,20260316T100000,20260323T100000\nEND:VEVENT";
        let exdates = extract_exdates(ical);
        assert_eq!(exdates.len(), 3);
    }
}
