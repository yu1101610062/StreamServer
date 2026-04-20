#[cfg(test)]
#[path = "tests/main.rs"]
mod tests;

mod auth;
mod callback;
mod config;
mod control_plane;
mod error;
mod repository;
mod scheduler;
mod telemetry;
mod ui;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{self, Read},
    net::{IpAddr, SocketAddr},
    path::{Component, Path as FsPath, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use auth::{
    ApiPermission, AuthConfig, generate_refresh_token, hash_password, hash_refresh_token,
    maybe_extract_bearer_token, verify_password,
};
use axum::{
    Json, Router,
    extract::{ConnectInfo, FromRequestParts, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::{DateTime, Utc};
use control_plane::{ControlPlaneService, NodeLiveLoad};
use error::AppError;
use repository::{
    AuthUser, CreateTaskResult, HookEventListFilter, MachineAllowlistEntry, MachineAllowlistWrite,
    NewRefreshSession, NodeSummary, RecordListFilter, SecurityAuditEventRecord, StreamListFilter,
    TaskCloneOverride, TaskEventFilter, TaskListFilter, TaskLogFilter, TaskRepository,
    ZlmPublishTaskRecord, ZlmRecordFileRecord, ZlmStreamEventRecord, ZlmTaskEventHookRecord,
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use tokio::{net::TcpListener, sync::watch, time::timeout};
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use uuid::Uuid;

use media_domain::{TaskOperation, TaskSpec};

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    repository: Arc<TaskRepository>,
    control_plane: ControlPlaneService,
    started_at: DateTime<Utc>,
    environment: String,
    auth: AuthConfig,
    http_client: Client,
    hook_shared_secret: String,
    hook_source_allowlist: Vec<IpAddr>,
    zlm_auto_close_on_no_reader_enabled: bool,
    storage_allowlist: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PeerAddress(pub Option<SocketAddr>);

impl<S> FromRequestParts<S> for PeerAddress
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|value| value.0);
        std::future::ready(Ok(Self(peer)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliCommand {
    Help { auth_only: bool },
    BootstrapAdmin { username: String },
    ResetPassword { username: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Some(command) = parse_cli_command()? {
        return match command {
            CliCommand::Help { auth_only } => {
                print_cli_help(auth_only);
                Ok(())
            }
            other => run_cli_command(other).await,
        };
    }

    let settings = config::Settings::load()?;
    telemetry::init(&settings.logging);

    info!(
        environment = %settings.environment,
        http_addr = %settings.core.http_addr,
        grpc_addr = %settings.core.grpc_addr,
        "starting media-core"
    );

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&settings.core.database_url)
        .await?;
    sqlx::migrate!("../../migrations").run(&pool).await?;

    let repository = Arc::new(TaskRepository::with_callback_settle_delay(
        pool,
        chrono::Duration::milliseconds(settings.core.callback_settle_delay_ms as i64),
    ));
    let control_plane = ControlPlaneService::new(repository.clone());
    let hook_source_allowlist = parse_hook_source_allowlist(&settings.core.hook_source_allowlist)?;
    let auth = AuthConfig::from_settings(&settings.core)?;
    if auth.supports_local_login() && !repository.has_enabled_admin_user().await? {
        anyhow::bail!(
            "local_password auth mode requires at least one enabled admin user; run `media-core auth bootstrap-admin --username <name> --password-stdin` first"
        );
    }

    let state = AppState {
        repository: repository.clone(),
        control_plane: control_plane.clone(),
        started_at: Utc::now(),
        environment: settings.environment.clone(),
        auth,
        http_client: Client::new(),
        hook_shared_secret: settings.core.hook_shared_secret.clone(),
        hook_source_allowlist,
        zlm_auto_close_on_no_reader_enabled: settings.core.zlm_auto_close_on_no_reader_enabled,
        storage_allowlist: settings.core.storage_allowlist.clone(),
    };

    let app = build_app(state);

    let listener = TcpListener::bind(&settings.core.http_addr).await?;
    info!(listen_addr = %listener.local_addr()?, "media-core http server ready");

    let grpc_addr = settings.core.grpc_addr.parse()?;
    let control_plane_server = control_plane.clone().into_server();
    info!(listen_addr = %settings.core.grpc_addr, "media-core grpc server ready");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let scheduler_handle = scheduler::spawn(
        repository.clone(),
        control_plane.clone(),
        shutdown_rx.clone(),
    );
    let callback_handle = callback::spawn(
        repository.clone(),
        Client::new(),
        callback::CallbackConfig {
            timeout: std::time::Duration::from_millis(settings.core.callback_timeout_ms),
            max_attempts: settings.core.callback_max_attempts,
            initial_backoff: std::time::Duration::from_millis(
                settings.core.callback_initial_backoff_ms,
            ),
            max_backoff: std::time::Duration::from_millis(settings.core.callback_max_backoff_ms),
            shared_secret: (!settings.core.callback_shared_secret.trim().is_empty())
                .then(|| settings.core.callback_shared_secret.clone()),
        },
        shutdown_rx.clone(),
    );
    let signal_handle = tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    let http_server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown(shutdown_rx.clone()));
    let mut grpc_builder = Server::builder();
    if let Some(tls_config) = load_grpc_tls_config(&settings.core)? {
        grpc_builder = grpc_builder.tls_config(tls_config)?;
    }
    let grpc_server = grpc_builder
        .add_service(control_plane_server)
        .serve_with_shutdown(grpc_addr, wait_for_shutdown(shutdown_rx.clone()));

    let (http_result, grpc_result) = tokio::join!(http_server, grpc_server);
    http_result?;
    grpc_result?;
    let _ = scheduler_handle.await;
    let _ = callback_handle.await;
    let _ = signal_handle.await;

    Ok(())
}

pub(crate) fn build_app(state: AppState) -> Router {
    let api_router = Router::new()
        .route("/me", get(ui::current_session))
        .route("/auth/login", post(auth_login))
        .route("/auth/refresh", post(auth_refresh))
        .route("/auth/logout", post(auth_logout))
        .route("/auth/change-password", post(auth_change_password))
        .route(
            "/security/machine-allowlist",
            get(list_machine_allowlist).put(update_machine_allowlist),
        )
        .route("/tasks/preview", post(ui::preview_task))
        .route("/tasks", post(create_task).get(list_tasks))
        .route("/tasks/{id}", get(get_task).delete(delete_task))
        .route("/tasks/{id}/events", get(get_task_events))
        .route("/tasks/{id}/logs", get(get_task_logs))
        .route("/tasks/{id}/resolved-spec", get(get_resolved_spec))
        .route("/tasks/{id}/start", post(start_task))
        .route("/tasks/{id}/stop", post(stop_task))
        .route("/tasks/{id}/cancel", post(cancel_task))
        .route("/tasks/{id}/retry", post(retry_task))
        .route("/tasks/{id}/clone", post(clone_task))
        .route("/streams", get(list_streams))
        .route("/records", get(list_records))
        .route("/file-artifacts", get(list_file_artifacts))
        .route("/nodes", get(list_nodes))
        .route("/nodes/{id}/heartbeats", get(list_node_heartbeats))
        .route("/debug/hooks", get(list_debug_hooks))
        .route("/debug/zlm/media", get(debug_zlm_media))
        .route("/debug/zlm/sessions", get(debug_zlm_sessions))
        .route("/debug/zlm/players", get(debug_zlm_players))
        .route("/debug/zlm/statistic", get(debug_zlm_statistic))
        .route("/debug/zlm/threads-load", get(debug_zlm_threads_load))
        .route(
            "/debug/zlm/work-threads-load",
            get(debug_zlm_work_threads_load),
        )
        .route("/debug/zlm/kick-session", post(debug_zlm_kick_session))
        .route("/debug/zlm/kick-sessions", post(debug_zlm_kick_sessions))
        .route("/debug/zlm/close-stream", post(debug_zlm_close_stream))
        .route("/debug/zlm/snap", get(debug_zlm_snap));

    Router::new()
        .route("/health/live", get(live_health))
        .route("/health/ready", get(ready_health))
        .route("/internal/hooks/zlm/{server_id}", post(receive_zlm_hook))
        .route(
            "/internal/hooks/zlm/{server_id}/{hook_name}",
            post(receive_named_zlm_hook),
        )
        .nest("/api/v1", api_router)
        .merge(ui::router())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn live_health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        started_at: state.started_at,
        environment: state.environment,
    })
}

async fn ready_health(State(state): State<AppState>) -> Result<Json<HealthResponse>, AppError> {
    state.repository.health_check().await?;
    Ok(Json(HealthResponse {
        status: "ready",
        started_at: state.started_at,
        environment: state.environment,
    }))
}

pub(crate) async fn authorize_business_request(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    permission: ApiPermission,
) -> Result<auth::AuthenticatedPrincipal, AppError> {
    if !state.auth.enabled() {
        return state.auth.authorize(headers, permission);
    }

    match maybe_extract_bearer_token(headers)? {
        Some(_) => state.auth.authorize(headers, permission),
        None => {
            let peer_ip = peer.map(|addr| addr.ip()).ok_or_else(|| {
                AppError::Forbidden(
                    "missing Authorization header and peer address is unavailable".to_string(),
                )
            })?;
            let allowed = state.repository.is_machine_ip_allowlisted(peer_ip).await?;
            if !allowed {
                return Err(AppError::Forbidden(
                    "missing Authorization header and client IP is not allowlisted".to_string(),
                ));
            }
            let principal = auth::AuthenticatedPrincipal::machine_allowlisted(&peer_ip.to_string());
            principal.require_permission(permission)?;
            Ok(principal)
        }
    }
}

fn require_local_password_login(state: &AppState) -> Result<(), AppError> {
    if state.auth.supports_local_login() {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "local password authentication is not enabled".to_string(),
        ))
    }
}

