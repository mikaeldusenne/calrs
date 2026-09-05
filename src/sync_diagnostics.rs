//! PodSaN diagnostics: observe existing futures/HTTP responses without changing their results.

use anyhow::Result;
use std::{future::Future, time::Instant};
use tracing::{Instrument, Span};

pub(crate) const TARGET: &str = "calrs::sync_diagnostics";

/// Classify errors without exposing URLs, credentials, SQL values or response bodies.
pub(crate) fn error_kind(error: &anyhow::Error) -> &'static str {
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
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

/// Correlate one existing sync attempt without copying its implementation.
pub(crate) fn sync<T>(
    source_id: &str,
    trigger: &'static str,
    future: impl Future<Output = Result<T>>,
) -> impl Future<Output = Result<T>> {
    Box::pin(
        async move { trace("sync", future).await }.instrument(tracing::info_span!(
            "calendar_sync", sync_id = %uuid::Uuid::new_v4(), source_id, trigger
        )),
    )
}

/// Extension point for the existing CalDAV request chains.
pub(crate) trait RequestDiagnostics {
    fn send_observed(
        self,
        request_kind: &'static str,
    ) -> impl Future<Output = Result<ObservedResponse>> + Send;
}

impl RequestDiagnostics for reqwest::RequestBuilder {
    fn send_observed(
        self,
        request_kind: &'static str,
    ) -> impl Future<Output = Result<ObservedResponse>> + Send {
        async move {
            let response = trace("response_headers", async { Ok(self.send().await?) }).await?;
            tracing::info!(target: TARGET, http_status = response.status().as_u16(), "HTTP response headers received");
            Ok(ObservedResponse { response, span: Span::current() })
        }.instrument(tracing::info_span!(
            "caldav_request", request_id = %uuid::Uuid::new_v4(), request_kind
        ))
    }
}

/// Only observes the two response operations used by CalDAV: status and text.
pub(crate) struct ObservedResponse {
    response: reqwest::Response,
    span: Span,
}

impl ObservedResponse {
    pub(crate) fn status(&self) -> reqwest::StatusCode {
        self.response.status()
    }

    pub(crate) fn text(self) -> impl Future<Output = Result<String>> {
        async move {
            let text = trace("response_body", async { Ok(self.response.text().await?) }).await?;
            tracing::info!(target: TARGET, response_bytes = text.len(), "HTTP response body read");
            Ok(text)
        }
        .instrument(self.span)
    }
}

pub(crate) fn window_start(since_utc: &str) {
    tracing::info!(target: TARGET, window_start = since_utc, "full fetch lower bound");
}

pub(crate) fn event_count(count: usize) {
    tracing::info!(target: TARGET, event_count = count, "CalDAV events fetched");
}

#[cfg(test)]
mod tests;
