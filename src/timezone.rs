//! Resolve iCalendar TZIDs that Exchange/DavMail emit (Windows names, Microsoft
//! Olson URIs, VTIMEZONE ids) into chrono-tz zones.

use chrono_tz::Tz;
use std::collections::HashMap;

use crate::utils::{parse_ical_datetime, unfold_ical};

/// CLDR windowsZones territory 001 (Unicode CLDR 48).
const WINDOWS_ZONES: &[(&str, &str)] = &[
    ("Afghanistan Standard Time", "Asia/Kabul"),
    ("Alaskan Standard Time", "America/Anchorage"),
    ("Aleutian Standard Time", "America/Adak"),
    ("Altai Standard Time", "Asia/Barnaul"),
    ("Arab Standard Time", "Asia/Riyadh"),
    ("Arabian Standard Time", "Asia/Dubai"),
    ("Arabic Standard Time", "Asia/Baghdad"),
    ("Argentina Standard Time", "America/Buenos_Aires"),
    ("Astrakhan Standard Time", "Europe/Astrakhan"),
    ("Atlantic Standard Time", "America/Halifax"),
    ("AUS Central Standard Time", "Australia/Darwin"),
    ("Aus Central W. Standard Time", "Australia/Eucla"),
    ("AUS Eastern Standard Time", "Australia/Sydney"),
    ("Azerbaijan Standard Time", "Asia/Baku"),
    ("Azores Standard Time", "Atlantic/Azores"),
    ("Bahia Standard Time", "America/Bahia"),
    ("Bangladesh Standard Time", "Asia/Dhaka"),
    ("Belarus Standard Time", "Europe/Minsk"),
    ("Bougainville Standard Time", "Pacific/Bougainville"),
    ("Canada Central Standard Time", "America/Regina"),
    ("Cape Verde Standard Time", "Atlantic/Cape_Verde"),
    ("Caucasus Standard Time", "Asia/Yerevan"),
    ("Cen. Australia Standard Time", "Australia/Adelaide"),
    ("Central America Standard Time", "America/Guatemala"),
    ("Central Asia Standard Time", "Asia/Bishkek"),
    ("Central Brazilian Standard Time", "America/Cuiaba"),
    ("Central Europe Standard Time", "Europe/Budapest"),
    ("Central European Standard Time", "Europe/Warsaw"),
    ("Central Pacific Standard Time", "Pacific/Guadalcanal"),
    ("Central Standard Time", "America/Chicago"),
    ("Central Standard Time (Mexico)", "America/Mexico_City"),
    ("Chatham Islands Standard Time", "Pacific/Chatham"),
    ("China Standard Time", "Asia/Shanghai"),
    ("Cuba Standard Time", "America/Havana"),
    ("Dateline Standard Time", "Etc/GMT+12"),
    ("E. Africa Standard Time", "Africa/Nairobi"),
    ("E. Australia Standard Time", "Australia/Brisbane"),
    ("E. Europe Standard Time", "Europe/Chisinau"),
    ("E. South America Standard Time", "America/Sao_Paulo"),
    ("Easter Island Standard Time", "Pacific/Easter"),
    ("Eastern Standard Time", "America/New_York"),
    ("Eastern Standard Time (Mexico)", "America/Cancun"),
    ("Egypt Standard Time", "Africa/Cairo"),
    ("Ekaterinburg Standard Time", "Asia/Yekaterinburg"),
    ("Fiji Standard Time", "Pacific/Fiji"),
    ("FLE Standard Time", "Europe/Kiev"),
    ("Georgian Standard Time", "Asia/Tbilisi"),
    ("GMT Standard Time", "Europe/London"),
    ("Greenland Standard Time", "America/Godthab"),
    ("Greenwich Standard Time", "Atlantic/Reykjavik"),
    ("GTB Standard Time", "Europe/Bucharest"),
    ("Haiti Standard Time", "America/Port-au-Prince"),
    ("Hawaiian Standard Time", "Pacific/Honolulu"),
    ("India Standard Time", "Asia/Calcutta"),
    ("Iran Standard Time", "Asia/Tehran"),
    ("Israel Standard Time", "Asia/Jerusalem"),
    ("Jordan Standard Time", "Asia/Amman"),
    ("Kaliningrad Standard Time", "Europe/Kaliningrad"),
    ("Korea Standard Time", "Asia/Seoul"),
    ("Libya Standard Time", "Africa/Tripoli"),
    ("Line Islands Standard Time", "Pacific/Kiritimati"),
    ("Lord Howe Standard Time", "Australia/Lord_Howe"),
    ("Magadan Standard Time", "Asia/Magadan"),
    ("Magallanes Standard Time", "America/Punta_Arenas"),
    ("Marquesas Standard Time", "Pacific/Marquesas"),
    ("Mauritius Standard Time", "Indian/Mauritius"),
    ("Middle East Standard Time", "Asia/Beirut"),
    ("Montevideo Standard Time", "America/Montevideo"),
    ("Morocco Standard Time", "Africa/Casablanca"),
    ("Mountain Standard Time", "America/Denver"),
    ("Mountain Standard Time (Mexico)", "America/Mazatlan"),
    ("Myanmar Standard Time", "Asia/Rangoon"),
    ("N. Central Asia Standard Time", "Asia/Novosibirsk"),
    ("Namibia Standard Time", "Africa/Windhoek"),
    ("Nepal Standard Time", "Asia/Katmandu"),
    ("New Zealand Standard Time", "Pacific/Auckland"),
    ("Newfoundland Standard Time", "America/St_Johns"),
    ("Norfolk Standard Time", "Pacific/Norfolk"),
    ("North Asia East Standard Time", "Asia/Irkutsk"),
    ("North Asia Standard Time", "Asia/Krasnoyarsk"),
    ("North Korea Standard Time", "Asia/Pyongyang"),
    ("Omsk Standard Time", "Asia/Omsk"),
    ("Pacific SA Standard Time", "America/Santiago"),
    ("Pacific Standard Time", "America/Los_Angeles"),
    ("Pacific Standard Time (Mexico)", "America/Tijuana"),
    ("Pakistan Standard Time", "Asia/Karachi"),
    ("Paraguay Standard Time", "America/Asuncion"),
    ("Qyzylorda Standard Time", "Asia/Qyzylorda"),
    ("Romance Standard Time", "Europe/Paris"),
    ("Russia Time Zone 10", "Asia/Srednekolymsk"),
    ("Russia Time Zone 11", "Asia/Kamchatka"),
    ("Russia Time Zone 3", "Europe/Samara"),
    ("Russian Standard Time", "Europe/Moscow"),
    ("SA Eastern Standard Time", "America/Cayenne"),
    ("SA Pacific Standard Time", "America/Bogota"),
    ("SA Western Standard Time", "America/La_Paz"),
    ("Saint Pierre Standard Time", "America/Miquelon"),
    ("Sakhalin Standard Time", "Asia/Sakhalin"),
    ("Samoa Standard Time", "Pacific/Apia"),
    ("Sao Tome Standard Time", "Africa/Sao_Tome"),
    ("Saratov Standard Time", "Europe/Saratov"),
    ("SE Asia Standard Time", "Asia/Bangkok"),
    ("Singapore Standard Time", "Asia/Singapore"),
    ("South Africa Standard Time", "Africa/Johannesburg"),
    ("South Sudan Standard Time", "Africa/Juba"),
    ("Sri Lanka Standard Time", "Asia/Colombo"),
    ("Sudan Standard Time", "Africa/Khartoum"),
    ("Syria Standard Time", "Asia/Damascus"),
    ("Taipei Standard Time", "Asia/Taipei"),
    ("Tasmania Standard Time", "Australia/Hobart"),
    ("Tocantins Standard Time", "America/Araguaina"),
    ("Tokyo Standard Time", "Asia/Tokyo"),
    ("Tomsk Standard Time", "Asia/Tomsk"),
    ("Tonga Standard Time", "Pacific/Tongatapu"),
    ("Transbaikal Standard Time", "Asia/Chita"),
    ("Turkey Standard Time", "Europe/Istanbul"),
    ("Turks And Caicos Standard Time", "America/Grand_Turk"),
    ("Ulaanbaatar Standard Time", "Asia/Ulaanbaatar"),
    ("US Eastern Standard Time", "America/Indianapolis"),
    ("US Mountain Standard Time", "America/Phoenix"),
    ("UTC", "Etc/UTC"),
    ("UTC+12", "Etc/GMT-12"),
    ("UTC+13", "Etc/GMT-13"),
    ("UTC-02", "Etc/GMT+2"),
    ("UTC-08", "Etc/GMT+8"),
    ("UTC-09", "Etc/GMT+9"),
    ("UTC-11", "Etc/GMT+11"),
    ("Venezuela Standard Time", "America/Caracas"),
    ("Vladivostok Standard Time", "Asia/Vladivostok"),
    ("Volgograd Standard Time", "Europe/Volgograd"),
    ("W. Australia Standard Time", "Australia/Perth"),
    ("W. Central Africa Standard Time", "Africa/Lagos"),
    ("W. Europe Standard Time", "Europe/Berlin"),
    ("W. Mongolia Standard Time", "Asia/Hovd"),
    ("West Asia Standard Time", "Asia/Tashkent"),
    ("West Bank Standard Time", "Asia/Hebron"),
    ("West Pacific Standard Time", "Pacific/Port_Moresby"),
    ("Yakutsk Standard Time", "Asia/Yakutsk"),
    ("Yukon Standard Time", "America/Whitehorse"),
];

