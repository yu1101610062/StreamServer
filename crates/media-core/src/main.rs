#[cfg(test)]
#[path = "tests/main.rs"]
mod tests;

#[cfg(test)]
mod test_database;

mod agent_identity;
mod agent_management;
mod auth;
mod callback;
mod config;
mod control_plane;
mod error;
mod repository;
mod repository_paths;
mod scheduler;
mod source_gateway;
mod telemetry;
mod ui;
mod upload;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{self, Read, Write},
    net::{IpAddr, SocketAddr, TcpListener as StdTcpListener},
    path::{Component, Path as FsPath, PathBuf},
    sync::Arc,
};

use agent_identity::{
    AgentCertificateAuthority, AgentEnrollmentAdmissionError, AgentEnrollmentPublicConfig,
    AgentIdentityService, AgentIdentityServiceError, CreatedAgentEnrollment,
    VerifiedAgentEnrollmentToken,
};
use agent_management::{
    AgentCapabilitySigner, AgentManagementClient, AgentManagementService,
    AgentManagementTlsMaterial, RoutedAgentManagementService, TracingAgentManagementAuditSink,
};
use anyhow::Context;
use auth::{
    ApiPermission, AuthConfig, extract_bearer_token, generate_refresh_token, hash_password,
    hash_refresh_token, maybe_extract_bearer_token, verify_password,
};
use axum::{
    Json, Router,
    extract::{
        ConnectInfo, DefaultBodyLimit, Extension, FromRequestParts, Path, Query, Request, State,
    },
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use axum_server::{Handle as HttpServerHandle, tls_rustls::RustlsConfig};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::{DateTime, Utc};
use control_plane::{
    ControlPlaneService, NodeLiveLoad, ZlmDebugCallError, ZlmDebugCommand, ZlmDebugResult,
};
use error::AppError;
use media_domain::RecordingControlSpec;
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
use x509_parser::prelude::FromDer;

use media_domain::{TaskOperation, TaskSpec};

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    repository: Arc<TaskRepository>,
    control_plane: ControlPlaneService,
    started_at: DateTime<Utc>,
    environment: String,
    auth: AuthConfig,
    agent_identity: Option<AgentIdentityService>,
    agent_management: Option<Arc<dyn AgentManagementService>>,
    hook_shared_secret: String,
    hook_source_allowlist: Vec<IpAddr>,
    zlm_auto_close_on_no_reader_enabled: bool,
    storage_allowlist: Vec<String>,
}

#[derive(Debug, Clone)]
struct ZlmHookBusinessContext {
    repository: Arc<TaskRepository>,
    zlm_auto_close_on_no_reader_enabled: bool,
    storage_allowlist: Vec<String>,
}

impl ZlmHookBusinessContext {
    fn new(
        repository: Arc<TaskRepository>,
        zlm_auto_close_on_no_reader_enabled: bool,
        storage_allowlist: Vec<String>,
    ) -> Self {
        Self {
            repository,
            zlm_auto_close_on_no_reader_enabled,
            storage_allowlist,
        }
    }

    fn from_app_state(state: &AppState) -> Self {
        Self::new(
            state.repository.clone(),
            state.zlm_auto_close_on_no_reader_enabled,
            state.storage_allowlist.clone(),
        )
    }
}

#[derive(Debug, Clone)]
struct CoreZlmHookHandler {
    context: ZlmHookBusinessContext,
}

impl CoreZlmHookHandler {
    fn new(
        repository: Arc<TaskRepository>,
        zlm_auto_close_on_no_reader_enabled: bool,
        storage_allowlist: Vec<String>,
    ) -> Self {
        Self {
            context: ZlmHookBusinessContext::new(
                repository,
                zlm_auto_close_on_no_reader_enabled,
                storage_allowlist,
            ),
        }
    }
}

impl control_plane::ZlmHookHandler for CoreZlmHookHandler {
    fn handle(
        &self,
        request: control_plane::AuthenticatedZlmHook,
    ) -> control_plane::ZlmHookFuture<'_> {
        Box::pin(async move {
            match process_authenticated_zlm_hook(
                &self.context,
                request.node_id,
                request.node_id.to_string(),
                request.hook_name,
                request.body,
            )
            .await
            {
                Ok((status, Json(body))) => control_plane::ZlmHookHandlerResponse {
                    http_status: status.as_u16(),
                    body,
                },
                Err(error) => map_zlm_hook_handler_error(error),
            }
        })
    }
}

fn map_zlm_hook_handler_error(error: AppError) -> control_plane::ZlmHookHandlerResponse {
    let (http_status, message) = match error {
        AppError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
        AppError::Validation(message) => (StatusCode::BAD_REQUEST, message.to_string()),
        AppError::Unauthorized(message) => (StatusCode::UNAUTHORIZED, message),
        AppError::Forbidden(message) => (StatusCode::FORBIDDEN, message),
        AppError::Conflict(message) => (StatusCode::CONFLICT, message),
        AppError::NotFound(message) => (StatusCode::NOT_FOUND, message),
        AppError::ControlPlane(_) | AppError::Repository(_) | AppError::Internal(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "ZLM hook processing failed".to_string(),
        ),
    };
    control_plane::ZlmHookHandlerResponse {
        http_status: http_status.as_u16(),
        body: json!({
            "code": -1,
            "msg": message,
        }),
    }
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
    Serve {
        insecure_dev: bool,
    },
    Help {
        auth_only: bool,
    },
    AgentHelp,
    AgentCreateEnrollment {
        node_id: Uuid,
    },
    CheckAuthConfig,
    CheckAdmin,
    BootstrapStatus {
        username: String,
        handoff_id: String,
    },
    RecoverBootstrapAdmin {
        username: String,
        handoff_id: String,
        expected_version: String,
    },
    BootstrapAdmin {
        username: String,
    },
    ResetPassword {
        username: String,
    },
}

const BOOTSTRAP_ADMIN_MUST_CHANGE_PASSWORD: bool = true;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = parse_cli_command()?.expect("server mode is always explicit after CLI parsing");
    let insecure_dev = match command {
        CliCommand::Serve { insecure_dev } => insecure_dev,
        CliCommand::Help { auth_only } => {
            print_cli_help(auth_only);
            return Ok(());
        }
        CliCommand::AgentHelp => {
            print_agent_cli_help();
            return Ok(());
        }
        CliCommand::AgentCreateEnrollment { node_id } => {
            return run_agent_create_enrollment(node_id).await;
        }
        other => return run_cli_command(other).await,
    };

    let settings = config::Settings::load_with_insecure_dev(insecure_dev)?;
    telemetry::init(&settings.logging);

    info!(
        environment = %settings.environment,
        http_addr = %settings.core.http_addr,
        grpc_addr = %settings.core.grpc_addr,
        "starting media-core"
    );
    let agent_enrollment_material = load_agent_certificate_authority(&settings.core, Utc::now())?;

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&settings.core.database_url)
        .await?;
    sqlx::migrate!("../../migrations").run(&pool).await?;

    let repository = Arc::new(TaskRepository::with_callback_settle_delay(
        pool,
        chrono::Duration::milliseconds(settings.core.callback_settle_delay_ms as i64),
    ));
    let core_instance_id = agent_enrollment_material
        .as_ref()
        .map(|(_, _, core_instance_id)| *core_instance_id)
        .or_else(|| Uuid::parse_str(settings.core.core_instance_id.trim()).ok())
        .filter(|value| !value.is_nil())
        .unwrap_or_else(Uuid::now_v7);
    let agent_management_client = agent_enrollment_material
        .as_ref()
        .map(|(_, public_config, _)| {
            load_agent_management_client(&settings.core, public_config.capability_jwt_kid.as_str())
        })
        .transpose()?;
    let agent_identity = agent_enrollment_material.map(|(authority, public_config, _)| {
        AgentIdentityService::new(repository.clone(), authority, public_config)
    });
    let source_gateway = source_gateway::SourceGatewayClient::from_settings(&settings.core)?;
    let control_plane = if let Some(source_gateway) = source_gateway {
        ControlPlaneService::with_source_gateway_and_core_instance_id(
            repository.clone(),
            source_gateway,
            core_instance_id,
        )
    } else {
        ControlPlaneService::new_with_core_instance_id(repository.clone(), core_instance_id)
    };
    let control_plane = control_plane.with_zlm_hook_handler(Arc::new(CoreZlmHookHandler::new(
        repository.clone(),
        settings.core.zlm_auto_close_on_no_reader_enabled,
        settings.core.storage_allowlist.clone(),
    )));
    let control_plane = match (&agent_identity, &agent_management_client) {
        (Some(agent_identity), Some(agent_management_client)) => control_plane
            .with_agent_identity_and_readiness(
                agent_identity.clone(),
                agent_management_client.clone(),
            ),
        (None, None) => control_plane,
        _ => anyhow::bail!(
            "Agent identity and authenticated management client must be configured together"
        ),
    };
    let agent_management = agent_management_client.map(|client| {
        Arc::new(RoutedAgentManagementService::new(
            client,
            Arc::new(control_plane.clone()),
        )) as Arc<dyn AgentManagementService>
    });
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
        agent_identity,
        agent_management,
        hook_shared_secret: settings.core.hook_shared_secret.clone(),
        hook_source_allowlist,
        zlm_auto_close_on_no_reader_enabled: settings.core.zlm_auto_close_on_no_reader_enabled,
        storage_allowlist: settings.core.storage_allowlist.clone(),
    };

    let app = build_app(state);

    let listener = TcpListener::bind(&settings.core.http_addr).await?;
    let http_listen_addr = listener.local_addr()?;
    let listener = listener.into_std()?;
    let http_tls_config = load_http_tls_config(&settings.core).await?;
    info!(
        listen_addr = %http_listen_addr,
        tls = http_tls_config.is_some(),
        "media-core http server ready"
    );

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

    let http_server = serve_http(listener, app, http_tls_config, shutdown_rx.clone());
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
    let enrollment_route = post(enroll_agent)
        .layer(DefaultBodyLimit::max(40 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admit_agent_enrollment,
        ));
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
        .route("/admin/agent-enrollments", post(create_agent_enrollment))
        .route("/agent-enroll", enrollment_route)
        .route("/tasks/preview", post(ui::preview_task))
        .route(
            "/uploads/media",
            post(upload::upload_media)
                .get(upload::list_media_upload_assets)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/uploads/media/{id}",
            get(upload::get_media_upload_asset).delete(upload::delete_media_upload_asset),
        )
        .route("/tasks", post(create_task).get(list_tasks))
        .route("/tasks/{id}", get(get_task).delete(delete_task))
        .route("/tasks/{id}/events", get(get_task_events))
        .route("/tasks/{id}/logs", get(get_task_logs))
        .route("/tasks/{id}/resolved-spec", get(get_resolved_spec))
        .route("/tasks/{id}/start", post(start_task))
        .route("/tasks/{id}/stop", post(stop_task))
        .route("/tasks/{id}/cancel", post(cancel_task))
        .route("/tasks/{id}/retry", post(retry_task))
        .route("/tasks/{id}/recording/start", post(start_task_recording))
        .route("/tasks/{id}/recording/stop", post(stop_task_recording))
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

    let app = Router::new()
        .route("/health/live", get(live_health))
        .route("/health/ready", get(ready_health));
    let app = if state.environment == "development" {
        app.route("/internal/hooks/zlm/{server_id}", post(receive_zlm_hook))
            .route(
                "/internal/hooks/zlm/{server_id}/{hook_name}",
                post(receive_named_zlm_hook),
            )
    } else {
        app
    };
    app.nest("/api/v1", api_router)
        .merge(ui::router())
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request| {
                tracing::debug_span!(
                    "request",
                    method = %request.method(),
                    path = %request.uri().path(),
                )
            }),
        )
        .with_state(state)
}

