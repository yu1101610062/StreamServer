mod artifact_cleanup;
mod capability;
mod config;
mod control_plane;
mod ffmpeg_args;
mod ffmpeg_plan;
mod ffmpeg_probe;
mod heartbeat;
mod identity;
mod management;
mod management_auth;
mod media_policy;
mod runtime;
mod runtime_adoption;
mod runtime_artifacts;
mod runtime_controls;
mod runtime_events;
mod runtime_executor;
mod runtime_io;
mod runtime_live_relay_cleanup;
mod runtime_live_relay_events;
mod runtime_live_relay_monitor;
mod runtime_live_relay_recording;
mod runtime_manager;
mod runtime_metadata;
mod runtime_monitors;
mod runtime_outputs;
mod runtime_persistence;
mod runtime_plan;
mod runtime_process;
mod runtime_process_exit;
mod runtime_process_monitors;
mod runtime_process_start;
mod runtime_recording;
mod runtime_recovery;
mod runtime_registry;
mod runtime_rtp_monitor;
mod runtime_start;
mod runtime_startup_probe;
mod runtime_stop;
mod runtime_transcode;
mod runtime_types;
mod runtime_zlm;
mod runtime_zlm_start;
mod telemetry;
mod upload;
mod zlm_debug;
mod zlm_hook;

use std::{
    fs,
    future::Future,
    net::{SocketAddr, TcpListener as StdTcpListener},
    path::{Path, PathBuf},
    task::Poll,
    time::Duration,
};

use anyhow::Context;
use axum::{Json, Router, extract::State};
use axum_server::{
    Handle as HttpServerHandle,
    tls_rustls::{RustlsAcceptor, RustlsConfig},
};
use capability::binary_available;
use chrono::{DateTime, Utc};
use config::AgentEnvironment;
use control_plane::AgentController;
use hyper_util::rt::TokioTimer;
use identity::LoadedIdentity;
use management_auth::CapabilityVerifier;
use serde::Serialize;
use tokio::sync::{oneshot, watch};
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use uuid::Uuid;

#[derive(Debug, Clone)]
struct AppState {
    started_at: DateTime<Utc>,
    environment: String,
    readiness: AgentReadiness,
    node_id: Uuid,
    upload: upload::UploadConfig,
}

#[derive(Debug, Clone, Serialize)]
struct AgentReadiness {
    ffmpeg_available: bool,
    ffprobe_available: bool,
    work_root_exists: bool,
    zlm_hook_listener_available: bool,
}