/// Explicit IANA location aliases from VTIMEZONE components in one resource.
/// A declaration alone does not make its STANDARD/DAYLIGHT rules interpretable.
#[derive(Default)]
pub(crate) struct VTimezones {
    locations: HashMap<String, String>,
}

impl VTimezones {
    pub(crate) fn parse(ical: &str) -> Self {
        let unfolded = unfold_ical(ical);
        let mut out = Self::default();
        let mut search = 0;
        while let Some(rel) = unfolded[search..].find("BEGIN:VTIMEZONE") {
            let start = search + rel;
            let Some(rel_end) = unfolded[start..].find("END:VTIMEZONE") else {
                break;
            };
            let block = &unfolded[start..start + rel_end];
            if let Some(tzid) = block_property(block, "TZID") {
                if let Some(loc) = block_property(block, "X-LIC-LOCATION") {
                    out.locations.insert(tzid, loc);
                }
            }
            search = start + rel_end + "END:VTIMEZONE".len();
        }
        out
    }

    /// Shared by snapshot validation and recurrence exclusions. Never interpret
    /// an unresolved named zone as floating time, even if a VTIMEZONE exists.
    pub(crate) fn resolve(&self, tzid: Option<&str>) -> Result<Option<Tz>, &'static str> {
        let Some(id) = tzid else {
            return Ok(None); // A genuinely floating/all-day value has no TZID.
        };
        resolve_tzid(id)
            .or_else(|| self.locations.get(id).and_then(|loc| resolve_tzid(loc)))
            .map(Some)
            .ok_or("unsupported_timezone")
    }
}