async fn admit_agent_enrollment(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let service = match configured_agent_identity_service(&state) {
        Ok(service) => service,
        Err(error) => return error.into_response(),
    };
    let Some(peer_ip) = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|value| value.0.ip())
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "code": "AGENT_ENROLLMENT_PEER_UNAVAILABLE",
                "message": "agent enrollment peer identity is unavailable",
                "request_id": Uuid::now_v7().to_string(),
            })),
        )
            .into_response();
    };
    let permit = match service.try_admit_http(peer_ip) {
        Ok(permit) => permit,
        Err(AgentEnrollmentAdmissionError::Busy) => {
            return enrollment_admission_limited_response(std::time::Duration::from_secs(1));
        }
        Err(AgentEnrollmentAdmissionError::RateLimited { retry_after }) => {
            return enrollment_admission_limited_response(retry_after);
        }
    };
    let token = match extract_bearer_token(request.headers()) {
        Ok(token) => token,
        Err(_) => {
            return AppError::Unauthorized("invalid or expired agent enrollment".to_string())
                .into_response();
        }
    };
    let verified = match service.verify_enrollment_token(token, Utc::now()) {
        Ok(verified) => verified,
        Err(_) => {
            return AppError::Unauthorized("invalid or expired agent enrollment".to_string())
                .into_response();
        }
    };
    request.headers_mut().remove(header::AUTHORIZATION);
    request.extensions_mut().insert(verified);
    let response =
        tokio::time::timeout(std::time::Duration::from_secs(10), next.run(request)).await;
    drop(permit);
    match response {
        Ok(response) => response,
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "code": "AGENT_ENROLLMENT_TIMEOUT",
                "message": "agent enrollment did not complete within its deadline",
                "request_id": Uuid::now_v7().to_string(),
            })),
        )
            .into_response(),
    }
}