fn user_agent_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn normalize_username_value(username: &str) -> anyhow::Result<String> {
    let normalized = username.trim().to_lowercase();
    anyhow::ensure!(!normalized.is_empty(), "username must not be empty");
    anyhow::ensure!(
        normalized
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '@')),
        "username contains unsupported characters"
    );
    Ok(normalized)
}

fn normalize_username(username: &str) -> Result<String, AppError> {
    normalize_username_value(username)
        .map_err(|error| AppError::BadRequest(format!("invalid username: {error}")))
}

fn normalize_machine_allowlist_entries(
    entries: Vec<MachineAllowlistWrite>,
) -> Result<Vec<MachineAllowlistWrite>, AppError> {
    entries
        .into_iter()
        .map(|entry| {
            let cidr = normalize_machine_allowlist_cidr(&entry.cidr)?;
            Ok(MachineAllowlistWrite {
                cidr,
                description: entry.description.trim().to_string(),
            })
        })
        .collect()
}

fn normalize_machine_allowlist_cidr(value: &str) -> Result<String, AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest(
            "machine allowlist cidr must not be empty".to_string(),
        ));
    }
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return Ok(match ip {
            IpAddr::V4(ip) => format!("{ip}/32"),
            IpAddr::V6(ip) => format!("{ip}/128"),
        });
    }

    let (ip_text, prefix_text) = trimmed.split_once('/').ok_or_else(|| {
        AppError::BadRequest(format!(
            "machine allowlist entry `{trimmed}` must be an IP or CIDR"
        ))
    })?;
    let ip = ip_text.parse::<IpAddr>().map_err(|_| {
        AppError::BadRequest(format!(
            "machine allowlist entry `{trimmed}` has an invalid IP address"
        ))
    })?;
    let prefix: u8 = prefix_text.parse().map_err(|_| {
        AppError::BadRequest(format!(
            "machine allowlist entry `{trimmed}` has an invalid prefix length"
        ))
    })?;
    let max_prefix = match ip {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    if prefix > max_prefix {
        return Err(AppError::BadRequest(format!(
            "machine allowlist entry `{trimmed}` exceeds prefix length {max_prefix}"
        )));
    }
    Ok(format!("{ip}/{prefix}"))
}

fn auth_user_role(user: &AuthUser) -> Result<auth::ApiRole, AppError> {
    match user.role.trim() {
        "admin" => Ok(auth::ApiRole::Admin),
        other => Err(AppError::Internal(format!(
            "unsupported auth user role stored in database: {other}"
        ))),
    }
}

fn invalid_credentials_error() -> AppError {
    AppError::Forbidden("invalid username or password".to_string())
}

async fn record_security_event(
    state: &AppState,
    event_type: &str,
    actor: &str,
    subject: Option<&str>,
    remote_ip: Option<IpAddr>,
    user_agent: Option<&str>,
    payload: Value,
) -> Result<(), AppError> {
    state
        .repository
        .insert_security_audit_event(SecurityAuditEventRecord {
            event_type: event_type.to_string(),
            actor: actor.to_string(),
            subject: subject.map(str::to_string),
            remote_ip,
            user_agent: user_agent.map(str::to_string),
            payload,
        })
        .await?;
    Ok(())
}

fn build_auth_tokens_response(
    user: &AuthUser,
    issued: auth::IssuedAccessToken,
    refresh_token: String,
    refresh_expires_at: DateTime<Utc>,
) -> AuthTokensResponse {
    AuthTokensResponse {
        access_token: issued.token,
        access_token_expires_at: issued.expires_at,
        refresh_token,
        refresh_token_expires_at: refresh_expires_at,
        subject: user.username.clone(),
        role: auth::ApiRole::Admin,
        must_change_password: user.must_change_password,
    }
}

async fn auth_login(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Json(request): Json<AuthLoginRequest>,
) -> Result<Json<AuthTokensResponse>, AppError> {
    require_local_password_login(&state)?;
    let remote_ip = peer.map(|addr| addr.ip());
    let user_agent = user_agent_from_headers(&headers);
    let username = normalize_username(&request.username)?;
    let Some(user) = state
        .repository
        .find_auth_user_by_username(&username)
        .await?
    else {
        record_security_event(
            &state,
            "login_failed",
            &username,
            Some(&username),
            remote_ip,
            user_agent.as_deref(),
            json!({ "reason": "invalid_credentials" }),
        )
        .await?;
        return Err(invalid_credentials_error());
    };
    if !user.enabled {
        record_security_event(
            &state,
            "login_failed",
            &username,
            Some(&username),
            remote_ip,
            user_agent.as_deref(),
            json!({ "reason": "user_disabled" }),
        )
        .await?;
        return Err(invalid_credentials_error());
    }
    let _role = auth_user_role(&user)?;
    if !verify_password(&user.password_hash, &request.password)
        .map_err(|error| AppError::Internal(format!("failed to verify password: {error}")))?
    {
        record_security_event(
            &state,
            "login_failed",
            &username,
            Some(&username),
            remote_ip,
            user_agent.as_deref(),
            json!({ "reason": "invalid_credentials" }),
        )
        .await?;
        return Err(invalid_credentials_error());
    }

    let now = Utc::now();
    let refresh_token = generate_refresh_token();
    let refresh_expires_at = now + state.auth.refresh_token_ttl();
    let issued = state
        .auth
        .issue_access_token(&user.username, auth::ApiRole::Admin)
        .map_err(|error| AppError::Internal(format!("failed to issue access token: {error}")))?;
    state
        .repository
        .insert_refresh_session(NewRefreshSession {
            id: Uuid::now_v7(),
            user_id: user.id,
            token_hash: hash_refresh_token(&refresh_token),
            expires_at: refresh_expires_at,
            created_at: now,
            client_ip: remote_ip,
            user_agent: user_agent.clone(),
        })
        .await?;
    state.repository.touch_auth_user_login(user.id, now).await?;
    record_security_event(
        &state,
        "login_succeeded",
        &user.username,
        Some(&user.username),
        remote_ip,
        user_agent.as_deref(),
        json!({}),
    )
    .await?;

    Ok(Json(build_auth_tokens_response(
        &user,
        issued,
        refresh_token,
        refresh_expires_at,
    )))
}

async fn auth_refresh(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Json(request): Json<AuthRefreshRequest>,
) -> Result<Json<AuthTokensResponse>, AppError> {
    require_local_password_login(&state)?;
    let remote_ip = peer.map(|addr| addr.ip());
    let user_agent = user_agent_from_headers(&headers);
    let refresh_token = request.refresh_token.trim();
    if refresh_token.is_empty() {
        return Err(AppError::BadRequest(
            "refresh_token must not be empty".to_string(),
        ));
    }
    let now = Utc::now();
    let Some(session) = state
        .repository
        .find_refresh_session(&hash_refresh_token(refresh_token))
        .await?
    else {
        return Err(AppError::Forbidden("invalid refresh token".to_string()));
    };
    if session.revoked_at.is_some() || session.expires_at <= now || !session.user.enabled {
        return Err(AppError::Forbidden("invalid refresh token".to_string()));
    }
    let _role = auth_user_role(&session.user)?;

    let next_refresh_token = generate_refresh_token();
    let next_refresh_expires_at = now + state.auth.refresh_token_ttl();
    let issued = state
        .auth
        .issue_access_token(&session.user.username, auth::ApiRole::Admin)
        .map_err(|error| AppError::Internal(format!("failed to issue access token: {error}")))?;
    state
        .repository
        .rotate_refresh_session(
            session.id,
            &hash_refresh_token(&next_refresh_token),
            next_refresh_expires_at,
            now,
            remote_ip,
            user_agent.as_deref(),
        )
        .await?;
    record_security_event(
        &state,
        "refresh_succeeded",
        &session.user.username,
        Some(&session.user.username),
        remote_ip,
        user_agent.as_deref(),
        json!({}),
    )
    .await?;

    Ok(Json(build_auth_tokens_response(
        &session.user,
        issued,
        next_refresh_token,
        next_refresh_expires_at,
    )))
}