fn block_property(block: &str, name: &str) -> Option<String> {
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix(name) {
            if let Some(value) = rest.strip_prefix(':') {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            } else if rest.starts_with(';') {
                if let Some((_, value)) = params_and_value(rest) {
                    if !value.is_empty() {
                        return Some(value);
                    }
                }
            }
        }
    }
    None
}

/// Map a TZID from DTSTART/DTEND/EXDATE to a chrono-tz zone.
///
/// Order: IANA (including UTC/GMT aliases), Windows CLDR names, then the IANA
/// suffix of a recognized Microsoft/libical URI (including three-part names).
pub fn resolve_tzid(tzid: &str) -> Option<Tz> {
    let tzid = tzid.trim().trim_matches('"').trim();
    if tzid.is_empty() {
        return None;
    }
    if tzid.eq_ignore_ascii_case("Z")
        || tzid.eq_ignore_ascii_case("UTC")
        || tzid.eq_ignore_ascii_case("GMT")
        || tzid.eq_ignore_ascii_case("Etc/UTC")
        || tzid.eq_ignore_ascii_case("Etc/GMT")
    {
        return Some(Tz::UTC);
    }
    if let Ok(tz) = tzid.parse::<Tz>() {
        return Some(tz);
    }
    for (windows, iana) in WINDOWS_ZONES {
        if windows.eq_ignore_ascii_case(tzid) {
            return iana.parse().ok();
        }
    }
    iana_from_path(tzid)
}