fn enrollment_admission_limited_response(retry_after: std::time::Duration) -> Response {
    let retry_after_seconds = retry_after.as_secs_f64().ceil().max(1.0) as u64;
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({
            "code": "AGENT_ENROLLMENT_RATE_LIMITED",
            "message": "agent enrollment admission is temporarily limited",
            "request_id": Uuid::now_v7().to_string(),
        })),
    )
        .into_response();
    response.headers_mut().insert(
        header::RETRY_AFTER,
        header::HeaderValue::from_str(&retry_after_seconds.to_string())
            .expect("Retry-After seconds are always a valid header value"),
    );
    response
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
        return authorize_api_request(state, headers, permission).await;
    }

    match maybe_extract_bearer_token(headers)? {
        Some(_) => authorize_api_request(state, headers, permission).await,
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

pub(crate) async fn authenticated_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<auth::AuthenticatedPrincipal, AppError> {
    let principal = state.auth.verify_session_claims(headers)?;
    if !state.auth.supports_local_login() {
        return Ok(principal);
    }
    if !principal.is_user() {
        return Err(AppError::Forbidden(
            "local access token does not identify a user".to_string(),
        ));
    }
    let credential_version = principal.credential_version().ok_or_else(|| {
        AppError::Forbidden("local access token is missing credential state".to_string())
    })?;
    let current = state
        .repository
        .local_access_token_is_current(
            principal.subject(),
            credential_version,
            principal.must_change_password(),
        )
        .await?;
    if !current {
        return Err(AppError::Forbidden(
            "local access token is no longer current".to_string(),
        ));
    }
    Ok(principal)
}

async fn authorize_api_request(
    state: &AppState,
    headers: &HeaderMap,
    permission: ApiPermission,
) -> Result<auth::AuthenticatedPrincipal, AppError> {
    let principal = authenticated_session(state, headers).await?;
    principal.require_permission(permission)?;
    Ok(principal)
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
        .map(|value| truncate_utf8_for_storage(value, 256))
}

fn truncate_utf8_for_storage(value: &str, max_bytes: usize) -> String {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
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
    let login_session_created = state
        .repository
        .insert_login_refresh_session(
            NewRefreshSession {
                id: Uuid::now_v7(),
                user_id: user.id,
                token_hash: hash_refresh_token(&refresh_token),
                expires_at: refresh_expires_at,
                created_at: now,
                client_ip: remote_ip,
                user_agent: user_agent.clone(),
            },
            &user.password_hash,
        )
        .await?;
    if !login_session_created {
        record_security_event(
            &state,
            "login_failed",
            &username,
            Some(&username),
            remote_ip,
            user_agent.as_deref(),
            json!({ "reason": "password_changed_during_login" }),
        )
        .await?;
        return Err(invalid_credentials_error());
    }
    let issued = state
        .auth
        .issue_access_token(
            &user.username,
            auth::ApiRole::Admin,
            user.credential_version,
            user.must_change_password,
        )
        .map_err(|error| AppError::Internal(format!("failed to issue access token: {error}")))?;
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
    let refresh_rotated = state
        .repository
        .rotate_refresh_session(
            session.id,
            session.user.id,
            &session.user.password_hash,
            &hash_refresh_token(&next_refresh_token),
            next_refresh_expires_at,
            now,
            remote_ip,
            user_agent.as_deref(),
        )
        .await?;
    if !refresh_rotated {
        return Err(AppError::Forbidden("invalid refresh token".to_string()));
    }
    let issued = state
        .auth
        .issue_access_token(
            &session.user.username,
            auth::ApiRole::Admin,
            session.user.credential_version,
            session.user.must_change_password,
        )
        .map_err(|error| AppError::Internal(format!("failed to issue access token: {error}")))?;
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
    let principal = authenticated_session(&state, &headers).await?;
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
    let password_changed = state
        .repository
        .change_user_password(
            &username,
            &user.password_hash,
            user.bootstrap_handoff_id,
            user.bootstrap_handoff_version,
            &next_password_hash,
            &username,
            remote_ip,
            user_agent.as_deref(),
        )
        .await?;
    if !password_changed {
        return Err(AppError::Forbidden(
            "password changed concurrently; retry with the current password".to_string(),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list_machine_allowlist(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MachineAllowlistResponse>, AppError> {
    let _principal = authorize_api_request(&state, &headers, ApiPermission::SecurityWrite).await?;
    let entries = state.repository.list_machine_allowlist().await?;
    Ok(Json(MachineAllowlistResponse { entries }))
}

async fn update_machine_allowlist(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<UpdateMachineAllowlistRequest>,
) -> Result<Json<MachineAllowlistResponse>, AppError> {
    let principal = authorize_api_request(&state, &headers, ApiPermission::SecurityWrite).await?;
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

async fn create_agent_enrollment(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Json(request): Json<CreateAgentEnrollmentRequest>,
) -> Result<
    (
        StatusCode,
        [(header::HeaderName, header::HeaderValue); 2],
        Json<CreateAgentEnrollmentResponse>,
    ),
    AppError,
> {
    let principal = authorize_api_request(&state, &headers, ApiPermission::SecurityWrite).await?;
    if request.node_id.is_nil() {
        return Err(AppError::BadRequest(
            "node_id must be a non-nil UUID".to_string(),
        ));
    }
    let service = configured_agent_identity_service(&state)?;
    let created = service
        .create_enrollment(
            request.node_id,
            principal.subject(),
            peer.map(|address| address.ip()),
            user_agent_from_headers(&headers),
            Utc::now(),
        )
        .await
        .map_err(map_agent_identity_service_error)?;
    Ok((
        StatusCode::CREATED,
        [
            (
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            ),
            (header::PRAGMA, header::HeaderValue::from_static("no-cache")),
        ],
        Json(CreateAgentEnrollmentResponse {
            enrollment_id: created.enrollment_id,
            node_id: created.node_id,
            token: created.token.to_string(),
            expires_at: created.expires_at,
        }),
    ))
}

async fn enroll_agent(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    Extension(verified): Extension<VerifiedAgentEnrollmentToken>,
    headers: HeaderMap,
    Json(request): Json<EnrollAgentRequest>,
) -> Result<Json<EnrollAgentResponse>, AppError> {
    if request.node_id.is_nil() {
        return Err(AppError::BadRequest(
            "node_id must be a non-nil UUID".to_string(),
        ));
    }
    let csr_pem = request.csr_pem.trim();
    if csr_pem.is_empty() || csr_pem.len() > 16 * 1024 {
        return Err(AppError::BadRequest(
            "csr_pem must contain a PEM certificate signing request within 16 KiB".to_string(),
        ));
    }
    let management_csr_pem = request.management_csr_pem.trim();
    if management_csr_pem.is_empty() || management_csr_pem.len() > 16 * 1024 {
        return Err(AppError::BadRequest(
            "management_csr_pem must contain a PEM certificate signing request within 16 KiB"
                .to_string(),
        ));
    }
    let service = configured_agent_identity_service(&state)?;
    let completed = service
        .enroll_verified(
            &verified,
            request.node_id,
            csr_pem,
            management_csr_pem,
            peer.map(|address| address.ip()),
            user_agent_from_headers(&headers),
            Utc::now(),
        )
        .await
        .map_err(map_agent_identity_service_error)?;
    Ok(Json(EnrollAgentResponse {
        node_id: completed.node_id,
        certificate_pem: completed.certificate_pem,
        ca_certificate_pem: completed.ca_certificate_pem,
        agent_client_issuer_ca_pem: completed.agent_client_issuer_ca_pem,
        control_plane_server_ca_pem: completed.control_plane_server_ca_pem,
        management_client_ca_pem: completed.management_client_ca_pem,
        fingerprint_sha256: completed.fingerprint_sha256,
        serial_number: completed.serial_number,
        not_before: completed.not_before,
        not_after: completed.not_after,
        management_certificate_pem: completed.management_certificate_pem,
        management_fingerprint_sha256: completed.management_fingerprint_sha256,
        management_serial_number: completed.management_serial_number,
        management_not_before: completed.management_not_before,
        management_not_after: completed.management_not_after,
        capability_jwt_public_key_pem: completed.capability_jwt_public_key_pem,
        capability_jwt_kid: completed.capability_jwt_kid,
    }))
}

fn configured_agent_identity_service(state: &AppState) -> Result<&AgentIdentityService, AppError> {
    state.agent_identity.as_ref().ok_or_else(|| {
        AppError::Conflict("Agent enrollment is not configured on this Core".to_string())
    })
}

fn map_agent_identity_service_error(error: AgentIdentityServiceError) -> AppError {
    match error {
        AgentIdentityServiceError::IdentityAlreadyActive => {
            AppError::Conflict("agent identity is already active".to_string())
        }
        AgentIdentityServiceError::IdentityRevoked => {
            AppError::Conflict("agent identity is revoked".to_string())
        }
        AgentIdentityServiceError::InvalidEnrollment => {
            AppError::Unauthorized("invalid or expired agent enrollment".to_string())
        }
        AgentIdentityServiceError::InvalidCsr => {
            AppError::BadRequest("agent CSR is invalid".to_string())
        }
        AgentIdentityServiceError::CertificateSigning => {
            AppError::Internal("agent certificate signing failed".to_string())
        }
        AgentIdentityServiceError::InvalidRotation => {
            AppError::Conflict("agent certificate rotation is not authorized".to_string())
        }
        AgentIdentityServiceError::RotationExpired => AppError::Conflict(
            "agent certificate rotation bundle expired before activation".to_string(),
        ),
        AgentIdentityServiceError::Repository(error) => AppError::Repository(error),
    }
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

#[derive(Debug, Clone, Serialize)]
struct RecordingControlResponse {
    task_id: Uuid,
    attempt_no: i32,
    desired_enabled: bool,
    recording_state: String,
    message: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RecordingStopRequest {
    #[serde(default)]
    reason: Option<String>,
}

async fn start_task_recording(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
    Json(payload): Json<RecordingControlSpec>,
) -> Result<(StatusCode, Json<RecordingControlResponse>), AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    validate_recording_control_spec(&payload)?;
    let command_id = Uuid::now_v7().to_string();
    let command = state
        .control_plane
        .request_recording_control(
            task_id,
            "start",
            Some(
                serde_json::to_value(&payload)
                    .map_err(|error| AppError::Internal(error.to_string()))?,
            ),
            "user_requested",
            command_id,
        )
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(RecordingControlResponse {
            task_id,
            attempt_no: command.attempt_no,
            desired_enabled: true,
            recording_state: "requested".to_string(),
            message: "recording control accepted".to_string(),
        }),
    ))
}

async fn stop_task_recording(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
    body: Option<Json<RecordingStopRequest>>,
) -> Result<(StatusCode, Json<RecordingControlResponse>), AppError> {
    let _principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    let reason = body
        .and_then(|Json(payload)| payload.reason)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "user_requested".to_string());
    let command_id = Uuid::now_v7().to_string();
    let command = state
        .control_plane
        .request_recording_control(task_id, "stop", None, reason, command_id)
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(RecordingControlResponse {
            task_id,
            attempt_no: command.attempt_no,
            desired_enabled: false,
            recording_state: "requested".to_string(),
            message: "recording control accepted".to_string(),
        }),
    ))
}

fn validate_recording_control_spec(spec: &RecordingControlSpec) -> Result<(), AppError> {
    if spec.duration_sec == Some(0) {
        return Err(AppError::BadRequest(
            "duration_sec must be greater than zero".to_string(),
        ));
    }
    if spec.segment_sec == Some(0) {
        return Err(AppError::BadRequest(
            "segment_sec must be greater than zero".to_string(),
        ));
    }
    Ok(())
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
    sort_streams_by_created_at_desc(&mut streams);
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
    let _principal = authorize_api_request(&state, &headers, ApiPermission::NodeRead).await?;
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
    let _principal = authorize_api_request(&state, &headers, ApiPermission::NodeRead).await?;
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
    let _principal = authorize_api_request(&state, &headers, ApiPermission::DebugRead).await?;
    Ok(Json(state.repository.list_hook_events(query).await?))
}

async fn debug_zlm_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ZlmMediaQuery>,
) -> Result<Json<Value>, AppError> {
    let _principal = authorize_api_request(&state, &headers, ApiPermission::DebugRead).await?;
    Ok(Json(
        call_zlm_json(
            &state,
            query.node_id,
            ZlmDebugCommand::ListMedia {
                schema: normalized_zlm_filter(query.schema),
                vhost: normalized_zlm_filter(query.vhost),
                app: normalized_zlm_filter(query.app),
                stream: normalized_zlm_filter(query.stream),
            },
        )
        .await?,
    ))
}

async fn debug_zlm_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NodeScopedQuery>,
) -> Result<Json<Value>, AppError> {
    let _principal = authorize_api_request(&state, &headers, ApiPermission::DebugRead).await?;
    Ok(Json(
        call_zlm_json(&state, query.node_id, ZlmDebugCommand::ListSessions).await?,
    ))
}

async fn debug_zlm_players(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NodeScopedQuery>,
) -> Result<Json<Value>, AppError> {
    let _principal = authorize_api_request(&state, &headers, ApiPermission::DebugRead).await?;
    Ok(Json(
        call_zlm_json(&state, query.node_id, ZlmDebugCommand::ListPlayers).await?,
    ))
}

async fn debug_zlm_statistic(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NodeScopedQuery>,
) -> Result<Json<Value>, AppError> {
    let _principal = authorize_api_request(&state, &headers, ApiPermission::DebugRead).await?;
    Ok(Json(
        call_zlm_json(&state, query.node_id, ZlmDebugCommand::GetStatistic).await?,
    ))
}

async fn debug_zlm_threads_load(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NodeScopedQuery>,
) -> Result<Json<Value>, AppError> {
    let _principal = authorize_api_request(&state, &headers, ApiPermission::DebugRead).await?;
    Ok(Json(
        call_zlm_json(&state, query.node_id, ZlmDebugCommand::GetThreadsLoad).await?,
    ))
}

async fn debug_zlm_work_threads_load(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NodeScopedQuery>,
) -> Result<Json<Value>, AppError> {
    let _principal = authorize_api_request(&state, &headers, ApiPermission::DebugRead).await?;
    Ok(Json(
        call_zlm_json(&state, query.node_id, ZlmDebugCommand::GetWorkThreadsLoad).await?,
    ))
}

async fn debug_zlm_kick_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DebugKickSessionRequest>,
) -> Result<Json<Value>, AppError> {
    let _principal = authorize_api_request(&state, &headers, ApiPermission::DebugRead).await?;
    Ok(Json(
        call_zlm_json(
            &state,
            request.node_id,
            ZlmDebugCommand::KickSession {
                session_id: request.session_id,
            },
        )
        .await?,
    ))
}

