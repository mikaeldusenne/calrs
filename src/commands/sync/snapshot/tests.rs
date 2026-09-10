use super::*;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    routing::any,
    Router,
};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc, Mutex,
};

const MASTER: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:old-series\r\nDTSTART;TZID=Europe/Paris:20000101T090000\r\nDTEND;TZID=Europe/Paris:20000101T100000\r\nRRULE:FREQ=DAILY\r\nEXDATE;TZID=Europe/Paris:20300908T090000\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:old-series\r\nRECURRENCE-ID;TZID=Europe/Paris:20300909T090000\r\nDTSTART;TZID=Europe/Paris:20300909T140000\r\nDTEND;TZID=Europe/Paris:20300909T150000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

#[derive(Default)]
pub(crate) struct Mock {
    pub mode: AtomicU8,
    pub requests: Mutex<Vec<String>>,
}

#[test]
fn duration_is_parsed_or_explicitly_rejected() {
    assert_eq!(parse_duration("PT30M").unwrap(), Duration::minutes(30));
    assert_eq!(parse_duration("P1DT2H").unwrap(), Duration::hours(26));
    assert_eq!(parse_duration("+P2W").unwrap(), Duration::weeks(2));
    for value in [
        "P",
        "PT",
        "PTS",
        "PT1M1H",
        "P1W1D",
        "PT1",
        "-PT1H",
        "PT999999999999999999999H",
    ] {
        assert!(parse_duration(value).is_err(), "{value}");
    }
}

fn multistatus(inner: &str) -> String {
    format!(
        r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">{inner}</d:multistatus>"#
    )
}

async fn server(State(mock): State<Arc<Mock>>, request: Request) -> (StatusCode, String) {
    let path = request.uri().path().to_owned();
    let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    mock.requests.lock().unwrap().push(body.clone());
    assert!(
        !body.contains("sync-collection"),
        "DavMail must never receive an unfiltered token probe"
    );
    let mode = mock.mode.load(Ordering::SeqCst);
    let result = match path.as_str() {
        "/" => "<d:current-user-principal><d:href>/principal</d:href></d:current-user-principal>"
            .to_string(),
        "/principal" => {
            "<c:calendar-home-set><d:href>/home</d:href></c:calendar-home-set>".to_string()
        }
        "/home" => multistatus(&format!(
            r#"<d:response><d:href>/cal/</d:href><d:propstat><d:prop>
            <d:resourcetype><c:calendar/></d:resourcetype><cs:getctag>ctag-{mode}</cs:getctag>
            </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#
        )),
        "/cal/" => {
            let inventory = body.contains("calendar-query");
            if inventory {
                assert!(body.contains("time-range start="));
                assert!(!body.contains("calendar-data"));
            } else {
                assert!(body.contains("calendar-multiget"));
                if mode == 1 {
                    return (StatusCode::GATEWAY_TIMEOUT, "PRIVATE-EMAIL-CONTENT".into());
                }
                if mode == 5 {
                    return std::future::pending().await;
                }
            }
            if mode == 3 {
                multistatus("")
            } else {
                let etag = if matches!(mode, 1 | 4..=6) {
                    "changed"
                } else {
                    "v1"
                };
                let ical = if mode == 4 {
                    MASTER.replace("DTEND;TZID=Europe/Paris:20000101T100000", "DTEND:invalid")
                } else if mode == 6 {
                    MASTER
                        .replace("Europe/Paris", "Opaque")
                        .replace(
                            "VERSION:2.0",
                            "VERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Opaque\r\nEND:VTIMEZONE",
                        )
                        .replace(
                            "UID:old-series",
                            "UID:old-series\r\nSUMMARY:Private calendar event",
                        )
                } else {
                    MASTER.into()
                };
                let data = if inventory {
                    String::new()
                } else {
                    format!("<c:calendar-data><![CDATA[{ical}]]></c:calendar-data>")
                };
                multistatus(&format!(
                    r#"<d:response><d:href>/cal/series.ics</d:href><d:propstat><d:prop>
                    <d:getetag>"{etag}"</d:getetag>{data}</d:prop><d:status>HTTP/1.1 200 OK</d:status>
                    </d:propstat></d:response>"#
                ))
            }
        }
        _ => panic!("Unexpected mock request"),
    };
    (StatusCode::MULTI_STATUS, result)
}

