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
use tokio::{net::TcpListener, sync::watch};
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

#[derive(Debug, Clone)]
enum CliCommand {
    BootstrapAdmin { username: String },
    ResetPassword { username: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Some(command) = parse_cli_command()? {
        return run_cli_command(command).await;
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
        .route("/tasks/{id}", get(get_task))
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
    let stale_indexes = enrich_streams_with_runtime(&state, &mut streams, &node_lookup).await;
    if !stale_indexes.is_empty() {
        streams = streams
            .into_iter()
            .enumerate()
            .filter_map(|(index, stream)| (!stale_indexes.contains(&index)).then_some(stream))
            .collect();
    }
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
        node.connected = Some(load.connected);
        node.cpu_percent = Some(load.cpu_percent);
        node.mem_percent = Some(load.mem_percent);
        node.disk_percent = Some(load.disk_percent);
        node.zlm_alive = Some(load.zlm_alive);
        node.ffmpeg_alive = Some(load.ffmpeg_alive);
        node.gpu_runtime = Some(load.gpu_runtime);
        node.healthy = load.connected;
    } else {
        node.connected = Some(false);
    }
}

#[derive(Debug, Default, Clone)]
struct StreamRuntimeInfo {
    viewer_count: u32,
    bitrate_kbps: f64,
    schemas: BTreeSet<String>,
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
        match load_zlm_media_index(state, node_id).await {
            Ok(index) => {
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
            Err(error) => {
                warn!(
                    node_id = %node_id,
                    error = %error,
                    "failed to enrich stream runtime from ZLM; using stored summary only"
                );
                for stream_index in indexes {
                    let stream = &mut streams[stream_index];
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
                .find_task_for_publish_stream(node_id, &hook.app, &hook.stream)
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
                .find_task_for_rtp_stream(node_id, &hook.stream_id)
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
                .update_node_health(node_id, true, Some(Utc::now()))
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
        "enable_hls": expose.enable_hls.unwrap_or(false) || record.wants_hls(),
        "enable_hls_fmp4": false,
        "enable_mp4": record.wants_mp4(),
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
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return Ok(None);
    };
    if command != "auth" {
        anyhow::bail!("unsupported command `{command}`");
    }
    let Some(subcommand) = args.next() else {
        anyhow::bail!("missing auth subcommand");
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
        response::IntoResponse,
        routing::get,
    };
    use media_domain::{AgentRegistration, HeartbeatSnapshot, NetworkMode};
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use tokio::{
        net::{TcpListener, TcpStream},
        task::JoinHandle,
        time::timeout,
    };
    use tower::util::ServiceExt;

    const TEST_RSA_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDRNk+CElS+M3My1DbTUInl9aeU\nYCLza8Uftij7kPTApECFQcy1em6CZwb+PDHjjtFB2i8Ncfbx+dt2S6CbJHSF0dDB\n+GoiaVaYolB9XoQODqA7LXTy/D4e9jdNJQgDVXlzXsTm4k3v1CnC1As7RfUkgdM/\npsbfsbeai7RULN2NnQIDAQAB\n-----END PUBLIC KEY-----";

    struct TestDatabase {
        admin_pool: sqlx::PgPool,
        pool: sqlx::PgPool,
        database_name: String,
    }

    impl TestDatabase {
        async fn new(run_migrations: bool) -> anyhow::Result<Self> {
            let admin_url = test_admin_database_url();
            let admin_pool = PgPoolOptions::new()
                .max_connections(1)
                .connect(&admin_url)
                .await?;
            let database_name = format!("streamserver_test_{}", Uuid::now_v7().simple());
            sqlx::query(&format!("create database {database_name}"))
                .execute(&admin_pool)
                .await?;

            let database_url = test_database_url(&admin_url, &database_name)?;
            let pool = PgPoolOptions::new()
                .max_connections(5)
                .connect(&database_url)
                .await?;
            if run_migrations {
                sqlx::migrate!("../../migrations").run(&pool).await?;
            }

            Ok(Self {
                admin_pool,
                pool,
                database_name,
            })
        }

        async fn maybe_new(run_migrations: bool) -> anyhow::Result<Option<Self>> {
            if !database_is_reachable(&test_admin_database_url()).await {
                eprintln!("skipping database-backed test: database is unreachable");
                return Ok(None);
            }
            match Self::new(run_migrations).await {
                Ok(database) => Ok(Some(database)),
                Err(error) => {
                    eprintln!("skipping database-backed test: {error}");
                    Ok(None)
                }
            }
        }

        async fn cleanup(self) -> anyhow::Result<()> {
            self.pool.close().await;
            sqlx::query(
                r#"
                select pg_terminate_backend(pid)
                  from pg_stat_activity
                 where datname = $1
                   and pid <> pg_backend_pid()
                "#,
            )
            .bind(&self.database_name)
            .execute(&self.admin_pool)
            .await?;
            sqlx::query(&format!("drop database if exists {}", self.database_name))
                .execute(&self.admin_pool)
                .await?;
            self.admin_pool.close().await;
            Ok(())
        }
    }

    fn test_admin_database_url() -> String {
        std::env::var("TEST_DATABASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgresql://postgres:test@127.0.0.1/postgres".to_string())
    }

    fn test_database_url(admin_url: &str, database_name: &str) -> anyhow::Result<String> {
        let mut url = reqwest::Url::parse(admin_url)?;
        url.set_path(&format!("/{database_name}"));
        url.set_query(None);
        Ok(url.to_string())
    }

    async fn database_is_reachable(database_url: &str) -> bool {
        let Ok(url) = reqwest::Url::parse(database_url) else {
            return false;
        };
        let Some(host) = url.host_str() else {
            return false;
        };
        let port = url.port().unwrap_or(5432);
        timeout(
            std::time::Duration::from_secs(1),
            TcpStream::connect((host, port)),
        )
        .await
        .is_ok_and(|result| result.is_ok())
    }

    async fn require_test_database(run_migrations: bool) -> anyhow::Result<Option<TestDatabase>> {
        TestDatabase::maybe_new(run_migrations).await
    }

    fn test_app_state(pool: sqlx::PgPool) -> AppState {
        let repository = Arc::new(TaskRepository::new(pool));
        let control_plane = ControlPlaneService::new(repository.clone());
        AppState {
            repository,
            control_plane,
            started_at: Utc::now(),
            environment: "test".to_string(),
            auth: AuthConfig::disabled(),
            http_client: Client::new(),
            hook_shared_secret: String::new(),
            hook_source_allowlist: Vec::new(),
            zlm_auto_close_on_no_reader_enabled: false,
            storage_allowlist: vec![std::env::temp_dir().to_string_lossy().to_string()],
        }
    }

    fn test_app_state_with_auth(pool: sqlx::PgPool) -> AppState {
        let mut state = test_app_state(pool);
        state.auth =
            AuthConfig::from_public_key(true, TEST_RSA_PUBLIC_KEY).expect("rsa key should load");
        state
    }

    async fn upsert_test_node(
        repository: &TaskRepository,
        node_id: Uuid,
        zlm_api_base: &str,
        agent_stream_addr: &str,
    ) -> anyhow::Result<()> {
        repository
            .upsert_node_registration(
                &AgentRegistration {
                    node_id,
                    node_name: format!("node-{}", short_id(node_id)),
                    agent_version: "test".to_string(),
                    hostname: "worker-a".to_string(),
                    labels: vec!["edge".to_string()],
                    interfaces: vec!["192.168.1.20".to_string()],
                    zlm_api_base: zlm_api_base.to_string(),
                    zlm_api_secret: "secret".to_string(),
                    agent_stream_addr: agent_stream_addr.to_string(),
                    network_mode: NetworkMode::Bridge,
                    ffmpeg_bin: "ffmpeg".to_string(),
                    ffprobe_bin: "ffprobe".to_string(),
                },
                Utc::now(),
            )
            .await?;
        Ok(())
    }

    async fn insert_running_stream_task(
        pool: &sqlx::PgPool,
        node_id: Uuid,
        resolved_spec: Value,
        app: &str,
        stream: &str,
    ) -> anyhow::Result<Uuid> {
        let now = Utc::now();
        let task_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            insert into tasks (
              id, name, type, status, idempotency_key,
              priority, requested_spec, resolved_spec, created_by, assigned_node_id,
              current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
            ) values (
              $1, 'relay-camera-01', 'stream_ingest'::task_type, 'RUNNING'::task_status, $2,
              50, $3, $3, 'tester', $4,
              1, 'immediate', $5, $5, $5, null
            )
            "#,
        )
        .bind(task_id)
        .bind(format!("stream-{task_id}"))
        .bind(&resolved_spec)
        .bind(node_id)
        .bind(now)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
            insert into task_attempts (
              id, task_id, attempt_no, node_id, worker_kind, status,
              pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
              rtp_port, exit_code, failure_code, failure_reason,
              checkpoint_json, started_at, ended_at, created_at
            ) values (
              $1, $2, 1, $3, 'zlm_proxy'::worker_kind, 'RUNNING'::attempt_status,
              null, null, 'rtsp', '__defaultVhost__', $4, $5,
              null, null, null, null,
              null, $6, null, $6
            )
            "#,
        )
        .bind(attempt_id)
        .bind(task_id)
        .bind(node_id)
        .bind(app)
        .bind(stream)
        .bind(now)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
            insert into stream_bindings (
              id, task_id, attempt_id, schema, vhost, app, stream, zlm_proxy_key, zlm_pusher_key, rtp_stream_id, created_at
            ) values (
              $1, $2, $3, 'rtsp', '__defaultVhost__', $4, $5, null, null, null, $6
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(task_id)
        .bind(attempt_id)
        .bind(app)
        .bind(stream)
        .bind(now)
        .execute(pool)
        .await?;
        Ok(task_id)
    }

    async fn insert_starting_stream_task(
        pool: &sqlx::PgPool,
        node_id: Uuid,
        resolved_spec: Value,
        app: &str,
        stream: &str,
    ) -> anyhow::Result<Uuid> {
        let now = Utc::now();
        let task_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            insert into tasks (
              id, name, type, status, idempotency_key,
              priority, requested_spec, resolved_spec, created_by, assigned_node_id,
              current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
            ) values (
              $1, 'relay-camera-01', 'stream_ingest'::task_type, 'STARTING'::task_status, $2,
              50, $3, $3, 'tester', $4,
              1, 'immediate', $5, $5, $5, null
            )
            "#,
        )
        .bind(task_id)
        .bind(format!("stream-starting-{task_id}"))
        .bind(&resolved_spec)
        .bind(node_id)
        .bind(now)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
            insert into task_attempts (
              id, task_id, attempt_no, node_id, worker_kind, status,
              pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
              rtp_port, exit_code, failure_code, failure_reason,
              checkpoint_json, started_at, ended_at, created_at
            ) values (
              $1, $2, 1, $3, 'zlm_proxy'::worker_kind, 'STARTING'::attempt_status,
              null, null, 'rtsp', '__defaultVhost__', $4, $5,
              null, null, null, null,
              null, $6, null, $6
            )
            "#,
        )
        .bind(attempt_id)
        .bind(task_id)
        .bind(node_id)
        .bind(app)
        .bind(stream)
        .bind(now)
        .execute(pool)
        .await?;
        Ok(task_id)
    }