async fn debug_zlm_kick_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DebugKickSessionsRequest>,
) -> Result<Json<Value>, AppError> {
    let _principal = authorize_api_request(&state, &headers, ApiPermission::DebugRead).await?;
    Ok(Json(
        call_zlm_json(
            &state,
            request.node_id,
            ZlmDebugCommand::KickSessions {
                local_port: request.local_port,
                peer_ip: normalized_zlm_filter(request.peer_ip),
            },
        )
        .await?,
    ))
}

async fn debug_zlm_close_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DebugCloseStreamRequest>,
) -> Result<Json<Value>, AppError> {
    let _principal = authorize_api_request(&state, &headers, ApiPermission::DebugRead).await?;
    Ok(Json(
        call_zlm_json(
            &state,
            request.node_id,
            ZlmDebugCommand::CloseStream {
                schema: request.schema,
                vhost: request.vhost,
                app: request.app,
                stream: request.stream,
                force: request.force,
            },
        )
        .await?,
    ))
}

async fn debug_zlm_snap(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DebugSnapQuery>,
) -> Result<Json<DebugSnapResponse>, AppError> {
    let _principal = authorize_api_request(&state, &headers, ApiPermission::DebugRead).await?;
    let (content_type, body) = match state
        .control_plane
        .zlm_debug(
            query.node_id,
            ZlmDebugCommand::Snapshot {
                source_url: query.url,
                timeout_sec: query.timeout_sec,
                expire_sec: query.expire_sec,
            },
        )
        .await
        .map_err(map_zlm_debug_error)?
    {
        ZlmDebugResult::Snapshot { content_type, data } => (content_type, data),
        ZlmDebugResult::Json(_) => {
            return Err(AppError::Internal(
                "Agent returned JSON for a ZLM snapshot request".to_string(),
            ));
        }
    };
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
        node.running_tasks = Some(load.running_tasks);
        node.starting_tasks = Some(load.starting_tasks);
        node.stopping_tasks = Some(load.stopping_tasks);
        node.orphaned_tasks = Some(load.orphaned_tasks);
        node.runtime_slot_loads = Some(load.runtime_slot_loads);
        node.connected = Some(load.connected);
        node.control_connected = load.connected;
        node.cpu_percent = Some(load.cpu_percent);
        node.mem_percent = Some(load.mem_percent);
        node.disk_percent = Some(load.disk_percent);
        node.upload_disk_total_bytes = Some(load.upload_disk_total_bytes);
        node.upload_disk_available_bytes = Some(load.upload_disk_available_bytes);
        node.upload_disk_used_percent = Some(load.upload_disk_used_percent);
        node.zlm_alive = Some(load.zlm_alive);
        node.ffmpeg_alive = Some(load.ffmpeg_alive);
        node.gpu_runtime = Some(load.gpu_runtime);
        node.healthy = node.control_connected && !load.artifact_cleanup_blocked;
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
            stream.play_urls =
                build_fallback_play_urls(node, &stream.schema, &stream.app, &stream.stream);
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
                        stream.play_urls =
                            build_play_urls(node, &runtime.schemas, &stream.app, &stream.stream);
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

fn sort_streams_by_created_at_desc(streams: &mut [repository::StreamSummary]) {
    streams.sort_by(|left, right| {
        right
            .sort_created_at
            .cmp(&left.sort_created_at)
            .then_with(|| right.id.cmp(&left.id))
    });
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
    existing.sort_created_at = existing.sort_created_at.max(incoming.sort_created_at);
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
    let body = call_zlm_json(
        state,
        node_id,
        ZlmDebugCommand::ListMedia {
            schema: None,
            vhost: None,
            app: None,
            stream: None,
        },
    )
    .await?;
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
    node: &repository::NodeSummary,
    schema: &str,
    app: &str,
    stream: &str,
) -> Vec<String> {
    let mut schemas = BTreeSet::new();
    schemas.insert(schema.trim().to_string());
    build_play_urls(node, &schemas, app, stream)
}

pub(crate) fn build_play_urls(
    node: &repository::NodeSummary,
    schemas: &BTreeSet<String>,
    app: &str,
    stream: &str,
) -> Vec<String> {
    let Ok(base) = Url::parse(node.agent_stream_addr.as_str()) else {
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
            "rtsp" => urls.push(format!(
                "rtsp://{}:{}/{app}/{stream}",
                host, node.zlm_rtsp_port
            )),
            "rtmp" => {
                urls.push(format!(
                    "rtmp://{}:{}/{app}/{stream}",
                    host, node.zlm_rtmp_port
                ));
                urls.push(format!("{http_base}/{app}/{stream}.live.flv"));
            }
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

fn normalized_zlm_filter(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn call_zlm_json(
    state: &AppState,
    node_id: Uuid,
    command: ZlmDebugCommand,
) -> Result<Value, AppError> {
    match state
        .control_plane
        .zlm_debug(node_id, command)
        .await
        .map_err(map_zlm_debug_error)?
    {
        ZlmDebugResult::Json(body) => ensure_zlm_debug_success("Agent ZLM debug", body),
        ZlmDebugResult::Snapshot { .. } => Err(AppError::Internal(
            "Agent returned a snapshot for a JSON ZLM debug request".to_string(),
        )),
    }
}

fn map_zlm_debug_error(error: ZlmDebugCallError) -> AppError {
    match error {
        ZlmDebugCallError::InvalidRequest => {
            AppError::BadRequest("invalid ZLM debug request".to_string())
        }
        ZlmDebugCallError::Disconnected => {
            AppError::Conflict("the selected Agent is not connected".to_string())
        }
        ZlmDebugCallError::Busy => {
            AppError::Conflict("too many ZLM debug requests are pending".to_string())
        }
        ZlmDebugCallError::DeadlineExceeded => {
            AppError::Internal("ZLM debug request timed out".to_string())
        }
        ZlmDebugCallError::ProtocolViolation => {
            AppError::Internal("Agent returned an invalid ZLM debug response".to_string())
        }
        ZlmDebugCallError::ResponseTooLarge => {
            AppError::Internal("Agent ZLM debug response exceeded the size limit".to_string())
        }
        ZlmDebugCallError::Remote { code, message } => {
            AppError::Internal(format!("Agent ZLM debug failed: {code}: {message}"))
        }
    }
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

    // Hook 入口先做来源和密钥校验，再解析 server_id 到节点；只有通过这层边界后，
    // 后续分支才允许把 ZLM 事件写入任务状态或产物表。
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

    process_authenticated_zlm_hook(
        &ZlmHookBusinessContext::from_app_state(&state),
        node_id,
        server_id,
        hook_name,
        payload,
    )
    .await
}

async fn process_authenticated_zlm_hook(
    state: &ZlmHookBusinessContext,
    node_id: Uuid,
    server_id: String,
    hook_name: String,
    payload: serde_json::Value,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let hook_name = hook_name.trim().to_string();
    if hook_name.is_empty() {
        return Err(AppError::BadRequest(
            "hook_name must not be empty".to_string(),
        ));
    }

    let sanitized_payload = sanitize_hook_payload(&payload);
    let dedup_key = hash_hook_payload(server_id.trim(), &hook_name, &sanitized_payload);

    // 不同 ZLM hook 的业务语义差异很大：on_publish 会影响是否允许推流，
    // record hook 写产物，stream hook 只记事件，keepalive 刷新 media server 可见性。
    let response = match hook_name.as_str() {
        "on_publish" => {
            // on_publish 是唯一需要返回播放/推流策略的 hook；匹配到任务时还会把
            // gb_rtp 接入提升为 running，因为 RTP server 本身不等同于媒体真正到达。
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
            // RTP server 超时来自 ZLM 的被动通知；能匹配任务时转成任务事件，
            // 匹配不到则只保留原始 hook，避免误改无归属任务。
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
                            resolved_spec: target.resolved_spec.clone(),
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
            // 录制文件路径必须经过 allowlist 校验，防止 ZLM 回调把任意宿主机路径
            // 写入可下载产物或清理索引。
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
            // HLS/TS hook 有时只携带播放 URL，不携带完整文件路径；解析不到路径时
            // 只能记录原始 hook，不能虚构产物行。
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
            // keepalive 同时记录原始 hook 和节点可见时间，调度层用后者判断
            // media server 是否仍然可作为任务目标。
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
        "add_mute_audio": false,
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
        return Ok(Some(CliCommand::Serve {
            insecure_dev: false,
        }));
    };
    if is_help_flag(&command) {
        return Ok(Some(CliCommand::Help { auth_only: false }));
    }
    if command == "--insecure-dev" {
        anyhow::ensure!(
            args.next().is_none(),
            "--insecure-dev does not accept a subcommand; use it alone to start the server"
        );
        return Ok(Some(CliCommand::Serve { insecure_dev: true }));
    }
    if command == "agent" {
        let Some(subcommand) = args.next() else {
            anyhow::bail!("missing agent subcommand");
        };
        if is_help_flag(&subcommand) {
            anyhow::ensure!(
                args.next().is_none(),
                "agent help does not accept additional arguments"
            );
            return Ok(Some(CliCommand::AgentHelp));
        }
        let remaining_args: Vec<String> = args.collect();
        if remaining_args.len() == 1 && is_help_flag(&remaining_args[0]) {
            return Ok(Some(CliCommand::AgentHelp));
        }
        anyhow::ensure!(
            subcommand == "create-enrollment",
            "unsupported agent subcommand `{subcommand}`"
        );
        let mut args = remaining_args.into_iter();
        let mut node_id = None;
        let mut token_stdout = false;
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--node-id" => {
                    anyhow::ensure!(node_id.is_none(), "--node-id may be specified only once");
                    node_id = Some(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("missing value for --node-id"))?,
                    );
                }
                "--token-stdout" => {
                    anyhow::ensure!(!token_stdout, "--token-stdout may be specified only once");
                    token_stdout = true;
                }
                other => anyhow::bail!("unsupported agent argument `{other}`"),
            }
        }
        anyhow::ensure!(
            token_stdout,
            "--token-stdout is required because the one-time token is sensitive"
        );
        let node_id = node_id.ok_or_else(|| anyhow::anyhow!("--node-id is required"))?;
        let parsed = Uuid::parse_str(&node_id)
            .map_err(|_| anyhow::anyhow!("--node-id must be a canonical non-nil UUID"))?;
        anyhow::ensure!(
            !parsed.is_nil() && parsed.to_string() == node_id,
            "--node-id must be a canonical non-nil UUID"
        );
        return Ok(Some(CliCommand::AgentCreateEnrollment { node_id: parsed }));
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
    if subcommand == "check-admin" {
        anyhow::ensure!(
            remaining_args.is_empty(),
            "auth check-admin does not accept arguments"
        );
        return Ok(Some(CliCommand::CheckAdmin));
    }
    if subcommand == "check-config" {
        anyhow::ensure!(
            remaining_args.is_empty(),
            "auth check-config does not accept arguments"
        );
        return Ok(Some(CliCommand::CheckAuthConfig));
    }
    if subcommand == "bootstrap-status" {
        anyhow::ensure!(
            remaining_args.len() == 4
                && remaining_args[0] == "--username"
                && remaining_args[2] == "--handoff-id",
            "auth bootstrap-status requires --username <name> --handoff-id <uuid> and never accepts password input"
        );
        return Ok(Some(CliCommand::BootstrapStatus {
            username: remaining_args[1].clone(),
            handoff_id: remaining_args[3].clone(),
        }));
    }

    let mut args = remaining_args.into_iter();
    let mut username = None;
    let mut handoff_id = None;
    let mut expected_version = None;
    let mut expects_password_stdin = false;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--username" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing value for --username"))?;
                username = Some(value);
            }
            "--handoff-id" => {
                handoff_id = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("missing value for --handoff-id"))?,
                );
            }
            "--expected-version" => {
                expected_version = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("missing value for --expected-version"))?,
                );
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
        "bootstrap-admin" => {
            anyhow::ensure!(
                handoff_id.is_none() && expected_version.is_none(),
                "bootstrap-admin does not accept handoff arguments"
            );
            CliCommand::BootstrapAdmin { username }
        }
        "recover-bootstrap-admin" => CliCommand::RecoverBootstrapAdmin {
            username,
            handoff_id: handoff_id.ok_or_else(|| anyhow::anyhow!("--handoff-id is required"))?,
            expected_version: expected_version
                .ok_or_else(|| anyhow::anyhow!("--expected-version is required"))?,
        },
        "reset-password" => {
            anyhow::ensure!(
                handoff_id.is_none() && expected_version.is_none(),
                "reset-password does not accept handoff arguments"
            );
            CliCommand::ResetPassword { username }
        }
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