fn iana_from_path(tzid: &str) -> Option<Tz> {
    let mut suffix = tzid
        .strip_prefix("tzone://Microsoft/Olson/")
        .or_else(|| tzid.strip_prefix("/freeassociation.sourceforge.net/"))?;
    loop {
        if let Ok(tz) = suffix.parse::<Tz>() {
            return Some(tz);
        }
        for (windows, iana) in WINDOWS_ZONES {
            if windows.eq_ignore_ascii_case(suffix) {
                return iana.parse().ok();
            }
        }
        suffix = suffix.split_once('/')?.1;
    }
}

/// Store only a resolved IANA name so all availability paths can apply DST.
pub(crate) fn accept_tzid(
    tzid: Option<&str>,
    vtz: &VTimezones,
) -> Result<Option<String>, &'static str> {
    vtz.resolve(tzid)
        .map(|tz| tz.map(|tz| tz.name().to_string()))
}

/// Split `;PARAM=...:value` with RFC 5545 quoting. Outlook's unquoted
/// `tzone://...` TZIDs put extra colons before the datetime; if the first
/// unquoted colon does not yield a datetime, take the last colon that does.
pub(crate) fn params_and_value(rest: &str) -> Option<(Vec<(String, String)>, String)> {
    if let Some(value) = rest.strip_prefix(':') {
        return Some((Vec::new(), value.to_string()));
    }
    if !rest.starts_with(';') {
        return None;
    }
    let first = unquoted_colon(rest)?;
    let mut params_str = &rest[1..first];
    let mut value = rest[first + 1..].to_string();
    let is_date_list = |value: &str| {
        value
            .split(',')
            .all(|v| parse_ical_datetime(v.trim()).is_some())
    };
    if !is_date_list(&value) {
        if let Some(last) = rest.rfind(':') {
            if last > first && is_date_list(&rest[last + 1..]) {
                params_str = &rest[1..last];
                value = rest[last + 1..].to_string();
            }
        }
    }
    Some((parse_params(params_str), value))
}

fn unquoted_colon(s: &str) -> Option<usize> {
    let mut in_quotes = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => return Some(i),
            _ => {}
        }
    }
    None
}