async fn auth_logout(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Json(request): Json<AuthLogoutRequest>,
) -> Result<StatusCode, AppError> {
    require_local_password_login(&state)?;
    let remote_ip = peer.map(|addr| addr.ip());
    let user_agent = user_agent_from_headers(&headers);
    let refresh_token = request.refresh_token.trim();
    if refresh_token.is_empty() {
        return Err(AppError::BadRequest(
            "refresh_token must not be empty".to_string(),
        ));
    }
    if let Some(session) = state
        .repository
        .find_refresh_session(&hash_refresh_token(refresh_token))
        .await?
    {
        state
            .repository
            .revoke_refresh_session(&session.token_hash, Utc::now())
            .await?;
        record_security_event(
            &state,
            "logout",
            &session.user.username,
            Some(&session.user.username),
            remote_ip,
            user_agent.as_deref(),
            json!({}),
        )
        .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn auth_change_password(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Json(request): Json<AuthChangePasswordRequest>,
) -> Result<StatusCode, AppError> {
    require_local_password_login(&state)?;
    let principal = state.auth.session(&headers)?;
    if principal.is_machine() {
        return Err(AppError::Forbidden(
            "machine allowlisted callers cannot change passwords".to_string(),
        ));
    }
    let remote_ip = peer.map(|addr| addr.ip());
    let user_agent = user_agent_from_headers(&headers);
    let username = normalize_username(principal.subject())?;
    let user = state
        .repository
        .find_auth_user_by_username(&username)
        .await?
        .ok_or_else(|| AppError::Forbidden("current account no longer exists".to_string()))?;
    if !user.enabled {
        return Err(AppError::Forbidden(
            "current account is disabled".to_string(),
        ));
    }
    if !verify_password(&user.password_hash, &request.current_password)
        .map_err(|error| AppError::Internal(format!("failed to verify password: {error}")))?
    {
        return Err(AppError::Forbidden(
            "current password is incorrect".to_string(),
        ));
    }
    let next_password_hash = hash_password(&request.new_password)
        .map_err(|error| AppError::BadRequest(error.to_string()))?;
    state
        .repository
        .reset_user_password(
            &username,
            &next_password_hash,
            false,
            &username,
            "password_changed",
            remote_ip,
            user_agent.as_deref(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_machine_allowlist(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MachineAllowlistResponse>, AppError> {
    let _principal = state
        .auth
        .authorize(&headers, ApiPermission::SecurityWrite)?;
    let entries = state.repository.list_machine_allowlist().await?;
    Ok(Json(MachineAllowlistResponse { entries }))
}

async fn update_machine_allowlist(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<UpdateMachineAllowlistRequest>,
) -> Result<Json<MachineAllowlistResponse>, AppError> {
    let principal = state
        .auth
        .authorize(&headers, ApiPermission::SecurityWrite)?;
    let entries = normalize_machine_allowlist_entries(request.entries)?;
    state.repository.replace_machine_allowlist(&entries).await?;
    record_security_event(
        &state,
        "machine_allowlist_updated",
        principal.subject(),
        Some(principal.subject()),
        None,
        user_agent_from_headers(&headers).as_deref(),
        json!({ "entries": entries.iter().map(|entry| &entry.cidr).collect::<Vec<_>>() }),
    )
    .await?;
    let updated = state.repository.list_machine_allowlist().await?;
    Ok(Json(MachineAllowlistResponse { entries: updated }))
}

async fn create_task(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Json(task): Json<TaskSpec>,
) -> Result<(StatusCode, Json<repository::TaskSummary>), AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    let idempotency_key = extract_idempotency_key(&headers)?;
    let request_hash = hash_json(&task)?;

    match state
        .repository
        .create_task(&idempotency_key, &request_hash, task)
        .await?
    {
        CreateTaskResult::Fresh(task) => Ok((
            StatusCode::CREATED,
            Json(maybe_dispatch_immediate_task(&state, task).await?),
        )),
        CreateTaskResult::Replay(task) => Ok((StatusCode::OK, Json(task))),
    }
}

async fn list_tasks(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Query(filter): Query<TaskListFilter>,
) -> Result<Json<media_domain::Page<repository::TaskSummary>>, AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskRead).await?;
    let tasks = state.repository.list_tasks(filter).await?;
    Ok(Json(tasks))
}

async fn get_task(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
) -> Result<Json<repository::TaskDetail>, AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskRead).await?;
    let task = state.repository.get_task(task_id).await?;
    Ok(Json(task))
}

async fn delete_task(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
) -> Result<Json<repository::TaskSummary>, AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    let task = state.repository.delete_task(task_id).await?;
    Ok(Json(task))
}

async fn get_resolved_spec(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskRead).await?;
    let spec = state.repository.get_resolved_spec(task_id).await?;
    Ok(Json(spec))
}

async fn start_task(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
) -> Result<(StatusCode, Json<repository::TaskSummary>), AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    let current = state.repository.get_task_summary(task_id).await?;
    match current.status {
        media_domain::TaskStatus::Created
        | media_domain::TaskStatus::Failed
        | media_domain::TaskStatus::Canceled => {
            state
                .repository
                .transition_task(task_id, TaskOperation::Start)
                .await?;
        }
        media_domain::TaskStatus::Validating | media_domain::TaskStatus::Queued => {}
        _ => {
            return Err(AppError::Repository(repository::RepoError::TaskState(
                media_domain::TaskStateError::InvalidOperation {
                    operation: TaskOperation::Start,
                    status: current.status,
                },
            )));
        }
    }

    state.control_plane.dispatch_task(task_id).await?;
    let task = state.repository.get_task_summary(task_id).await?;
    Ok((StatusCode::ACCEPTED, Json(task)))
}

async fn stop_task(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
) -> Result<(StatusCode, Json<repository::TaskSummary>), AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    let current = state.repository.get_task_summary(task_id).await?;
    if current.status == media_domain::TaskStatus::Stopping {
        let stop_intent_persisted = if current.current_attempt_no > 0 {
            state
                .repository
                .attempt_has_stop_intent(task_id, current.current_attempt_no)
                .await?
        } else {
            true
        };
        if stop_intent_persisted {
            return Ok((StatusCode::ACCEPTED, Json(current)));
        }

        state
            .control_plane
            .request_stop(task_id, "user_requested", 30, 5)
            .await?;
        let task = state.repository.get_task_summary(task_id).await?;
        return Ok((StatusCode::ACCEPTED, Json(task)));
    }

    let task = state
        .repository
        .transition_task(task_id, TaskOperation::Stop)
        .await?;
    state
        .control_plane
        .request_stop(task_id, "user_requested", 30, 5)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(task)))
}

async fn cancel_task(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
) -> Result<(StatusCode, Json<repository::TaskSummary>), AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    let task = state
        .repository
        .transition_task(task_id, TaskOperation::Cancel)
        .await?;
    if task.status == media_domain::TaskStatus::Stopping {
        state
            .control_plane
            .request_stop(task_id, "user_canceled", 30, 5)
            .await?;
    }
    Ok((StatusCode::ACCEPTED, Json(task)))
}

async fn retry_task(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
) -> Result<(StatusCode, Json<repository::AttemptSummary>), AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    let attempt = state.repository.retry_task(task_id).await?;
    Ok((StatusCode::ACCEPTED, Json(attempt)))
}

async fn clone_task(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
    overrides: Option<Json<TaskCloneOverride>>,
) -> Result<(StatusCode, Json<repository::TaskSummary>), AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    let task = state
        .repository
        .clone_task(task_id, overrides.map(|Json(value)| value))
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(maybe_dispatch_immediate_task(&state, task).await?),
    ))
}

async fn get_task_events(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
    Query(filter): Query<TaskEventFilter>,
) -> Result<Json<media_domain::Page<repository::TaskEventSummary>>, AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskRead).await?;
    Ok(Json(
        state.repository.list_task_events(task_id, filter).await?,
    ))
}

async fn get_task_logs(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
    Query(filter): Query<TaskLogFilter>,
) -> Result<Json<repository::TaskLogResponse>, AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskRead).await?;
    Ok(Json(
        state.repository.list_task_logs(task_id, filter).await?,
    ))
}

async fn list_streams(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Query(filter): Query<StreamListFilter>,
) -> Result<Json<Vec<repository::StreamSummary>>, AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskRead).await?;
    let expected_has_viewer = filter.has_viewer;
    let mut streams = state.repository.list_streams(filter).await?;
    if streams.is_empty() {
        return Ok(Json(streams));
    }

    let node_lookup = state
        .repository
        .list_nodes()
        .await?
        .into_iter()
        .map(|node| (node.id, node))
        .collect::<HashMap<_, _>>();
    apply_stream_runtime_fallbacks(&mut streams, &node_lookup);
    let stale_indexes = enrich_streams_with_runtime(&state, &mut streams, &node_lookup).await;
    if !stale_indexes.is_empty() {
        streams = streams
            .into_iter()
            .enumerate()
            .filter_map(|(index, stream)| (!stale_indexes.contains(&index)).then_some(stream))
            .collect();
    }
    streams = collapse_duplicate_streams(streams);
    if let Some(expected_has_viewer) = expected_has_viewer {
        streams.retain(|stream| stream_has_viewers(stream) == Some(expected_has_viewer));
    }
    Ok(Json(streams))
}

async fn list_records(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Query(filter): Query<RecordListFilter>,
) -> Result<Json<media_domain::Page<repository::RecordFileSummary>>, AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::RecordRead).await?;
    Ok(Json(state.repository.list_record_files(filter).await?))
}

async fn list_file_artifacts(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Query(filter): Query<repository::FileArtifactListFilter>,
) -> Result<Json<media_domain::Page<repository::FileArtifactSummary>>, AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::RecordRead).await?;
    Ok(Json(state.repository.list_file_artifacts(filter).await?))
}

async fn list_nodes(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<repository::NodeSummary>>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::NodeRead)?;
    let mut nodes = state.repository.list_nodes().await?;
    let live_loads = state.control_plane.current_node_loads().await;
    for node in &mut nodes {
        apply_live_load(node, live_loads.get(&node.id).cloned());
    }
    Ok(Json(nodes))
}