fn print_agent_cli_help() {
    println!("{AGENT_CLI_HELP_TEXT}");
}

const CLI_HELP_TEXT: &str = "\
Usage:
  media-core
  media-core --insecure-dev
  media-core auth bootstrap-admin --username <name> --password-stdin
  media-core auth recover-bootstrap-admin --username <name> --handoff-id <uuid> --expected-version <n> --password-stdin
  media-core auth reset-password --username <name> --password-stdin
  media-core auth check-config
  media-core auth check-admin
  media-core auth bootstrap-status --username <name> --handoff-id <uuid>
  media-core agent create-enrollment --node-id <canonical-uuid> --token-stdout
  media-core [help|-h|--help]
  media-core auth [help|-h|--help]
  media-core agent [help|-h|--help]

Description:
  Run without a subcommand to start the media-core server.
  --insecure-dev permits plaintext development listeners only when both bind to loopback.

Auth commands:
  bootstrap-admin  Create the initial enabled admin user; reads the password from stdin.
  recover-bootstrap-admin  Atomically create or recover only a pending bootstrap administrator.
  reset-password   Reset an existing user's password; reads the new password from stdin.
  check-config     Read-only check for listener policy, JWT keys, and the Agent signing CA.
  check-admin      Read-only check that succeeds only when an enabled admin exists.

Agent commands:
  create-enrollment  Create a 10-minute one-time enrollment token for an exact node identity.";

const AUTH_CLI_HELP_TEXT: &str = "\
Usage:
  media-core auth bootstrap-admin --username <name> --password-stdin
  media-core auth recover-bootstrap-admin --username <name> --handoff-id <uuid> --expected-version <n> --password-stdin
  media-core auth reset-password --username <name> --password-stdin
  media-core auth check-config
  media-core auth check-admin
  media-core auth bootstrap-status --username <name> --handoff-id <uuid>
  media-core auth [help|-h|--help]

Description:
  bootstrap-admin and reset-password read the password value from stdin.
  check-config, check-admin, and bootstrap-status never read a password or run migrations.";

const AGENT_CLI_HELP_TEXT: &str = "\
Usage:
  media-core agent create-enrollment --node-id <canonical-uuid> --token-stdout
  media-core agent [help|-h|--help]

Description:
  Loads the normal Core security configuration and Agent PKI, runs pending database
  migrations, and creates a node-bound 10-minute one-time enrollment token.

Sensitive output:
  --token-stdout is mandatory. The token is written as the only stdout line.
  stdout is sensitive: capture it only into protected ephemeral storage and never log it.";

fn read_password_from_stdin() -> anyhow::Result<String> {
    let mut password = String::new();
    io::stdin().read_to_string(&mut password)?;
    let password = password.trim_end_matches(['\r', '\n']).to_string();
    anyhow::ensure!(!password.is_empty(), "password must not be empty");
    Ok(password)
}