fn parse_params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => {
                if let Some(param) = one_param(&s[start..i]) {
                    out.push(param);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    if let Some(param) = one_param(&s[start..]) {
        out.push(param);
    }
    out
}

fn one_param(s: &str) -> Option<(String, String)> {
    let (key, value) = s.split_once('=')?;
    Some((
        key.trim().to_string(),
        value.trim().trim_matches('"').to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, Timelike};

    #[test]
    fn windows_targets_parse_as_chrono_tz() {
        for (windows, iana) in WINDOWS_ZONES {
            assert!(
                iana.parse::<Tz>().is_ok(),
                "{windows} -> {iana} is not in chrono-tz"
            );
        }
    }

    #[test]
    fn romance_standard_time_is_paris() {
        assert_eq!(
            resolve_tzid("Romance Standard Time").unwrap().name(),
            "Europe/Paris"
        );
        assert_eq!(
            resolve_tzid("romance standard time").unwrap().name(),
            "Europe/Paris"
        );
    }

    #[test]
    fn microsoft_olson_uri_uses_the_iana_suffix() {
        assert_eq!(
            resolve_tzid("tzone://Microsoft/Olson/Europe/Paris")
                .unwrap()
                .name(),
            "Europe/Paris"
        );
        assert_eq!(
            resolve_tzid("tzone://Microsoft/Olson/America/Argentina/Buenos_Aires")
                .unwrap()
                .name(),
            "America/Argentina/Buenos_Aires"
        );
        assert!(resolve_tzid("Opaque/Europe/Paris").is_none());
        assert_eq!(
            resolve_tzid("/freeassociation.sourceforge.net/Europe/Paris")
                .unwrap()
                .name(),
            "Europe/Paris"
        );
    }

    #[test]
    fn vtimezone_location_and_opaque_id() {
        let ical = "BEGIN:VCALENDAR\nBEGIN:VTIMEZONE\nTZID:Customized Time Zone\nX-LIC-LOCATION:Europe/Paris\nEND:VTIMEZONE\nBEGIN:VTIMEZONE\nTZID:Local-Only\nEND:VTIMEZONE\nEND:VCALENDAR\n";
        let vtz = VTimezones::parse(ical);
        assert_eq!(
            accept_tzid(Some("Customized Time Zone"), &vtz)
                .unwrap()
                .as_deref(),
            Some("Europe/Paris")
        );
        assert!(accept_tzid(Some("Local-Only"), &vtz).is_err());
        assert!(accept_tzid(Some("Not/AZone"), &vtz).is_err());
    }

    #[test]
    fn unknown_without_vtimezone_is_rejected() {
        assert!(accept_tzid(Some("Not/AZone"), &VTimezones::default()).is_err());
        assert_eq!(accept_tzid(None, &VTimezones::default()).unwrap(), None);
    }

    #[test]
    fn quoted_microsoft_uri_datetime_split() {
        let rest = r#";TZID="tzone://Microsoft/Olson/Europe/Paris":20260310T100000"#;
        let (params, value) = params_and_value(rest).unwrap();
        assert_eq!(value, "20260310T100000");
        assert_eq!(
            params
                .iter()
                .find(|(k, _)| k == "TZID")
                .map(|(_, v)| v.as_str()),
            Some("tzone://Microsoft/Olson/Europe/Paris")
        );
    }

    #[test]
    fn unquoted_microsoft_uri_uses_the_datetime_colon() {
        let rest = ";TZID=tzone://Microsoft/Olson/Europe/Paris:20260310T100000";
        let (params, value) = params_and_value(rest).unwrap();
        assert_eq!(value, "20260310T100000");
        assert_eq!(
            params
                .iter()
                .find(|(k, _)| k == "TZID")
                .map(|(_, v)| v.as_str()),
            Some("tzone://Microsoft/Olson/Europe/Paris")
        );
    }

    #[test]
    fn display_name_with_offset_colon() {
        let rest = r#";TZID="(UTC+01:00) Brussels, Copenhagen, Madrid, Paris":20260310T100000"#;
        let (params, value) = params_and_value(rest).unwrap();
        assert_eq!(value, "20260310T100000");
        assert_eq!(
            params
                .iter()
                .find(|(k, _)| k == "TZID")
                .map(|(_, v)| v.as_str()),
            Some("(UTC+01:00) Brussels, Copenhagen, Madrid, Paris")
        );
    }

    #[test]
    fn windows_eastern_converts_to_paris() {
        let dt = NaiveDate::from_ymd_opt(2026, 7, 15)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap();
        let etz = resolve_tzid("Eastern Standard Time").unwrap();
        let paris: Tz = "Europe/Paris".parse().unwrap();
        use chrono::TimeZone;
        let result = etz
            .from_local_datetime(&dt)
            .earliest()
            .unwrap()
            .with_timezone(&paris)
            .naive_local();
        assert_eq!(result.hour(), 16);
    }
}
