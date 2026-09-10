//! Interpret the observances carried by VTIMEZONE; never guess an IANA zone.
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::utils::{extract_vevent_field as field, parse_ical_datetime};

type Result<T> = std::result::Result<T, &'static str>;
const INVALID: &str = "unsupported_timezone";
const LIMIT: u16 = 8192;

#[derive(Debug, Serialize, Deserialize)]
struct Observance {
    start: NaiveDateTime,
    from: i32,
    to: i32,
    rules: Vec<String>,
    dates: Vec<NaiveDateTime>,
}

#[derive(Debug, Clone)]
struct Transition {
    utc: NaiveDateTime,
    from: i32,
    to: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CustomZone {
    observances: Vec<Observance>,
    #[serde(skip)]
    cache: Mutex<HashMap<i32, Arc<Vec<Transition>>>>,
}

impl CustomZone {
    pub(super) fn parse(block: &str) -> Result<Self> {
        let mut observances = Vec::new();
        let mut current = None;
        for line in block.lines() {
            match line {
                "BEGIN:STANDARD" | "BEGIN:DAYLIGHT" => {
                    if current.is_some() || observances.len() >= 128 {
                        return Err(INVALID);
                    }
                    current = Some((line.trim_start_matches("BEGIN:"), String::new()));
                }
                "END:STANDARD" | "END:DAYLIGHT" => {
                    let (kind, data) = current.take().ok_or(INVALID)?;
                    if line.trim_start_matches("END:") != kind {
                        return Err(INVALID);
                    }
                    let start = field(&data, "DTSTART")
                        .and_then(|s| local_date(&s))
                        .ok_or(INVALID)?;
                    let from = offset(&field(&data, "TZOFFSETFROM").ok_or(INVALID)?)?;
                    let to = offset(&field(&data, "TZOFFSETTO").ok_or(INVALID)?)?;
                    let mut rules = Vec::new();
                    let mut dates = Vec::new();
                    for line in data.lines() {
                        if let Some(rest) = line
                            .strip_prefix("RRULE")
                            .filter(|s| s.starts_with([';', ':']))
                        {
                            if !rules.is_empty() {
                                return Err(INVALID);
                            }
                            let (_, rule) = super::params_and_value(rest).ok_or(INVALID)?;
                            // Observance UNTIL is UTC, whereas DTSTART is civil
                            // time under TZOFFSETFROM (RFC 5545 §3.6.5).
                            let rule = rule
                                .split(';')
                                .map(|part| {
                                    if let Some(until) =
                                        part.strip_prefix("UNTIL=").filter(|s| s.ends_with('Z'))
                                    {
                                        let local = parse_ical_datetime(until)
                                            .and_then(|d| {
                                                d.checked_add_signed(Duration::seconds(from.into()))
                                            })
                                            .ok_or(INVALID)?;
                                        Ok(format!("UNTIL={}Z", local.format("%Y%m%dT%H%M%S")))
                                    } else {
                                        Ok(part.to_string())
                                    }
                                })
                                .collect::<Result<Vec<_>>>()?
                                .join(";");
                            civil_rule(start, &rule)?;
                            rules.push(rule);
                        } else if let Some(rest) = line
                            .strip_prefix("RDATE")
                            .filter(|s| s.starts_with([';', ':']))
                        {
                            let (_, values) = super::params_and_value(rest).ok_or(INVALID)?;
                            for value in values.split(',') {
                                if dates.len() >= usize::from(LIMIT) {
                                    return Err(INVALID);
                                }
                                dates.push(local_date(value.trim()).ok_or(INVALID)?);
                            }
                        }
                    }
                    observances.push(Observance {
                        start,
                        from,
                        to,
                        rules,
                        dates,
                    });
                }
                _ => {
                    if let Some((_, data)) = &mut current {
                        data.push_str(line);
                        data.push('\n');
                    }
                }
            }
        }
        if current.is_some() || observances.is_empty() {
            return Err(INVALID);
        }
        Ok(Self {
            observances,
            cache: Mutex::default(),
        })
    }