async fn list_node_heartbeats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(node_id): Path<Uuid>,
    Query(query): Query<NodeHeartbeatQuery>,
) -> Result<Json<Vec<repository::NodeHeartbeatSummary>>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::NodeRead)?;
    Ok(Json(
        state
            .repository
            .list_node_heartbeats(node_id, query.limit.unwrap_or(24))
            .await?,
    ))
}

async fn list_debug_hooks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HookEventListFilter>,
) -> Result<Json<Vec<repository::HookEventSummary>>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::DebugRead)?;
    Ok(Json(state.repository.list_hook_events(query).await?))
}

async fn debug_zlm_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ZlmMediaQuery>,
) -> Result<Json<Value>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::DebugRead)?;
    Ok(Json(
        call_zlm_api(
            &state,
            query.node_id,
            "/index/api/getMediaList",
            debug_media_query_params(&query),
        )
        .await?,
    ))
}

async fn debug_zlm_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NodeScopedQuery>,
) -> Result<Json<Value>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::DebugRead)?;
    Ok(Json(
        call_zlm_api(
            &state,
            query.node_id,
            "/index/api/getAllSession",
            Vec::new(),
        )
        .await?,
    ))
}

async fn debug_zlm_players(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NodeScopedQuery>,
) -> Result<Json<Value>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::DebugRead)?;
    Ok(Json(
        call_zlm_api(
            &state,
            query.node_id,
            "/index/api/getMediaPlayerList",
            Vec::new(),
        )
        .await?,
    ))
}

async fn debug_zlm_statistic(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NodeScopedQuery>,
) -> Result<Json<Value>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::DebugRead)?;
    Ok(Json(
        call_zlm_api(&state, query.node_id, "/index/api/getStatistic", Vec::new()).await?,
    ))
}

async fn debug_zlm_threads_load(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NodeScopedQuery>,
) -> Result<Json<Value>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::DebugRead)?;
    Ok(Json(
        call_zlm_api(
            &state,
            query.node_id,
            "/index/api/getThreadsLoad",
            Vec::new(),
        )
        .await?,
    ))
}

async fn debug_zlm_work_threads_load(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NodeScopedQuery>,
) -> Result<Json<Value>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::DebugRead)?;
    Ok(Json(
        call_zlm_api(
            &state,
            query.node_id,
            "/index/api/getWorkThreadsLoad",
            Vec::new(),
        )
        .await?,
    ))
}

async fn debug_zlm_kick_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DebugKickSessionRequest>,
) -> Result<Json<Value>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::DebugRead)?;
    Ok(Json(
        call_zlm_api(
            &state,
            request.node_id,
            "/index/api/kick_session",
            vec![("id".to_string(), request.session_id)],
        )
        .await?,
    ))
}

async fn debug_zlm_kick_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DebugKickSessionsRequest>,
) -> Result<Json<Value>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::DebugRead)?;
    let mut params = Vec::new();
    if let Some(local_port) = request.local_port {
        params.push(("local_port".to_string(), local_port.to_string()));
    }
    if let Some(peer_ip) = request
        .peer_ip
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        params.push(("peer_ip".to_string(), peer_ip.to_string()));
    }
    Ok(Json(
        call_zlm_api(&state, request.node_id, "/index/api/kick_sessions", params).await?,
    ))
}

async fn debug_zlm_close_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DebugCloseStreamRequest>,
) -> Result<Json<Value>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::DebugRead)?;
    Ok(Json(
        call_zlm_api(
            &state,
            request.node_id,
            "/index/api/close_streams",
            vec![
                ("schema".to_string(), request.schema),
                ("vhost".to_string(), request.vhost),
                ("app".to_string(), request.app),
                ("stream".to_string(), request.stream),
                ("force".to_string(), request.force.to_string()),
            ],
        )
        .await?,
    ))
}

async fn debug_zlm_snap(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DebugSnapQuery>,
) -> Result<Json<DebugSnapResponse>, AppError> {
    let _principal = state.auth.authorize(&headers, ApiPermission::DebugRead)?;
    let (content_type, body) = call_zlm_binary_api(
        &state,
        query.node_id,
        "/index/api/getSnap",
        vec![
            ("url".to_string(), query.url),
            ("timeout_sec".to_string(), query.timeout_sec.to_string()),
            ("expire_sec".to_string(), query.expire_sec.to_string()),
        ],
    )
    .await?;
    Ok(Json(DebugSnapResponse {
        content_type: content_type.clone(),
        data_url: format!(
            "data:{content_type};base64,{}",
            BASE64_STANDARD.encode(body)
        ),
    }))
}

fn apply_live_load(node: &mut repository::NodeSummary, load: Option<NodeLiveLoad>) {
    if let Some(load) = load {
        node.slot_usage = Some(load.slot_usage);
        node.running_tasks = Some(load.running_tasks);
        node.starting_tasks = Some(load.starting_tasks);
        node.stopping_tasks = Some(load.stopping_tasks);
        node.orphaned_tasks = Some(load.orphaned_tasks);
        node.connected = Some(load.connected);
        node.control_connected = load.connected;
        node.cpu_percent = Some(load.cpu_percent);
        node.mem_percent = Some(load.mem_percent);
        node.disk_percent = Some(load.disk_percent);
        node.zlm_alive = Some(load.zlm_alive);
        node.ffmpeg_alive = Some(load.ffmpeg_alive);
        node.gpu_runtime = Some(load.gpu_runtime);
        node.healthy = node.control_connected;
    } else {
        node.connected = Some(false);
        node.control_connected = false;
        node.healthy = false;
    }
}

#[derive(Debug, Default, Clone)]
struct StreamRuntimeInfo {
    viewer_count: u32,
    bitrate_kbps: f64,
    schemas: BTreeSet<String>,
}

const STREAM_RUNTIME_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

fn apply_stream_runtime_fallbacks(
    streams: &mut [repository::StreamSummary],
    nodes: &HashMap<Uuid, NodeSummary>,
) {
    for stream in streams {
        let Some(node_id) = stream.node_id else {
            continue;
        };
        let Some(node) = nodes.get(&node_id) else {
            continue;
        };
        if stream.play_urls.is_empty() {
            stream.play_urls = build_fallback_play_urls(
                &node.agent_stream_addr,
                &stream.schema,
                &stream.app,
                &stream.stream,
            );
        }
    }
}

async fn enrich_streams_with_runtime(
    state: &AppState,
    streams: &mut [repository::StreamSummary],
    nodes: &HashMap<Uuid, NodeSummary>,
) -> HashSet<usize> {
    let mut stream_indexes_by_node = HashMap::<Uuid, Vec<usize>>::new();
    let mut stale_indexes = HashSet::new();
    for (index, stream) in streams.iter().enumerate() {
        if let Some(node_id) = stream.node_id {
            stream_indexes_by_node
                .entry(node_id)
                .or_default()
                .push(index);
        }
    }

    for (node_id, indexes) in stream_indexes_by_node {
        let Some(node) = nodes.get(&node_id) else {
            continue;
        };
        match timeout(
            STREAM_RUNTIME_LOOKUP_TIMEOUT,
            load_zlm_media_index(state, node_id),
        )
        .await
        {
            Ok(Ok(index)) => {
                for stream_index in indexes {
                    let stream = &mut streams[stream_index];
                    let key = (
                        stream.vhost.clone(),
                        stream.app.clone(),
                        stream.stream.clone(),
                    );
                    if let Some(runtime) = index.get(&key) {
                        stream.viewer_count = Some(runtime.viewer_count);
                        stream.has_viewer = Some(runtime.viewer_count > 0);
                        if runtime.bitrate_kbps > 0.0 {
                            stream.bitrate_kbps = Some(runtime.bitrate_kbps);
                        }
                        stream.play_urls = build_play_urls(
                            &node.agent_stream_addr,
                            &runtime.schemas,
                            &stream.app,
                            &stream.stream,
                        );
                    } else {
                        stale_indexes.insert(stream_index);
                    }
                }
            }
            Ok(index) => {
                let error = index.expect_err("ok result handled above");
                warn!(
                    node_id = %node_id,
                    error = %error,
                    "failed to enrich stream runtime from ZLM; returning fallback stream data"
                );
            }
            Err(_) => {
                warn!(
                    node_id = %node_id,
                    timeout_ms = STREAM_RUNTIME_LOOKUP_TIMEOUT.as_millis(),
                    "timed out enriching stream runtime from ZLM; returning fallback stream data"
                );
            }
        }
    }

    stale_indexes
}

fn stream_has_viewers(stream: &repository::StreamSummary) -> Option<bool> {
    stream
        .viewer_count
        .map(|count| count > 0)
        .or(stream.has_viewer)
}

fn collapse_duplicate_streams(
    streams: Vec<repository::StreamSummary>,
) -> Vec<repository::StreamSummary> {
    let mut collapsed = Vec::with_capacity(streams.len());
    let mut indexes = HashMap::new();

    for stream in streams {
        let key = (
            stream.task_id,
            stream.attempt_id,
            stream.vhost.clone(),
            stream.app.clone(),
            stream.stream.clone(),
        );
        if let Some(index) = indexes.get(&key).copied() {
            merge_stream_summary(&mut collapsed[index], stream);
        } else {
            indexes.insert(key, collapsed.len());
            collapsed.push(stream);
        }
    }

    collapsed
}