pub(crate) async fn fixture() -> (
    SqlitePool,
    CaldavClient,
    Arc<Mock>,
    tokio::task::JoinHandle<()>,
) {
    let mock = Arc::new(Mock::default());
    let app = Router::new().fallback(any(server)).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::migrate(&pool).await.unwrap();
    sqlx::query("INSERT INTO users (id, email, name, role, auth_provider, username) VALUES ('host', 'host@example.test', 'Host', 'user', 'local', 'host')").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO accounts (id, name, email, user_id) VALUES ('a', 'Test', 'host@example.test', 'host')").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO caldav_sources (id, account_id, name, url, username, password_enc) VALUES ('s', 'a', 'Test', ?, 'user', ?)")
        .bind(&url).bind(crate::crypto::encrypt_password(&[0; 32], "secret").unwrap()).execute(&pool).await.unwrap();
    (
        pool,
        CaldavClient::new(&url, "user", "secret"),
        mock,
        server,
    )
}

#[tokio::test]
async fn old_master_exceptions_and_etag_reuse_then_empty_snapshot() {
    let (pool, client, mock, server) = fixture().await;
    sync(&pool, &client, "s", false, 0).await.unwrap();
    let events: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT start_at, recurrence_id FROM events ORDER BY start_at")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, "20000101T090000");
    assert_eq!(events[1].1.as_deref(), Some("20300909T090000"));
    mock.requests.lock().unwrap().clear();
    sync(&pool, &client, "s", false, 0).await.unwrap();
    assert!(!mock
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.contains("calendar-query")));
    mock.mode.store(2, Ordering::SeqCst);
    sync(&pool, &client, "s", false, 0).await.unwrap();
    assert!(!mock
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.contains("calendar-multiget")));
    mock.mode.store(3, Ordering::SeqCst);
    sync(&pool, &client, "s", false, 0).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    server.abort();
}

#[tokio::test]
async fn upstream_or_validation_failure_preserves_entire_snapshot_and_success_timestamp() {
    let (pool, client, mock, server) = fixture().await;
    sync(&pool, &client, "s", false, 0).await.unwrap();
    sqlx::query("UPDATE caldav_sources SET last_synced = '2000-01-01 00:00:00'")
        .execute(&pool)
        .await
        .unwrap();
    for mode in [1, 4, 6] {
        mock.mode.store(mode, Ordering::SeqCst);
        let error = sync(&pool, &client, "s", true, 0).await.unwrap_err();
        assert!(!error.to_string().contains("PRIVATE"));
        let (tag, count): (String, i64) =
            sqlx::query_as("SELECT ctag, (SELECT COUNT(*) FROM events) FROM calendars")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((tag.as_str(), count), ("ctag-0", 2));
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT last_synced FROM caldav_sources")
                .fetch_one(&pool)
                .await
                .unwrap(),
            "2000-01-01 00:00:00"
        );
    }
    server.abort();
}