fn write_agent_enrollment_token(
    writer: &mut impl Write,
    enrollment: &CreatedAgentEnrollment,
) -> io::Result<()> {
    writer.write_all(enrollment.token.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

async fn create_agent_enrollment_for_cli(
    settings: &config::CoreSettings,
    node_id: Uuid,
    now: DateTime<Utc>,
) -> anyhow::Result<CreatedAgentEnrollment> {
    let (authority, public_config, _) = load_agent_certificate_authority(settings, now)?
        .ok_or_else(|| anyhow::anyhow!("Agent enrollment PKI is not configured"))?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&settings.database_url)
        .await?;
    sqlx::migrate!("../../migrations").run(&pool).await?;
    let repository = Arc::new(TaskRepository::new(pool));
    AgentIdentityService::new(repository, authority, public_config)
        .create_enrollment(
            node_id,
            "local-agent-enrollment-cli",
            None,
            Some("media-core agent create-enrollment".to_string()),
            now,
        )
        .await
        .map_err(Into::into)
}

async fn run_agent_create_enrollment(node_id: Uuid) -> anyhow::Result<()> {
    // Use the normal server loader so production listener/auth/PKI policy is
    // validated. Deliberately do not initialize telemetry: the installer must
    // be able to treat stdout as exactly one sensitive token line.
    let settings = config::Settings::load_with_insecure_dev(false)?;
    let enrollment = create_agent_enrollment_for_cli(&settings.core, node_id, Utc::now()).await?;
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_agent_enrollment_token(&mut stdout, &enrollment)?;
    Ok(())
}

async fn run_cli_command(command: CliCommand) -> anyhow::Result<()> {
    let settings = config::Settings::load_for_auth_cli()?;
    telemetry::init(&settings.logging);

    if command == CliCommand::CheckAuthConfig {
        config::validate_security_policy(&settings.environment, &settings.core, false)?;
        AuthConfig::from_settings(&settings.core)?;
        let _authority = load_agent_certificate_authority(&settings.core, Utc::now())?;
        println!("authentication and Agent CA configuration is valid");
        return Ok(());
    }

    let pool_options = PgPoolOptions::new().max_connections(2);
    let read_only_database_probe = matches!(
        command,
        CliCommand::CheckAdmin | CliCommand::BootstrapStatus { .. }
    );
    let pool = if read_only_database_probe {
        pool_options
            .connect(&settings.core.database_url)
            .await
            .map_err(|_| anyhow::anyhow!("auth check could not connect to the database"))?
    } else {
        pool_options.connect(&settings.core.database_url).await?
    };
    if command == CliCommand::CheckAdmin {
        let repository = TaskRepository::new(pool);
        anyhow::ensure!(
            repository.has_enabled_admin_user().await?,
            "no enabled admin user exists"
        );
        println!("enabled admin user exists");
        return Ok(());
    }
    if let CliCommand::BootstrapStatus {
        username,
        handoff_id,
    } = &command
    {
        let username = normalize_username_value(username)?;
        let handoff_id = Uuid::parse_str(handoff_id)
            .map_err(|_| anyhow::anyhow!("--handoff-id must be a UUID"))?;
        let repository = TaskRepository::new(pool);
        println!(
            "{}",
            repository
                .bootstrap_admin_password_state(&username, handoff_id)
                .await?
                .as_cli_value()
        );
        return Ok(());
    }
    sqlx::migrate!("../../migrations").run(&pool).await?;
    let repository = TaskRepository::new(pool);
    let password = read_password_from_stdin()?;
    let password_hash = hash_password(&password)?;

    match command {
        CliCommand::Serve { .. } => unreachable!("server commands are handled before DB setup"),
        CliCommand::Help { .. } => unreachable!("help commands are handled before DB setup"),
        CliCommand::AgentHelp | CliCommand::AgentCreateEnrollment { .. } => {
            unreachable!("Agent CLI commands are handled before auth DB setup")
        }
        CliCommand::CheckAuthConfig => {
            unreachable!("auth configuration check is handled before DB setup")
        }
        CliCommand::CheckAdmin => unreachable!("admin check is handled before migrations"),
        CliCommand::BootstrapStatus { .. } => {
            unreachable!("bootstrap status is handled before migrations")
        }
        CliCommand::RecoverBootstrapAdmin {
            username,
            handoff_id,
            expected_version,
        } => {
            let username = normalize_username_value(&username)?;
            let handoff_id = Uuid::parse_str(&handoff_id)
                .map_err(|_| anyhow::anyhow!("--handoff-id must be a UUID"))?;
            let expected_version = expected_version.parse::<i64>().map_err(|_| {
                anyhow::anyhow!("--expected-version must be a non-negative integer")
            })?;
            anyhow::ensure!(
                expected_version >= 0,
                "--expected-version must be a non-negative integer"
            );
            let outcome = repository
                .reconcile_bootstrap_admin_password(
                    &username,
                    handoff_id,
                    expected_version,
                    &password_hash,
                )
                .await?;
            anyhow::ensure!(
                matches!(
                    outcome,
                    repository::BootstrapAdminReconcileOutcome::Created
                        | repository::BootstrapAdminReconcileOutcome::Recovered
                ),
                "bootstrap administrator is already complete or conflicts with the pending handoff"
            );
            println!("reconciled pending bootstrap administrator `{username}`");
        }
        CliCommand::BootstrapAdmin { username } => {
            let username = normalize_username_value(&username)?;
            anyhow::ensure!(
                !repository.has_enabled_admin_user().await?,
                "an enabled admin user already exists"
            );
            repository
                .create_bootstrap_admin(
                    &username,
                    &password_hash,
                    BOOTSTRAP_ADMIN_MUST_CHANGE_PASSWORD,
                )
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

fn load_agent_management_client(
    settings: &config::CoreSettings,
    capability_kid: &str,
) -> anyhow::Result<Arc<AgentManagementClient>> {
    let client_certificate_path = settings.agent_management_client_cert_path.trim();
    let client_private_key_path = settings.agent_management_client_key_path.trim();
    let agent_issuer_ca_path = settings.agent_ca_cert_path.trim();
    let capability_private_key_path = settings.agent_capability_jwt_private_key_path.trim();
    anyhow::ensure!(
        !client_certificate_path.is_empty()
            && !client_private_key_path.is_empty()
            && !agent_issuer_ca_path.is_empty()
            && !capability_private_key_path.is_empty(),
        "authenticated Agent management requires its mTLS and capability key material"
    );

    let client_certificate_pem =
        fs::read_to_string(client_certificate_path).with_context(|| {
            format!("failed to read Agent management client certificate {client_certificate_path}")
        })?;
    let client_private_key_pem =
        zeroize::Zeroizing::new(fs::read_to_string(client_private_key_path).with_context(
            || format!("failed to read Agent management client key {client_private_key_path}"),
        )?);
    let agent_issuer_ca_pem = fs::read_to_string(agent_issuer_ca_path)
        .with_context(|| format!("failed to read Agent signing CA {agent_issuer_ca_path}"))?;
    let capability_private_key_pem = zeroize::Zeroizing::new(
        fs::read_to_string(capability_private_key_path).with_context(|| {
            format!("failed to read Agent capability key {capability_private_key_path}")
        })?,
    );
    let signer = AgentCapabilitySigner::new(
        capability_private_key_pem.as_str(),
        capability_kid,
        settings.agent_capability_ttl_sec,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "failed to configure Agent capability signer ({})",
            error.safe_code()
        )
    })?;
    let tls = AgentManagementTlsMaterial::from_pem(
        &client_certificate_pem,
        client_private_key_pem.as_str(),
        &agent_issuer_ca_pem,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "failed to configure Agent management mTLS ({})",
            error.safe_code()
        )
    })?;
    let client = AgentManagementClient::new(signer, tls, Arc::new(TracingAgentManagementAuditSink))
        .map_err(|error| {
            anyhow::anyhow!(
                "failed to configure Agent management client ({})",
                error.safe_code()
            )
        })?;
    Ok(Arc::new(client))
}

fn load_agent_certificate_authority(
    settings: &config::CoreSettings,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<(AgentCertificateAuthority, AgentEnrollmentPublicConfig, Uuid)>> {
    let certificate_path = settings.agent_ca_cert_path.trim();
    let private_key_path = settings.agent_ca_key_path.trim();
    anyhow::ensure!(
        certificate_path.is_empty() == private_key_path.is_empty(),
        "CORE_AGENT_CA_CERT_PATH and CORE_AGENT_CA_KEY_PATH must be set together"
    );
    if certificate_path.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        !settings.grpc_tls_client_ca_path.trim().is_empty(),
        "Agent enrollment requires CORE_GRPC_TLS_CLIENT_CA_PATH"
    );
    let authority = AgentCertificateAuthority::from_paths(
        FsPath::new(certificate_path),
        FsPath::new(private_key_path),
        now,
    )?;
    authority
        .ensure_present_in_client_ca_bundle(FsPath::new(settings.grpc_tls_client_ca_path.trim()))?;
    let control_plane_server_ca_pem = load_and_validate_control_plane_server_ca(settings, now)?;
    let (core_instance_id, management_client_ca_pem) =
        load_and_validate_management_client_identity(
            settings,
            now,
            authority.certificate_pem(),
            &control_plane_server_ca_pem,
        )?;
    let (capability_jwt_public_key_pem, capability_jwt_kid) =
        load_and_validate_agent_capability_key_pair(settings)?;
    validate_dedicated_agent_security_keys(settings, &capability_jwt_public_key_pem)?;
    Ok(Some((
        authority,
        AgentEnrollmentPublicConfig {
            control_plane_server_ca_pem,
            management_client_ca_pem,
            capability_jwt_public_key_pem,
            capability_jwt_kid,
        },
        core_instance_id,
    )))
}

fn load_and_validate_control_plane_server_ca(
    settings: &config::CoreSettings,
    now: DateTime<Utc>,
) -> anyhow::Result<String> {
    let ca_path = settings.grpc_tls_server_ca_path.trim();
    anyhow::ensure!(
        !ca_path.is_empty(),
        "Agent enrollment requires CORE_GRPC_TLS_SERVER_CA_PATH"
    );
    let ca_pem = fs::read_to_string(ca_path)
        .with_context(|| format!("failed to read gRPC server CA {ca_path}"))?;
    let ca_der = decode_exact_certificate_pem(&ca_pem, "gRPC server CA")?;
    let (_, ca) = x509_parser::certificate::X509Certificate::from_der(&ca_der)
        .map_err(|_| anyhow::anyhow!("gRPC server CA is not valid X.509"))?;
    let validation_time = x509_parser::time::ASN1Time::from_timestamp(now.timestamp())
        .map_err(|_| anyhow::anyhow!("gRPC server CA validation time is invalid"))?;
    anyhow::ensure!(
        ca.validity().is_valid_at(validation_time),
        "gRPC server CA is not currently valid"
    );
    let basic_constraints = ca
        .basic_constraints()
        .map_err(|_| anyhow::anyhow!("gRPC server CA BasicConstraints is malformed"))?
        .ok_or_else(|| anyhow::anyhow!("gRPC server CA requires BasicConstraints CA:TRUE"))?;
    anyhow::ensure!(
        basic_constraints.value.ca,
        "gRPC server CA requires BasicConstraints CA:TRUE"
    );
    let key_usage = ca
        .key_usage()
        .map_err(|_| anyhow::anyhow!("gRPC server CA key usage is malformed"))?
        .ok_or_else(|| anyhow::anyhow!("gRPC server CA requires keyCertSign key usage"))?;
    anyhow::ensure!(
        key_usage.value.key_cert_sign(),
        "gRPC server CA requires keyCertSign key usage"
    );
    anyhow::ensure!(
        ca.subject() == ca.issuer(),
        "gRPC server CA must be a single self-signed root certificate"
    );
    ca.verify_signature(None)
        .map_err(|_| anyhow::anyhow!("gRPC server CA self-signature is invalid"))?;

    let server_certificate_pem = fs::read_to_string(settings.grpc_tls_cert_path.trim())
        .with_context(|| {
            format!(
                "failed to read gRPC server certificate {}",
                settings.grpc_tls_cert_path
            )
        })?;
    let server_der =
        decode_exact_certificate_pem(&server_certificate_pem, "gRPC server certificate")?;
    let (_, server) = x509_parser::certificate::X509Certificate::from_der(&server_der)
        .map_err(|_| anyhow::anyhow!("gRPC server certificate is not valid X.509"))?;
    anyhow::ensure!(
        server.validity().is_valid_at(validation_time),
        "gRPC server certificate is not currently valid"
    );
    anyhow::ensure!(
        server.validity().not_before >= ca.validity().not_before
            && server.validity().not_after <= ca.validity().not_after,
        "gRPC server certificate validity must be contained by its CA"
    );
    anyhow::ensure!(
        server.issuer() == ca.subject(),
        "gRPC server certificate is not directly issued by CORE_GRPC_TLS_SERVER_CA_PATH"
    );
    server
        .verify_signature(Some(ca.public_key()))
        .map_err(|_| anyhow::anyhow!("gRPC server certificate signature does not match its CA"))?;
    let basic_constraints = server
        .basic_constraints()
        .map_err(|_| anyhow::anyhow!("gRPC server certificate BasicConstraints is malformed"))?;
    anyhow::ensure!(
        basic_constraints.is_none_or(|extension| !extension.value.ca),
        "gRPC server certificate must not be a CA"
    );
    let extended = server
        .extended_key_usage()
        .map_err(|_| anyhow::anyhow!("gRPC server certificate EKU is malformed"))?
        .ok_or_else(|| anyhow::anyhow!("gRPC server certificate requires serverAuth EKU"))?;
    anyhow::ensure!(
        extended.value.server_auth && !extended.value.client_auth && !extended.value.any,
        "gRPC server certificate requires serverAuth-only EKU"
    );
    validate_private_key_matches_certificate(
        settings.grpc_tls_key_path.trim(),
        server.public_key().raw,
        "gRPC server",
    )?;
    Ok(ca_pem)
}