    async fn spawn_zlm_stub() -> anyhow::Result<(String, JoinHandle<()>)> {
        async fn media_list() -> Json<Value> {
            Json(json!({
                "code": 0,
                "data": [
                    {
                        "schema": "rtsp",
                        "vhost": "__defaultVhost__",
                        "app": "live",
                        "stream": "camera01",
                        "totalReaderCount": 3,
                        "bytesSpeed": 4000
                    },
                    {
                        "schema": "rtmp",
                        "vhost": "__defaultVhost__",
                        "app": "live",
                        "stream": "camera01",
                        "totalReaderCount": 3,
                        "bytesSpeed": 4000
                    },
                    {
                        "schema": "hls",
                        "vhost": "__defaultVhost__",
                        "app": "live",
                        "stream": "camera01",
                        "totalReaderCount": 3,
                        "bytesSpeed": 4000
                    }
                ]
            }))
        }

        async fn snap() -> impl IntoResponse {
            (
                [(header::CONTENT_TYPE, "image/jpeg")],
                vec![0xFFu8, 0xD8, 0xFF, 0xD9],
            )
        }

        let app = Router::new()
            .route("/index/api/getMediaList", get(media_list))
            .route("/index/api/getSnap", get(snap));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("stub server should run");
        });
        Ok((format!("http://{addr}"), handle))
    }

    async fn spawn_callback_stub(
        status: StatusCode,
    ) -> anyhow::Result<(
        String,
        Arc<tokio::sync::Mutex<Vec<(HeaderMap, Value)>>>,
        JoinHandle<()>,
    )> {
        use axum::{body::Bytes, extract::State, routing::post};

        #[derive(Clone)]
        struct CallbackStubState {
            calls: Arc<tokio::sync::Mutex<Vec<(HeaderMap, Value)>>>,
            status: StatusCode,
        }

        async fn callback_handler(
            State(state): State<CallbackStubState>,
            headers: HeaderMap,
            body: Bytes,
        ) -> impl IntoResponse {
            let payload = serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| json!({}));
            state.calls.lock().await.push((headers, payload));
            (state.status, Json(json!({"ok": true})))
        }

        let calls = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/callback", post(callback_handler))
            .with_state(CallbackStubState {
                calls: calls.clone(),
                status,
            });
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("callback stub should run");
        });
        Ok((format!("http://{addr}/callback"), calls, handle))
    }

    async fn wait_for_callback_count(
        calls: &Arc<tokio::sync::Mutex<Vec<(HeaderMap, Value)>>>,
        expected: usize,
    ) -> anyhow::Result<Vec<(HeaderMap, Value)>> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            let snapshot = calls.lock().await.clone();
            if snapshot.len() >= expected {
                return Ok(snapshot);
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for {expected} callback(s)");
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    async fn insert_running_transcode_task(
        pool: &sqlx::PgPool,
        node_id: Uuid,
        resolved_spec: Value,
    ) -> anyhow::Result<Uuid> {
        let now = Utc::now();
        let task_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            insert into tasks (
              id, name, type, status, idempotency_key,
              priority, requested_spec, resolved_spec, created_by, assigned_node_id,
              current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
            ) values (
              $1, 'transcode-job-01', 'file_transcode'::task_type, 'RUNNING'::task_status, $2,
              50, $3, $3, 'tester', $4,
              1, 'immediate', $5, $5, $5, null
            )
            "#,
        )
        .bind(task_id)
        .bind(format!("transcode-{task_id}"))
        .bind(&resolved_spec)
        .bind(node_id)
        .bind(now)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
            insert into task_attempts (
              id, task_id, attempt_no, node_id, worker_kind, status,
              pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
              rtp_port, exit_code, failure_code, failure_reason,
              checkpoint_json, started_at, ended_at, created_at
            ) values (
              $1, $2, 1, $3, 'ffmpeg'::worker_kind, 'RUNNING'::attempt_status,
              null, null, null, null, null, null,
              null, null, null, null,
              null, $4, null, $4
            )
            "#,
        )
        .bind(attempt_id)
        .bind(task_id)
        .bind(node_id)
        .bind(now)
        .execute(pool)
        .await?;
        Ok(task_id)
    }

    async fn insert_running_bridge_task(
        pool: &sqlx::PgPool,
        node_id: Uuid,
        resolved_spec: Value,
    ) -> anyhow::Result<Uuid> {
        let now = Utc::now();
        let task_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            insert into tasks (
              id, name, type, status, idempotency_key,
              priority, requested_spec, resolved_spec, created_by, assigned_node_id,
              current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
            ) values (
              $1, 'bridge-job-01', 'stream_bridge'::task_type, 'RUNNING'::task_status, $2,
              50, $3, $3, 'tester', $4,
              1, 'immediate', $5, $5, $5, null
            )
            "#,
        )
        .bind(task_id)
        .bind(format!("bridge-{task_id}"))
        .bind(&resolved_spec)
        .bind(node_id)
        .bind(now)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
            insert into task_attempts (
              id, task_id, attempt_no, node_id, worker_kind, status,
              pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
              rtp_port, exit_code, failure_code, failure_reason,
              checkpoint_json, started_at, ended_at, created_at
            ) values (
              $1, $2, 1, $3, 'ffmpeg'::worker_kind, 'RUNNING'::attempt_status,
              null, null, null, null, null, null,
              null, null, null, null,
              null, $4, null, $4
            )
            "#,
        )
        .bind(attempt_id)
        .bind(task_id)
        .bind(node_id)
        .bind(now)
        .execute(pool)
        .await?;
        Ok(task_id)
    }

    async fn insert_running_ingest_task(
        pool: &sqlx::PgPool,
        node_id: Uuid,
        resolved_spec: Value,
    ) -> anyhow::Result<Uuid> {
        let now = Utc::now();
        let task_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            insert into tasks (
              id, name, type, status, idempotency_key,
              priority, requested_spec, resolved_spec, created_by, assigned_node_id,
              current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
            ) values (
              $1, 'ingest-job-01', 'stream_ingest'::task_type, 'RUNNING'::task_status, $2,
              50, $3, $3, 'tester', $4,
              1, 'immediate', $5, $5, $5, null
            )
            "#,
        )
        .bind(task_id)
        .bind(format!("ingest-{task_id}"))
        .bind(&resolved_spec)
        .bind(node_id)
        .bind(now)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
            insert into task_attempts (
              id, task_id, attempt_no, node_id, worker_kind, status,
              pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
              rtp_port, exit_code, failure_code, failure_reason,
              checkpoint_json, started_at, ended_at, created_at
            ) values (
              $1, $2, 1, $3, 'ffmpeg'::worker_kind, 'RUNNING'::attempt_status,
              null, null, null, null, null, null,
              null, null, null, null,
              null, $4, null, $4
            )
            "#,
        )
        .bind(attempt_id)
        .bind(task_id)
        .bind(node_id)
        .bind(now)
        .execute(pool)
        .await?;
        Ok(task_id)
    }

    fn short_id(value: Uuid) -> String {
        value.simple().to_string()[..8].to_string()
    }

    fn sample_create_task_payload(start_mode: &str) -> serde_json::Value {
        json!({
            "name": "relay-camera-01",
            "type": "stream_ingest",
            "priority": 50,
            "common": {
                "created_by": "alice"
            },
            "input": {
                "kind": "rtsp",
                "source_mode": "live",
                "url": "rtsp://192.168.1.10/live"
            },
            "expose": {
                "enable_rtsp": true,
                "enable_rtmp": true
            },
            "record": {
                "enabled": false
            },
            "schedule": {
                "start_mode": start_mode
            }
        })
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should read");
        serde_json::from_slice(&bytes).expect("response body should be valid json")
    }

    #[tokio::test]
    async fn ddl_migrations_create_core_schema() -> anyhow::Result<()> {
        let Some(db) = require_test_database(false).await? else {
            return Ok(());
        };
        sqlx::migrate!("../../migrations").run(&db.pool).await?;

        let tasks: Option<String> = sqlx::query_scalar("select to_regclass('public.tasks')::text")
            .fetch_one(&db.pool)
            .await?;
        let media_nodes: Option<String> =
            sqlx::query_scalar("select to_regclass('public.media_nodes')::text")
                .fetch_one(&db.pool)
                .await?;
        let task_status_type: bool = sqlx::query_scalar(
            "select exists (select 1 from pg_type where typname = 'task_status')",
        )
        .fetch_one(&db.pool)
        .await?;
        let node_name_unique_exists: bool = sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from pg_constraint
               where conrelid = 'media_nodes'::regclass
                 and conname = 'media_nodes_node_name_key'
            )
            "#,
        )
        .fetch_one(&db.pool)
        .await?;

        assert_eq!(tasks.as_deref(), Some("tasks"));
        assert_eq!(media_nodes.as_deref(), Some("media_nodes"));
        assert!(task_status_type);
        assert!(!node_name_unique_exists);

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn create_task_replays_when_idempotency_key_and_body_match() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let app = build_app(test_app_state(db.pool.clone()));
        let payload = sample_create_task_payload("manual");
        let body = serde_json::to_vec(&payload)?;

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/tasks")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Idempotency-Key", "task-create-1")
                    .body(Body::from(body.clone()))?,
            )
            .await?;
        assert_eq!(first.status(), StatusCode::CREATED);
        let first_body = json_body(first).await;

        let second = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/tasks")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Idempotency-Key", "task-create-1")
                    .body(Body::from(body))?,
            )
            .await?;
        assert_eq!(second.status(), StatusCode::OK);
        let second_body = json_body(second).await;

        assert_eq!(first_body["id"], second_body["id"]);
        assert_eq!(first_body["status"], json!("CREATED"));
        assert_eq!(second_body["status"], json!("CREATED"));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn create_task_conflicts_when_idempotency_key_body_differs() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let app = build_app(test_app_state(db.pool.clone()));
        let first_body = serde_json::to_vec(&sample_create_task_payload("manual"))?;
        let second_body = serde_json::to_vec(&json!({
            "name": "relay-camera-02",
            "type": "stream_ingest",
            "priority": 50,
            "common": {
                "created_by": "alice"
            },
            "input": {
                "kind": "rtsp",
                "source_mode": "live",
                "url": "rtsp://192.168.1.11/live"
            },
            "expose": {
                "enable_rtsp": true,
                "enable_rtmp": true
            },
            "record": {
                "enabled": false
            },
            "schedule": {
                "start_mode": "manual"
            }
        }))?;

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/tasks")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Idempotency-Key", "task-create-conflict")
                    .body(Body::from(first_body))?,
            )
            .await?;
        assert_eq!(first.status(), StatusCode::CREATED);

        let second = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/tasks")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Idempotency-Key", "task-create-conflict")
                    .body(Body::from(second_body))?,
            )
            .await?;
        assert_eq!(second.status(), StatusCode::CONFLICT);
        let second_body = json_body(second).await;
        assert_eq!(second_body["code"], json!("CONFLICT_IDEMPOTENCY_KEY"));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn create_task_returns_validation_error_for_invalid_spec() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let app = build_app(test_app_state(db.pool.clone()));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/tasks")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Idempotency-Key", "task-create-invalid")
                    .body(Body::from(serde_json::to_vec(&json!({
                        "name": "",
                        "type": "stream_ingest",
                        "common": {
                            "created_by": ""
                        },
                        "input": {}
                    }))?))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert_eq!(body["code"], json!("VALIDATION_TASK_SPEC_INVALID"));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn list_due_at_tasks_includes_queued_immediate_tasks_after_failed_initial_dispatch()
    -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(db.pool.clone());
        let task = match repository
            .create_task(
                "queued-sweep-task",
                "queued-sweep-task-hash",
                serde_json::from_value::<TaskSpec>(sample_create_task_payload("immediate"))?,
            )
            .await?
        {
            CreateTaskResult::Fresh(task) | CreateTaskResult::Replay(task) => task,
        };
        let task = repository.ensure_task_queued(task.id).await?;
        assert_eq!(task.status, media_domain::TaskStatus::Queued);

        let due_tasks = repository.list_due_at_tasks(Utc::now()).await?;
        assert!(
            due_tasks.contains(&task.id),
            "queued immediate task should be picked up by scheduler sweep"
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn list_due_at_tasks_includes_validating_immediate_tasks() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(db.pool.clone());
        let task = match repository
            .create_task(
                "validating-immediate-task",
                "validating-immediate-task-hash",
                serde_json::from_value::<TaskSpec>(sample_create_task_payload("immediate"))?,
            )
            .await?
        {
            CreateTaskResult::Fresh(task) | CreateTaskResult::Replay(task) => task,
        };
        assert_eq!(task.status, media_domain::TaskStatus::Validating);

        let due_tasks = repository.list_due_at_tasks(Utc::now()).await?;
        assert!(
            due_tasks.contains(&task.id),
            "validating immediate task should be picked up by scheduler sweep"
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn clone_task_applies_supported_request_overrides() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let source_spec = serde_json::from_value::<TaskSpec>(sample_create_task_payload("manual"))?;
        let source_task = match repository
            .create_task("source-task", "source-hash", source_spec)
            .await?
        {
            CreateTaskResult::Fresh(task) | CreateTaskResult::Replay(task) => task,
        };
        repository
            .transition_task(source_task.id, TaskOperation::Cancel)
            .await?;

        let app = build_app(test_app_state(db.pool.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/tasks/{}/clone", source_task.id))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&json!({
                        "name": "relay-camera-01-copy",
                        "priority": 15,
                        "common": { "created_by": "bob" },
                        "schedule": { "start_mode": "manual" }
                    }))?))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = json_body(response).await;
        let cloned_id = Uuid::parse_str(body["id"].as_str().expect("clone id should exist"))?;

        assert_eq!(body["name"], json!("relay-camera-01-copy"));
        assert_eq!(body["priority"], json!(15));
        assert_eq!(body["status"], json!("CREATED"));

        let detail = repository.get_task(cloned_id).await?;
        assert_eq!(detail.task.name, "relay-camera-01-copy");
        assert_eq!(detail.task.priority, 15);
        assert_eq!(detail.requested_spec["common"]["created_by"], json!("bob"));
        assert_eq!(
            detail.requested_spec["schedule"]["start_mode"],
            json!("manual")
        );

        let source_detail = repository.get_task(source_task.id).await?;
        assert_eq!(source_detail.task.name, "relay-camera-01");
        assert_eq!(
            source_detail.requested_spec["common"]["created_by"],
            json!("alice")
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn clone_task_dispatches_immediate_tasks_like_create_task() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let source_spec = serde_json::from_value::<TaskSpec>(sample_create_task_payload("manual"))?;
        let source_task = match repository
            .create_task(
                "source-task-immediate-clone",
                "source-hash-immediate-clone",
                source_spec,
            )
            .await?
        {
            CreateTaskResult::Fresh(task) | CreateTaskResult::Replay(task) => task,
        };
        repository
            .transition_task(source_task.id, TaskOperation::Cancel)
            .await?;

        let app = build_app(test_app_state(db.pool.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/tasks/{}/clone", source_task.id))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&json!({
                        "name": "relay-camera-01-immediate-copy",
                        "schedule": { "start_mode": "immediate" }
                    }))?))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = json_body(response).await;
        let cloned_id = Uuid::parse_str(body["id"].as_str().expect("clone id should exist"))?;

        assert_eq!(body["status"], json!("QUEUED"));

        let detail = repository.get_task(cloned_id).await?;
        assert_eq!(detail.task.status, media_domain::TaskStatus::Queued);
        assert_eq!(
            detail.requested_spec["schedule"]["start_mode"],
            json!("immediate")
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn clone_task_rejects_invalid_override_payload() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let source_spec = serde_json::from_value::<TaskSpec>(sample_create_task_payload("manual"))?;
        let source_task = match repository
            .create_task(
                "source-task-invalid-clone",
                "source-hash-invalid-clone",
                source_spec,
            )
            .await?
        {
            CreateTaskResult::Fresh(task) | CreateTaskResult::Replay(task) => task,
        };
        repository
            .transition_task(source_task.id, TaskOperation::Cancel)
            .await?;

        let app = build_app(test_app_state(db.pool.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/tasks/{}/clone", source_task.id))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&json!({
                        "name": "",
                        "common": { "created_by": "bob" }
                    }))?))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert_eq!(body["code"], json!("VALIDATION_TASK_SPEC_INVALID"));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn stop_task_rejects_created_state_via_api() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let source_spec = serde_json::from_value::<TaskSpec>(sample_create_task_payload("manual"))?;
        let task = match repository
            .create_task(
                "source-stop-created",
                "source-hash-stop-created",
                source_spec,
            )
            .await?
        {
            CreateTaskResult::Fresh(task) | CreateTaskResult::Replay(task) => task,
        };

        let app = build_app(test_app_state(db.pool.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/tasks/{}/stop", task.id))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = json_body(response).await;
        assert_eq!(body["code"], json!("TASK_INVALID_STATE"));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn api_rejects_missing_authorization_when_auth_is_enabled() -> anyhow::Result<()> {
        let pool = PgPoolOptions::new().connect_lazy("postgresql://postgres@127.0.0.1/postgres")?;
        let app = build_app(test_app_state_with_auth(pool));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/tasks")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Idempotency-Key", "auth-missing")
                    .body(Body::from(serde_json::to_vec(
                        &sample_create_task_payload("manual"),
                    )?))?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = json_body(response).await;
        assert_eq!(body["code"], json!("ACCESS_FORBIDDEN"));
        Ok(())
    }

    #[tokio::test]
    async fn current_session_returns_admin_when_auth_is_disabled() -> anyhow::Result<()> {
        let pool = PgPoolOptions::new().connect_lazy("postgresql://postgres@127.0.0.1/postgres")?;
        let app = build_app(test_app_state(pool));

        let response = app
            .clone()
            .oneshot(Request::builder().uri("/api/v1/me").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["auth_enabled"], json!(false));
        assert_eq!(body["role"], json!("admin"));
        assert_eq!(body["subject"], json!("auth_disabled"));
        Ok(())
    }

    #[tokio::test]
    async fn current_session_requires_bearer_token_when_auth_is_enabled() -> anyhow::Result<()> {
        let pool = PgPoolOptions::new().connect_lazy("postgresql://postgres@127.0.0.1/postgres")?;
        let app = build_app(test_app_state_with_auth(pool));

        let response = app
            .clone()
            .oneshot(Request::builder().uri("/api/v1/me").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = json_body(response).await;
        assert_eq!(body["code"], json!("ACCESS_FORBIDDEN"));
        Ok(())
    }

    #[tokio::test]
    async fn preview_task_returns_resolved_spec_without_persisting() -> anyhow::Result<()> {
        let pool = PgPoolOptions::new().connect_lazy("postgresql://postgres@127.0.0.1/postgres")?;
        let app = build_app(test_app_state(pool));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/tasks/preview")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(
                        &sample_create_task_payload("manual"),
                    )?))?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["requested_spec"]["name"], json!("relay-camera-01"));
        assert_eq!(
            body["resolved_spec"]["schedule"]["start_mode"],
            json!("manual")
        );
        assert_eq!(body["resolved_spec"]["expose"]["enable_rtsp"], json!(true));
        assert_eq!(body["resolved_spec"]["input"]["loop_enabled"], json!(false));
        Ok(())
    }

    #[tokio::test]
    async fn preview_task_preserves_record_duration_sec() -> anyhow::Result<()> {
        let pool = PgPoolOptions::new().connect_lazy("postgresql://postgres@127.0.0.1/postgres")?;
        let app = build_app(test_app_state(pool));
        let mut payload = sample_create_task_payload("manual");
        payload["record"] = json!({
            "enabled": true,
            "format": "mp4",
            "duration_sec": 300
        });

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/tasks/preview")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&payload)?))?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["requested_spec"]["record"]["duration_sec"], json!(300));
        assert_eq!(body["resolved_spec"]["record"]["duration_sec"], json!(300));
        Ok(())
    }

    #[tokio::test]
    async fn ui_routes_serve_shell_and_static_assets() -> anyhow::Result<()> {
        let pool = PgPoolOptions::new().connect_lazy("postgresql://postgres@127.0.0.1/postgres")?;
        let app = build_app(test_app_state(pool));

        let root = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty())?)
            .await?;
        assert_eq!(root.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            root.headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok()),
            Some("/overview")
        );

        let tasks = app
            .clone()
            .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
            .await?;
        assert_eq!(tasks.status(), StatusCode::OK);
        let html = to_bytes(tasks.into_body(), usize::MAX).await?;
        let html = String::from_utf8(html.to_vec())?;
        assert!(html.contains("StreamServer Console"));
        assert!(html.contains("/assets/"));
        let asset_path = html
            .split('"')
            .find(|segment| segment.starts_with("/assets/") && segment.ends_with(".js"))
            .ok_or_else(|| anyhow::anyhow!("missing built js asset reference in html"))?;

        let asset = app
            .clone()
            .oneshot(Request::builder().uri(asset_path).body(Body::empty())?)
            .await?;
        assert_eq!(asset.status(), StatusCode::OK);
        assert_eq!(
            asset
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/javascript; charset=utf-8")
        );
        let body = to_bytes(asset.into_body(), usize::MAX).await?;
        assert!(!body.is_empty());

        Ok(())
    }

    #[test]
    fn canonicalize_json_sorts_object_keys() {
        let payload = json!({
            "b": 1,
            "a": {"d": 2, "c": 1}
        });

        assert_eq!(
            canonicalize_json_value(&payload),
            "{\"a\":{\"c\":1,\"d\":2},\"b\":1}"
        );
    }

    #[test]
    fn sanitize_hook_payload_removes_secret_field() {
        let payload = json!({
            "secret": "top",
            "app": "live",
            "nested": {"secret": "kept"}
        });

        assert_eq!(
            sanitize_hook_payload(&payload),
            json!({
                "app": "live",
                "nested": {}
            })
        );
    }

    #[test]
    fn normalize_record_root_accepts_allowlisted_file_path() {
        let root = std::env::temp_dir().join("streamserver-hook-root");
        let file = root.join("task").join("output.mp4");

        assert!(
            validate_record_file_path(
                file.to_string_lossy().as_ref(),
                &[root.to_string_lossy().to_string()]
            )
            .is_ok()
        );
    }

    #[test]
    fn normalize_record_root_rejects_path_outside_allowlist() {
        let allowed = std::env::temp_dir().join("streamserver-hook-allowed");
        let blocked = std::env::temp_dir().join("streamserver-hook-blocked/output.mp4");

        let error = validate_record_file_path(
            blocked.to_string_lossy().as_ref(),
            &[allowed.to_string_lossy().to_string()],
        )
        .expect_err("path outside allowlist should be rejected");

        assert!(matches!(error, AppError::Forbidden(_)));
    }

    #[test]
    fn stream_none_reader_ack_keeps_stream_open() {
        assert_eq!(
            hook_ack("on_stream_none_reader"),
            json!({"code": 0, "close": false})
        );
    }

    #[test]
    fn record_hls_hook_resolves_file_path_from_folder_and_file_name() {
        let hook = ZlmOnRecordHlsPayload {
            app: "live".to_string(),
            stream: "camera01".to_string(),
            vhost: "__defaultVhost__".to_string(),
            file_path: None,
            file_name: Some("index.m3u8".to_string()),
            file_size: None,
            folder: Some("/data/zlm/record/live/camera01".to_string()),
            start_time: None,
            time_len: None,
            url: None,
            m3u8_url: None,
        };

        assert_eq!(
            resolve_record_hls_file_path(&hook).as_deref(),
            Some("/data/zlm/record/live/camera01/index.m3u8")
        );
    }

    #[test]
    fn build_publish_hook_response_maps_task_publish_policy() {
        let spec = serde_json::from_value::<TaskSpec>(json!({
            "type": "stream_ingest",
            "name": "push",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": true,
                "enable_http_ts": false,
                "enable_http_fmp4": true,
                "enable_hls": true,
                "stop_on_no_reader": true
            },
            "record": {"enabled": true, "format": "both", "as_player": true},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }))
        .expect("task spec should parse");

        let response = build_publish_hook_response(Some(&spec), true);

        assert_eq!(response["enable_rtsp"], json!(false));
        assert_eq!(response["enable_hls"], json!(true));
        assert_eq!(response["enable_mp4"], json!(true));
        assert_eq!(response["auto_close"], json!(true));
        assert_eq!(response["mp4_as_player"], json!(true));
    }

    #[test]
    fn build_publish_hook_response_uses_documented_defaults_without_task_spec() {
        let response = build_publish_hook_response(None, true);

        assert_eq!(response["enable_rtsp"], json!(true));
        assert_eq!(response["enable_rtmp"], json!(true));
        assert_eq!(response["enable_ts"], json!(true));
        assert_eq!(response["enable_hls"], json!(false));
        assert_eq!(response["auto_close"], json!(false));
    }

    #[test]
    fn hook_source_allowlist_parses_ip_addresses() {
        let allowlist = parse_hook_source_allowlist(&["127.0.0.1".to_string(), "::1".to_string()])
            .expect("ip allowlist should parse");

        assert_eq!(allowlist.len(), 2);
        assert!(allowlist.contains(&"127.0.0.1".parse().unwrap()));
        assert!(allowlist.contains(&"::1".parse().unwrap()));
    }

    #[test]
    fn hook_source_allowlist_rejects_invalid_ip_addresses() {
        let error = parse_hook_source_allowlist(&["not-an-ip".to_string()])
            .expect_err("invalid ip should fail");

        assert!(
            error
                .to_string()
                .contains("invalid HOOK_SOURCE_ALLOWLIST entry")
        );
    }

    #[test]
    fn hash_hook_payload_is_stable_across_key_order_and_secret() {
        let left = json!({
            "hook_name": "on_publish",
            "stream": "camera01",
            "app": "live",
            "secret": "top",
            "nested": {"b": 2, "a": 1}
        });
        let right = json!({
            "nested": {"a": 1, "b": 2},
            "app": "live",
            "stream": "camera01",
            "hook_name": "on_publish",
            "secret": "different"
        });

        assert_eq!(
            hash_hook_payload("node-1", "on_publish", &sanitize_hook_payload(&left)),
            hash_hook_payload("node-1", "on_publish", &sanitize_hook_payload(&right))
        );
    }

    #[test]
    fn parse_stream_not_found_hook_accepts_protocol_fields() {
        let payload = json!({
            "app": "live",
            "schema": "rtsp",
            "protocol": "rtsp",
            "stream": "camera01",
            "vhost": "__defaultVhost__",
            "ip": "127.0.0.1",
            "port": 554,
            "params": "token=test",
            "id": "session-1"
        });

        let hook = parse_stream_not_found_hook(&payload).expect("payload should parse");
        assert_eq!(hook.protocol.as_deref(), Some("rtsp"));
        assert_eq!(hook.stream, "camera01");
    }

    #[test]
    fn parse_rtp_server_timeout_hook_accepts_documented_fields() {
        let payload = json!({
            "local_port": 30000,
            "re_use_port": true,
            "ssrc": 0,
            "stream_id": "0195-test-1",
            "tcp_mode": 0
        });

        let hook = parse_rtp_server_timeout_hook(&payload).expect("payload should parse");
        assert_eq!(hook.local_port, Some(30000));
        assert_eq!(hook.re_use_port, Some(true));
        assert_eq!(hook.stream_id, "0195-test-1");
        assert_eq!(hook.tcp_mode, Some(0));
    }

    #[tokio::test]
    async fn tasks_list_exposes_created_by() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let app = build_app(test_app_state(db.pool.clone()));
        let payload = sample_create_task_payload("manual");
        let body = serde_json::to_vec(&payload)?;

        let create = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/tasks")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Idempotency-Key", "task-created-by-1")
                    .body(Body::from(body))?,
            )
            .await?;
        assert_eq!(create.status(), StatusCode::CREATED);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/tasks")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["items"][0]["created_by"], json!("alice"));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn list_node_heartbeats_returns_recent_samples() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(db.pool.clone());
        let node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        repository
            .record_node_heartbeat(
                node_id,
                &HeartbeatSnapshot {
                    node_time: Utc::now(),
                    cpu_percent: 12.5,
                    mem_percent: 48.0,
                    disk_percent: 61.0,
                    running_tasks: 2,
                    slot_usage: 0.4,
                    zlm_alive: true,
                    ffmpeg_alive: true,
                    gpu_runtime: Vec::new(),
                },
            )
            .await?;
        repository
            .record_node_heartbeat(
                node_id,
                &HeartbeatSnapshot {
                    node_time: Utc::now(),
                    cpu_percent: 20.0,
                    mem_percent: 52.0,
                    disk_percent: 63.0,
                    running_tasks: 3,
                    slot_usage: 0.55,
                    zlm_alive: true,
                    ffmpeg_alive: false,
                    gpu_runtime: Vec::new(),
                },
            )
            .await?;

        let app = build_app(test_app_state(db.pool.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/nodes/{node_id}/heartbeats?limit=10"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let items = body.as_array().expect("heartbeats should be a list");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["node_id"], json!(node_id));
        assert_eq!(items[0]["running_tasks"], json!(3));
        assert_eq!(items[1]["running_tasks"], json!(2));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn list_streams_enriches_viewer_count_and_play_urls_from_zlm() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let (zlm_base, zlm_handle) = spawn_zlm_stub().await?;
        let repository = TaskRepository::new(db.pool.clone());
        let node_id = Uuid::now_v7();
        upsert_test_node(&repository, node_id, &zlm_base, "http://stream.example").await?;
        let resolved_spec = json!({
            "type": "live_relay",
            "name": "relay-camera-01",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
            "publish": {
                "enable_rtsp": true,
                "enable_rtmp": true,
                "enable_http_ts": true,
                "enable_http_fmp4": true,
                "enable_hls": true
            },
            "record": {"enabled": false},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;

        let app = build_app(test_app_state(db.pool.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/streams")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let items = body.as_array().expect("streams should be a list");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["viewer_count"], json!(3));
        assert_eq!(items[0]["has_viewer"], json!(true));
        assert!(items[0]["bitrate_kbps"].as_f64().unwrap_or_default() >= 32.0);
        let play_urls = items[0]["play_urls"]
            .as_array()
            .expect("play_urls should be a list");
        assert!(
            play_urls
                .iter()
                .any(|value| value == "rtsp://stream.example/live/camera01")
        );
        assert!(
            play_urls
                .iter()
                .any(|value| value == "rtmp://stream.example/live/camera01")
        );
        assert!(
            play_urls
                .iter()
                .any(|value| value == "http://stream.example/live/camera01/hls.m3u8")
        );

        zlm_handle.abort();
        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn create_task_rejects_invalid_callback_url() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let app = build_app(test_app_state(db.pool.clone()));
        let payload = json!({
            "name": "relay-camera-01",
            "type": "live_relay",
            "priority": 50,
            "common": {
                "created_by": "alice",
                "callback_url": "not-a-url"
            },
            "input": {
                "kind": "rtsp",
                "url": "rtsp://camera.example/live"
            },
            "publish": {
                "enable_rtsp": true
            },
            "record": {
                "enabled": false
            },
            "schedule": {
                "start_mode": "manual"
            }
        });

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/tasks")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Idempotency-Key", "task-callback-invalid")
                    .body(Body::from(serde_json::to_vec(&payload)?))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert_eq!(body["code"], json!("VALIDATION_TASK_SPEC_INVALID"));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn callback_dispatcher_delivers_terminal_and_artifact_update_callbacks()
    -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::with_callback_settle_delay(
            db.pool.clone(),
            chrono::Duration::zero(),
        ));
        let node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        let (callback_url, calls, callback_handle) = spawn_callback_stub(StatusCode::OK).await?;
        let resolved_spec = json!({
            "type": "live_relay",
            "name": "relay-camera-01",
            "common": {"created_by": "tester", "callback_url": callback_url},
            "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
            "publish": {
                "enable_rtsp": true,
                "enable_http_ts": true
            },
            "record": {"enabled": true, "format": "mp4"},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        let task_id =
            insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01")
                .await?;
        repository
            .record_agent_task_event(
                node_id,
                repository::AgentTaskEventRecord {
                    task_id,
                    attempt_no: 1,
                    event_type: "succeeded".to_string(),
                    event_level: "info".to_string(),
                    message: "finished".to_string(),
                    payload: json!({}),
                },
            )
            .await?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let dispatcher = callback::spawn(
            repository.clone(),
            Client::new(),
            callback::CallbackConfig {
                timeout: std::time::Duration::from_secs(2),
                max_attempts: 3,
                initial_backoff: std::time::Duration::from_millis(50),
                max_backoff: std::time::Duration::from_millis(200),
                shared_secret: Some("secret".to_string()),
            },
            shutdown_rx,
        );

        let first_calls = wait_for_callback_count(&calls, 1).await?;
        assert_eq!(first_calls.len(), 1);
        assert_eq!(first_calls[0].1["event_type"], json!("task.completed"));
        assert_eq!(first_calls[0].1["reason"], json!("terminal_state"));
        assert_eq!(first_calls[0].1["task"]["status"], json!("SUCCEEDED"));
        assert!(
            first_calls[0].1["streams"][0]["play_urls"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .any(|value| value == "rtsp://stream.example/live/camera01")
        );
        assert_eq!(
            first_calls[0]
                .0
                .get("X-StreamServer-Signature")
                .and_then(|value| value.to_str().ok())
                .is_some(),
            true
        );

        repository
            .record_zlm_record_file_hook(
                &node_id.to_string(),
                "on_record_mp4",
                "record-hook-1",
                json!({}),
                repository::ZlmRecordFileRecord {
                    record_format: Some("mp4".to_string()),
                    schema: Some("rtsp".to_string()),
                    vhost: "__defaultVhost__".to_string(),
                    app: "live".to_string(),
                    stream: "camera01".to_string(),
                    file_path: "/data/zlm/www/record/live/camera01/clip.mp4".to_string(),
                    file_size: 4096,
                    time_len_sec: Some(12),
                    start_time: Some(Utc::now()),
                    file_name: Some("clip.mp4".to_string()),
                    folder: Some("/data/zlm/www/record/live/camera01".to_string()),
                    url: None,
                },
            )
            .await?;

        let second_calls = wait_for_callback_count(&calls, 2).await?;
        assert_eq!(second_calls[1].1["reason"], json!("artifact_update"));
        assert_eq!(
            second_calls[1].1["records"][0]["http_url"],
            json!("http://stream.example/record/live/camera01/clip.mp4")
        );

        let detail = repository.get_task(task_id).await?;
        assert_eq!(
            detail
                .callback_delivery
                .as_ref()
                .map(|value| value.status.as_str()),
            Some("delivered")
        );

        let _ = shutdown_tx.send(true);
        dispatcher.abort();
        callback_handle.abort();
        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn callback_dispatcher_delivers_running_status_callback() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::with_callback_settle_delay(
            db.pool.clone(),
            chrono::Duration::zero(),
        ));
        let node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        let (callback_url, calls, callback_handle) = spawn_callback_stub(StatusCode::OK).await?;
        let resolved_spec = json!({
            "type": "live_relay",
            "name": "relay-camera-01",
            "common": {"created_by": "tester", "callback_url": callback_url},
            "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
            "publish": {
                "enable_rtsp": true,
                "enable_http_ts": true
            },
            "record": {"enabled": false},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        let task_id =
            insert_starting_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01")
                .await?;
        repository
            .record_agent_task_event(
                node_id,
                repository::AgentTaskEventRecord {
                    task_id,
                    attempt_no: 1,
                    event_type: "running".to_string(),
                    event_level: "info".to_string(),
                    message: "task is running".to_string(),
                    payload: json!({}),
                },
            )
            .await?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let dispatcher = callback::spawn(
            repository.clone(),
            Client::new(),
            callback::CallbackConfig {
                timeout: std::time::Duration::from_secs(2),
                max_attempts: 3,
                initial_backoff: std::time::Duration::from_millis(50),
                max_backoff: std::time::Duration::from_millis(200),
                shared_secret: None,
            },
            shutdown_rx,
        );

        let delivered = wait_for_callback_count(&calls, 1).await?;
        assert_eq!(
            delivered[0]
                .0
                .get("X-StreamServer-Event")
                .and_then(|value| value.to_str().ok()),
            Some("task.status")
        );
        assert_eq!(delivered[0].1["event_type"], json!("task.status"));
        assert_eq!(delivered[0].1["reason"], json!("running"));
        assert_eq!(delivered[0].1["status"], json!("RUNNING"));
        assert_eq!(delivered[0].1["task"]["status"], json!("RUNNING"));
        assert_eq!(delivered[0].1["attempt"]["status"], json!("RUNNING"));
        assert_eq!(
            delivered[0].1["latest_event"]["event_type"],
            json!("running")
        );
        assert!(delivered[0].1.get("streams").is_none());
        assert!(delivered[0].1.get("records").is_none());
        assert!(delivered[0].1.get("file_artifacts").is_none());

        let detail = repository.get_task(task_id).await?;
        assert_eq!(
            detail
                .callback_delivery
                .as_ref()
                .map(|value| value.event_type.as_str()),
            Some("task.status")
        );
        assert_eq!(
            detail
                .callback_delivery
                .as_ref()
                .map(|value| value.reason.as_str()),
            Some("running")
        );

        let _ = shutdown_tx.send(true);
        dispatcher.abort();
        callback_handle.abort();
        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn hls_expose_hooks_do_not_create_record_rows() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(db.pool.clone());
        let node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        let resolved_spec = json!({
            "type": "stream_ingest",
            "name": "live-hls-expose",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
            "stream": {"app": "live", "name": "camera01"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": true
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {"enabled": false},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        let task_id =
            insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01")
                .await?;

        repository
            .record_zlm_record_file_hook(
                &node_id.to_string(),
                "on_record_hls",
                "hls-expose-hook-1",
                json!({}),
                repository::ZlmRecordFileRecord {
                    record_format: Some("hls".to_string()),
                    schema: None,
                    vhost: "__defaultVhost__".to_string(),
                    app: "live".to_string(),
                    stream: "camera01".to_string(),
                    file_path: "/data/zlm/www/live/camera01/hls.m3u8".to_string(),
                    file_size: 512,
                    time_len_sec: Some(6),
                    start_time: Some(Utc::now()),
                    file_name: Some("hls.m3u8".to_string()),
                    folder: Some("/data/zlm/www/live/camera01".to_string()),
                    url: Some("http://stream.example/live/camera01/hls.m3u8".to_string()),
                },
            )
            .await?;

        let records = repository.list_task_record_files(task_id).await?;
        assert!(records.is_empty());

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn hls_record_hooks_only_persist_playlist_rows() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(db.pool.clone());
        let node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        let resolved_spec = json!({
            "type": "stream_ingest",
            "name": "live-hls-record",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
            "stream": {"app": "live", "name": "camera01"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {"enabled": true, "format": "hls"},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        let task_id =
            insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01")
                .await?;

        repository
            .record_zlm_record_file_hook(
                &node_id.to_string(),
                "on_record_ts",
                "hls-record-hook-ts-1",
                json!({}),
                repository::ZlmRecordFileRecord {
                    record_format: Some("hls".to_string()),
                    schema: None,
                    vhost: "__defaultVhost__".to_string(),
                    app: "live".to_string(),
                    stream: "camera01".to_string(),
                    file_path: "/data/zlm/www/record/live/camera01/index-00001.ts".to_string(),
                    file_size: 4096,
                    time_len_sec: Some(6),
                    start_time: Some(Utc::now()),
                    file_name: Some("index-00001.ts".to_string()),
                    folder: Some("/data/zlm/www/record/live/camera01".to_string()),
                    url: Some(
                        "http://stream.example/record/live/camera01/index-00001.ts".to_string(),
                    ),
                },
            )
            .await?;
        repository
            .record_zlm_record_file_hook(
                &node_id.to_string(),
                "on_record_hls",
                "hls-record-hook-m3u8-1",
                json!({}),
                repository::ZlmRecordFileRecord {
                    record_format: Some("hls".to_string()),
                    schema: None,
                    vhost: "__defaultVhost__".to_string(),
                    app: "live".to_string(),
                    stream: "camera01".to_string(),
                    file_path: "/data/zlm/www/record/live/camera01/index.m3u8".to_string(),
                    file_size: 1024,
                    time_len_sec: Some(30),
                    start_time: Some(Utc::now()),
                    file_name: Some("index.m3u8".to_string()),
                    folder: Some("/data/zlm/www/record/live/camera01".to_string()),
                    url: Some("http://stream.example/record/live/camera01/index.m3u8".to_string()),
                },
            )
            .await?;

        let records = repository.list_task_record_files(task_id).await?;
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].file_path,
            "/data/zlm/www/record/live/camera01/index.m3u8"
        );
        assert_eq!(
            records[0].http_url.as_deref(),
            Some("http://stream.example/record/live/camera01/index.m3u8")
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn running_status_callback_is_not_duplicated_after_delivery() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::with_callback_settle_delay(
            db.pool.clone(),
            chrono::Duration::zero(),
        ));
        let node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        let (callback_url, calls, callback_handle) = spawn_callback_stub(StatusCode::OK).await?;
        let resolved_spec = json!({
            "type": "live_relay",
            "name": "relay-camera-01",
            "common": {"created_by": "tester", "callback_url": callback_url},
            "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
            "publish": {
                "enable_rtsp": true
            },
            "record": {"enabled": false},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        let task_id =
            insert_starting_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01")
                .await?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let dispatcher = callback::spawn(
            repository.clone(),
            Client::new(),
            callback::CallbackConfig {
                timeout: std::time::Duration::from_secs(2),
                max_attempts: 3,
                initial_backoff: std::time::Duration::from_millis(50),
                max_backoff: std::time::Duration::from_millis(200),
                shared_secret: None,
            },
            shutdown_rx,
        );

        repository
            .record_agent_task_event(
                node_id,
                repository::AgentTaskEventRecord {
                    task_id,
                    attempt_no: 1,
                    event_type: "running".to_string(),
                    event_level: "info".to_string(),
                    message: "task is running".to_string(),
                    payload: json!({}),
                },
            )
            .await?;

        let delivered = wait_for_callback_count(&calls, 1).await?;
        assert_eq!(delivered.len(), 1);

        repository
            .record_agent_progress(
                node_id,
                repository::TaskProgressRecord {
                    task_id,
                    attempt_no: 1,
                    frame: 10,
                    fps: 25.0,
                    bitrate_kbps: 3200.0,
                    speed: 1.0,
                    out_time_ms: 400,
                    dup_frames: 0,
                    drop_frames: 0,
                },
            )
            .await?;

        let callback_count: i64 = sqlx::query_scalar(
            r#"
            select count(*)
              from task_callback_outbox
             where task_id = $1
               and attempt_no = 1
               and event_type = 'task.status'
               and reason = 'running'
            "#,
        )
        .bind(task_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(callback_count, 1);

        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        let final_calls = calls.lock().await.clone();
        assert_eq!(final_calls.len(), 1);

        let _ = shutdown_tx.send(true);
        dispatcher.abort();
        callback_handle.abort();
        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn callback_payload_includes_file_artifact_http_url_for_transcode_output()
    -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::with_callback_settle_delay(
            db.pool.clone(),
            chrono::Duration::zero(),
        ));
        let node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        let (callback_url, calls, callback_handle) = spawn_callback_stub(StatusCode::OK).await?;
        let resolved_spec = json!({
            "type": "file_transcode",
            "name": "transcode-job-01",
            "common": {"created_by": "tester", "callback_url": callback_url},
            "input": {"kind": "file", "url": "input-hevc.mp4"},
            "process": {"mode": "copy_or_transcode"},
            "publish": {"kind": "file"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        let task_id = insert_running_transcode_task(&db.pool, node_id, resolved_spec).await?;
        repository
            .record_agent_snapshot(
                node_id,
                repository::TaskSnapshotRecord {
                    runtime_id: Uuid::now_v7(),
                    task_id,
                    attempt_no: 1,
                    worker_kind: "ffmpeg".to_string(),
                    pid: Some(1234),
                    state: "RUNNING".to_string(),
                    command_line: Some("ffmpeg ...".to_string()),
                    outputs: vec![
                        "/data/zlm/www/artifacts/transcode/verify/output.mp4".to_string(),
                    ],
                    metadata: json!({
                        "transcode_artifact": {
                            "file_name": "output.mp4",
                            "file_path": "/data/zlm/www/artifacts/transcode/verify/output.mp4",
                            "file_size": 8192
                        }
                    }),
                },
            )
            .await?;
        repository
            .record_agent_task_event(
                node_id,
                repository::AgentTaskEventRecord {
                    task_id,
                    attempt_no: 1,
                    event_type: "succeeded".to_string(),
                    event_level: "info".to_string(),
                    message: "finished".to_string(),
                    payload: json!({}),
                },
            )
            .await?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let dispatcher = callback::spawn(
            repository.clone(),
            Client::new(),
            callback::CallbackConfig {
                timeout: std::time::Duration::from_secs(2),
                max_attempts: 3,
                initial_backoff: std::time::Duration::from_millis(50),
                max_backoff: std::time::Duration::from_millis(200),
                shared_secret: None,
            },
            shutdown_rx,
        );

        let delivered = wait_for_callback_count(&calls, 1).await?;
        assert_eq!(
            delivered[0].1["file_artifacts"][0]["http_url"],
            json!("http://stream.example/artifacts/transcode/verify/output.mp4")
        );
        assert_eq!(
            delivered[0].1["file_artifacts"][0]["artifact_kind"],
            json!("transcode_output")
        );

        let _ = shutdown_tx.send(true);
        dispatcher.abort();
        callback_handle.abort();
        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn callback_payload_includes_file_artifact_http_url_for_bridge_output()
    -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::with_callback_settle_delay(
            db.pool.clone(),
            chrono::Duration::zero(),
        ));
        let node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        let (callback_url, calls, callback_handle) = spawn_callback_stub(StatusCode::OK).await?;
        let resolved_spec = json!({
            "type": "stream_bridge",
            "name": "bridge-job-01",
            "common": {"created_by": "tester", "callback_url": callback_url},
            "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
            "publish": {"kind": "file", "format": "mp4"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        let task_id = insert_running_bridge_task(&db.pool, node_id, resolved_spec).await?;
        repository
            .record_agent_snapshot(
                node_id,
                repository::TaskSnapshotRecord {
                    runtime_id: Uuid::now_v7(),
                    task_id,
                    attempt_no: 1,
                    worker_kind: "ffmpeg".to_string(),
                    pid: Some(2234),
                    state: "RUNNING".to_string(),
                    command_line: Some("ffmpeg ...".to_string()),
                    outputs: vec!["/data/zlm/www/artifacts/bridge/verify/output.mp4".to_string()],
                    metadata: json!({
                        "bridge_artifact": {
                            "file_name": "output.mp4",
                            "file_path": "/data/zlm/www/artifacts/bridge/verify/output.mp4",
                            "file_size": 4096
                        }
                    }),
                },
            )
            .await?;
        repository
            .record_agent_task_event(
                node_id,
                repository::AgentTaskEventRecord {
                    task_id,
                    attempt_no: 1,
                    event_type: "succeeded".to_string(),
                    event_level: "info".to_string(),
                    message: "finished".to_string(),
                    payload: json!({}),
                },
            )
            .await?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let dispatcher = callback::spawn(
            repository.clone(),
            Client::new(),
            callback::CallbackConfig {
                timeout: std::time::Duration::from_secs(2),
                max_attempts: 3,
                initial_backoff: std::time::Duration::from_millis(50),
                max_backoff: std::time::Duration::from_millis(200),
                shared_secret: None,
            },
            shutdown_rx,
        );

        let delivered = wait_for_callback_count(&calls, 1).await?;
        assert_eq!(
            delivered[0].1["file_artifacts"][0]["http_url"],
            json!("http://stream.example/artifacts/bridge/verify/output.mp4")
        );
        assert_eq!(
            delivered[0].1["file_artifacts"][0]["artifact_kind"],
            json!("bridge_output")
        );

        let _ = shutdown_tx.send(true);
        dispatcher.abort();
        callback_handle.abort();
        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn callback_payload_includes_file_artifact_http_url_for_stream_ingest_fast_record()
    -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::with_callback_settle_delay(
            db.pool.clone(),
            chrono::Duration::zero(),
        ));
        let node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        let (callback_url, calls, callback_handle) = spawn_callback_stub(StatusCode::OK).await?;
        let resolved_spec = json!({
            "type": "stream_ingest",
            "name": "ingest-fast-record-01",
            "common": {"created_by": "tester", "callback_url": callback_url},
            "input": {"kind": "http_mp4", "source_mode": "vod", "url": "http://vod.example.com/archive.mp4"},
            "stream": {"app": "live", "name": "archive-fast"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {"enabled": true, "format": "mp4", "duration_sec": 300},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        let task_id = insert_running_ingest_task(&db.pool, node_id, resolved_spec).await?;
        repository
            .record_agent_snapshot(
                node_id,
                repository::TaskSnapshotRecord {
                    runtime_id: Uuid::now_v7(),
                    task_id,
                    attempt_no: 1,
                    worker_kind: "ffmpeg".to_string(),
                    pid: Some(3234),
                    state: "RUNNING".to_string(),
                    command_line: Some("ffmpeg ...".to_string()),
                    outputs: vec![
                        "/data/zlm/www/artifacts/stream-ingest-record/verify/output.mp4"
                            .to_string(),
                    ],
                    metadata: json!({
                        "stream_ingest_record_artifacts": [
                            {
                                "file_name": "output.mp4",
                                "file_path": "/data/zlm/www/artifacts/stream-ingest-record/verify/output.mp4",
                                "file_size": 16384
                            }
                        ]
                    }),
                },
            )
            .await?;
        repository
            .record_agent_task_event(
                node_id,
                repository::AgentTaskEventRecord {
                    task_id,
                    attempt_no: 1,
                    event_type: "succeeded".to_string(),
                    event_level: "info".to_string(),
                    message: "finished".to_string(),
                    payload: json!({}),
                },
            )
            .await?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let dispatcher = callback::spawn(
            repository.clone(),
            Client::new(),
            callback::CallbackConfig {
                timeout: std::time::Duration::from_secs(2),
                max_attempts: 3,
                initial_backoff: std::time::Duration::from_millis(50),
                max_backoff: std::time::Duration::from_millis(200),
                shared_secret: None,
            },
            shutdown_rx,
        );

        let delivered = wait_for_callback_count(&calls, 1).await?;
        assert_eq!(
            delivered[0].1["file_artifacts"][0]["http_url"],
            json!("http://stream.example/artifacts/stream-ingest-record/verify/output.mp4")
        );
        assert_eq!(
            delivered[0].1["file_artifacts"][0]["artifact_kind"],
            json!("stream_ingest_record")
        );

        let _ = shutdown_tx.send(true);
        dispatcher.abort();
        callback_handle.abort();
        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn callback_dispatcher_delivers_bridge_artifact_update_callback() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::with_callback_settle_delay(
            db.pool.clone(),
            chrono::Duration::zero(),
        ));
        let node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        let (callback_url, calls, callback_handle) = spawn_callback_stub(StatusCode::OK).await?;
        let resolved_spec = json!({
            "type": "stream_bridge",
            "name": "bridge-job-01",
            "common": {"created_by": "tester", "callback_url": callback_url},
            "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
            "publish": {"kind": "file", "format": "mp4"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        let task_id = insert_running_bridge_task(&db.pool, node_id, resolved_spec).await?;
        repository
            .record_agent_task_event(
                node_id,
                repository::AgentTaskEventRecord {
                    task_id,
                    attempt_no: 1,
                    event_type: "succeeded".to_string(),
                    event_level: "info".to_string(),
                    message: "finished".to_string(),
                    payload: json!({}),
                },
            )
            .await?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let dispatcher = callback::spawn(
            repository.clone(),
            Client::new(),
            callback::CallbackConfig {
                timeout: std::time::Duration::from_secs(2),
                max_attempts: 3,
                initial_backoff: std::time::Duration::from_millis(50),
                max_backoff: std::time::Duration::from_millis(200),
                shared_secret: None,
            },
            shutdown_rx,
        );

        let first_calls = wait_for_callback_count(&calls, 1).await?;
        assert_eq!(first_calls[0].1["reason"], json!("terminal_state"));

        repository
            .record_agent_snapshot(
                node_id,
                repository::TaskSnapshotRecord {
                    runtime_id: Uuid::now_v7(),
                    task_id,
                    attempt_no: 1,
                    worker_kind: "ffmpeg".to_string(),
                    pid: Some(2234),
                    state: "EXITED".to_string(),
                    command_line: Some("ffmpeg ...".to_string()),
                    outputs: vec!["/data/zlm/www/artifacts/bridge/late/output.mp4".to_string()],
                    metadata: json!({
                        "bridge_artifact": {
                            "file_name": "output.mp4",
                            "file_path": "/data/zlm/www/artifacts/bridge/late/output.mp4",
                            "file_size": 4096
                        }
                    }),
                },
            )
            .await?;

        let second_calls = wait_for_callback_count(&calls, 2).await?;
        assert_eq!(second_calls[1].1["reason"], json!("artifact_update"));
        assert_eq!(
            second_calls[1].1["file_artifacts"][0]["http_url"],
            json!("http://stream.example/artifacts/bridge/late/output.mp4")
        );
        assert_eq!(
            second_calls[1].1["file_artifacts"][0]["artifact_kind"],
            json!("bridge_output")
        );

        let _ = shutdown_tx.send(true);
        dispatcher.abort();
        callback_handle.abort();
        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn list_file_artifacts_returns_bridge_outputs() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(db.pool.clone());
        let node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        let resolved_spec = json!({
            "type": "stream_bridge",
            "name": "bridge-job-01",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
            "publish": {"kind": "file", "format": "mp4"},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        let task_id = insert_running_bridge_task(&db.pool, node_id, resolved_spec).await?;
        repository
            .record_agent_snapshot(
                node_id,
                repository::TaskSnapshotRecord {
                    runtime_id: Uuid::now_v7(),
                    task_id,
                    attempt_no: 1,
                    worker_kind: "ffmpeg".to_string(),
                    pid: Some(2234),
                    state: "RUNNING".to_string(),
                    command_line: Some("ffmpeg ...".to_string()),
                    outputs: vec!["/data/zlm/www/artifacts/bridge/verify/output.mp4".to_string()],
                    metadata: json!({
                        "bridge_artifact": {
                            "file_name": "output.mp4",
                            "file_path": "/data/zlm/www/artifacts/bridge/verify/output.mp4",
                            "file_size": 4096
                        }
                    }),
                },
            )
            .await?;

        let app = build_app(test_app_state(db.pool.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/file-artifacts?page=1&page_size=10")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["total"], json!(1));
        assert_eq!(body["items"][0]["task_id"], json!(task_id.to_string()));
        assert_eq!(body["items"][0]["artifact_kind"], json!("bridge_output"));
        assert_eq!(
            body["items"][0]["http_url"],
            json!("http://stream.example/artifacts/bridge/verify/output.mp4")
        );
        assert_eq!(
            body["items"][0]["file_path"],
            json!("/data/zlm/www/artifacts/bridge/verify/output.mp4")
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn list_file_artifacts_returns_stream_ingest_fast_record_outputs() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(db.pool.clone());
        let node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        let resolved_spec = json!({
            "type": "stream_ingest",
            "name": "ingest-fast-record-01",
            "common": {"created_by": "tester"},
            "input": {"kind": "http_mp4", "source_mode": "vod", "url": "http://vod.example.com/archive.mp4"},
            "stream": {"app": "live", "name": "archive-fast"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {"enabled": true, "format": "mp4", "duration_sec": 300},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        let task_id = insert_running_ingest_task(&db.pool, node_id, resolved_spec).await?;
        repository
            .record_agent_snapshot(
                node_id,
                repository::TaskSnapshotRecord {
                    runtime_id: Uuid::now_v7(),
                    task_id,
                    attempt_no: 1,
                    worker_kind: "ffmpeg".to_string(),
                    pid: Some(3234),
                    state: "RUNNING".to_string(),
                    command_line: Some("ffmpeg ...".to_string()),
                    outputs: vec![
                        "/data/zlm/www/artifacts/stream-ingest-record/verify/output.mp4"
                            .to_string(),
                    ],
                    metadata: json!({
                        "stream_ingest_record_artifacts": [
                            {
                                "file_name": "output.mp4",
                                "file_path": "/data/zlm/www/artifacts/stream-ingest-record/verify/output.mp4",
                                "file_size": 16384
                            }
                        ]
                    }),
                },
            )
            .await?;

        let app = build_app(test_app_state(db.pool.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/file-artifacts?artifact_kind=stream_ingest_record&page=1&page_size=10")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["total"], json!(1));
        assert_eq!(body["items"][0]["task_id"], json!(task_id.to_string()));
        assert_eq!(
            body["items"][0]["artifact_kind"],
            json!("stream_ingest_record")
        );
        assert_eq!(
            body["items"][0]["http_url"],
            json!("http://stream.example/artifacts/stream-ingest-record/verify/output.mp4")
        );
        assert_eq!(
            body["items"][0]["file_path"],
            json!("/data/zlm/www/artifacts/stream-ingest-record/verify/output.mp4")
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn list_streams_omits_stale_entries_when_runtime_lookup_succeeds() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let (zlm_base, zlm_handle) = spawn_zlm_stub().await?;
        let repository = TaskRepository::new(db.pool.clone());
        let node_id = Uuid::now_v7();
        upsert_test_node(&repository, node_id, &zlm_base, "http://stream.example").await?;
        let resolved_spec = json!({
            "type": "live_relay",
            "name": "relay-camera",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
            "publish": {},
            "record": {"enabled": false},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        });
        insert_running_stream_task(&db.pool, node_id, resolved_spec.clone(), "live", "camera01")
            .await?;
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera02").await?;

        let app = build_app(test_app_state(db.pool.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/streams")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let items = body.as_array().expect("streams should be a list");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["stream"], json!("camera01"));

        zlm_handle.abort();
        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn debug_hooks_route_filters_by_node() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(db.pool.clone());
        let node_id = Uuid::now_v7();
        let other_node_id = Uuid::now_v7();
        upsert_test_node(
            &repository,
            node_id,
            "http://127.0.0.1:65535",
            "http://stream.example",
        )
        .await?;
        upsert_test_node(
            &repository,
            other_node_id,
            "http://127.0.0.1:65534",
            "http://stream-b.example",
        )
        .await?;
        sqlx::query(
            r#"
            insert into hook_events (
              id, server_id, hook_name, dedup_key, payload, received_at, processed_at
            ) values
              ($1, $2, 'on_publish', 'hook-node-a', '{"app":"live"}'::jsonb, $3, $3),
              ($4, $5, 'on_record_mp4', 'hook-node-b', '{"app":"archive"}'::jsonb, $3, $3)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(node_id.to_string())
        .bind(Utc::now())
        .bind(Uuid::now_v7())
        .bind(other_node_id.to_string())
        .execute(&db.pool)
        .await?;

        let app = build_app(test_app_state(db.pool.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/debug/hooks?node_id={node_id}&limit=10"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let items = body.as_array().expect("hooks should be a list");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["server_id"], json!(node_id.to_string()));
        assert_eq!(items[0]["hook_name"], json!("on_publish"));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn debug_zlm_snap_returns_data_url() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let (zlm_base, zlm_handle) = spawn_zlm_stub().await?;
        let repository = TaskRepository::new(db.pool.clone());
        let node_id = Uuid::now_v7();
        upsert_test_node(&repository, node_id, &zlm_base, "http://stream.example").await?;
        let app = build_app(test_app_state(db.pool.clone()));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/v1/debug/zlm/snap?node_id={node_id}&url={}",
                        "rtsp%3A%2F%2Fstream.example%2Flive%2Fcamera01"
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["content_type"], json!("image/jpeg"));
        assert!(
            body["data_url"]
                .as_str()
                .unwrap_or_default()
                .starts_with("data:image/jpeg;base64,")
        );

        zlm_handle.abort();
        db.cleanup().await?;
        Ok(())
    }
}