fn merge_stream_summary(
    existing: &mut repository::StreamSummary,
    incoming: repository::StreamSummary,
) {
    if existing.zlm_proxy_key.is_none() {
        existing.zlm_proxy_key = incoming.zlm_proxy_key;
    }
    if existing.zlm_pusher_key.is_none() {
        existing.zlm_pusher_key = incoming.zlm_pusher_key;
    }
    if existing.rtp_stream_id.is_none() {
        existing.rtp_stream_id = incoming.rtp_stream_id;
    }
    if existing.started_at.is_none() {
        existing.started_at = incoming.started_at;
    }
    existing.updated_at = existing.updated_at.max(incoming.updated_at);
    existing.viewer_count = match (existing.viewer_count, incoming.viewer_count) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    };
    existing.bitrate_kbps = match (existing.bitrate_kbps, incoming.bitrate_kbps) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    };
    existing.has_viewer = match (existing.has_viewer, incoming.has_viewer) {
        (Some(left), Some(right)) => Some(left || right),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    };

    if !incoming.play_urls.is_empty() {
        let mut merged = existing.play_urls.iter().cloned().collect::<BTreeSet<_>>();
        merged.extend(incoming.play_urls);
        existing.play_urls = merged.into_iter().collect();
    }
}

async fn load_zlm_media_index(
    state: &AppState,
    node_id: Uuid,
) -> Result<HashMap<(String, String, String), StreamRuntimeInfo>, AppError> {
    let body = call_zlm_api(state, node_id, "/index/api/getMediaList", Vec::new()).await?;
    Ok(build_stream_runtime_index(&body))
}

fn build_stream_runtime_index(
    body: &Value,
) -> HashMap<(String, String, String), StreamRuntimeInfo> {
    let mut index = HashMap::new();
    let Some(items) = body.get("data").and_then(Value::as_array) else {
        return index;
    };

    for item in items {
        let Some(vhost) = item.get("vhost").and_then(Value::as_str) else {
            continue;
        };
        let Some(app) = item.get("app").and_then(Value::as_str) else {
            continue;
        };
        let Some(stream) = item.get("stream").and_then(Value::as_str) else {
            continue;
        };
        let key = (vhost.to_string(), app.to_string(), stream.to_string());
        let entry = index.entry(key).or_insert_with(StreamRuntimeInfo::default);
        entry.viewer_count = entry
            .viewer_count
            .max(value_to_u32(item.get("totalReaderCount")).unwrap_or_default());
        entry.bitrate_kbps = entry
            .bitrate_kbps
            .max(value_to_f64(item.get("bytesSpeed")).unwrap_or(0.0) * 8.0 / 1000.0);
        if let Some(schema) = item.get("schema").and_then(Value::as_str) {
            entry.schemas.insert(schema.trim().to_string());
        }
    }

    index
}

pub(crate) fn build_fallback_play_urls(
    agent_stream_addr: &str,
    schema: &str,
    app: &str,
    stream: &str,
) -> Vec<String> {
    let mut schemas = BTreeSet::new();
    schemas.insert(schema.trim().to_string());
    build_play_urls(agent_stream_addr, &schemas, app, stream)
}

pub(crate) fn build_play_urls(
    agent_stream_addr: &str,
    schemas: &BTreeSet<String>,
    app: &str,
    stream: &str,
) -> Vec<String> {
    let Ok(base) = Url::parse(agent_stream_addr) else {
        return Vec::new();
    };
    let Some(host) = base.host_str() else {
        return Vec::new();
    };
    let http_authority = match base.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let http_base = format!("{}://{http_authority}", base.scheme());
    let mut urls = Vec::new();

    for schema in schemas {
        match schema.as_str() {
            "rtsp" => urls.push(format!("rtsp://{host}/{app}/{stream}")),
            "rtmp" => urls.push(format!("rtmp://{host}/{app}/{stream}")),
            "hls" => urls.push(format!("{http_base}/{app}/{stream}/hls.m3u8")),
            "ts" | "http_ts" => urls.push(format!("{http_base}/{app}/{stream}.live.ts")),
            "fmp4" | "http_fmp4" | "http_fmp4_ts" => {
                urls.push(format!("{http_base}/{app}/{stream}.live.mp4"))
            }
            _ => {}
        }
    }

    urls
}

fn value_to_u32(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().map(|v| v.max(0) as u64))
        })
        .and_then(|value| u32::try_from(value).ok())
}

fn value_to_f64(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| value.as_f64().or_else(|| value.as_u64().map(|v| v as f64)))
}

fn debug_media_query_params(query: &ZlmMediaQuery) -> Vec<(String, String)> {
    let mut params = Vec::new();
    if let Some(schema) = query
        .schema
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        params.push(("schema".to_string(), schema.trim().to_string()));
    }
    if let Some(vhost) = query
        .vhost
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        params.push(("vhost".to_string(), vhost.trim().to_string()));
    }
    if let Some(app) = query
        .app
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        params.push(("app".to_string(), app.trim().to_string()));
    }
    if let Some(stream) = query
        .stream
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        params.push(("stream".to_string(), stream.trim().to_string()));
    }
    params
}

async fn call_zlm_api(
    state: &AppState,
    node_id: Uuid,
    path: &str,
    params: Vec<(String, String)>,
) -> Result<Value, AppError> {
    let target = state.repository.get_node_debug_target(node_id).await?;
    let mut url = build_zlm_debug_url(&target, path)?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in &params {
            query.append_pair(key, value);
        }
    }

    let response = state
        .http_client
        .get(url)
        .send()
        .await
        .map_err(|error| AppError::Internal(format!("failed to call ZLM API: {error}")))?
        .error_for_status()
        .map_err(|error| AppError::Internal(format!("ZLM API returned error: {error}")))?;
    let body: Value = response
        .json()
        .await
        .map_err(|error| AppError::Internal(format!("failed to decode ZLM API body: {error}")))?;

    ensure_zlm_debug_success(path, body)
}

async fn call_zlm_binary_api(
    state: &AppState,
    node_id: Uuid,
    path: &str,
    params: Vec<(String, String)>,
) -> Result<(String, Vec<u8>), AppError> {
    let target = state.repository.get_node_debug_target(node_id).await?;
    let mut url = build_zlm_debug_url(&target, path)?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in &params {
            query.append_pair(key, value);
        }
    }

    let response = state
        .http_client
        .get(url)
        .send()
        .await
        .map_err(|error| AppError::Internal(format!("failed to call ZLM API: {error}")))?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let body = response
        .bytes()
        .await
        .map_err(|error| AppError::Internal(format!("failed to decode ZLM binary body: {error}")))?
        .to_vec();

    if content_type.contains("application/json") {
        let value: Value = serde_json::from_slice(&body).map_err(|error| {
            AppError::Internal(format!("failed to decode unexpected JSON body: {error}"))
        })?;
        let _ = ensure_zlm_debug_success(path, value)?;
        return Err(AppError::Internal(format!(
            "{path} returned JSON instead of a binary image payload"
        )));
    }

    if !status.is_success() {
        return Err(AppError::Internal(format!(
            "{path} returned HTTP status {status}"
        )));
    }

    Ok((content_type, body))
}

fn build_zlm_debug_url(target: &repository::NodeDebugTarget, path: &str) -> Result<Url, AppError> {
    let mut url = Url::parse(target.zlm_api_base.trim())
        .map_err(|error| AppError::Internal(format!("invalid node zlm_api_base: {error}")))?
        .join(path)
        .map_err(|error| AppError::Internal(format!("invalid ZLM API path join: {error}")))?;
    if !target.zlm_api_secret.trim().is_empty() {
        url.query_pairs_mut()
            .append_pair("secret", target.zlm_api_secret.trim());
    }
    Ok(url)
}

fn ensure_zlm_debug_success(path: &str, body: Value) -> Result<Value, AppError> {
    match body.get("code").and_then(Value::as_i64) {
        Some(0) | None => Ok(body),
        Some(code) => Err(AppError::Internal(format!(
            "{path} returned code {code}: {}",
            body.get("msg")
                .and_then(Value::as_str)
                .unwrap_or("unknown ZLM error")
        ))),
    }
}