    fn transitions(&self, year: i32) -> Result<Arc<Vec<Transition>>> {
        if let Some(cached) = self.cache.lock().map_err(|_| INVALID)?.get(&year).cloned() {
            return Ok(cached);
        }
        let until = NaiveDate::from_ymd_opt(year.checked_add(2).ok_or(INVALID)?, 1, 1)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .ok_or(INVALID)?;
        let mut transitions = Vec::new();
        for obs in &self.observances {
            let mut dates = vec![obs.start];
            dates.extend(obs.dates.iter().copied().filter(|d| *d < until));
            for rule in &obs.rules {
                let set = civil_rule(obs.start, rule)?;
                let end = until.and_utc().with_timezone(&rrule::Tz::UTC);
                let result = set.before(end).all(LIMIT);
                if result.limited {
                    return Err(INVALID);
                }
                dates.extend(result.dates.into_iter().map(|d| d.naive_utc()));
            }
            for date in dates {
                let utc = date
                    .checked_sub_signed(Duration::seconds(obs.from.into()))
                    .ok_or(INVALID)?;
                transitions.push(Transition {
                    utc,
                    from: obs.from,
                    to: obs.to,
                });
                if transitions.len() > usize::from(LIMIT) {
                    return Err(INVALID);
                }
            }
        }
        transitions.sort_by_key(|t| t.utc);
        transitions.dedup_by(|a, b| a.utc == b.utc && a.from == b.from && a.to == b.to);
        let transitions = Arc::new(transitions);
        let mut cache = self.cache.lock().map_err(|_| INVALID)?;
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(year, transitions.clone());
        Ok(transitions)
    }

    fn offset_at(transitions: &[Transition], utc: NaiveDateTime) -> Result<i32> {
        let index = transitions.partition_point(|t| t.utc <= utc);
        let active = index.saturating_sub(1);
        let transition = transitions.get(active).ok_or(INVALID)?;
        // RFC 5545 §3.6.5: the latest onset determines the offset. Exchange
        // may emit conflicting epoch seeds (1601-01-01); they must not poison
        // dates governed by later, unambiguous transitions. Never pick a side
        // while a conflict is active, including before the first onset.
        if (active > 0 && transitions[active - 1].utc == transition.utc)
            || transitions
                .get(active + 1)
                .is_some_and(|next| next.utc == transition.utc)
        {
            return Err(INVALID);
        }
        Ok(if index == 0 {
            transition.from
        } else {
            transition.to
        })
    }

    pub(super) fn to_local(&self, utc: NaiveDateTime) -> Result<NaiveDateTime> {
        let transitions = self.transitions(utc.year())?;
        utc.checked_add_signed(Duration::seconds(
            Self::offset_at(&transitions, utc)?.into(),
        ))
        .ok_or(INVALID)
    }