fn load_and_validate_management_client_identity(
    settings: &config::CoreSettings,
    now: DateTime<Utc>,
    agent_issuer_ca_pem: &str,
    control_plane_server_ca_pem: &str,
) -> anyhow::Result<(Uuid, String)> {
    let instance_text = settings.core_instance_id.trim();
    let core_instance_id = Uuid::parse_str(instance_text)
        .map_err(|_| anyhow::anyhow!("CORE_INSTANCE_ID must be a non-nil canonical UUID"))?;
    anyhow::ensure!(
        !core_instance_id.is_nil() && core_instance_id.to_string() == instance_text,
        "CORE_INSTANCE_ID must be a non-nil canonical UUID"
    );

    let ca_path = settings.agent_management_ca_path.trim();
    let ca_pem = fs::read_to_string(ca_path)
        .with_context(|| format!("failed to read Agent management client CA {ca_path}"))?;
    let ca_der = decode_exact_certificate_pem(&ca_pem, "Agent management client CA")?;
    let agent_issuer_der = decode_exact_certificate_pem(agent_issuer_ca_pem, "Agent signing CA")?;
    let server_ca_der =
        decode_exact_certificate_pem(control_plane_server_ca_pem, "gRPC server CA")?;
    anyhow::ensure!(
        ca_der != agent_issuer_der && ca_der != server_ca_der && agent_issuer_der != server_ca_der,
        "Agent signing, gRPC server, and management client trust roots must be distinct"
    );
    let (_, ca) = x509_parser::certificate::X509Certificate::from_der(&ca_der)
        .map_err(|_| anyhow::anyhow!("Agent management client CA is not valid X.509"))?;
    let (_, agent_issuer_ca) =
        x509_parser::certificate::X509Certificate::from_der(&agent_issuer_der)
            .map_err(|_| anyhow::anyhow!("Agent signing CA is not valid X.509"))?;
    let (_, control_plane_server_ca) =
        x509_parser::certificate::X509Certificate::from_der(&server_ca_der)
            .map_err(|_| anyhow::anyhow!("gRPC server CA is not valid X.509"))?;
    anyhow::ensure!(
        ca.public_key().raw != agent_issuer_ca.public_key().raw
            && ca.public_key().raw != control_plane_server_ca.public_key().raw
            && agent_issuer_ca.public_key().raw != control_plane_server_ca.public_key().raw,
        "Agent signing, gRPC server, and management client trust roots must use distinct keys"
    );
    let validation_time = x509_parser::time::ASN1Time::from_timestamp(now.timestamp())
        .map_err(|_| anyhow::anyhow!("Agent management client CA validation time is invalid"))?;
    anyhow::ensure!(
        ca.validity().is_valid_at(validation_time),
        "Agent management client CA is not currently valid"
    );
    let basic_constraints = ca
        .basic_constraints()
        .map_err(|_| anyhow::anyhow!("Agent management client CA BasicConstraints is malformed"))?
        .ok_or_else(|| {
            anyhow::anyhow!("Agent management client CA requires BasicConstraints CA:TRUE")
        })?;
    anyhow::ensure!(
        basic_constraints.value.ca,
        "Agent management client CA requires BasicConstraints CA:TRUE"
    );
    let key_usage = ca
        .key_usage()
        .map_err(|_| anyhow::anyhow!("Agent management client CA key usage is malformed"))?
        .ok_or_else(|| {
            anyhow::anyhow!("Agent management client CA requires keyCertSign key usage")
        })?;
    anyhow::ensure!(
        key_usage.value.key_cert_sign(),
        "Agent management client CA requires keyCertSign key usage"
    );
    anyhow::ensure!(
        ca.subject() == ca.issuer(),
        "Agent management client CA must be a single self-signed root certificate"
    );
    ca.verify_signature(None)
        .map_err(|_| anyhow::anyhow!("Agent management client CA self-signature is invalid"))?;

    let certificate_path = settings.agent_management_client_cert_path.trim();
    let certificate_pem = fs::read_to_string(certificate_path).with_context(|| {
        format!("failed to read Agent management client certificate {certificate_path}")
    })?;
    let certificate_der =
        decode_exact_certificate_pem(&certificate_pem, "Agent management client certificate")?;
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(&certificate_der)
        .map_err(|_| anyhow::anyhow!("Agent management client certificate is not valid X.509"))?;
    anyhow::ensure!(
        certificate.validity().is_valid_at(validation_time),
        "Agent management client certificate is not currently valid"
    );
    anyhow::ensure!(
        certificate.validity().not_before >= ca.validity().not_before
            && certificate.validity().not_after <= ca.validity().not_after,
        "Agent management client certificate validity must be contained by its CA"
    );
    anyhow::ensure!(
        certificate.issuer() == ca.subject(),
        "Agent management client certificate is not directly issued by CORE_AGENT_MANAGEMENT_CA_PATH"
    );
    certificate
        .verify_signature(Some(ca.public_key()))
        .map_err(|_| anyhow::anyhow!("Agent management client certificate signature is invalid"))?;
    let basic_constraints = certificate.basic_constraints().map_err(|_| {
        anyhow::anyhow!("Agent management client certificate BasicConstraints is malformed")
    })?;
    anyhow::ensure!(
        basic_constraints.is_none_or(|extension| !extension.value.ca),
        "Agent management client certificate must not be a CA"
    );
    let san = certificate
        .subject_alternative_name()
        .map_err(|_| anyhow::anyhow!("Agent management client certificate SAN is malformed"))?
        .ok_or_else(|| anyhow::anyhow!("Agent management client certificate requires URI SAN"))?;
    let expected_uri = format!("spiffe://streamserver/core/{core_instance_id}");
    anyhow::ensure!(
        matches!(
            san.value.general_names.as_slice(),
            [x509_parser::extensions::GeneralName::URI(uri)] if *uri == expected_uri
        ),
        "Agent management client certificate URI SAN must match CORE_INSTANCE_ID"
    );
    let key_usage = certificate
        .key_usage()
        .map_err(|_| anyhow::anyhow!("Agent management client key usage is malformed"))?
        .ok_or_else(|| {
            anyhow::anyhow!("Agent management client certificate requires digitalSignature")
        })?;
    anyhow::ensure!(
        key_usage.value.flags == 1 && key_usage.value.digital_signature(),
        "Agent management client certificate requires digitalSignature-only key usage"
    );
    let extended = certificate
        .extended_key_usage()
        .map_err(|_| anyhow::anyhow!("Agent management client certificate EKU is malformed"))?
        .ok_or_else(|| {
            anyhow::anyhow!("Agent management client certificate requires clientAuth EKU")
        })?;
    anyhow::ensure!(
        extended.value.client_auth
            && !extended.value.server_auth
            && !extended.value.any
            && !extended.value.code_signing
            && !extended.value.email_protection
            && !extended.value.time_stamping
            && !extended.value.ocsp_signing
            && extended.value.other.is_empty(),
        "Agent management client certificate requires clientAuth-only EKU"
    );
    validate_private_key_matches_certificate(
        settings.agent_management_client_key_path.trim(),
        certificate.public_key().raw,
        "Agent management client",
    )?;
    Ok((core_instance_id, ca_pem))
}

fn validate_private_key_matches_certificate(
    private_key_path: &str,
    certificate_public_key_der: &[u8],
    description: &str,
) -> anyhow::Result<()> {
    use rcgen::KeyPair;
    use zeroize::Zeroize as _;

    let mut private_key_pem = fs::read_to_string(private_key_path)
        .with_context(|| format!("failed to read {description} private key {private_key_path}"))?;
    let mut private_key = match KeyPair::from_pem(&private_key_pem) {
        Ok(key) => key,
        Err(_) => {
            private_key_pem.zeroize();
            anyhow::bail!("{description} private key is invalid")
        }
    };
    private_key_pem.zeroize();
    let matches = private_key.public_key_der() == certificate_public_key_der;
    private_key.zeroize();
    anyhow::ensure!(
        matches,
        "{description} certificate and private key do not match"
    );
    Ok(())
}