async fn receive_zlm_hook(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(server_id): Path<String>,
    Query(query): Query<ZlmHookQuery>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let hook_name = query
        .hook_name
        .clone()
        .or_else(|| {
            payload
                .get("hook_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| AppError::BadRequest("hook_name is required".to_string()))?;
    process_zlm_hook(
        state,
        server_id,
        hook_name,
        query.secret,
        addr.ip(),
        headers,
        payload,
    )
    .await
}

async fn receive_named_zlm_hook(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path((server_id, hook_name)): Path<(String, String)>,
    Query(query): Query<ZlmHookQuery>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    process_zlm_hook(
        state,
        server_id,
        hook_name,
        query.secret,
        addr.ip(),
        headers,
        payload,
    )
    .await
}

async fn process_zlm_hook(
    state: AppState,
    server_id: String,
    hook_name: String,
    query_secret: Option<String>,
    peer_ip: IpAddr,
    headers: HeaderMap,
    payload: serde_json::Value,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let payload_server_id = payload
        .get("mediaServerId")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(payload_server_id) = payload_server_id {
        if payload_server_id != server_id {
            return Err(AppError::BadRequest(format!(
                "payload mediaServerId {payload_server_id} does not match path server_id {server_id}"
            )));
        }
    }

    validate_hook_source_ip(&state, peer_ip)?;
    validate_hook_secret(&state, &headers, query_secret.as_deref(), &payload)?;
    let node_id = state
        .repository
        .resolve_node_id_by_server_id(server_id.trim())
        .await?;
    let Some(node_id) = node_id else {
        return Err(AppError::NotFound(format!(
            "hook server_id {} was not found",
            server_id.trim()
        )));
    };

    let hook_name = hook_name.trim().to_string();
    if hook_name.is_empty() {
        return Err(AppError::BadRequest(
            "hook_name must not be empty".to_string(),
        ));
    }

    let sanitized_payload = sanitize_hook_payload(&payload);
    let dedup_key = hash_hook_payload(server_id.trim(), &hook_name, &sanitized_payload);

    let response = match hook_name.as_str() {
        "on_publish" => {
            let hook = parse_publish_hook(&sanitized_payload)?;
            let publish_target = state
                .repository
                .find_task_for_publish_stream(
                    server_id.trim(),
                    &hook.vhost,
                    &hook.app,
                    &hook.stream,
                )
                .await?;
            if let Some(target) = publish_target {
                let resolved_spec = serde_json::from_value::<TaskSpec>(
                    target.resolved_spec.clone(),
                )
                .map_err(|error| {
                    AppError::Internal(format!(
                        "failed to deserialize resolved_spec for on_publish: {error}"
                    ))
                })?;
                let is_rtp_receive = resolved_spec.task_type
                    == media_domain::TaskType::StreamIngest
                    && resolved_spec.input.kind == Some(media_domain::InputKind::GbRtp);
                let _ = state
                    .repository
                    .record_zlm_publish_hook(
                        server_id.trim(),
                        &hook_name,
                        &dedup_key,
                        node_id,
                        sanitized_payload,
                        ZlmPublishTaskRecord {
                            task_id: target.task_id,
                            attempt_id: target.attempt_id,
                            attempt_no: target.attempt_no,
                            schema: hook.schema.clone(),
                            vhost: hook.vhost.clone(),
                            app: hook.app.clone(),
                            stream: hook.stream.clone(),
                            rtp_stream_id: is_rtp_receive
                                .then(|| build_rtp_stream_id(target.task_id, target.attempt_no)),
                            promote_running: is_rtp_receive,
                            event_payload: json!({
                                "schema": hook.schema,
                                "vhost": hook.vhost,
                                "app": hook.app,
                                "stream": hook.stream,
                                "ip": hook.ip,
                                "port": hook.port,
                                "params": hook.params,
                                "session_id": hook.id,
                            }),
                        },
                    )
                    .await?;
                build_publish_hook_response(
                    Some(&resolved_spec),
                    state.zlm_auto_close_on_no_reader_enabled,
                )
            } else {
                let _ = state
                    .repository
                    .record_zlm_hook(server_id.trim(), &hook_name, &dedup_key, sanitized_payload)
                    .await?;
                build_publish_hook_response(None, state.zlm_auto_close_on_no_reader_enabled)
            }
        }
        "on_rtp_server_timeout" => {
            let hook = parse_rtp_server_timeout_hook(&sanitized_payload)?;
            let timeout_target = state
                .repository
                .find_task_for_rtp_stream(server_id.trim(), &hook.stream_id)
                .await?;
            if let Some(target) = timeout_target {
                let _ = state
                    .repository
                    .record_zlm_lost_task_event_hook(
                        server_id.trim(),
                        &hook_name,
                        &dedup_key,
                        node_id,
                        sanitized_payload,
                        ZlmTaskEventHookRecord {
                            task_id: target.task_id,
                            attempt_id: target.attempt_id,
                            attempt_no: Some(target.attempt_no),
                            event_type: "rtp_server_timeout".to_string(),
                            event_level: "warn".to_string(),
                            payload: json!({
                                "local_port": hook.local_port,
                                "re_use_port": hook.re_use_port,
                                "ssrc": hook.ssrc,
                                "stream_id": hook.stream_id,
                                "tcp_mode": hook.tcp_mode,
                            }),
                        },
                        "rtp_server_timeout",
                        "rtp_receive server timed out waiting for media",
                    )
                    .await?;
            } else {
                let _ = state
                    .repository
                    .record_zlm_hook(server_id.trim(), &hook_name, &dedup_key, sanitized_payload)
                    .await?;
            }
            hook_ack(&hook_name)
        }
        "on_record_mp4" => {
            let hook = parse_record_mp4_hook(&sanitized_payload)?;
            validate_record_file_path(&hook.file_path, &state.storage_allowlist)?;
            let _ = state
                .repository
                .record_zlm_record_file_hook(
                    server_id.trim(),
                    &hook_name,
                    &dedup_key,
                    sanitized_payload,
                    ZlmRecordFileRecord {
                        record_format: Some("mp4".to_string()),
                        schema: None,
                        vhost: hook.vhost,
                        app: hook.app,
                        stream: hook.stream,
                        file_path: hook.file_path,
                        file_size: hook.file_size.unwrap_or_default(),
                        time_len_sec: hook.time_len.map(|value| value.round() as i32),
                        start_time: hook
                            .start_time
                            .and_then(|value| DateTime::<Utc>::from_timestamp(value, 0)),
                        file_name: hook.file_name,
                        folder: hook.folder,
                        url: hook.url,
                    },
                )
                .await?;
            hook_ack(&hook_name)
        }
        "on_record_ts" | "on_record_hls" => {
            let hook = parse_record_hls_hook(&sanitized_payload, &hook_name)?;
            if let Some(file_path) = resolve_record_hls_file_path(&hook) {
                validate_record_file_path(&file_path, &state.storage_allowlist)?;
                let _ = state
                    .repository
                    .record_zlm_record_file_hook(
                        server_id.trim(),
                        &hook_name,
                        &dedup_key,
                        sanitized_payload,
                        ZlmRecordFileRecord {
                            record_format: Some("hls".to_string()),
                            schema: None,
                            vhost: hook.vhost.clone(),
                            app: hook.app.clone(),
                            stream: hook.stream.clone(),
                            file_path: file_path.clone(),
                            file_size: hook.file_size.unwrap_or_default(),
                            time_len_sec: hook.time_len.map(|value| value.round() as i32),
                            start_time: hook
                                .start_time
                                .and_then(|value| DateTime::<Utc>::from_timestamp(value, 0)),
                            file_name: hook
                                .file_name
                                .clone()
                                .or_else(|| file_name_from_path(&file_path)),
                            folder: hook.folder.clone().or_else(|| folder_from_path(&file_path)),
                            url: hook.m3u8_url.clone().or(hook.url.clone()),
                        },
                    )
                    .await?;
            } else {
                let _ = state
                    .repository
                    .record_zlm_hook(server_id.trim(), &hook_name, &dedup_key, sanitized_payload)
                    .await?;
            }
            hook_ack(&hook_name)
        }
        "on_stream_none_reader" => {
            let hook = parse_stream_none_reader_hook(&sanitized_payload)?;
            let _ = state
                .repository
                .record_zlm_stream_event_hook(
                    server_id.trim(),
                    &hook_name,
                    &dedup_key,
                    sanitized_payload,
                    ZlmStreamEventRecord {
                        schema: Some(hook.schema.clone()),
                        vhost: hook.vhost.clone(),
                        app: hook.app.clone(),
                        stream: hook.stream.clone(),
                        event_type: "stream_no_reader".to_string(),
                        event_level: "warn".to_string(),
                        payload: json!({
                            "schema": hook.schema,
                            "vhost": hook.vhost,
                            "app": hook.app,
                            "stream": hook.stream,
                            "close_requested": false,
                        }),
                    },
                )
                .await?;
            hook_ack(&hook_name)
        }
        "on_stream_not_found" => {
            let hook = parse_stream_not_found_hook(&sanitized_payload)?;
            let _ = state
                .repository
                .record_zlm_stream_event_hook(
                    server_id.trim(),
                    &hook_name,
                    &dedup_key,
                    sanitized_payload,
                    ZlmStreamEventRecord {
                        schema: Some(hook.schema.clone()),
                        vhost: hook.vhost.clone(),
                        app: hook.app.clone(),
                        stream: hook.stream.clone(),
                        event_type: "stream_lookup_miss".to_string(),
                        event_level: "info".to_string(),
                        payload: json!({
                            "schema": hook.schema,
                            "protocol": hook.protocol,
                            "vhost": hook.vhost,
                            "app": hook.app,
                            "stream": hook.stream,
                            "ip": hook.ip,
                            "port": hook.port,
                            "params": hook.params,
                            "session_id": hook.id,
                        }),
                    },
                )
                .await?;
            hook_ack(&hook_name)
        }
        "on_server_keepalive" | "on_server_started" => {
            let _ = state
                .repository
                .record_zlm_hook(server_id.trim(), &hook_name, &dedup_key, sanitized_payload)
                .await?;
            state
                .repository
                .record_media_server_seen(node_id, server_id.trim(), Utc::now())
                .await?;
            hook_ack(&hook_name)
        }
        _ => {
            let _ = state
                .repository
                .record_zlm_hook(server_id.trim(), &hook_name, &dedup_key, sanitized_payload)
                .await?;
            hook_ack(&hook_name)
        }
    };

    Ok((StatusCode::OK, Json(response)))
}

fn extract_idempotency_key(headers: &HeaderMap) -> Result<String, AppError> {
    let value = headers
        .get("Idempotency-Key")
        .ok_or_else(|| AppError::BadRequest("Idempotency-Key header is required".to_string()))?;

    let key = value
        .to_str()
        .map_err(|_| AppError::BadRequest("Idempotency-Key must be valid UTF-8".to_string()))?
        .trim()
        .to_string();

    if key.is_empty() {
        return Err(AppError::BadRequest(
            "Idempotency-Key must not be empty".to_string(),
        ));
    }

    Ok(key)
}

fn hash_json<T: Serialize>(value: &T) -> Result<String, AppError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| AppError::Internal(format!("failed to serialize request: {error}")))?;
    let digest = Sha256::digest(bytes);
    Ok(format!("{digest:x}"))
}

async fn maybe_dispatch_immediate_task(
    state: &AppState,
    task: repository::TaskSummary,
) -> Result<repository::TaskSummary, AppError> {
    if task.status == media_domain::TaskStatus::Validating
        && resolved_task_is_immediate(state, task.id).await?
    {
        match state.control_plane.dispatch_task(task.id).await {
            Ok(()) => {}
            Err(control_plane::ControlPlaneError::NoConnectedNode)
            | Err(control_plane::ControlPlaneError::NodeDisconnected(_)) => {}
            Err(error) => return Err(error.into()),
        }
        return Ok(state.repository.get_task_summary(task.id).await?);
    }

    Ok(task)
}

async fn resolved_task_is_immediate(state: &AppState, task_id: Uuid) -> Result<bool, AppError> {
    let resolved_spec = state.repository.get_resolved_spec(task_id).await?;
    Ok(matches!(
        resolved_spec
            .get("schedule")
            .and_then(|value| value.get("start_mode"))
            .and_then(serde_json::Value::as_str),
        Some("immediate") | None
    ))
}

fn validate_hook_secret(
    state: &AppState,
    headers: &HeaderMap,
    query_secret: Option<&str>,
    payload: &serde_json::Value,
) -> Result<(), AppError> {
    if state.hook_shared_secret.trim().is_empty() {
        return Ok(());
    }

    let provided = query_secret
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            headers
                .get("X-Hook-Secret")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            payload
                .get("secret")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });

    match provided {
        Some(secret) if secret == state.hook_shared_secret => Ok(()),
        _ => Err(AppError::Forbidden(
            "invalid hook shared secret".to_string(),
        )),
    }
}

