use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde::Serialize;

use crate::{
    AppState, PeerAddress, auth::ApiPermission, authorize_business_request, error::AppError,
    repository::TaskPreview,
};
use media_domain::TaskSpec;

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const STYLES_CSS: &str = include_str!("../ui/styles.css");

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(root_redirect))
        .route("/login", get(shell))
        .route("/overview", get(shell))
        .route("/api-docs", get(shell))
        .route("/tasks", get(shell))
        .route("/tasks/{*rest}", get(shell))
        .route("/streams", get(shell))
        .route("/multicast", get(shell))
        .route("/records", get(shell))
        .route("/transcode-artifacts", get(shell))
        .route("/security", get(shell))
        .route("/nodes", get(shell))
        .route("/debug", get(shell))
        .route("/debug/{*rest}", get(shell))
        .route("/assets/app.js", get(app_js))
        .route("/assets/styles.css", get(styles_css))
        .route("/favicon.ico", get(favicon))
}

pub(crate) async fn current_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CurrentSessionResponse>, AppError> {
    let principal = state.auth.session(&headers)?;
    Ok(Json(CurrentSessionResponse {
        auth_enabled: state.auth.enabled(),
        auth_mode: state.auth.mode(),
        subject: principal.subject().to_string(),
        role: principal.role(),
        must_change_password: principal.must_change_password(),
        permissions: principal
            .granted_permissions()
            .into_iter()
            .map(ApiPermission::as_str)
            .collect(),
        environment: state.environment.clone(),
    }))
}

pub(crate) async fn preview_task(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Json(task): Json<TaskSpec>,
) -> Result<Json<TaskPreview>, AppError> {
    authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    Ok(Json(state.repository.preview_task_spec(task).await?))
}

async fn root_redirect() -> Redirect {
    Redirect::temporary("/overview")
}

async fn shell() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn app_js() -> Response {
    asset_response("text/javascript; charset=utf-8", APP_JS)
}

async fn styles_css() -> Response {
    asset_response("text/css; charset=utf-8", STYLES_CSS)
}

async fn favicon() -> impl IntoResponse {
    StatusCode::NO_CONTENT
}

fn asset_response(content_type: &'static str, body: &'static str) -> Response {
    let mut response = body.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-store, must-revalidate"),
    );
    response
}

#[derive(Debug, Serialize)]
pub(crate) struct CurrentSessionResponse {
    auth_enabled: bool,
    auth_mode: crate::config::AuthMode,
    subject: String,
    role: crate::auth::ApiRole,
    must_change_password: bool,
    permissions: Vec<&'static str>,
    environment: String,
}