fn decode_exact_certificate_pem(value: &str, description: &str) -> anyhow::Result<Vec<u8>> {
    let (remaining, pem) = x509_parser::pem::parse_x509_pem(value.as_bytes())
        .map_err(|_| anyhow::anyhow!("{description} is not valid PEM"))?;
    anyhow::ensure!(
        remaining.iter().all(u8::is_ascii_whitespace),
        "{description} must contain exactly one PEM certificate"
    );
    anyhow::ensure!(
        pem.label == "CERTIFICATE",
        "{description} PEM is not a certificate"
    );
    Ok(pem.contents)
}

fn load_and_validate_agent_capability_key_pair(
    settings: &config::CoreSettings,
) -> anyhow::Result<(String, String)> {
    use rcgen::KeyPair;
    use zeroize::Zeroize as _;

    let public_path = settings.agent_capability_jwt_public_key_path.trim();
    let private_path = settings.agent_capability_jwt_private_key_path.trim();
    anyhow::ensure!(
        !public_path.is_empty() && !private_path.is_empty(),
        "Agent enrollment requires CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH and CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH"
    );
    let public_pem = fs::read_to_string(public_path)
        .with_context(|| format!("failed to read Agent capability public key {public_path}"))?;
    let (remaining, public_block) = x509_parser::pem::parse_x509_pem(public_pem.as_bytes())
        .map_err(|_| anyhow::anyhow!("Agent capability public key is not valid PEM"))?;
    anyhow::ensure!(
        remaining.iter().all(u8::is_ascii_whitespace) && public_block.label == "PUBLIC KEY",
        "Agent capability public key must be exactly one PUBLIC KEY PEM"
    );
    let (spki_remaining, spki) =
        x509_parser::x509::SubjectPublicKeyInfo::from_der(&public_block.contents)
            .map_err(|_| anyhow::anyhow!("Agent capability public key is not valid SPKI DER"))?;
    anyhow::ensure!(
        spki_remaining.is_empty()
            && spki.algorithm.algorithm.to_id_string() == "1.3.101.112"
            && spki.algorithm.parameters.is_none(),
        "Agent capability key must use Ed25519"
    );

    let mut private_pem = fs::read_to_string(private_path)
        .with_context(|| format!("failed to read Agent capability private key {private_path}"))?;
    let mut private_key = match KeyPair::from_pem(&private_pem) {
        Ok(key) => key,
        Err(_) => {
            private_pem.zeroize();
            anyhow::bail!("Agent capability private key is invalid")
        }
    };
    private_pem.zeroize();
    let matches = private_key.public_key_der() == public_block.contents;
    private_key.zeroize();
    anyhow::ensure!(
        matches,
        "Agent capability public and private keys do not match"
    );
    let kid = bytes_to_lower_hex(&Sha256::digest(&public_block.contents));
    Ok((public_pem, kid))
}

fn validate_dedicated_agent_security_keys(
    settings: &config::CoreSettings,
    capability_public_key_pem: &str,
) -> anyhow::Result<()> {
    let capability_key =
        decode_exact_public_key_pem(capability_public_key_pem, "Agent capability public key")?;
    let agent_ca_key = certificate_public_key_der_from_path(
        settings.agent_ca_cert_path.trim(),
        "Agent signing CA",
    )?;
    let grpc_server_key = certificate_public_key_der_from_path(
        settings.grpc_tls_cert_path.trim(),
        "gRPC server certificate",
    )?;
    let grpc_server_ca_key = certificate_public_key_der_from_path(
        settings.grpc_tls_server_ca_path.trim(),
        "gRPC server CA",
    )?;
    let management_client_key = certificate_public_key_der_from_path(
        settings.agent_management_client_cert_path.trim(),
        "Agent management client certificate",
    )?;
    let management_client_ca_key = certificate_public_key_der_from_path(
        settings.agent_management_ca_path.trim(),
        "Agent management client CA",
    )?;
    let keys = [
        ("Agent capability", capability_key.as_slice()),
        ("Agent signing CA", agent_ca_key.as_slice()),
        ("gRPC server", grpc_server_key.as_slice()),
        ("gRPC server CA", grpc_server_ca_key.as_slice()),
        ("Agent management client", management_client_key.as_slice()),
        (
            "Agent management client CA",
            management_client_ca_key.as_slice(),
        ),
    ];
    for left in 0..keys.len() {
        for right in (left + 1)..keys.len() {
            anyhow::ensure!(
                keys[left].1 != keys[right].1,
                "{} and {} keys must be dedicated and must not be reused",
                keys[left].0,
                keys[right].0
            );
        }
    }

    if settings.auth_mode == config::AuthMode::LocalPassword {
        let auth_path = settings.auth_jwt_public_key_path.trim();
        if !auth_path.is_empty() {
            let auth_pem = fs::read_to_string(auth_path)
                .with_context(|| format!("failed to read local auth public key {auth_path}"))?;
            let auth_key = decode_exact_public_key_pem(&auth_pem, "local auth public key")?;
            anyhow::ensure!(
                capability_key != auth_key,
                "Agent capability key must not reuse the local auth signing key"
            );
        }
    }
    Ok(())
}

fn certificate_public_key_der_from_path(path: &str, description: &str) -> anyhow::Result<Vec<u8>> {
    let pem =
        fs::read_to_string(path).with_context(|| format!("failed to read {description} {path}"))?;
    let der = decode_exact_certificate_pem(&pem, description)?;
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(&der)
        .map_err(|_| anyhow::anyhow!("{description} is not valid X.509"))?;
    Ok(certificate.public_key().raw.to_vec())
}

fn decode_exact_public_key_pem(value: &str, description: &str) -> anyhow::Result<Vec<u8>> {
    let (remaining, pem) = x509_parser::pem::parse_x509_pem(value.as_bytes())
        .map_err(|_| anyhow::anyhow!("{description} is not valid PEM"))?;
    anyhow::ensure!(
        remaining.iter().all(u8::is_ascii_whitespace) && pem.label == "PUBLIC KEY",
        "{description} must be exactly one PUBLIC KEY PEM"
    );
    let (der_remaining, _) = x509_parser::x509::SubjectPublicKeyInfo::from_der(&pem.contents)
        .map_err(|_| anyhow::anyhow!("{description} is not valid SPKI DER"))?;
    anyhow::ensure!(
        der_remaining.is_empty(),
        "{description} contains trailing SPKI DER data"
    );
    Ok(pem.contents)
}

fn bytes_to_lower_hex(value: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(value.len() * 2);
    for byte in value {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

async fn load_http_tls_config(
    settings: &config::CoreSettings,
) -> anyhow::Result<Option<RustlsConfig>> {
    if settings.http_tls_cert_path.trim().is_empty() && settings.http_tls_key_path.trim().is_empty()
    {
        return Ok(None);
    }
    anyhow::ensure!(
        !settings.http_tls_cert_path.trim().is_empty()
            && !settings.http_tls_key_path.trim().is_empty(),
        "CORE_HTTP_TLS_CERT_PATH and CORE_HTTP_TLS_KEY_PATH must be set together"
    );

    RustlsConfig::from_pem_file(&settings.http_tls_cert_path, &settings.http_tls_key_path)
        .await
        .with_context(|| {
            format!(
                "failed to load Core HTTP TLS certificate {} and key {}",
                settings.http_tls_cert_path, settings.http_tls_key_path
            )
        })
        .map(Some)
}

async fn serve_http(
    listener: StdTcpListener,
    app: Router,
    tls_config: Option<RustlsConfig>,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    if let Some(tls_config) = tls_config {
        let handle = HttpServerHandle::new();
        let shutdown_handle = handle.clone();
        let shutdown_task = tokio::spawn(async move {
            wait_for_shutdown(shutdown).await;
            shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(30)));
        });
        let result = axum_server::from_tcp_rustls(listener, tls_config)?
            .handle(handle)
            .serve(service)
            .await;
        shutdown_task.abort();
        result?;
    } else {
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        axum::serve(listener, service)
            .with_graceful_shutdown(wait_for_shutdown(shutdown))
            .await?;
    }
    Ok(())
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

#[derive(Deserialize)]
struct CreateAgentEnrollmentRequest {
    node_id: Uuid,
}

#[derive(Serialize)]
struct CreateAgentEnrollmentResponse {
    enrollment_id: Uuid,
    node_id: Uuid,
    token: String,
    expires_at: DateTime<Utc>,
}

impl Drop for CreateAgentEnrollmentResponse {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.token);
    }
}

#[derive(Deserialize)]
struct EnrollAgentRequest {
    node_id: Uuid,
    csr_pem: String,
    management_csr_pem: String,
}

#[derive(Serialize)]
struct EnrollAgentResponse {
    node_id: Uuid,
    certificate_pem: String,
    ca_certificate_pem: String,
    agent_client_issuer_ca_pem: String,
    control_plane_server_ca_pem: String,
    management_client_ca_pem: String,
    fingerprint_sha256: String,
    serial_number: String,
    not_before: DateTime<Utc>,
    not_after: DateTime<Utc>,
    management_certificate_pem: String,
    management_fingerprint_sha256: String,
    management_serial_number: String,
    management_not_before: DateTime<Utc>,
    management_not_after: DateTime<Utc>,
    capability_jwt_public_key_pem: String,
    capability_jwt_kid: String,
}

const fn default_snap_timeout_sec() -> u32 {
    10
}

const fn default_snap_expire_sec() -> u32 {
    30
}