fn validate_hook_source_ip(state: &AppState, peer_ip: IpAddr) -> Result<(), AppError> {
    if state.hook_source_allowlist.is_empty() {
        return Ok(());
    }

    if state.hook_source_allowlist.contains(&peer_ip) {
        Ok(())
    } else {
        Err(AppError::Forbidden(format!(
            "hook source ip {peer_ip} is not allowlisted"
        )))
    }
}

fn sanitize_hook_payload(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sanitized = map
                .iter()
                .filter(|(key, _)| key.as_str() != "secret")
                .map(|(key, value)| (key.clone(), sanitize_hook_payload(value)))
                .collect();
            serde_json::Value::Object(sanitized)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sanitize_hook_payload).collect())
        }
        _ => value.clone(),
    }
}

fn hash_hook_payload(server_id: &str, hook_name: &str, payload: &serde_json::Value) -> String {
    let canonical = canonicalize_json_value(payload);
    let digest = Sha256::digest(format!("{server_id}:{hook_name}:{canonical}"));
    format!("{digest:x}")
}

fn canonicalize_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::String(value) => serde_json::to_string(value).unwrap_or_default(),
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(canonicalize_json_value)
                .collect::<Vec<_>>()
                .join(",")
        ),
        serde_json::Value::Object(map) => {
            let ordered = map
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize_json_value(value)))
                .collect::<BTreeMap<_, _>>();
            format!(
                "{{{}}}",
                ordered
                    .iter()
                    .map(|(key, value)| {
                        format!(
                            "{}:{}",
                            serde_json::to_string(key).unwrap_or_default(),
                            value
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

fn parse_record_mp4_hook(payload: &serde_json::Value) -> Result<ZlmOnRecordMp4Payload, AppError> {
    serde_json::from_value(payload.clone())
        .map_err(|error| AppError::BadRequest(format!("invalid on_record_mp4 payload: {error}")))
}

fn parse_stream_none_reader_hook(
    payload: &serde_json::Value,
) -> Result<ZlmOnStreamNoneReaderPayload, AppError> {
    serde_json::from_value(payload.clone()).map_err(|error| {
        AppError::BadRequest(format!("invalid on_stream_none_reader payload: {error}"))
    })
}

fn parse_record_hls_hook(
    payload: &serde_json::Value,
    hook_name: &str,
) -> Result<ZlmOnRecordHlsPayload, AppError> {
    serde_json::from_value(payload.clone())
        .map_err(|error| AppError::BadRequest(format!("invalid {hook_name} payload: {error}")))
}

fn parse_publish_hook(payload: &serde_json::Value) -> Result<ZlmOnPublishPayload, AppError> {
    serde_json::from_value(payload.clone())
        .map_err(|error| AppError::BadRequest(format!("invalid on_publish payload: {error}")))
}

fn parse_rtp_server_timeout_hook(
    payload: &serde_json::Value,
) -> Result<ZlmOnRtpServerTimeoutPayload, AppError> {
    serde_json::from_value(payload.clone()).map_err(|error| {
        AppError::BadRequest(format!("invalid on_rtp_server_timeout payload: {error}"))
    })
}

fn parse_stream_not_found_hook(
    payload: &serde_json::Value,
) -> Result<ZlmOnStreamNotFoundPayload, AppError> {
    serde_json::from_value(payload.clone()).map_err(|error| {
        AppError::BadRequest(format!("invalid on_stream_not_found payload: {error}"))
    })
}

fn build_publish_hook_response(
    spec: Option<&TaskSpec>,
    auto_close_enabled: bool,
) -> serde_json::Value {
    let resolved = spec.cloned().map(|value| value.resolved());
    let expose = resolved
        .as_ref()
        .map(|value| &value.expose)
        .cloned()
        .unwrap_or_default();
    let record = resolved
        .as_ref()
        .map(|value| &value.record)
        .cloned()
        .unwrap_or_default();

    json!({
        "code": 0,
        "msg": "success",
        "enable_audio": true,
        "add_mute_audio": true,
        "enable_rtsp": expose.enable_rtsp.unwrap_or(true),
        "enable_rtmp": expose.enable_rtmp.unwrap_or(true),
        "enable_ts": expose.enable_http_ts.unwrap_or(true),
        "enable_fmp4": expose.enable_http_fmp4.unwrap_or(true),
        "enable_hls": expose.enable_hls.unwrap_or(false),
        "enable_hls_fmp4": false,
        "enable_mp4": false,
        "modify_stamp": 2,
        "continue_push_ms": 15_000,
        "mp4_as_player": record.as_player.unwrap_or(false),
        "auto_close": auto_close_enabled && expose.stop_on_no_reader.unwrap_or(false),
        "stream_replace": "",
    })
}

fn hook_ack(hook_name: &str) -> serde_json::Value {
    match hook_name {
        "on_stream_none_reader" => json!({
            "code": 0,
            "close": false,
        }),
        _ => json!({
            "code": 0,
            "msg": "success",
        }),
    }
}

fn build_rtp_stream_id(task_id: Uuid, attempt_no: i32) -> String {
    format!("{task_id}-{attempt_no}")
}

fn validate_record_file_path(path: &str, allowlist: &[String]) -> Result<(), AppError> {
    let path = normalize_filesystem_path(path)?;
    let allowed = allowlist.iter().any(|root| {
        normalize_filesystem_path(root)
            .map(|root| path.starts_with(&root))
            .unwrap_or(false)
    });
    if allowed {
        Ok(())
    } else {
        Err(AppError::Forbidden(format!(
            "record file path {} is outside storage allowlist",
            path.display()
        )))
    }
}

fn resolve_record_hls_file_path(hook: &ZlmOnRecordHlsPayload) -> Option<String> {
    hook.file_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let folder = hook
                .folder
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())?;
            let file_name = hook
                .file_name
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())?;
            Some(
                PathBuf::from(folder)
                    .join(file_name)
                    .to_string_lossy()
                    .to_string(),
            )
        })
}

fn file_name_from_path(path: &str) -> Option<String> {
    FsPath::new(path)
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
}

fn folder_from_path(path: &str) -> Option<String> {
    FsPath::new(path)
        .parent()
        .map(|value| value.to_string_lossy().to_string())
}

fn normalize_filesystem_path(value: &str) -> Result<PathBuf, AppError> {
    let path = FsPath::new(value.trim());
    if path.as_os_str().is_empty() {
        return Err(AppError::BadRequest(
            "file path must not be empty".to_string(),
        ));
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| AppError::Internal(format!("failed to resolve cwd: {error}")))?
            .join(path)
    };
    Ok(normalize_path_components(path))
}

