mod capability;
mod config;
mod control_plane;
mod heartbeat;
mod runtime;
mod telemetry;

use std::path::Path;

use axum::{Json, Router, extract::State, routing::get};
use capability::binary_available;
use chrono::{DateTime, Utc};
use control_plane::AgentController;
use serde::Serialize;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::info;

#[derive(Debug, Clone)]
struct AppState {
    started_at: DateTime<Utc>,
    environment: String,
    readiness: AgentReadiness,
}

#[derive(Debug, Clone, Serialize)]
struct AgentReadiness {
    ffmpeg_available: bool,
    ffprobe_available: bool,
    work_root_exists: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings = config::Settings::load()?;
    telemetry::init(&settings.logging);

    let readiness = AgentReadiness {
        ffmpeg_available: binary_available(&settings.agent.ffmpeg_bin),
        ffprobe_available: binary_available(&settings.agent.ffprobe_bin),
        work_root_exists: Path::new(&settings.agent.work_root).exists(),
    };

    info!(
        environment = %settings.environment,
        node_name = %settings.agent.node_name,
        http_addr = %settings.agent.http_addr,
        core_endpoint = %settings.agent.core_endpoint,
        "starting media-agent"
    );

    let state = AppState {
        started_at: Utc::now(),
        environment: settings.environment.clone(),
        readiness,
    };

    let controller = AgentController::new(settings.clone())?;
    tokio::spawn(async move {
        controller.run().await;
    });

    let app = Router::new()
        .route("/health/live", get(live_health))
        .route("/health/ready", get(ready_health))
        .route("/health/metadata", get(agent_metadata))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = TcpListener::bind(&settings.agent.http_addr).await?;
    info!(listen_addr = %listener.local_addr()?, "media-agent http server ready");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn live_health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        started_at: state.started_at,
        environment: state.environment,
    })
}

async fn ready_health(State(state): State<AppState>) -> Json<ReadyResponse> {
    Json(ReadyResponse {
        status: if state.readiness.ffmpeg_available
            && state.readiness.ffprobe_available
            && state.readiness.work_root_exists
        {
            "ready"
        } else {
            "degraded"
        },
        started_at: state.started_at,
        environment: state.environment,
        readiness: state.readiness,
    })
}

async fn agent_metadata(State(state): State<AppState>) -> Json<ReadyResponse> {
    ready_health(State(state)).await
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

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    started_at: DateTime<Utc>,
    environment: String,
}

#[derive(Debug, Serialize)]
struct ReadyResponse {
    status: &'static str,
    started_at: DateTime<Utc>,
    environment: String,
    readiness: AgentReadiness,
}