impl AgentReadiness {
    fn is_ready(&self) -> bool {
        self.ffmpeg_available
            && self.ffprobe_available
            && self.work_root_exists
            && self.zlm_hook_listener_available
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorExit {
    Shutdown,
    RestartRequired,
}

const HTTP_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if identity::run_enrollment_cli_if_requested().await? {
        return Ok(());
    }
    let settings = config::Settings::load()?;
    telemetry::init(&settings.logging);

    let readiness = AgentReadiness {
        ffmpeg_available: binary_available(&settings.agent.ffmpeg_bin),
        ffprobe_available: binary_available(&settings.agent.ffprobe_bin),
        work_root_exists: Path::new(&settings.agent.work_root).exists(),
        zlm_hook_listener_available: true,
    };

    let (zlm_hook_relay, zlm_hook_requests) =
        zlm_hook::zlm_hook_channel(settings.agent.zlm_hook_queue_capacity);
    let zlm_hook_ingress = zlm_hook::ZlmHookIngress::new(
        settings.agent.zlm_hook_shared_secret.clone(),
        zlm_hook_relay,
        Duration::from_secs(settings.agent.zlm_hook_timeout_sec),
    );
    let controller =
        AgentController::new_with_zlm_hook_requests(settings.clone(), zlm_hook_requests)?;
    let node_id = controller.node_id();
    let loaded_identity = controller.loaded_identity();
    let upload = upload::UploadConfig::from_settings(&settings.agent)?;

    let state = AppState {
        started_at: Utc::now(),
        environment: settings.environment.clone(),
        readiness,
        node_id,
        upload,
    };

    let (management_tls, verifier) =
        load_management_security(&settings, node_id, loaded_identity.as_deref())?;
    let public_tls = load_public_tls_config(&settings.agent).await?;
    let management_addr = settings
        .agent
        .management_addr
        .parse::<SocketAddr>()
        .context("parse AGENT_MANAGEMENT_ADDR")?;
    let public_addr = settings
        .agent
        .public_media_addr
        .parse::<SocketAddr>()
        .context("parse AGENT_PUBLIC_MEDIA_ADDR")?;
    let zlm_hook_addr = settings
        .agent
        .zlm_hook_addr
        .parse::<SocketAddr>()
        .context("parse AGENT_ZLM_HOOK_ADDR")?;
    let management_listener = StdTcpListener::bind(management_addr)
        .with_context(|| format!("bind Agent management listener {management_addr}"))?;
    let public_listener = StdTcpListener::bind(public_addr)
        .with_context(|| format!("bind Agent public media listener {public_addr}"))?;
    let zlm_hook_listener = StdTcpListener::bind(zlm_hook_addr)
        .with_context(|| format!("bind Agent ZLMediaKit hook listener {zlm_hook_addr}"))?;
    let delete_jti_root = if settings.environment_kind()? == AgentEnvironment::Production {
        PathBuf::from(&settings.agent.identity_dir).join("capability-jti/delete")
    } else {
        state
            .upload
            .work_root
            .join(".streamserver/management/delete-jti")
    };
    let management_state = management::ManagementState::new(
        state.clone(),
        verifier,
        delete_jti_root,
        settings.agent.management_max_concurrency,
    )?;
    let public_app = management::public_router(state).layer(TraceLayer::new_for_http());
    let management_app =
        management::management_router(management_state).layer(TraceLayer::new_for_http());
    // ZLM authenticates with a query token. Do not attach the default HTTP
    // TraceLayer here because its request span records the complete URI.
    let zlm_hook_app = zlm_hook::zlm_hook_router(zlm_hook_ingress);

    info!(
        environment = %settings.environment,
        node_name = %settings.agent.node_name,
        public_media_addr = %public_addr,
        public_media_tls = public_tls.is_some(),
        management_addr = %management_addr,
        management_mtls = true,
        zlm_hook_addr = %zlm_hook_addr,
        core_endpoint = %settings.agent.core_endpoint,
        "starting media-agent listeners"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (management_ready_tx, management_ready_rx) = oneshot::channel();
    let (public_ready_tx, public_ready_rx) = oneshot::channel();
    let (zlm_hook_ready_tx, zlm_hook_ready_rx) = oneshot::channel();
    let mut management_task = tokio::spawn(serve_http(
        management_listener,
        management_app,
        Some(management_tls),
        shutdown_rx.clone(),
        management_ready_tx,
    ));
    let mut public_task = tokio::spawn(serve_http(
        public_listener,
        public_app,
        public_tls,
        shutdown_rx.clone(),
        public_ready_tx,
    ));
    let mut zlm_hook_task = tokio::spawn(serve_http(
        zlm_hook_listener,
        zlm_hook_app,
        None,
        shutdown_rx,
        zlm_hook_ready_tx,
    ));
    if let Err(error) = tokio::try_join!(
        async {
            management_ready_rx
                .await
                .context("Agent management listener exited before accepting connections")
        },
        async {
            public_ready_rx
                .await
                .context("Agent public media listener exited before accepting connections")
        },
        async {
            zlm_hook_ready_rx
                .await
                .context("Agent ZLMediaKit hook listener exited before accepting connections")
        }
    ) {
        let _ = shutdown_tx.send(true);
        let _ = management_task.await;
        let _ = public_task.await;
        let _ = zlm_hook_task.await;
        return Err(error);
    }
    info!(
        "Agent management, public media, and ZLMediaKit hook listeners are accepting connections"
    );
    let mut controller_task = tokio::spawn(controller.run());

    let outcome = tokio::select! {
        _ = shutdown_signal() => Ok(SupervisorExit::Shutdown),
        result = &mut management_task => unexpected_task_exit("Agent management listener", result).map(|()| SupervisorExit::Shutdown),
        result = &mut public_task => unexpected_task_exit("Agent public media listener", result).map(|()| SupervisorExit::Shutdown),
        result = &mut zlm_hook_task => unexpected_task_exit("Agent ZLMediaKit hook listener", result).map(|()| SupervisorExit::Shutdown),
        result = &mut controller_task => classify_controller_exit(result),
    };
    let _ = shutdown_tx.send(true);
    controller_task.abort();
    if !management_task.is_finished() {
        let _ = management_task.await;
    }
    if !public_task.is_finished() {
        let _ = public_task.await;
    }
    if !zlm_hook_task.is_finished() {
        let _ = zlm_hook_task.await;
    }
    match outcome {
        Ok(exit) => finish_supervisor_exit(exit),
        Err(error) => {
            error!(error = %error, "media-agent critical task exited");
            Err(error)
        }
    }
}

fn load_management_security(
    settings: &config::Settings,
    node_id: Uuid,
    loaded_identity: Option<&LoadedIdentity>,
) -> anyhow::Result<(RustlsConfig, CapabilityVerifier)> {
    let environment = settings.environment_kind()?;
    let complete_identity =
        loaded_identity.filter(|identity| identity.ensure_production_complete().is_ok());
    match select_management_security_source(environment, complete_identity.is_some())? {
        ManagementSecuritySource::EnrolledBundle => {
            let identity = complete_identity.expect("security source checked");
            anyhow::ensure!(
                identity.node_id() == node_id,
                "management identity node does not match Agent control-plane identity"
            );
            let capability_public_key = identity.capability_jwt_public_key_pem()?;
            let capability_kid = identity.capability_jwt_kid()?;
            let verifier = CapabilityVerifier::from_ed25519_public_pem(
                capability_public_key,
                capability_kid,
                node_id,
            )?;
            let tls = management::build_management_tls_config(
                identity.management_certificate_pem()?.as_bytes(),
                identity.management_private_key_pem()?.as_bytes(),
                identity.management_client_ca_pem()?.as_bytes(),
            )?;
            Ok((RustlsConfig::from_config(tls), verifier))
        }
        ManagementSecuritySource::StaticDevelopment => {
            let certificate = read_security_file(
                "development management server certificate",
                &settings.agent.management_tls_cert_path,
            )?;
            let private_key = read_security_file(
                "development management server private key",
                &settings.agent.management_tls_key_path,
            )?;
            let client_ca = read_security_file(
                "development Core client CA",
                &settings.agent.management_tls_client_ca_path,
            )?;
            let capability_public_key = read_security_file(
                "development capability JWT public key",
                &settings.agent.management_capability_jwt_public_key_path,
            )?;
            let capability_public_key = std::str::from_utf8(&capability_public_key)
                .context("development capability JWT public key must be UTF-8 PEM")?;
            let (verifier, _) = CapabilityVerifier::from_ed25519_public_pem_with_derived_kid(
                capability_public_key,
                node_id,
            )?;
            let tls =
                management::build_management_tls_config(&certificate, &private_key, &client_ca)?;
            Ok((RustlsConfig::from_config(tls), verifier))
        }
    }
}

fn read_security_file(label: &str, path: &str) -> anyhow::Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("failed to read {label} from {path}"))
}

