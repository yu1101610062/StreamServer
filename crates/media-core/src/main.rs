mod config;
mod control_plane;
mod error;
mod repository;
mod telemetry;

use std::{
    collections::BTreeMap,
    fs,
    net::{IpAddr, SocketAddr},
    path::{Component, Path as FsPath, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use control_plane::ControlPlaneService;
use error::AppError;
use repository::{
    CreateTaskResult, TaskCloneOverride, TaskListFilter, TaskRepository, ZlmPublishTaskRecord,
    ZlmRecordFileRecord, ZlmStreamEventRecord, ZlmTaskEventHookRecord,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use tokio::{net::TcpListener, sync::watch};
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tower_http::trace::TraceLayer;
use tracing::info;
use uuid::Uuid;

use media_domain::{TaskOperation, TaskSpec};

#[derive(Debug, Clone)]
struct AppState {
    repository: Arc<TaskRepository>,
    control_plane: ControlPlaneService,
    started_at: DateTime<Utc>,
    environment: String,
    hook_shared_secret: String,
    hook_source_allowlist: Vec<IpAddr>,
    zlm_auto_close_on_no_reader_enabled: bool,
    storage_allowlist: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    let repository = Arc::new(TaskRepository::new(pool));
    let control_plane = ControlPlaneService::new(repository.clone());
    let hook_source_allowlist = parse_hook_source_allowlist(&settings.core.hook_source_allowlist)?;

    let state = AppState {
        repository: repository.clone(),
        control_plane: control_plane.clone(),
        started_at: Utc::now(),
        environment: settings.environment.clone(),
        hook_shared_secret: settings.core.hook_shared_secret.clone(),
        hook_source_allowlist,
        zlm_auto_close_on_no_reader_enabled: settings.core.zlm_auto_close_on_no_reader_enabled,
        storage_allowlist: settings.core.storage_allowlist.clone(),
    };

    let app = build_app(state);

    let listener = TcpListener::bind(&settings.core.http_addr).await?;
    info!(listen_addr = %listener.local_addr()?, "media-core http server ready");

    let grpc_addr = settings.core.grpc_addr.parse()?;
    let control_plane = control_plane.into_server();
    info!(listen_addr = %settings.core.grpc_addr, "media-core grpc server ready");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
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
        .add_service(control_plane)
        .serve_with_shutdown(grpc_addr, wait_for_shutdown(shutdown_rx.clone()));

    let (http_result, grpc_result) = tokio::join!(http_server, grpc_server);
    http_result?;
    grpc_result?;
    let _ = signal_handle.await;

    Ok(())
}

fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/health/live", get(live_health))
        .route("/health/ready", get(ready_health))
        .route("/internal/hooks/zlm/{server_id}", post(receive_zlm_hook))
        .route(
            "/internal/hooks/zlm/{server_id}/{hook_name}",
            post(receive_named_zlm_hook),
        )
        .nest(
            "/api/v1",
            Router::new()
                .route("/tasks", post(create_task).get(list_tasks))
                .route("/tasks/{id}", get(get_task))
                .route("/tasks/{id}/resolved-spec", get(get_resolved_spec))
                .route("/tasks/{id}/start", post(start_task))
                .route("/tasks/{id}/stop", post(stop_task))
                .route("/tasks/{id}/cancel", post(cancel_task))
                .route("/tasks/{id}/retry", post(retry_task))
                .route("/tasks/{id}/clone", post(clone_task)),
        )
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

async fn create_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(task): Json<TaskSpec>,
) -> Result<(StatusCode, Json<repository::TaskSummary>), AppError> {
    let idempotency_key = extract_idempotency_key(&headers)?;
    let request_hash = hash_json(&task)?;

    match state
        .repository
        .create_task(&idempotency_key, &request_hash, task)
        .await?
    {
        CreateTaskResult::Fresh(task) => {
            if task.status == media_domain::TaskStatus::Validating {
                match state.control_plane.dispatch_task(task.id).await {
                    Ok(()) => {}
                    Err(control_plane::ControlPlaneError::NoConnectedNode)
                    | Err(control_plane::ControlPlaneError::NodeDisconnected(_)) => {}
                    Err(error) => return Err(error.into()),
                }
                let task = state.repository.get_task_summary(task.id).await?;
                Ok((StatusCode::CREATED, Json(task)))
            } else {
                Ok((StatusCode::CREATED, Json(task)))
            }
        }
        CreateTaskResult::Replay(task) => Ok((StatusCode::OK, Json(task))),
    }
}

async fn list_tasks(
    State(state): State<AppState>,
    Query(filter): Query<TaskListFilter>,
) -> Result<Json<media_domain::Page<repository::TaskSummary>>, AppError> {
    let tasks = state.repository.list_tasks(filter).await?;
    Ok(Json(tasks))
}

async fn get_task(
    State(state): State<AppState>,
    Path(task_id): Path<Uuid>,
) -> Result<Json<repository::TaskDetail>, AppError> {
    let task = state.repository.get_task(task_id).await?;
    Ok(Json(task))
}

async fn get_resolved_spec(
    State(state): State<AppState>,
    Path(task_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let spec = state.repository.get_resolved_spec(task_id).await?;
    Ok(Json(spec))
}

async fn start_task(
    State(state): State<AppState>,
    Path(task_id): Path<Uuid>,
) -> Result<(StatusCode, Json<repository::TaskSummary>), AppError> {
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
    Path(task_id): Path<Uuid>,
) -> Result<(StatusCode, Json<repository::TaskSummary>), AppError> {
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
    Path(task_id): Path<Uuid>,
) -> Result<(StatusCode, Json<repository::TaskSummary>), AppError> {
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
    Path(task_id): Path<Uuid>,
) -> Result<(StatusCode, Json<repository::AttemptSummary>), AppError> {
    let attempt = state.repository.retry_task(task_id).await?;
    Ok((StatusCode::ACCEPTED, Json(attempt)))
}

async fn clone_task(
    State(state): State<AppState>,
    Path(task_id): Path<Uuid>,
    overrides: Option<Json<TaskCloneOverride>>,
) -> Result<(StatusCode, Json<repository::TaskSummary>), AppError> {
    let task = state
        .repository
        .clone_task(task_id, overrides.map(|Json(value)| value))
        .await?;
    Ok((StatusCode::CREATED, Json(task)))
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
                let is_rtp_receive = resolved_spec.task_type == media_domain::TaskType::RtpReceive;
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
        "on_record_hls" => {
            let hook = parse_record_hls_hook(&sanitized_payload)?;
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
                            url: hook.url.clone().or(hook.m3u8_url.clone()),
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

fn parse_record_hls_hook(payload: &serde_json::Value) -> Result<ZlmOnRecordHlsPayload, AppError> {
    serde_json::from_value(payload.clone())
        .map_err(|error| AppError::BadRequest(format!("invalid on_record_hls payload: {error}")))
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
    let publish = resolved
        .as_ref()
        .map(|value| &value.publish)
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
        "enable_rtsp": publish.enable_rtsp.unwrap_or(true),
        "enable_rtmp": publish.enable_rtmp.unwrap_or(true),
        "enable_ts": publish.enable_http_ts.unwrap_or(true),
        "enable_fmp4": publish.enable_http_fmp4.unwrap_or(true),
        "enable_hls": publish.enable_hls.unwrap_or(false),
        "enable_hls_fmp4": false,
        "enable_mp4": false,
        "modify_stamp": 2,
        "continue_push_ms": 15_000,
        "mp4_as_player": record.as_player.unwrap_or(false),
        "auto_close": auto_close_enabled && publish.stop_on_no_reader.unwrap_or(false),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use tower::util::ServiceExt;

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

    fn test_app_state(pool: sqlx::PgPool) -> AppState {
        let repository = Arc::new(TaskRepository::new(pool));
        let control_plane = ControlPlaneService::new(repository.clone());
        AppState {
            repository,
            control_plane,
            started_at: Utc::now(),
            environment: "test".to_string(),
            hook_shared_secret: String::new(),
            hook_source_allowlist: Vec::new(),
            zlm_auto_close_on_no_reader_enabled: false,
            storage_allowlist: vec![std::env::temp_dir().to_string_lossy().to_string()],
        }
    }

    fn sample_create_task_payload(start_mode: &str) -> serde_json::Value {
        json!({
            "name": "relay-camera-01",
            "type": "live_relay",
            "priority": 50,
            "common": {
                "tenant_id": "default",
                "created_by": "alice"
            },
            "input": {
                "kind": "rtsp",
                "url": "rtsp://192.168.1.10/live"
            },
            "publish": {
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
        let db = TestDatabase::new(false).await?;
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
        let db = TestDatabase::new(true).await?;
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
        let db = TestDatabase::new(true).await?;
        let app = build_app(test_app_state(db.pool.clone()));
        let first_body = serde_json::to_vec(&sample_create_task_payload("manual"))?;
        let second_body = serde_json::to_vec(&json!({
            "name": "relay-camera-02",
            "type": "live_relay",
            "priority": 50,
            "common": {
                "tenant_id": "default",
                "created_by": "alice"
            },
            "input": {
                "kind": "rtsp",
                "url": "rtsp://192.168.1.11/live"
            },
            "publish": {
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
        let db = TestDatabase::new(true).await?;
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
                        "type": "live_relay",
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
    async fn clone_task_applies_supported_request_overrides() -> anyhow::Result<()> {
        let db = TestDatabase::new(true).await?;
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
                        "profile": "archival",
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
        assert_eq!(body["profile"], json!("archival"));
        assert_eq!(body["status"], json!("CREATED"));

        let detail = repository.get_task(cloned_id).await?;
        assert_eq!(detail.task.name, "relay-camera-01-copy");
        assert_eq!(detail.task.priority, 15);
        assert_eq!(detail.task.profile.as_deref(), Some("archival"));
        assert_eq!(detail.requested_spec["common"]["created_by"], json!("bob"));
        assert_eq!(
            detail.requested_spec["schedule"]["start_mode"],
            json!("manual")
        );
        assert_eq!(
            detail.resolved_spec.as_ref().unwrap()["profile"],
            json!("archival")
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
    async fn clone_task_rejects_invalid_override_payload() -> anyhow::Result<()> {
        let db = TestDatabase::new(true).await?;
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
        let db = TestDatabase::new(true).await?;
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
            "type": "file_to_live",
            "name": "push",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "/tmp/input.mp4"},
            "publish": {
                "kind": "zlm_ingest",
                "url": "rtmp://127.0.0.1/live/stream-a",
                "enable_rtsp": false,
                "enable_rtmp": true,
                "enable_http_ts": false,
                "enable_http_fmp4": true,
                "enable_hls": true,
                "stop_on_no_reader": true
            },
            "record": {"enabled": false, "as_player": true},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }))
        .expect("task spec should parse");

        let response = build_publish_hook_response(Some(&spec), true);

        assert_eq!(response["enable_rtsp"], json!(false));
        assert_eq!(response["enable_hls"], json!(true));
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
}
