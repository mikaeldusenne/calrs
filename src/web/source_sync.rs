use super::*;
use axum::http::StatusCode;

pub(super) async fn enqueue_response(
    state: &AppState,
    id: &str,
    force: bool,
    trigger: &'static str,
) -> Response {
    match crate::sync_jobs::enqueue(&state.pool, &state.secret_key, id, force, trigger).await {
        Ok(_) => Redirect::to(&format!("/dashboard/sources/{id}/sync")).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Could not queue calendar synchronization",
        )
            .into_response(),
    }
}

async fn owned(state: &AppState, user: &crate::auth::AuthUser, id: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM caldav_sources cs
        JOIN accounts a ON a.id = cs.account_id WHERE cs.id = ? AND a.user_id = ?)",
    )
    .bind(id)
    .bind(&user.user.id)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(false)
}

pub(super) async fn start(
    State(state): State<Arc<AppState>>,
    user: crate::auth::AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(csrf): Form<CsrfForm>,
) -> Response {
    start_impl(state, user, headers, id, csrf, false).await
}

pub(super) async fn force(
    State(state): State<Arc<AppState>>,
    user: crate::auth::AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(csrf): Form<CsrfForm>,
) -> Response {
    start_impl(state, user, headers, id, csrf, true).await
}

async fn start_impl(
    state: Arc<AppState>,
    user: crate::auth::AuthUser,
    headers: HeaderMap,
    id: String,
    csrf: CsrfForm,
    force: bool,
) -> Response {
    if let Err(response) = verify_csrf_token(&headers, &csrf._csrf) {
        return response;
    }
    if !owned(&state, &user, &id).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    enqueue_response(&state, &id, force, "dashboard").await
}

#[derive(sqlx::FromRow, serde::Serialize)]
struct Status {
    name: String,
    sync_status: String,
    sync_id: Option<String>,
    sync_stage: Option<String>,
    sync_error: Option<String>,
    sync_http_status: Option<i64>,
    sync_verified_at: Option<String>,
    sync_started_at: Option<String>,
    sync_event_error: Option<String>,
    elapsed: i64,
    needs_setup: bool,
}

pub(super) async fn status(
    State(state): State<Arc<AppState>>,
    user: crate::auth::AuthUser,
    Path(id): Path<String>,
) -> Response {
    if crate::sync_jobs::expire(&state.pool).await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let job = sqlx::query_as::<_, Status>("SELECT name, sync_status, sync_id, sync_stage, sync_error,
        sync_http_status, sync_verified_at, sync_started_at, sync_event_error,
        COALESCE(unixepoch(COALESCE(sync_finished_at, datetime('now'))) - unixepoch(sync_started_at), 0) AS elapsed,
        write_calendar_href IS NULL AND EXISTS (SELECT 1 FROM calendars WHERE source_id = cs.id) AS needs_setup
        FROM caldav_sources cs WHERE id = ?
        AND EXISTS (SELECT 1 FROM accounts a WHERE a.id = cs.account_id AND a.user_id = ?)")
        .bind(&id).bind(&user.user.id).fetch_optional(&state.pool).await;
    let job = match job {
        Ok(Some(job)) => job,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let active = matches!(job.sync_status.as_str(), "queued" | "running");
    let event_error = job
        .sync_event_error
        .as_deref()
        .and_then(|s| serde_json::from_str::<crate::sync_diagnostics::EventFailure>(s).ok());
    let tmpl = match state.templates.get_template("source_sync.html") {
        Ok(t) => t,
        Err(e) => return internal_error_html("template render", &e).into_response(),
    };
    let (impersonating, impersonating_name, _) = impersonation_ctx(&user);
    let state_key = format!("sync-state-{}", job.sync_status);
    let stage_key = format!(
        "sync-stage-{}",
        job.sync_stage
            .as_deref()
            .unwrap_or("queued")
            .replace('_', "-")
    );
    let error_key = format!(
        "sync-error-{}",
        job.sync_error
            .as_deref()
            .unwrap_or("application")
            .replace('_', "-")
    );
    let html = tmpl.render(context! { job => job, source_id => id, active => active,
        state_key => state_key, stage_key => stage_key, error_key => error_key, event_error => event_error,
        sidebar => sidebar_context(&user, "sources"), lang => user.lang,
        impersonating => impersonating, impersonating_name => impersonating_name,
    });
    let mut response = match html {
        Ok(html) => Html(html).into_response(),
        Err(e) => return internal_error_html("template render", &e).into_response(),
    };
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    if active {
        response
            .headers_mut()
            .insert("refresh", "2".parse().unwrap());
    }
    response
}
