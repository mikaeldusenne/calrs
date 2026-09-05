use super::*;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing::instrument::WithSubscriber;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn diagnostics_redact_errors_and_report_dropped_futures() {
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    async {
        let error = sync("test-source", "test", trace::<()>("private_failure", async {
            Err(anyhow::anyhow!("https://user:SECRET@host/private?token=SECRET <calendar-data>SECRET</calendar-data>"))
        })).await;
        assert!(error.is_err());
        let abandoned = trace::<()>("pending_request", std::future::pending());
        assert!(tokio::time::timeout(std::time::Duration::from_millis(1), abandoned).await.is_err());
    }.with_subscriber(subscriber).await;
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("stage=\"private_failure\""));
    assert!(logs.contains("source_id=\"test-source\""));
    assert!(logs.contains("error_kind=\"application\""));
    assert!(logs.contains("outcome=\"abandoned\""));
    assert!(!logs.contains("outcome=\"ok\""));
    for sensitive in ["SECRET", "https://", "calendar-data"] {
        assert!(!logs.contains(sensitive));
    }
}

/// A body timeout must remain a reqwest timeout after receiving valid headers.
#[tokio::test]
async fn body_timeout_preserves_status_and_error() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        loop {
            let mut line = String::new();
            assert!(stream.read_line(&mut line).await.unwrap() > 0);
            if line == "\r\n" {
                break;
            }
        }
        stream
            .get_mut()
            .write_all(b"HTTP/1.1 207 Multi-Status\r\nContent-Length: 1000\r\n\r\n")
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let response = reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_millis(200))
        .send_observed("test")
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::MULTI_STATUS);
    let error = response.text().await.unwrap_err();
    assert_eq!(error_kind(&error), "timeout");
    assert!(error.downcast_ref::<reqwest::Error>().unwrap().is_timeout());
    server.abort();
}

/// Exercise the real sync on the normal test stack, including nested observation.
#[tokio::test]
async fn full_sync_with_observers_completes() {
    use axum::{extract::Request, http::StatusCode, routing::any, Router};
    let app = Router::new().fallback(any(|request: Request| async move {
        let body = match request.uri().path() {
            "/" => "<d:current-user-principal><d:href>/principal</d:href></d:current-user-principal>",
            "/principal" => "<cal:calendar-home-set><d:href>/home</d:href></cal:calendar-home-set>",
            "/home" => "<d:multistatus><d:response><d:href>/cal/</d:href><d:propstat><d:prop><d:resourcetype><cal:calendar/></d:resourcetype></d:prop></d:propstat></d:response></d:multistatus>",
            _ => "<d:multistatus><d:sync-token>test-token</d:sync-token></d:multistatus>",
        };
        (StatusCode::MULTI_STATUS, body)
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::migrate(&pool).await.unwrap();
    sqlx::query("INSERT INTO accounts (id, name, email) VALUES ('a', 'Test', 'test@example.com')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO caldav_sources (id, account_id, name, url, username) VALUES ('s', 'a', 'Test', ?, 'test')")
        .bind(&url).execute(&pool).await.unwrap();
    let client = crate::caldav::CaldavClient::new(&url, "test", "password");
    sync(
        "s",
        "test",
        crate::commands::sync::sync_source(&pool, &[0; 32], &client, "s"),
    )
    .await
    .unwrap();
    let token: String =
        sqlx::query_scalar("SELECT sync_token FROM calendars WHERE source_id = 's'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(token, "test-token");
    server.abort();
}