async fn load_public_tls_config(
    settings: &config::AgentSettings,
) -> anyhow::Result<Option<RustlsConfig>> {
    if settings.public_media_tls_cert_path.trim().is_empty()
        && settings.public_media_tls_key_path.trim().is_empty()
    {
        return Ok(None);
    }
    RustlsConfig::from_pem_file(
        &settings.public_media_tls_cert_path,
        &settings.public_media_tls_key_path,
    )
    .await
    .with_context(|| {
        format!(
            "failed to load Agent public media TLS certificate {} and key {}",
            settings.public_media_tls_cert_path, settings.public_media_tls_key_path
        )
    })
    .map(Some)
}

async fn serve_http(
    listener: StdTcpListener,
    app: Router,
    tls_config: Option<RustlsConfig>,
    shutdown: watch::Receiver<bool>,
    ready: oneshot::Sender<()>,
) -> anyhow::Result<()> {
    serve_http_with_grace_period(
        listener,
        app,
        tls_config,
        shutdown,
        ready,
        HTTP_GRACEFUL_SHUTDOWN_TIMEOUT,
    )
    .await
}

async fn serve_http_with_grace_period(
    listener: StdTcpListener,
    app: Router,
    tls_config: Option<RustlsConfig>,
    shutdown: watch::Receiver<bool>,
    ready: oneshot::Sender<()>,
    grace_period: Duration,
) -> anyhow::Result<()> {
    prepare_http_listener(&listener)?;
    let handle = HttpServerHandle::new();
    let shutdown_handle = handle.clone();
    let shutdown_task = tokio::spawn(async move {
        wait_for_shutdown(shutdown).await;
        shutdown_handle.graceful_shutdown(Some(grace_period));
    });
    if let Some(tls_config) = tls_config {
        let acceptor = RustlsAcceptor::new(tls_config).handshake_timeout(Duration::from_secs(10));
        let mut server = axum_server::from_tcp(listener)?.acceptor(acceptor);
        server
            .http_builder()
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(Duration::from_secs(5));
        let serve = server.handle(handle).serve(app.into_make_service());
        tokio::pin!(serve);
        if let Some(result) = poll_server_once(serve.as_mut()).await {
            shutdown_task.abort();
            result?;
            anyhow::bail!("TLS server exited during its first accept poll");
        }
        let _ = ready.send(());
        let result = serve.await;
        shutdown_task.abort();
        result?;
    } else {
        let serve = axum_server::from_tcp(listener)?
            .handle(handle)
            .serve(app.into_make_service());
        tokio::pin!(serve);
        if let Some(result) = poll_server_once(serve.as_mut()).await {
            shutdown_task.abort();
            result?;
            anyhow::bail!("HTTP server exited during its first accept poll");
        }
        let _ = ready.send(());
        let result = serve.await;
        shutdown_task.abort();
        result?;
    }
    Ok(())
}