#[tokio::test]
async fn failed_initial_fetch_does_not_cache_the_remote_ctag() {
    let (pool, client, mock, server) = fixture().await;
    mock.mode.store(1, Ordering::SeqCst);
    assert!(sync(&pool, &client, "s", false, 0).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM calendars")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    mock.mode.store(0, Ordering::SeqCst);
    sync(&pool, &client, "s", false, 0).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    server.abort();
}

#[tokio::test]
async fn credential_change_invalidates_cache_and_prevents_old_worker_publication() {
    let (pool, client, _, server) = fixture().await;
    sync(&pool, &client, "s", false, 0).await.unwrap();
    sqlx::query("UPDATE caldav_sources SET username = 'different-account' WHERE id = 's'")
        .execute(&pool)
        .await
        .unwrap();
    let (verified, revision): (Option<String>, i64) =
        sqlx::query_as("SELECT sync_verified_at, sync_revision FROM caldav_sources WHERE id = 's'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(verified.is_none());
    assert_eq!(revision, 1);
    let error = sync(&pool, &client, "s", true, 0).await.unwrap_err();
    assert_eq!(diagnostics::error_kind(&error), "remote_changed");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    server.abort();
}

async fn stored_calendar(ical: &str) -> Result<SqlitePool, String> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::migrate(&pool).await.unwrap();
    sqlx::query("INSERT INTO users (id, email, name, role, auth_provider, username) VALUES ('host', 'host@example.test', 'Host', 'user', 'local', 'host')").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO accounts (id, name, email, user_id) VALUES ('a', 'Test', 'host@example.test', 'host')").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO caldav_sources (id, account_id, name, url, username) VALUES ('s', 'a', 'Test', 'http://example.test', 'user')").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO calendars (id, source_id, href) VALUES ('c', 's', '/cal/')")
        .execute(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    store_events(&mut conn, "c", ical)
        .await
        .map_err(|e| diagnostics::error_kind(&e).to_string())?;
    drop(conn);
    Ok(pool)
}

async fn stored_timezone(ical: &str) -> Result<Option<String>, String> {
    let pool = stored_calendar(ical).await?;
    Ok(
        sqlx::query_scalar("SELECT timezone FROM events WHERE calendar_id = 'c'")
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn exchange_windows_tzid_is_stored_as_iana() {
    let ical = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Romance Standard Time\r\nX-LIC-LOCATION:Europe/Paris\r\nBEGIN:STANDARD\r\nDTSTART:16011028T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:exchange-1\r\nDTSTART;TZID=Romance Standard Time:20260310T100000\r\nDTEND;TZID=Romance Standard Time:20260310T110000\r\nSUMMARY:Staff\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    assert_eq!(
        stored_timezone(ical).await.unwrap().as_deref(),
        Some("Europe/Paris")
    );
}

#[tokio::test]
async fn microsoft_olson_uri_is_stored_as_iana() {
    let ical = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:ms-1\r\nDTSTART;TZID=\"tzone://Microsoft/Olson/Europe/Paris\":20260310T100000\r\nDTEND;TZID=\"tzone://Microsoft/Olson/Europe/Paris\":20260310T110000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    assert_eq!(
        stored_timezone(ical).await.unwrap().as_deref(),
        Some("Europe/Paris")
    );
}

#[tokio::test]
async fn outlook_and_davmail_timezones_preserve_the_complete_series() {
    // Reproduce the reported Outlook export, retaining dates/rules but no private data.
    let ical = "BEGIN:VCALENDAR\nPRODID:-//Microsoft Corporation//Outlook 16.0 MIMEDIR//EN\nVERSION:2.0\nMETHOD:PUBLISH\nX-MS-OLK-FORCEINSPECTOROPEN:TRUE\nBEGIN:VTIMEZONE\nTZID:Customized Time Zone\nBEGIN:STANDARD\nDTSTART:16011028T030000\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=10\nTZOFFSETFROM:+0200\nTZOFFSETTO:+0100\nEND:STANDARD\nBEGIN:DAYLIGHT\nDTSTART:16010325T020000\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=3\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0200\nEND:DAYLIGHT\nEND:VTIMEZONE\nBEGIN:VEVENT\nCLASS:PUBLIC\nCREATED:20260910T110153Z\nDTEND;TZID=\"Customized Time Zone\":20250721T100000\nDTSTAMP:20260910T110153Z\nDTSTART;TZID=\"Customized Time Zone\":20250721T090000\nLAST-MODIFIED:20260910T110153Z\nPRIORITY:5\nRRULE:FREQ=WEEKLY;COUNT=4;INTERVAL=4;BYDAY=MO;WKST=MO\nSEQUENCE:0\nSUMMARY;LANGUAGE=fr:Test Outlook event\nTRANSP:OPAQUE\nUID:synthetic-\n\toutlook-series\nX-ALT-DESC;FMTTYPE=text/html:<HTML>\n\t<BODY>Test</BODY></HTML>\nX-MICROSOFT-CDO-BUSYSTATUS:BUSY\nX-MICROSOFT-CDO-IMPORTANCE:1\nX-MS-OLK-AUTOFILLLOCATION:TRUE\nEND:VEVENT\nEND:VCALENDAR\n";
    // Exact VTIMEZONE captured from DavMail; event details are synthetic.
    let davmail = include_str!("../../../../tests/fixtures/davmail-custom-timezone.ics");
    for ical in [ical, davmail]
        .into_iter()
        .flat_map(|ical| [ical.to_string(), ical.replace('\n', "\r\n")])
    {
        let pool = stored_calendar(&ical).await.expect("valid calendar series");
        let recurring: Vec<(String, String, String, Option<String>, Option<String>)> =
            sqlx::query_as("SELECT start_at, end_at, rrule, raw_ical, timezone FROM events")
                .fetch_all(&pool)
                .await
                .unwrap();
        let date = |s: &str| parse_ical_datetime(s).unwrap();
        let busy = crate::web::expand_recurring_into_busy(
            &recurring,
            date("20250701T000000"),
            date("20251101T000000"),
            chrono_tz::UTC,
        );
        assert_eq!(
            busy,
            ["20250721", "20250818", "20250915", "20251013"]
                .map(|d| (date(&format!("{d}T070000")), date(&format!("{d}T080000"))))
        );
        assert!(crate::web::expand_recurring_into_busy(
            &recurring,
            date("20260901T000000"),
            date("20261001T000000"),
            chrono_tz::UTC
        )
        .is_empty());
    }
}

#[tokio::test]
async fn active_timezone_collision_still_rejects_the_event() {
    let ical = include_str!("../../../../tests/fixtures/davmail-custom-timezone.ics")
        .replace("END:STANDARD", "RDATE:20250721T030000\nEND:STANDARD")
        .replace("END:DAYLIGHT", "RDATE:20250721T020000\nEND:DAYLIGHT");
    assert_eq!(
        stored_timezone(&ical).await.unwrap_err(),
        "unsupported_timezone"
    );
}

#[tokio::test]
async fn vtimezone_without_iana_location_uses_its_actual_offset() {
    let ical = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Customized Time Zone\r\nBEGIN:STANDARD\r\nDTSTART:16010101T000000\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0100\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:custom-1\r\nDTSTART;TZID=Customized Time Zone:20260310T100000\r\nDTEND;TZID=Customized Time Zone:20260310T110000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    for ical in [
        ical.to_string(),
        ical.replace(
            "TZID:Customized Time Zone",
            "TZID:Customized Time Zone\r\nX-LIC-LOCATION:Europe/Paris",
        ),
    ] {
        let tz = stored_timezone(&ical).await.unwrap();
        let date = parse_ical_datetime("20260715T100000").unwrap();
        assert_eq!(
            crate::utils::checked_event_to_tz(date, tz.as_deref(), "Europe/Paris".parse().unwrap()),
            Some(parse_ical_datetime("20260715T110000").unwrap())
        );
    }
}

#[tokio::test]
async fn unknown_tzid_without_vtimezone_still_fails_closed() {
    let ical = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:bad-1\r\nDTSTART;TZID=Not/AZone:20260310T100000\r\nDTEND;TZID=Not/AZone:20260310T110000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    assert_eq!(
        stored_timezone(ical).await.unwrap_err(),
        "unsupported_timezone"
    );
}

const PARIS_VTIMEZONE: &str = "BEGIN:VTIMEZONE\r\nTZID:Custom-Paris\r\nBEGIN:STANDARD\r\nDTSTART:19701025T030000\r\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:19700329T020000\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\n";

#[tokio::test]
async fn custom_exdate_excludes_the_correct_occurrence() {
    let ical = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{PARIS_VTIMEZONE}BEGIN:VEVENT\r\nUID:exdate\r\nDTSTART:20260715T080000Z\r\nDTEND:20260715T090000Z\r\nRRULE:FREQ=DAILY;BYHOUR=8,10;COUNT=4\r\nEXDATE;TZID=Custom-Paris:20260715T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
    let pool = stored_calendar(&ical).await.unwrap();
    let rows = sqlx::query_as("SELECT start_at, end_at, rrule, raw_ical, timezone FROM events")
        .fetch_all(&pool)
        .await
        .unwrap();
    let busy = crate::web::expand_recurring_into_busy(
        &rows,
        parse_ical_datetime("20260715T000000").unwrap(),
        parse_ical_datetime("20260716T000000").unwrap(),
        chrono_tz::UTC,
    );
    // EXDATE 10:00 Paris excludes 08:00 UTC, NOT the 10:00 UTC meeting.
    assert_eq!(
        busy,
        vec![(
            parse_ical_datetime("20260715T100000").unwrap(),
            parse_ical_datetime("20260715T110000").unwrap()
        )]
    );
}

#[tokio::test]
async fn old_custom_series_keeps_dst_and_moved_occurrences() {
    let ical = format!("BEGIN:VCALENDAR\r\n{PARIS_VTIMEZONE}BEGIN:VEVENT\r\nUID:old\r\nDTSTART;TZID=Custom-Paris:20000101T100000\r\nDTEND;TZID=Custom-Paris:20000101T110000\r\nRRULE:FREQ=DAILY\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:old\r\nRECURRENCE-ID;TZID=Custom-Paris:20260329T100000\r\nDTSTART;TZID=Custom-Paris:20260329T140000\r\nDTEND;TZID=Custom-Paris:20260329T150000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
    let pool = stored_calendar(&ical).await.unwrap();
    // Exercise the actual busy-time query, including the moved occurrence.
    sqlx::query("UPDATE caldav_sources SET sync_verified_at = datetime('now'), sync_window_start = '20000101T000000Z'")
        .execute(&pool).await.unwrap();
    let busy = crate::web::fetch_busy_times_for_user(
        &pool,
        "host",
        parse_ical_datetime("20260328T000000").unwrap(),
        parse_ical_datetime("20260331T000000").unwrap(),
        chrono_tz::UTC,
        None,
    )
    .await;
    let mut actual: Vec<_> = busy
        .into_iter()
        .map(|(s, e)| {
            (
                s.format("%Y%m%dT%H%M%S").to_string(),
                e.format("%Y%m%dT%H%M%S").to_string(),
            )
        })
        .collect();
    actual.sort();
    assert_eq!(
        actual,
        vec![
            ("20260328T090000".into(), "20260328T100000".into()), // Winter UTC+1.
            ("20260329T120000".into(), "20260329T130000".into()), // Moved, UTC+2.
            ("20260330T080000".into(), "20260330T090000".into()), // Summer UTC+2.
        ]
    );
}

#[tokio::test]
async fn unresolved_end_and_exclusion_zones_fail_snapshot_validation() {
    for field in ["DTEND", "EXDATE", "RECURRENCE-ID"] {
        let ical = format!("BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:Opaque\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:unknown\r\nDTSTART:20260715T080000Z\r\n{field};TZID=Opaque:20260715T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
        assert_eq!(
            stored_calendar(&ical).await.unwrap_err(),
            "unsupported_timezone",
            "{field}"
        );
    }
}

#[tokio::test]
async fn timezone_upgrade_revalidates_once_without_deleting_cached_events() {
    let pool = stored_calendar(MASTER).await.unwrap();
    sqlx::query("UPDATE caldav_sources SET sync_verified_at = datetime('now'), sync_window_start = '20000101T000000Z'")
        .execute(&pool).await.unwrap();
    // Simulate a database immediately before migration 065.
    sqlx::query("DELETE FROM _migrations WHERE name = '065_revalidate_timezone_snapshots'")
        .execute(&pool)
        .await
        .unwrap();
    crate::db::migrate(&pool).await.unwrap();
    let state: (Option<String>, i64) =
        sqlx::query_as("SELECT sync_verified_at, sync_revision FROM caldav_sources WHERE id = 's'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, (None, 1));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert!(
        !crate::sync_jobs::available(&pool, "host", None, "20260328T000000Z", "20260331T000000Z")
            .await
    );
    crate::db::migrate(&pool).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT sync_revision FROM caldav_sources WHERE id = 's'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn error_report_identifies_the_broken_override_not_the_master() {
    let pool = stored_calendar(MASTER).await.unwrap();
    sqlx::query("DELETE FROM events")
        .execute(&pool)
        .await
        .unwrap();
    let broken = MASTER.replace(
        "RECURRENCE-ID;TZID=Europe/Paris:20300909T090000",
        "SUMMARY:Broken override\r\nRECURRENCE-ID;TZID=Unknown-TZ:20300909T090000",
    );
    let mut conn = pool.acquire().await.unwrap();
    let error = store_events(&mut conn, "c", &broken).await.unwrap_err();
    assert_eq!(diagnostics::error_kind(&error), "unsupported_timezone");
    let detail = error.downcast_ref::<diagnostics::EventFailure>().unwrap();
    assert_eq!(detail.title.as_deref(), Some("Broken override"));
    assert_eq!(detail.recurrence_id.as_deref(), Some("2030-09-09 09:00:00"));
    assert!(detail.timezones.iter().any(|tz| tz == "Unknown-TZ"));
}

#[tokio::test]
async fn custom_timezone_upgrade_keeps_cache_but_requires_revalidation() {
    let pool = stored_calendar(MASTER).await.unwrap();
    // Reconstruct the pre-066 schema of this isolated in-memory test database.
    sqlx::raw_sql(
        "DROP TRIGGER clear_private_sync_event_error;
        DROP TRIGGER clear_private_sync_event_error_on_owner_change;
        ALTER TABLE caldav_sources DROP COLUMN sync_event_error;
        DELETE FROM _migrations WHERE name = '066_private_sync_event_error';
        UPDATE caldav_sources SET sync_verified_at = datetime('now');",
    )
    .execute(&pool)
    .await
    .unwrap();
    crate::db::migrate(&pool).await.unwrap();
    let state: (Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT sync_verified_at, sync_revision, sync_event_error FROM caldav_sources WHERE id = 's'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(state, (None, 1, None));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    crate::db::migrate(&pool).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT sync_revision FROM caldav_sources WHERE id = 's'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}
