//! Metadata-only sync diagnostics. Never format arbitrary errors or HTTP bodies.

use anyhow::Result;
use std::{future::Future, time::Instant};
use tracing::{Instrument, Span};

pub(crate) const TARGET: &str = "calrs::sync_diagnostics";

#[derive(Debug)]
pub(crate) struct HttpStatus(pub reqwest::StatusCode);

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CalDAV request returned HTTP {}", self.0.as_u16())
    }
}

impl std::error::Error for HttpStatus {}

/// Classify errors without exposing URLs, credentials, SQL values or response bodies.
pub(crate) fn error_kind(error: &anyhow::Error) -> &'static str {
    if error.downcast_ref::<HttpStatus>().is_some() {
        "http_status"
    } else if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        if error.is_timeout() {
            "timeout"
        } else if error.is_connect() {
            "connect"
        } else if error.is_status() {
            "http_status"
        } else if error.is_body() || error.is_decode() {
            "body"
        } else {
            "http_transport"
        }
    } else if error.downcast_ref::<sqlx::Error>().is_some() {
        "database"
    } else {
        "application"
    }
}

/// Report a dropped future as inconclusive, never as a successful sync.
struct Progress {
    started: Instant,
    span: Span,
    finished: bool,
}

impl Drop for Progress {
    fn drop(&mut self) {
        if !self.finished {
            self.span.in_scope(|| {
                tracing::warn!(target: TARGET,
                    outcome = "abandoned",
                    elapsed_ms = self.started.elapsed().as_millis() as u64,
                    "sync step ended without a result (future dropped or panicked)"
                );
            });
        }
    }
}

/// Time a phase and keep its span attached across awaits and cancellation.
/// Box each phase so nested instrumentation does not grow the caller's future/stack.
pub(crate) fn trace<T>(
    stage: &'static str,
    future: impl Future<Output = Result<T>>,
) -> impl Future<Output = Result<T>> {
    Box::pin(
        async move {
            let mut progress = Progress {
                started: Instant::now(),
                span: Span::current(),
                finished: false,
            };
            tracing::info!(target: TARGET, "sync step started");
            let result = future.await;
            progress.finished = true;
            let elapsed_ms = progress.started.elapsed().as_millis() as u64;
            match &result {
                Ok(_) => {
                    tracing::info!(target: TARGET, outcome = "ok", elapsed_ms, "sync step finished")
                }
                Err(error) => {
                    let http_status = error
                        .downcast_ref::<reqwest::Error>()
                        .and_then(|e| e.status())
                        .or_else(|| error.downcast_ref::<HttpStatus>().map(|e| e.0))
                        .map(|status| status.as_u16());
                    let io_kind = error
                        .chain()
                        .find_map(|cause| cause.downcast_ref::<std::io::Error>().map(|e| e.kind()));
                    tracing::warn!(target: TARGET,
                        outcome = "error",
                        elapsed_ms,
                        error_kind = error_kind(error),
                        http_status,
                        ?io_kind,
                        "sync step failed"
                    );
                }
            }
            result
        }
        .instrument(tracing::info_span!("sync_step", stage)),
    )
}

#[cfg(test)]
mod tests {
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
            let error = trace::<()>("private_failure", async {
                Err(anyhow::anyhow!("https://user:SECRET@host/private?token=SECRET <calendar-data>SECRET</calendar-data>"))
            }).await;
            assert!(error.is_err());
            let abandoned = trace::<()>("pending_request", std::future::pending());
            assert!(tokio::time::timeout(std::time::Duration::from_millis(1), abandoned).await.is_err());
        }.with_subscriber(subscriber).await;
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("stage=\"private_failure\""));
        assert!(logs.contains("error_kind=\"application\""));
        assert!(logs.contains("outcome=\"abandoned\""));
        assert!(!logs.contains("outcome=\"ok\""));
        for sensitive in ["SECRET", "https://", "calendar-data"] {
            assert!(!logs.contains(sensitive));
        }
    }
}