fn prepare_http_listener(listener: &StdTcpListener) -> std::io::Result<()> {
    listener.set_nonblocking(true)
}

async fn poll_server_once<F>(mut server: std::pin::Pin<&mut F>) -> Option<F::Output>
where
    F: Future,
{
    std::future::poll_fn(|context| match server.as_mut().poll(context) {
        Poll::Ready(result) => Poll::Ready(Some(result)),
        Poll::Pending => Poll::Ready(None),
    })
    .await
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

fn unexpected_task_exit(
    name: &str,
    result: Result<anyhow::Result<()>, tokio::task::JoinError>,
) -> anyhow::Result<()> {
    match result {
        Ok(Ok(())) => anyhow::bail!("{name} exited unexpectedly"),
        Ok(Err(error)) => Err(error).with_context(|| format!("{name} failed")),
        Err(error) => Err(anyhow::Error::new(error)).with_context(|| format!("{name} panicked")),
    }
}

fn classify_controller_exit(
    result: Result<anyhow::Result<control_plane::AgentControllerExit>, tokio::task::JoinError>,
) -> anyhow::Result<SupervisorExit> {
    match result {
        Ok(Ok(control_plane::AgentControllerExit::RestartRequired)) => {
            Ok(SupervisorExit::RestartRequired)
        }
        Ok(Err(error)) => Err(error).context("Agent control-plane controller failed"),
        Err(error) => {
            Err(anyhow::Error::new(error)).context("Agent control-plane controller panicked")
        }
    }
}

fn finish_supervisor_exit(exit: SupervisorExit) -> anyhow::Result<()> {
    match exit {
        SupervisorExit::Shutdown => Ok(()),
        SupervisorExit::RestartRequired => {
            anyhow::bail!("certificate rotation requires a supervised Agent restart")
        }
    }
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
        status: if state.readiness.is_ready() {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagementSecuritySource {
    EnrolledBundle,
    StaticDevelopment,
}

fn select_management_security_source(
    environment: AgentEnvironment,
    has_complete_enrolled_identity: bool,
) -> anyhow::Result<ManagementSecuritySource> {
    if has_complete_enrolled_identity {
        return Ok(ManagementSecuritySource::EnrolledBundle);
    }
    anyhow::ensure!(
        environment == AgentEnvironment::Development,
        "production Agent management listener requires a complete enrolled identity bundle"
    );
    Ok(ManagementSecuritySource::StaticDevelopment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    #[cfg(unix)]
    use std::os::fd::AsRawFd;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    #[test]
    fn production_management_security_never_falls_back_to_static_paths() {
        assert_eq!(
            select_management_security_source(AgentEnvironment::Production, true).unwrap(),
            ManagementSecuritySource::EnrolledBundle
        );
        assert!(select_management_security_source(AgentEnvironment::Production, false).is_err());
        assert_eq!(
            select_management_security_source(AgentEnvironment::Development, false).unwrap(),
            ManagementSecuritySource::StaticDevelopment
        );
    }

    #[test]
    fn certificate_rotation_restart_bubbles_through_the_graceful_supervisor_path() {
        let classified =
            classify_controller_exit(Ok(Ok(control_plane::AgentControllerExit::RestartRequired)))
                .unwrap();
        assert_eq!(classified, SupervisorExit::RestartRequired);
        let error = finish_supervisor_exit(classified).unwrap_err();
        assert!(error.to_string().contains("certificate rotation"));
    }

    #[tokio::test]
    async fn server_ready_barrier_precedes_successful_accepts() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(serve_http(
            listener,
            Router::new().route("/ready-barrier", get(|| async { "ready" })),
            None,
            shutdown_rx,
            ready_tx,
        ));

        tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("server ready barrier timed out")
            .expect("server exited before ready barrier");
        let response = reqwest::get(format!("http://{address}/ready-barrier"))
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let _ = shutdown_tx.send(true);
        task.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn http_listener_is_nonblocking_before_async_server_conversion() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let before = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(before, -1);
        assert_eq!(before & libc::O_NONBLOCK, 0);

        prepare_http_listener(&listener).unwrap();

        let after = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(after, -1);
        assert_ne!(after & libc::O_NONBLOCK, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_shutdown_forces_a_slow_request_after_the_fixed_deadline() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let entered = Arc::new(Mutex::new(Some(entered_tx)));
        let app = Router::new().route(
            "/slow",
            get({
                let entered = entered.clone();
                move || {
                    let entered = entered.clone();
                    async move {
                        if let Some(sender) = entered.lock().unwrap().take() {
                            let _ = sender.send(());
                        }
                        std::future::pending::<()>().await;
                        "never"
                    }
                }
            }),
        );
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = oneshot::channel();
        let server = tokio::spawn(serve_http_with_grace_period(
            listener,
            app,
            None,
            shutdown_rx,
            ready_tx,
            Duration::from_millis(50),
        ));
        ready_rx.await.unwrap();
        let request = tokio::spawn(reqwest::get(format!("http://{address}/slow")));
        tokio::time::timeout(Duration::from_secs(1), entered_rx)
            .await
            .expect("slow request was not accepted")
            .unwrap();

        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("slow request blocked bounded graceful shutdown")
            .unwrap()
            .unwrap();
        request.abort();
    }
}