fn normalize_path_components(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn parse_hook_source_allowlist(values: &[String]) -> anyhow::Result<Vec<IpAddr>> {
    values
        .iter()
        .map(|value| {
            value
                .parse::<IpAddr>()
                .with_context(|| format!("invalid HOOK_SOURCE_ALLOWLIST entry {value}"))
        })
        .collect()
}

fn parse_cli_command() -> anyhow::Result<Option<CliCommand>> {
    parse_cli_command_from(std::env::args().skip(1))
}

fn parse_cli_command_from<I>(args: I) -> anyhow::Result<Option<CliCommand>>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Ok(None);
    };
    if is_help_flag(&command) {
        return Ok(Some(CliCommand::Help { auth_only: false }));
    }
    if command != "auth" {
        anyhow::bail!("unsupported command `{command}`");
    }
    let Some(subcommand) = args.next() else {
        anyhow::bail!("missing auth subcommand");
    };
    if is_help_flag(&subcommand) {
        return Ok(Some(CliCommand::Help { auth_only: true }));
    }

    let remaining_args: Vec<String> = args.collect();
    if remaining_args.len() == 1 && is_help_flag(&remaining_args[0]) {
        return Ok(Some(CliCommand::Help { auth_only: true }));
    }

    let mut args = remaining_args.into_iter();
    let mut username = None;
    let mut expects_password_stdin = false;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--username" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing value for --username"))?;
                username = Some(value);
            }
            "--password-stdin" => expects_password_stdin = true,
            other => anyhow::bail!("unsupported auth argument `{other}`"),
        }
    }

    let username = username.ok_or_else(|| anyhow::anyhow!("--username is required"))?;
    anyhow::ensure!(
        expects_password_stdin,
        "--password-stdin is required for auth commands"
    );

    let command = match subcommand.as_str() {
        "bootstrap-admin" => CliCommand::BootstrapAdmin { username },
        "reset-password" => CliCommand::ResetPassword { username },
        other => anyhow::bail!("unsupported auth subcommand `{other}`"),
    };
    Ok(Some(command))
}

fn is_help_flag(value: &str) -> bool {
    matches!(value, "help" | "-h" | "--help")
}

fn print_cli_help(auth_only: bool) {
    let help = if auth_only {
        AUTH_CLI_HELP_TEXT
    } else {
        CLI_HELP_TEXT
    };
    println!("{help}");
}

const CLI_HELP_TEXT: &str = "\
Usage:
  media-core
  media-core auth bootstrap-admin --username <name> --password-stdin
  media-core auth reset-password --username <name> --password-stdin
  media-core [help|-h|--help]
  media-core auth [help|-h|--help]

Description:
  Run without a subcommand to start the media-core server.

Auth commands:
  bootstrap-admin  Create the initial enabled admin user; reads the password from stdin.
  reset-password   Reset an existing user's password; reads the new password from stdin.";

const AUTH_CLI_HELP_TEXT: &str = "\
Usage:
  media-core auth bootstrap-admin --username <name> --password-stdin
  media-core auth reset-password --username <name> --password-stdin
  media-core auth [help|-h|--help]

Description:
  Auth commands read the password value from stdin.";

fn read_password_from_stdin() -> anyhow::Result<String> {
    let mut password = String::new();
    io::stdin().read_to_string(&mut password)?;
    let password = password.trim_end_matches(['\r', '\n']).to_string();
    anyhow::ensure!(!password.is_empty(), "password must not be empty");
    Ok(password)
}

async fn run_cli_command(command: CliCommand) -> anyhow::Result<()> {
    let settings = config::Settings::load()?;
    telemetry::init(&settings.logging);

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&settings.core.database_url)
        .await?;
    sqlx::migrate!("../../migrations").run(&pool).await?;
    let repository = TaskRepository::new(pool);
    let password = read_password_from_stdin()?;
    let password_hash = hash_password(&password)?;

    match command {
        CliCommand::Help { .. } => unreachable!("help commands are handled before DB setup"),
        CliCommand::BootstrapAdmin { username } => {
            let username = normalize_username_value(&username)?;
            anyhow::ensure!(
                !repository.has_enabled_admin_user().await?,
                "an enabled admin user already exists"
            );
            repository
                .create_bootstrap_admin(&username, &password_hash, false)
                .await?;
            println!("bootstrapped admin user `{username}`");
        }
        CliCommand::ResetPassword { username } => {
            let username = normalize_username_value(&username)?;
            repository
                .reset_user_password(
                    &username,
                    &password_hash,
                    true,
                    "cli",
                    "password_reset",
                    None,
                    None,
                )
                .await?;
            println!("reset password for `{username}`");
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};

        if let Ok(mut signal) = signal(SignalKind::terminate()) {
            signal.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

async fn wait_for_shutdown(mut receiver: watch::Receiver<bool>) {
    if *receiver.borrow() {
        return;
    }

    let _ = receiver.changed().await;
}

fn load_grpc_tls_config(
    settings: &config::CoreSettings,
) -> anyhow::Result<Option<ServerTlsConfig>> {
    if settings.grpc_tls_cert_path.trim().is_empty()
        && settings.grpc_tls_key_path.trim().is_empty()
        && settings.grpc_tls_client_ca_path.trim().is_empty()
    {
        return Ok(None);
    }

    let cert_pem = fs::read(&settings.grpc_tls_cert_path).with_context(|| {
        format!(
            "failed to read gRPC server certificate {}",
            settings.grpc_tls_cert_path
        )
    })?;
    let key_pem = fs::read(&settings.grpc_tls_key_path).with_context(|| {
        format!(
            "failed to read gRPC server key {}",
            settings.grpc_tls_key_path
        )
    })?;
    let ca_pem = fs::read(&settings.grpc_tls_client_ca_path).with_context(|| {
        format!(
            "failed to read gRPC client CA {}",
            settings.grpc_tls_client_ca_path
        )
    })?;

    Ok(Some(
        ServerTlsConfig::new()
            .identity(Identity::from_pem(cert_pem, key_pem))
            .client_ca_root(Certificate::from_pem(ca_pem)),
    ))
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    started_at: DateTime<Utc>,
    environment: String,
}

#[derive(Debug, Deserialize, Default)]
struct ZlmHookQuery {
    #[serde(default)]
    hook_name: Option<String>,
    #[serde(default)]
    secret: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ZlmOnRecordMp4Payload {
    app: String,
    stream: String,
    vhost: String,
    file_path: String,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    file_size: Option<i64>,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    start_time: Option<i64>,
    #[serde(default)]
    time_len: Option<f64>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ZlmOnRecordHlsPayload {
    app: String,
    stream: String,
    vhost: String,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    file_size: Option<i64>,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    start_time: Option<i64>,
    #[serde(default)]
    time_len: Option<f64>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    m3u8_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ZlmOnPublishPayload {
    app: String,
    id: String,
    ip: String,
    #[serde(default)]
    params: String,
    port: u16,
    schema: String,
    stream: String,
    vhost: String,
}

#[derive(Debug, Deserialize)]
struct ZlmOnRtpServerTimeoutPayload {
    #[serde(default)]
    local_port: Option<u16>,
    #[serde(default)]
    re_use_port: Option<bool>,
    #[serde(default)]
    ssrc: Option<u32>,
    stream_id: String,
    #[serde(default)]
    tcp_mode: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct ZlmOnStreamNoneReaderPayload {
    app: String,
    schema: String,
    stream: String,
    vhost: String,
}

#[derive(Debug, Deserialize)]
struct ZlmOnStreamNotFoundPayload {
    app: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    params: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    schema: String,
    #[serde(default)]
    protocol: Option<String>,
    stream: String,
    vhost: String,
}

#[derive(Debug, Deserialize)]
struct NodeScopedQuery {
    node_id: Uuid,
}

#[derive(Debug, Deserialize)]
struct NodeHeartbeatQuery {
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ZlmMediaQuery {
    node_id: Uuid,
    #[serde(default)]
    schema: Option<String>,
    #[serde(default)]
    vhost: Option<String>,
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    stream: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DebugKickSessionRequest {
    node_id: Uuid,
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct DebugKickSessionsRequest {
    node_id: Uuid,
    #[serde(default)]
    local_port: Option<u16>,
    #[serde(default)]
    peer_ip: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DebugCloseStreamRequest {
    node_id: Uuid,
    schema: String,
    vhost: String,
    app: String,
    stream: String,
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize)]
struct DebugSnapQuery {
    node_id: Uuid,
    url: String,
    #[serde(default = "default_snap_timeout_sec")]
    timeout_sec: u32,
    #[serde(default = "default_snap_expire_sec")]
    expire_sec: u32,
}

#[derive(Debug, Serialize)]
struct DebugSnapResponse {
    content_type: String,
    data_url: String,
}

#[derive(Debug, Deserialize)]
struct AuthLoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct AuthRefreshRequest {
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct AuthLogoutRequest {
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct AuthChangePasswordRequest {
    current_password: String,
    new_password: String,
}

#[derive(Debug, Serialize)]
struct AuthTokensResponse {
    access_token: String,
    access_token_expires_at: DateTime<Utc>,
    refresh_token: String,
    refresh_token_expires_at: DateTime<Utc>,
    subject: String,
    role: auth::ApiRole,
    must_change_password: bool,
}

#[derive(Debug, Deserialize)]
struct UpdateMachineAllowlistRequest {
    #[serde(default)]
    entries: Vec<MachineAllowlistWrite>,
}

#[derive(Debug, Serialize)]
struct MachineAllowlistResponse {
    entries: Vec<MachineAllowlistEntry>,
}

const fn default_snap_timeout_sec() -> u32 {
    10
}

const fn default_snap_expire_sec() -> u32 {
    30
}