    /// Return the absolute instant and whether this civil time exists.
    /// Folds use the first occurrence; gaps use the pre-transition offset for
    /// explicit dates. RRULE callers discard gaps (RFC 5545 §3.3.5/§3.3.10).
    pub(super) fn local_instant(&self, local: NaiveDateTime) -> Result<(NaiveDateTime, bool)> {
        let transitions = self.transitions(local.year())?;
        let mut candidates = Vec::new();
        for offset in self.observances.iter().flat_map(|o| [o.from, o.to]) {
            let utc = local
                .checked_sub_signed(Duration::seconds(offset.into()))
                .ok_or(INVALID)?;
            if Self::offset_at(&transitions, utc)? == offset {
                candidates.push(utc);
            }
        }
        if let Some(utc) = candidates.into_iter().min() {
            return Ok((utc, true));
        }
        for transition in transitions.iter().filter(|t| t.to > t.from) {
            let before = transition
                .utc
                .checked_add_signed(Duration::seconds(transition.from.into()))
                .ok_or(INVALID)?;
            let after = transition
                .utc
                .checked_add_signed(Duration::seconds(transition.to.into()))
                .ok_or(INVALID)?;
            if before <= local && local < after {
                // Gap handling must not bypass the active-conflict check.
                Self::offset_at(&transitions, transition.utc)?;
                return Ok((
                    local
                        .checked_sub_signed(Duration::seconds(transition.from.into()))
                        .ok_or(INVALID)?,
                    false,
                ));
            }
        }
        Err(INVALID)
    }
}

fn local_date(value: &str) -> Option<NaiveDateTime> {
    (value.len() == 15)
        .then(|| parse_ical_datetime(value))
        .flatten()
}

fn civil_rule(start: NaiveDateTime, rule: &str) -> Result<rrule::RRuleSet> {
    format!("DTSTART:{}Z\nRRULE:{rule}", start.format("%Y%m%dT%H%M%S"))
        .parse()
        .map_err(|_| INVALID)
}

fn offset(value: &str) -> Result<i32> {
    let bytes = value.as_bytes();
    if !matches!(bytes.len(), 5 | 7)
        || !matches!(bytes[0], b'+' | b'-')
        || !bytes[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(INVALID);
    }
    let part = |i| ((bytes[i] - b'0') as i32) * 10 + (bytes[i + 1] - b'0') as i32;
    let (hours, minutes, seconds) = (part(1), part(3), if bytes.len() == 7 { part(5) } else { 0 });
    if hours > 23 || minutes > 59 || seconds > 59 {
        return Err(INVALID);
    }
    Ok((hours * 3600 + minutes * 60 + seconds) * if bytes[0] == b'-' { -1 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const PARIS: &str = "BEGIN:VTIMEZONE\nTZID:Outlook Custom\nBEGIN:STANDARD\nDTSTART:16011028T030000\nTZOFFSETFROM:+0200\nTZOFFSETTO:+0100\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\nEND:STANDARD\nBEGIN:DAYLIGHT\nDTSTART:16010325T020000\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0200\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\nEND:DAYLIGHT\nEND:VTIMEZONE";

    fn dt(value: &str) -> NaiveDateTime {
        parse_ical_datetime(value).unwrap()
    }

    const DAVMAIL: &str = include_str!("../../tests/fixtures/davmail-custom-timezone.ics");

    #[test]
    fn davmail_epoch_collision_does_not_poison_later_dst_rules() {
        let zone = CustomZone::parse(DAVMAIL).unwrap();
        for (local, utc, exists) in [
            ("20260115T100000", "20260115T090000", true),
            ("20260715T100000", "20260715T080000", true),
            ("20260329T023000", "20260329T013000", false),
            ("20261025T023000", "20261025T003000", true),
        ] {
            assert_eq!(zone.local_instant(dt(local)), Ok((dt(utc), exists)));
            if exists {
                assert_eq!(zone.to_local(dt(utc)), Ok(dt(local)));
            }
        }
        assert_eq!(
            zone.to_local(dt("20261025T013000")),
            Ok(dt("20261025T023000"))
        );
        // Do not silently choose a side when the conflicting seeds are active.
        for utc in ["16001231T000000", "16010101T010000", "16010201T120000"] {
            assert_eq!(zone.to_local(dt(utc)), Err(INVALID));
        }
        assert_eq!(zone.local_instant(dt("16010201T120000")), Err(INVALID));
    }

    #[test]
    fn conflicting_transitions_block_only_their_effective_interval() {
        let initial = "BEGIN:STANDARD\nDTSTART:20200101T000000\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0100\nEND:STANDARD\n";
        let first = "BEGIN:DAYLIGHT\nDTSTART:20260329T020000\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0200\nEND:DAYLIGHT\n";
        let second = "BEGIN:DAYLIGHT\nDTSTART:20260329T020000\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0300\nEND:DAYLIGHT\n";
        let recovery = "BEGIN:STANDARD\nDTSTART:20261025T030000\nTZOFFSETFROM:+0200\nTZOFFSETTO:+0100\nEND:STANDARD\n";
        for conflicts in [format!("{first}{second}"), format!("{second}{first}")] {
            let zone = CustomZone::parse(&format!("{initial}{conflicts}{recovery}")).unwrap();
            assert_eq!(
                zone.to_local(dt("20260115T090000")),
                Ok(dt("20260115T100000"))
            );
            for utc in ["20260329T010000", "20260715T080000"] {
                assert_eq!(zone.to_local(dt(utc)), Err(INVALID));
            }
            for local in ["20260329T023000", "20260329T033000", "20260715T100000"] {
                assert_eq!(zone.local_instant(dt(local)), Err(INVALID));
            }
            assert_eq!(
                zone.to_local(dt("20261025T010000")),
                Ok(dt("20261025T020000"))
            );
            assert_eq!(
                zone.local_instant(dt("20261115T100000")),
                Ok((dt("20261115T090000"), true))
            );
            // A series starting before the conflict must not expose free slots
            // when an occurrence later falls inside the ambiguous interval.
            let stored = crate::timezone::Zone::Custom(Arc::new(zone))
                .stored_name()
                .unwrap();
            let recurring = [(
                "20260115T100000".into(),
                "20260115T110000".into(),
                "FREQ=DAILY".into(),
                None,
                Some(stored),
            )];
            let (start, end) = (dt("20260715T000000"), dt("20260716T000000"));
            assert_eq!(
                crate::web::expand_recurring_into_busy(&recurring, start, end, chrono_tz::UTC),
                vec![(start, end)]
            );
        }
    }

    #[test]
    fn duplicate_observances_are_not_conflicts() {
        let zone = CustomZone::parse(&format!("{DAVMAIL}\n{DAVMAIL}")).unwrap();
        assert_eq!(
            zone.local_instant(dt("20260715T100000")),
            Ok((dt("20260715T080000"), true))
        );
    }

    #[test]
    fn opaque_zone_uses_rules_for_winter_summer_gaps_and_folds() {
        let zone = CustomZone::parse(PARIS).unwrap();
        for (local, utc, exists) in [
            ("20260115T100000", "20260115T090000", true),
            ("20260715T100000", "20260715T080000", true),
            ("20260329T023000", "20260329T013000", false),
            ("20261025T023000", "20261025T003000", true),
        ] {
            assert_eq!(zone.local_instant(dt(local)).unwrap(), (dt(utc), exists));
        }
        assert_eq!(
            zone.to_local(dt("20261025T013000")).unwrap(),
            dt("20261025T023000")
        );
    }

    #[test]
    fn fixed_offsets_support_minutes_seconds_and_dates_before_first_observance() {
        let zone = CustomZone::parse("BEGIN:STANDARD\nDTSTART:20000101T000000\nTZOFFSETFROM:+053045\nTZOFFSETTO:+053045\nEND:STANDARD").unwrap();
        assert_eq!(
            zone.local_instant(dt("19991215T100000")).unwrap(),
            (dt("19991215T042915"), true)
        );
        assert_eq!(
            zone.to_local(dt("20260715T000000")).unwrap(),
            dt("20260715T053045")
        );
    }

    #[test]
    fn southern_hemisphere_and_half_hour_dst_are_not_approximated() {
        let zone = CustomZone::parse("BEGIN:STANDARD\nDTSTART:20000402T020000\nTZOFFSETFROM:+1100\nTZOFFSETTO:+1030\nRRULE:FREQ=YEARLY;BYMONTH=4;BYDAY=1SU\nEND:STANDARD\nBEGIN:DAYLIGHT\nDTSTART:20001001T020000\nTZOFFSETFROM:+1030\nTZOFFSETTO:+1100\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=1SU\nEND:DAYLIGHT").unwrap();
        for (local, utc, exists) in [
            ("20260115T100000", "20260114T230000", true),
            ("20260715T100000", "20260714T233000", true),
            ("20260405T014500", "20260404T144500", true),
            ("20261004T021500", "20261003T154500", false),
        ] {
            assert_eq!(zone.local_instant(dt(local)).unwrap(), (dt(utc), exists));
        }
    }

    #[test]
    fn rdate_and_utc_until_are_used_for_observance_transitions() {
        let zone = CustomZone::parse("BEGIN:STANDARD\nDTSTART:20200101T000000\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0200\nRRULE:FREQ=YEARLY;UNTIL=20211231T230000Z\nRDATE:20240101T000000\nEND:STANDARD\nBEGIN:DAYLIGHT\nDTSTART:20200601T000000\nTZOFFSETFROM:+0200\nTZOFFSETTO:+0100\nRRULE:FREQ=YEARLY\nEND:DAYLIGHT").unwrap();
        assert_eq!(
            zone.to_local(dt("20220101T000000")).unwrap(),
            dt("20220101T020000")
        );
        assert_eq!(
            zone.to_local(dt("20230101T000000")).unwrap(),
            dt("20230101T010000")
        );
        assert_eq!(
            zone.to_local(dt("20240101T000000")).unwrap(),
            dt("20240101T020000")
        );
    }

    #[test]
    fn custom_recurrence_counts_real_instances_and_compares_until_in_utc() {
        for definition in [PARIS, DAVMAIL] {
            let zone =
                crate::timezone::Zone::Custom(Arc::new(CustomZone::parse(definition).unwrap()));
            let window = (dt("20260328T000000"), dt("20260401T000000"));
            let busy = crate::rrule::expand_custom(
                dt("20260328T023000"),
                dt("20260328T033000"),
                "FREQ=DAILY;COUNT=2",
                &[],
                window,
                &zone,
            )
            .unwrap();
            assert_eq!(
                busy,
                vec![
                    (dt("20260328T013000"), dt("20260328T023000")),
                    (dt("20260330T003000"), dt("20260330T013000"))
                ]
            );
            let busy = crate::rrule::expand_custom(
                dt("20000101T100000"),
                dt("20000101T110000"),
                "FREQ=DAILY;UNTIL=20260329T080000Z",
                &[dt("20260328T100000")],
                window,
                &zone,
            )
            .unwrap();
            assert_eq!(busy, vec![(dt("20260329T080000"), dt("20260329T090000"))]);
        }
    }

    #[test]
    fn incomplete_or_invalid_definitions_are_rejected() {
        for data in ["TZID:Opaque", "BEGIN:STANDARD\nDTSTART:20200101T000000\nTZOFFSETFROM:+0100\nEND:STANDARD",
            "BEGIN:STANDARD\nDTSTART:20200101T000000\nTZOFFSETFROM:+0100\nTZOFFSETTO:+2500\nEND:STANDARD",
            "BEGIN:STANDARD\nDTSTART:20200101T000000Z\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0200\nEND:STANDARD",
            "BEGIN:STANDARD\nDTSTART:20200101T000000\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0200\nEND:DAYLIGHT"] {
            assert!(CustomZone::parse(data).is_err());
        }
    }
}
