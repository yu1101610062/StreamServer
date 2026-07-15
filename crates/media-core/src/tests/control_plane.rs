use super::*;
use crate::test_database::{acquire_test_database_slot, config_from_env, finish_setup};
use std::{
    net::Ipv4Addr,
    sync::atomic::{AtomicUsize, Ordering},
};

use axum::{Json, Router, http::StatusCode};
use media_domain::{
    CommonSpec, ExposeSpec, InputSpec, PublishSpec, RecordSpec, RecoverySpec, ResourceSpec,
    RuntimeSlotLoad, ScheduleSpec, SourceMode, StreamSpec, TaskStatus, TaskType,
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tonic::transport::{
    Certificate as TonicCertificate, Channel, ClientTlsConfig, Endpoint, Identity as TonicIdentity,
    Server, ServerTlsConfig,
};
use x509_parser::pem::parse_x509_pem;

use crate::agent_identity::{
    AgentCertificateAuthority, AgentEnrollmentPublicConfig, AgentIdentityService,
};

#[tokio::test]
async fn session_generation_cancel_before_waiter_first_poll_is_observed() {
    let generation = SessionGeneration::default();
    let canceled = generation.canceled();
    generation.cancel();

    timeout(std::time::Duration::from_millis(100), canceled)
        .await
        .expect("a cancellation recorded before the waiter is polled must remain observable");
}

#[derive(Debug)]
struct SequenceReadinessProbe {
    outcomes: std::sync::Mutex<VecDeque<bool>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl SequenceReadinessProbe {
    fn new(outcomes: impl IntoIterator<Item = bool>) -> Self {
        Self {
            outcomes: std::sync::Mutex::new(outcomes.into_iter().collect()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl crate::agent_management::AgentManagementReadinessProbe for SequenceReadinessProbe {
    fn probe<'a>(
        &'a self,
        _target: &'a crate::agent_management::AuthenticatedAgentManagementTarget,
    ) -> crate::agent_management::AgentManagementFuture<
        'a,
        Result<(), crate::agent_management::AgentManagementError>,
    > {
        Box::pin(async move {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.outcomes.lock().unwrap().pop_front().unwrap_or(false) {
                Ok(())
            } else {
                Err(crate::agent_management::AgentManagementError::NotReady)
            }
        })
    }
}

#[derive(Debug)]
struct ControlledReadinessProbe {
    calls: std::sync::atomic::AtomicUsize,
    sessions: std::sync::Mutex<Vec<Uuid>>,
    first_started: tokio::sync::Semaphore,
    first_release: tokio::sync::Semaphore,
}

impl ControlledReadinessProbe {
    fn new() -> Self {
        Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            sessions: std::sync::Mutex::new(Vec::new()),
            first_started: tokio::sync::Semaphore::new(0),
            first_release: tokio::sync::Semaphore::new(0),
        }
    }
}

impl crate::agent_management::AgentManagementReadinessProbe for ControlledReadinessProbe {
    fn probe<'a>(
        &'a self,
        target: &'a crate::agent_management::AuthenticatedAgentManagementTarget,
    ) -> crate::agent_management::AgentManagementFuture<
        'a,
        Result<(), crate::agent_management::AgentManagementError>,
    > {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.sessions.lock().unwrap().push(target.session_id());
            if call == 0 {
                self.first_started.add_permits(1);
                self.first_release
                    .acquire()
                    .await
                    .expect("controlled readiness release remains open")
                    .forget();
            }
            Ok(())
        })
    }
}

#[derive(Debug)]
struct CapturingZlmHookHandler {
    calls: Arc<Mutex<Vec<AuthenticatedZlmHook>>>,
    response: ZlmHookHandlerResponse,
}

impl CapturingZlmHookHandler {
    fn new(response: ZlmHookHandlerResponse) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            response,
        }
    }
}

impl ZlmHookHandler for CapturingZlmHookHandler {
    fn handle(&self, request: AuthenticatedZlmHook) -> ZlmHookFuture<'_> {
        Box::pin(async move {
            self.calls.lock().await.push(request);
            self.response.clone()
        })
    }
}

#[derive(Debug)]
struct BlockingZlmHookHandler {
    started: std::sync::atomic::AtomicUsize,
    release: tokio::sync::Semaphore,
}

impl BlockingZlmHookHandler {
    fn new() -> Self {
        Self {
            started: std::sync::atomic::AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

impl ZlmHookHandler for BlockingZlmHookHandler {
    fn handle(&self, _request: AuthenticatedZlmHook) -> ZlmHookFuture<'_> {
        Box::pin(async move {
            self.started
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.release
                .acquire()
                .await
                .expect("test release semaphore remains open")
                .forget();
            ZlmHookHandlerResponse {
                http_status: 200,
                body: json!({"code": 0}),
            }
        })
    }
}

#[derive(Debug)]
struct AuditedCoreZlmHookHandler {
    inner: crate::CoreZlmHookHandler,
    calls: std::sync::atomic::AtomicUsize,
    completed: tokio::sync::Semaphore,
}

impl AuditedCoreZlmHookHandler {
    fn new(repository: Arc<TaskRepository>) -> Self {
        Self {
            inner: crate::CoreZlmHookHandler::new(repository, false, Vec::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            completed: tokio::sync::Semaphore::new(0),
        }
    }
}

impl ZlmHookHandler for AuditedCoreZlmHookHandler {
    fn handle(&self, request: AuthenticatedZlmHook) -> ZlmHookFuture<'_> {
        Box::pin(async move {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let response = self.inner.handle(request).await;
            self.completed.add_permits(1);
            response
        })
    }
}

async fn pick_best_session_for_test(
    service: &ControlPlaneService,
    source_affinity_ip: Option<IpAddr>,
    spec: &TaskSpec,
    preference: ExecutionPreference,
) -> Option<SessionTarget> {
    let sessions = service.sessions.lock().await;
    pick_best_session_target(&sessions, source_affinity_ip, spec, preference)
}

fn sample_spec(kind: InputKind, url: Option<&str>, interface_ip: Option<&str>) -> TaskSpec {
    TaskSpec {
        task_type: TaskType::StreamIngest,
        name: "camera".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("test".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(kind),
            source_mode: kind.default_source_mode(),
            loop_enabled: None,
            start_offset_sec: None,
            url: url.map(str::to_string),
            group: None,
            port: None,
            interface_name: None,
            interface_ip: interface_ip.map(str::to_string),
            ttl: None,
            reuse: None,
            pkt_size: None,
            dscp: None,
            buffer_size: None,
            fifo_size: None,
            probe_timeout_ms: None,
            tcp_mode: None,
            ssrc: None,
        },
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: Default::default(),
        publish: PublishSpec::default(),
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    }
}

fn synthetic_session_identity() -> SessionIdentityState {
    let now = Utc::now();
    SessionIdentityState {
        certificate_id: Uuid::from_u128(1),
        fingerprint_sha256: [0x5a; 32],
        peer_ip: "192.0.2.10".parse().expect("synthetic peer IP"),
        connected_at: now,
        last_activity_at: now,
    }
}

#[test]
fn source_gateway_rewrites_live_http_input_to_relay_url() -> anyhow::Result<()> {
    let task_id = Uuid::parse_str("00000000-0000-0000-0000-000000000111")?;
    let mut spec = sample_spec(
        InputKind::HttpFlv,
        Some("http://customer.example/live.flv"),
        None,
    )
    .resolved();
    spec.input.start_offset_sec = Some(0);

    let action = crate::source_gateway::plan_gateway_action(&spec, task_id)
        .expect("live http input should use media relay");
    crate::source_gateway::apply_gateway_result(
        &mut spec,
        action,
        crate::source_gateway::GatewayActionResult::Relay {
            relay_url: "http://media:18080/relay/00000000-0000-0000-0000-000000000111?token=t"
                .to_string(),
        },
    )?;

    assert_eq!(spec.input.kind, Some(InputKind::HttpFlv));
    assert_eq!(spec.input.source_mode, Some(SourceMode::Live));
    assert_eq!(spec.input.start_offset_sec, None);
    assert_eq!(
        spec.input.url.as_deref(),
        Some("http://media:18080/relay/00000000-0000-0000-0000-000000000111?token=t")
    );
    Ok(())
}

#[test]
fn source_gateway_rewrites_vod_http_time_window_to_shared_file_path() -> anyhow::Result<()> {
    let task_id = Uuid::parse_str("00000000-0000-0000-0000-000000000222")?;
    let mut requested_spec = sample_spec(
        InputKind::HttpMp4,
        Some("http://customer.example/archive.mp4"),
        None,
    )
    .resolved();
    requested_spec.input.start_offset_sec = Some(600);
    requested_spec.record.enabled = Some(true);
    requested_spec.record.duration_sec = Some(180);
    let mut spec = requested_spec.clone();

    let action = crate::source_gateway::plan_gateway_action(&spec, task_id)
        .expect("vod http input should use media prefetch");
    assert_eq!(
        action,
        crate::source_gateway::GatewayAction::Prefetch {
            task_id,
            source_url: "http://customer.example/archive.mp4".to_string(),
            target_path: "imports/00000000-0000-0000-0000-000000000222/source.mp4".to_string(),
            source_kind: InputKind::HttpMp4,
            start_offset_sec: Some(600),
            duration_sec: Some(180),
        }
    );

    crate::source_gateway::apply_gateway_result(
        &mut spec,
        action,
        crate::source_gateway::GatewayActionResult::Prefetch {
            source_url: "imports/00000000-0000-0000-0000-000000000222/source.mp4".to_string(),
        },
    )?;

    assert_eq!(spec.input.kind, Some(InputKind::File));
    assert_eq!(spec.input.source_mode, Some(SourceMode::Vod));
    assert_eq!(spec.input.start_offset_sec, None);
    assert_eq!(spec.record.duration_sec, Some(180));
    assert_eq!(requested_spec.input.start_offset_sec, Some(600));
    assert_eq!(
        spec.input.url.as_deref(),
        Some("imports/00000000-0000-0000-0000-000000000222/source.mp4")
    );
    Ok(())
}

#[test]
fn source_gateway_normalizes_zero_vod_offset_before_prefetch() {
    let task_id = Uuid::from_u128(0x333);
    let mut spec = sample_spec(
        InputKind::HttpTs,
        Some("http://customer.example/archive"),
        None,
    )
    .resolved();
    spec.input.source_mode = Some(SourceMode::Vod);
    spec.input.start_offset_sec = Some(0);

    let action = crate::source_gateway::plan_gateway_action(&spec, task_id)
        .expect("vod http input should use media prefetch");
    assert!(matches!(
        action,
        crate::source_gateway::GatewayAction::Prefetch {
            source_kind: InputKind::HttpTs,
            start_offset_sec: None,
            duration_sec: None,
            ref target_path,
            ..
        } if target_path == &format!("imports/{task_id}/source.ts")
    ));
}

async fn spawn_prefetch_gateway_stub(response: Value) -> anyhow::Result<(String, JoinHandle<()>)> {
    use axum::{extract::State, routing::get, routing::post};

    async fn prefetch_response(State(response): State<Value>) -> Json<Value> {
        Json(response)
    }

    let app = Router::new()
        .route("/api/prefetch", post(prefetch_response))
        .route("/api/prefetch/{task_id}", get(prefetch_response))
        .with_state(response);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("prefetch gateway stub should run");
    });
    Ok((format!("http://{addr}"), handle))
}

#[tokio::test]
async fn source_gateway_rejects_old_ready_response_for_requested_time_slice() -> anyhow::Result<()>
{
    let task_id = Uuid::parse_str("00000000-0000-0000-0000-000000000334")?;
    let expected_source_url = format!("imports/{task_id}/source.mp4");
    let (gateway_base, gateway) = spawn_prefetch_gateway_stub(json!({
        "status": "ready",
        "source_url": expected_source_url
    }))
    .await?;
    let mut spec = sample_spec(
        InputKind::HttpMp4,
        Some("http://customer.example/archive.mp4"),
        None,
    )
    .resolved();
    spec.input.start_offset_sec = Some(600);
    spec.record.enabled = Some(true);
    spec.record.duration_sec = Some(180);

    let error = crate::source_gateway::SourceGatewayClient::new_for_test(&gateway_base)?
        .prepare_task_spec(task_id, &spec)
        .await
        .expect_err("an old ready response must not silently discard a requested time window");

    assert!(matches!(
        error,
        crate::source_gateway::SourceGatewayError::Rejected(ref reason)
            if reason.contains("time slice")
    ));
    assert_eq!(spec.input.kind, Some(InputKind::HttpMp4));
    assert_eq!(spec.input.start_offset_sec, Some(600));
    assert_eq!(spec.record.duration_sec, Some(180));
    gateway.abort();
    Ok(())
}

#[tokio::test]
async fn source_gateway_accepts_old_ready_response_without_time_slice() -> anyhow::Result<()> {
    let task_id = Uuid::parse_str("00000000-0000-0000-0000-000000000335")?;
    let expected_source_url = format!("imports/{task_id}/source.mp4");
    let (gateway_base, gateway) = spawn_prefetch_gateway_stub(json!({
        "status": "ready",
        "source_url": expected_source_url
    }))
    .await?;
    let spec = sample_spec(
        InputKind::HttpMp4,
        Some("http://customer.example/archive.mp4"),
        None,
    )
    .resolved();

    let rewritten = crate::source_gateway::SourceGatewayClient::new_for_test(&gateway_base)?
        .prepare_task_spec(task_id, &spec)
        .await?
        .expect("no-time prefetch should remain compatible with an old Gateway");

    assert_eq!(rewritten.input.kind, Some(InputKind::File));
    assert_eq!(rewritten.input.source_mode, Some(SourceMode::Vod));
    assert_eq!(
        rewritten.input.url.as_deref(),
        Some(expected_source_url.as_str())
    );
    assert_eq!(rewritten.input.start_offset_sec, None);
    assert_eq!(rewritten.record.duration_sec, None);
    gateway.abort();
    Ok(())
}

struct TestDatabase {
    _slot: tokio::sync::OwnedSemaphorePermit,
    admin_pool: PgPool,
    pool: PgPool,
    database_name: String,
}

impl TestDatabase {
    async fn new(admin_url: &str, run_migrations: bool) -> anyhow::Result<Self> {
        let slot = acquire_test_database_slot().await?;
        let admin_pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(admin_url)
            .await?;
        let database_name = format!("streamserver_test_{}", Uuid::now_v7().simple());
        sqlx::query(&format!("create database {database_name}"))
            .execute(&admin_pool)
            .await?;

        let database_url = test_database_url(admin_url, &database_name)?;
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await?;
        if run_migrations {
            sqlx::migrate!("../../migrations").run(&pool).await?;
        }

        Ok(Self {
            _slot: slot,
            admin_pool,
            pool,
            database_name,
        })
    }

    async fn maybe_new(run_migrations: bool) -> anyhow::Result<Option<Self>> {
        let config = config_from_env()?;
        if !database_is_reachable(&config.admin_url).await {
            return finish_setup(
                config.required,
                Err(anyhow::anyhow!(
                    "database is unreachable at {}",
                    config.admin_url
                )),
            );
        }
        finish_setup(
            config.required,
            Self::new(&config.admin_url, run_migrations).await,
        )
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

async fn current_attempt_lease_token(pool: &PgPool, task_id: Uuid) -> anyhow::Result<String> {
    Ok(sqlx::query_scalar::<_, String>(
        r#"
        select lease_token
          from task_attempts
         where task_id = $1
           and ended_at is null
         order by attempt_no desc
         limit 1
        "#,
    )
    .bind(task_id)
    .fetch_one(pool)
    .await?)
}

async fn resolved_spec_input(pool: &PgPool, task_id: Uuid) -> anyhow::Result<(String, String)> {
    let row = sqlx::query(
        "select resolved_spec->'input'->>'kind' as kind, resolved_spec->'input'->>'url' as url from tasks where id = $1",
    )
    .bind(task_id)
    .fetch_one(pool)
    .await?;
    Ok((row.try_get("kind")?, row.try_get("url")?))
}

async fn spawn_source_gateway_stub(
    relay_status: StatusCode,
) -> anyhow::Result<(String, Arc<tokio::sync::Mutex<Vec<Value>>>, JoinHandle<()>)> {
    use axum::{extract::State, routing::delete, routing::post};

    #[derive(Clone)]
    struct GatewayStubState {
        calls: Arc<tokio::sync::Mutex<Vec<Value>>>,
        relay_status: StatusCode,
    }

    async fn create_relay(
        State(state): State<GatewayStubState>,
        Json(payload): Json<Value>,
    ) -> impl axum::response::IntoResponse {
        state.calls.lock().await.push(payload.clone());
        if state.relay_status != StatusCode::OK {
            return (
                state.relay_status,
                Json(json!({"error": "upstream unavailable"})),
            );
        }
        let task_id = payload["task_id"].as_str().unwrap_or("missing");
        (
            StatusCode::OK,
            Json(json!({
                "relay_url": format!("http://media:18080/relay/{task_id}?token=test")
            })),
        )
    }

    async fn delete_relay() -> impl axum::response::IntoResponse {
        StatusCode::NO_CONTENT
    }

    let calls = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/api/relays", post(create_relay))
        .route("/api/relays/{task_id}", delete(delete_relay))
        .with_state(GatewayStubState {
            calls: calls.clone(),
            relay_status,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("source gateway stub should run");
    });
    Ok((format!("http://{addr}"), calls, handle))
}

async fn spawn_pending_prefetch_gateway_stub()
-> anyhow::Result<(String, Arc<AtomicUsize>, Arc<AtomicUsize>, JoinHandle<()>)> {
    use axum::{
        extract::Path,
        routing::{get, post},
    };

    #[derive(Clone)]
    struct PrefetchStubState {
        posts: Arc<AtomicUsize>,
        gets: Arc<AtomicUsize>,
    }

    async fn submit(
        axum::extract::State(state): axum::extract::State<PrefetchStubState>,
    ) -> Json<Value> {
        state.posts.fetch_add(1, Ordering::SeqCst);
        Json(json!({
            "status": "pending",
            "phase": "queued",
            "queue_position": 1,
            "poll_after_ms": 50,
            "time_slice_applied": false
        }))
    }

    async fn poll(
        axum::extract::State(state): axum::extract::State<PrefetchStubState>,
        Path(task_id): Path<Uuid>,
    ) -> Json<Value> {
        state.gets.fetch_add(1, Ordering::SeqCst);
        Json(json!({
            "status": "ready",
            "source_url": format!("imports/{task_id}/source.mp4"),
            "time_slice_applied": false
        }))
    }

    let posts = Arc::new(AtomicUsize::new(0));
    let gets = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/api/prefetch", post(submit))
        .route("/api/prefetch/{task_id}", get(poll))
        .with_state(PrefetchStubState {
            posts: posts.clone(),
            gets: gets.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("prefetch gateway stub should run");
    });
    Ok((format!("http://{addr}"), posts, gets, handle))
}

fn sample_immediate_task_spec() -> TaskSpec {
    let mut spec = sample_spec(InputKind::Rtsp, Some("rtsp://192.168.20.15/live"), None);
    spec.schedule.start_mode = Some(media_domain::StartMode::Immediate);
    spec
}

fn sample_registration(node_id: Uuid) -> AgentRegistration {
    AgentRegistration {
        node_id,
        node_name: format!("node-{node_id}"),
        agent_version: "test".to_string(),
        hostname: "worker-a".to_string(),
        labels: vec!["edge".to_string()],
        interfaces: vec!["eth0|192.168.20.2/24".to_string()],
        zlm_api_base: "http://127.0.0.1:65535".to_string(),
        zlm_api_secret: "secret".to_string(),
        agent_stream_addr: "http://stream.example".to_string(),
        agent_http_base_url: "http://stream.example:8081".to_string(),
        zlm_rtmp_port: 1935,
        zlm_rtsp_port: 554,
        network_mode: NetworkMode::Bridge,
        ffmpeg_bin: "ffmpeg".to_string(),
        ffprobe_bin: "ffprobe".to_string(),
        zlm_server_id: format!("zlm-{node_id}"),
        output_mount_relative_prefix_mp4: "output".to_string(),
        output_mount_relative_prefix_hls: "output".to_string(),
    }
}

fn control_plane_test_ca(now: DateTime<Utc>) -> AgentCertificateAuthority {
    let key = KeyPair::generate().expect("generate control-plane test CA key");
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.not_before =
        time::OffsetDateTime::from_unix_timestamp((now - chrono::Duration::days(1)).timestamp())
            .unwrap();
    params.not_after =
        time::OffsetDateTime::from_unix_timestamp((now + chrono::Duration::days(365)).timestamp())
            .unwrap();
    let certificate = params.self_signed(&key).expect("self-sign test CA");
    AgentCertificateAuthority::from_pem_for_test(certificate.pem(), key.serialize_pem(), now)
        .expect("load control-plane test CA")
}

async fn provision_authenticated_peer(
    pool: &PgPool,
    node_id: Uuid,
) -> anyhow::Result<AuthenticatedAgentPeer> {
    let now = Utc::now();
    let authority = control_plane_test_ca(now);
    let key = KeyPair::generate()?;
    let csr = CertificateParams::default()
        .serialize_request(&key)?
        .pem()?;
    let issued = authority.issue_agent_certificate(node_id, &csr, now)?;
    let (_, certificate) = parse_x509_pem(issued.certificate_pem.as_bytes())
        .map_err(|error| anyhow::anyhow!("parse test Agent certificate: {error}"))?;
    let peer = parse_authenticated_agent_peer(&certificate.contents, "192.0.2.25".parse()?, now)
        .map_err(|error| anyhow::anyhow!(error))?;

    sqlx::query(
        r#"
        insert into agent_identities (node_id, status, created_at, updated_at)
        values ($1, 'active', $2, $2)
        on conflict (node_id) do update
          set status = 'active', updated_at = excluded.updated_at,
              revoked_at = null, revocation_reason = null
        "#,
    )
    .bind(node_id)
    .bind(now)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        insert into agent_certificates (
          id, node_id, serial_number, fingerprint_sha256, public_key_sha256,
          certificate_pem, state, not_before, not_after, issued_at, activated_at, issued_via
        ) values ($1, $2, $3, $4, $5, 'test-certificate-pem', 'active', $6, $7, $8, $8, 'enrollment')
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(node_id)
    .bind(issued.serial_number)
    .bind(issued.fingerprint_sha256.as_slice())
    .bind(issued.public_key_sha256.as_slice())
    .bind(issued.not_before)
    .bind(issued.not_after)
    .bind(now)
    .execute(pool)
    .await?;
    let management_fingerprint = Sha256::digest(format!("management-{node_id}").as_bytes());
    let management_public_key = Sha256::digest(format!("management-key-{node_id}").as_bytes());
    sqlx::query(
        r#"
        insert into agent_management_certificates (
          id, node_id, serial_number, fingerprint_sha256, public_key_sha256,
          certificate_pem, state, not_before, not_after, issued_at, activated_at, issued_via
        ) values ($1, $2, $3, $4, $5, 'test-management-certificate-pem', 'active', $6, $7, $8, $8, 'enrollment')
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(node_id)
    .bind(Uuid::now_v7().simple().to_string())
    .bind(management_fingerprint.as_slice())
    .bind(management_public_key.as_slice())
    .bind(issued.not_before)
    .bind(issued.not_after)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(peer)
}

async fn provision_and_bootstrap_session(
    service: &ControlPlaneService,
    pool: &PgPool,
    node_id: Uuid,
    sender: mpsc::Sender<Result<CoreEnvelope, Status>>,
) -> anyhow::Result<Uuid> {
    let peer = provision_authenticated_peer(pool, node_id).await?;
    Ok(service
        .bootstrap_session(&sample_registration(node_id), &peer, sender)
        .await?)
}

#[tokio::test]
async fn management_target_provider_uses_authenticated_peer_and_register_limits()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository);
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let (sender, _receiver) = mpsc::channel(8);
    let session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            sender,
        )
        .await?;
    let target =
        crate::agent_management::AgentManagementTargetProvider::target(&service, node_id).await?;
    assert_eq!(target.node_id(), node_id);
    assert_eq!(target.session_id(), session_id);
    assert_eq!(target.peer_ip(), peer.peer_ip);
    assert_eq!(target.management_port(), 9443);
    assert_eq!(target.management_upload_max_bytes(), 64 * 1024 * 1024);
    assert_eq!(
        target.server_name(),
        format!("agent-{}.agent.streamserver.internal", node_id.simple())
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn management_target_provider_rejects_a_session_replaced_while_loading_pins()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let config = config_from_env()?;
    let database_url = test_database_url(&config.admin_url, &db.database_name)?;
    let blocking_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await?;
    let repository = Arc::new(TaskRepository::new(blocking_pool.clone()));
    let service = ControlPlaneService::new(repository);
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let (sender, _receiver) = mpsc::channel(8);
    service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            sender,
        )
        .await?;

    // Hold the only repository connection, then order the target lookup ahead
    // of our second sessions lock. Once we regain the lock, the lookup has
    // snapshotted the old session and is blocked loading its certificate pins.
    let sessions = service.sessions.lock().await;
    let held_connection = blocking_pool.acquire().await?;
    let lookup_service = service.clone();
    let lookup = tokio::spawn(async move {
        crate::agent_management::AgentManagementTargetProvider::target(&lookup_service, node_id)
            .await
    });
    tokio::task::yield_now().await;
    drop(sessions);
    let mut sessions = service.sessions.lock().await;
    sessions.get_mut(&node_id).expect("live session").session_id = Uuid::now_v7();
    drop(sessions);
    drop(held_connection);

    let outcome = timeout(std::time::Duration::from_secs(5), lookup)
        .await
        .expect("target lookup must finish")
        .expect("target lookup task must not panic");
    assert!(matches!(
        outcome,
        Err(crate::agent_management::AgentManagementError::TargetUnavailable)
    ));

    blocking_pool.close().await;
    db.cleanup().await?;
    Ok(())
}

fn control_plane_rotation_csr() -> anyhow::Result<String> {
    let key = KeyPair::generate()?;
    Ok(CertificateParams::default()
        .serialize_request(&key)?
        .pem()?)
}

#[tokio::test]
async fn expired_unactivated_rotation_resets_only_the_exact_transaction_and_allows_a_fresh_id()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let now = Utc::now();
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    sqlx::query("update agent_certificates set not_after = $1 where node_id = $2")
        .bind(now + chrono::Duration::days(20))
        .bind(node_id)
        .execute(&db.pool)
        .await?;
    sqlx::query("update agent_management_certificates set not_after = $1 where node_id = $2")
        .bind(now + chrono::Duration::days(20))
        .bind(node_id)
        .execute(&db.pool)
        .await?;
    let identity = AgentIdentityService::new(
        repository.clone(),
        control_plane_test_ca(now),
        AgentEnrollmentPublicConfig {
            control_plane_server_ca_pem: control_plane_test_ca(now).certificate_pem().to_string(),
            management_client_ca_pem: control_plane_test_ca(now).certificate_pem().to_string(),
            capability_jwt_public_key_pem: "capability-public-key-pem".to_string(),
            capability_jwt_kid: "capability-kid".to_string(),
        },
    );
    let service = ControlPlaneService::new(repository)
        .with_agent_identity_and_readiness(identity, Arc::new(SequenceReadinessProbe::new([])));
    let (sender, mut receiver) = mpsc::channel(8);
    let session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            sender,
        )
        .await?;
    let _probe_capabilities = receiver.recv().await.expect("bootstrap probe")?;

    let expired_rotation_id = Uuid::now_v7();
    let expired_request = media_rpc::control_plane::CertificateRotationRequest {
        rotation_id: expired_rotation_id.to_string(),
        control_csr_pem: control_plane_rotation_csr()?,
        management_csr_pem: control_plane_rotation_csr()?,
    };
    service
        .handle_payload(
            node_id,
            session_id,
            media_rpc::control_plane::agent_envelope::Payload::CertificateRotationRequest(
                expired_request.clone(),
            ),
        )
        .await?;
    assert!(matches!(
        receiver.recv().await.expect("initial bundle")?.payload,
        Some(media_rpc::control_plane::core_envelope::Payload::CertificateRotationBundle(_))
    ));
    sqlx::query(
        "update agent_certificate_rotations set authorized_at = $1, authorized_until = $2 where id = $3",
    )
        .bind(Utc::now() - chrono::Duration::minutes(2))
        .bind(Utc::now() - chrono::Duration::seconds(1))
        .bind(expired_rotation_id)
        .execute(&db.pool)
        .await?;

    service
        .handle_payload(
            node_id,
            session_id,
            media_rpc::control_plane::agent_envelope::Payload::CertificateRotationRequest(
                expired_request,
            ),
        )
        .await?;
    let reset = match receiver
        .recv()
        .await
        .expect("expired transaction reset")?
        .payload
        .expect("reset payload")
    {
        media_rpc::control_plane::core_envelope::Payload::CertificateRotationReset(reset) => reset,
        other => anyhow::bail!("unexpected expired transaction response: {other:?}"),
    };
    assert_eq!(reset.rotation_id, expired_rotation_id.to_string());
    assert_eq!(
        reset.reason,
        media_rpc::control_plane::CertificateRotationResetReason::Expired as i32
    );

    let fresh_rotation_id = Uuid::now_v7();
    service
        .handle_payload(
            node_id,
            session_id,
            media_rpc::control_plane::agent_envelope::Payload::CertificateRotationRequest(
                media_rpc::control_plane::CertificateRotationRequest {
                    rotation_id: fresh_rotation_id.to_string(),
                    control_csr_pem: control_plane_rotation_csr()?,
                    management_csr_pem: control_plane_rotation_csr()?,
                },
            ),
        )
        .await?;
    let fresh_bundle = match receiver
        .recv()
        .await
        .expect("fresh bundle")?
        .payload
        .expect("fresh bundle payload")
    {
        media_rpc::control_plane::core_envelope::Payload::CertificateRotationBundle(bundle) => {
            bundle
        }
        other => anyhow::bail!("unexpected fresh transaction response: {other:?}"),
    };
    assert_eq!(fresh_bundle.rotation_id, fresh_rotation_id.to_string());
    let expired_state: String =
        sqlx::query_scalar("select state from agent_certificate_rotations where id = $1")
            .bind(expired_rotation_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(expired_state, "expired");

    db.cleanup().await?;
    Ok(())
}

fn rotation_control_plane_service(
    repository: Arc<TaskRepository>,
    readiness: Arc<dyn crate::agent_management::AgentManagementReadinessProbe>,
    now: DateTime<Utc>,
) -> ControlPlaneService {
    let identity = AgentIdentityService::new(
        repository.clone(),
        control_plane_test_ca(now),
        AgentEnrollmentPublicConfig {
            control_plane_server_ca_pem: control_plane_test_ca(now).certificate_pem().to_string(),
            management_client_ca_pem: control_plane_test_ca(now).certificate_pem().to_string(),
            capability_jwt_public_key_pem: "capability-public-key-pem".to_string(),
            capability_jwt_kid: "capability-kid".to_string(),
        },
    );
    ControlPlaneService::new(repository).with_agent_identity_and_readiness(identity, readiness)
}

struct StagedControlRotation {
    rotation_id: Uuid,
    new_peer: AuthenticatedAgentPeer,
}

async fn stage_control_rotation(
    service: &ControlPlaneService,
    pool: &PgPool,
    node_id: Uuid,
    peer: &AuthenticatedAgentPeer,
) -> anyhow::Result<StagedControlRotation> {
    let now = Utc::now();
    sqlx::query("update agent_certificates set not_after = $1 where node_id = $2")
        .bind(now + chrono::Duration::days(20))
        .bind(node_id)
        .execute(pool)
        .await?;
    sqlx::query("update agent_management_certificates set not_after = $1 where node_id = $2")
        .bind(now + chrono::Duration::days(20))
        .bind(node_id)
        .execute(pool)
        .await?;
    let (old_sender, mut old_receiver) = mpsc::channel(8);
    let old_session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            peer,
            9443,
            64 * 1024 * 1024,
            old_sender,
        )
        .await?;
    old_receiver.recv().await.expect("old bootstrap probe")?;
    let rotation_id = Uuid::now_v7();
    service
        .handle_payload(
            node_id,
            old_session_id,
            media_rpc::control_plane::agent_envelope::Payload::CertificateRotationRequest(
                media_rpc::control_plane::CertificateRotationRequest {
                    rotation_id: rotation_id.to_string(),
                    control_csr_pem: control_plane_rotation_csr()?,
                    management_csr_pem: control_plane_rotation_csr()?,
                },
            ),
        )
        .await?;
    let bundle = match old_receiver
        .recv()
        .await
        .expect("rotation bundle response")?
        .payload
        .expect("rotation bundle payload")
    {
        media_rpc::control_plane::core_envelope::Payload::CertificateRotationBundle(bundle) => {
            bundle
        }
        other => anyhow::bail!("unexpected rotation response: {other:?}"),
    };
    let (_, new_control) = parse_x509_pem(bundle.control_certificate_pem.as_bytes())
        .map_err(|error| anyhow::anyhow!("parse rotated control leaf: {error}"))?;
    let new_peer = parse_authenticated_agent_peer(&new_control.contents, peer.peer_ip, Utc::now())
        .map_err(|error| anyhow::anyhow!(error))?;
    Ok(StagedControlRotation {
        rotation_id,
        new_peer,
    })
}

async fn receive_rotation_activation(
    receiver: &mut mpsc::Receiver<Result<CoreEnvelope, Status>>,
    rotation_id: Uuid,
    wait: std::time::Duration,
) -> anyhow::Result<ActivateCertificateRotation> {
    timeout(wait, async {
        loop {
            let envelope = receiver
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("control response stream closed"))??;
            if let Some(
                media_rpc::control_plane::core_envelope::Payload::ActivateCertificateRotation(
                    activation,
                ),
            ) = envelope.payload
            {
                if activation.rotation_id == rotation_id.to_string() {
                    return Ok(activation);
                }
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("rotation activation was not received before the deadline"))?
}

#[tokio::test]
async fn hung_rotation_readiness_does_not_block_bootstrap_heartbeats_or_spawn_duplicate_probes()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let now = Utc::now();
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let readiness = Arc::new(ControlledReadinessProbe::new());
    let service = rotation_control_plane_service(repository, readiness.clone(), now)
        .with_rotation_activation_retry_interval_for_test(std::time::Duration::from_secs(1));
    let staged = stage_control_rotation(&service, &db.pool, node_id, &peer).await?;
    let (new_sender, mut new_receiver) = mpsc::channel(8);
    let bootstrap_service = service.clone();
    let registration = sample_registration(node_id);
    let new_peer = staged.new_peer.clone();
    let mut bootstrap = tokio::spawn(async move {
        bootstrap_service
            .bootstrap_session_with_management(
                &registration,
                &new_peer,
                9443,
                64 * 1024 * 1024,
                new_sender,
            )
            .await
    });
    timeout(
        std::time::Duration::from_secs(1),
        readiness.first_started.acquire(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("management readiness probe did not start"))??
    .forget();

    let bootstrap_result = timeout(std::time::Duration::from_millis(150), &mut bootstrap).await;
    let bootstrap_was_nonblocking = bootstrap_result.is_ok();
    let new_session_id = if let Ok(result) = bootstrap_result {
        result??
    } else {
        readiness.first_release.add_permits(1);
        bootstrap.await??
    };
    let _bootstrap_probe = new_receiver.recv().await.expect("new bootstrap probe")?;

    let mut heartbeats_were_nonblocking = true;
    if bootstrap_was_nonblocking {
        for _ in 0..3 {
            let heartbeat = service.handle_payload(
                node_id,
                new_session_id,
                media_rpc::control_plane::agent_envelope::Payload::Heartbeat(
                    media_rpc::control_plane::Heartbeat {
                        node_time_ms: Utc::now().timestamp_millis(),
                        ..Default::default()
                    },
                ),
            );
            if timeout(std::time::Duration::from_millis(150), heartbeat)
                .await
                .is_err()
            {
                heartbeats_were_nonblocking = false;
                break;
            }
        }
    }
    let calls_while_hung = readiness.calls.load(std::sync::atomic::Ordering::SeqCst);
    readiness.first_release.add_permits(1);
    service.close_session(node_id, new_session_id).await;

    assert!(
        bootstrap_was_nonblocking && heartbeats_were_nonblocking && calls_while_hung == 1,
        "hung readiness must run as one background probe without blocking bootstrap or heartbeat: bootstrap_nonblocking={bootstrap_was_nonblocking}, heartbeats_nonblocking={heartbeats_were_nonblocking}, calls={calls_while_hung}"
    );
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn timed_out_rotation_readiness_retries_in_background_and_activates() -> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let now = Utc::now();
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let readiness = Arc::new(ControlledReadinessProbe::new());
    let service = rotation_control_plane_service(repository, readiness.clone(), now)
        .with_rotation_activation_retry_interval_for_test(std::time::Duration::from_millis(20));
    let staged = stage_control_rotation(&service, &db.pool, node_id, &peer).await?;
    let (new_sender, mut new_receiver) = mpsc::channel(8);
    let bootstrap_service = service.clone();
    let registration = sample_registration(node_id);
    let new_peer = staged.new_peer.clone();
    let mut bootstrap = tokio::spawn(async move {
        bootstrap_service
            .bootstrap_session_with_management(
                &registration,
                &new_peer,
                9443,
                64 * 1024 * 1024,
                new_sender,
            )
            .await
    });
    timeout(
        std::time::Duration::from_secs(1),
        readiness.first_started.acquire(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("management readiness probe did not start"))??
    .forget();
    let bootstrap_result = timeout(std::time::Duration::from_millis(150), &mut bootstrap).await;
    let bootstrap_was_nonblocking = bootstrap_result.is_ok();
    let new_session_id = if let Ok(result) = bootstrap_result {
        result??
    } else {
        // Unblock the current synchronous implementation so RED can clean up
        // without leaving a task and database behind.
        readiness.first_release.add_permits(1);
        bootstrap.await??
    };

    let activation_before_manual_release = if bootstrap_was_nonblocking {
        receive_rotation_activation(
            &mut new_receiver,
            staged.rotation_id,
            std::time::Duration::from_secs(4),
        )
        .await
        .is_ok()
    } else {
        false
    };
    let calls = readiness.calls.load(std::sync::atomic::Ordering::SeqCst);
    readiness.first_release.add_permits(1);
    service.close_session(node_id, new_session_id).await;

    assert!(
        bootstrap_was_nonblocking && activation_before_manual_release && calls >= 2,
        "a hung readiness probe must time out within three seconds and retry independently: bootstrap_nonblocking={bootstrap_was_nonblocking}, activation={activation_before_manual_release}, calls={calls}"
    );
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn slow_rotation_readiness_result_is_fenced_after_session_takeover() -> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let now = Utc::now();
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let readiness = Arc::new(ControlledReadinessProbe::new());
    let service = rotation_control_plane_service(repository, readiness.clone(), now)
        .with_rotation_activation_retry_interval_for_test(std::time::Duration::from_secs(1));
    let staged = stage_control_rotation(&service, &db.pool, node_id, &peer).await?;

    let (first_sender, mut first_receiver) = mpsc::channel(8);
    let first_service = service.clone();
    let registration = sample_registration(node_id);
    let first_peer = staged.new_peer.clone();
    let mut first_bootstrap = tokio::spawn(async move {
        first_service
            .bootstrap_session_with_management(
                &registration,
                &first_peer,
                9443,
                64 * 1024 * 1024,
                first_sender,
            )
            .await
    });
    timeout(
        std::time::Duration::from_secs(1),
        readiness.first_started.acquire(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("slow readiness probe did not start"))??
    .forget();
    let first_bootstrap_result =
        timeout(std::time::Duration::from_millis(150), &mut first_bootstrap).await;
    let first_bootstrap_was_nonblocking = first_bootstrap_result.is_ok();
    let first_session_id = if let Ok(result) = first_bootstrap_result {
        result??
    } else {
        service
            .sessions
            .lock()
            .await
            .get(&node_id)
            .expect("first rotation session is installed before readiness")
            .session_id
    };

    sqlx::query(
        r#"
        update agent_control_sessions
           set connected_at = clock_timestamp() - interval '31 seconds',
               last_activity_at = clock_timestamp() - interval '31 seconds',
               lease_expires_at = clock_timestamp() - interval '1 second'
         where node_id = $1 and session_id = $2
        "#,
    )
    .bind(node_id)
    .bind(first_session_id)
    .execute(&db.pool)
    .await?;

    let (second_sender, mut second_receiver) = mpsc::channel(8);
    let second_session_id = timeout(
        std::time::Duration::from_secs(1),
        service.bootstrap_session_with_management(
            &sample_registration(node_id),
            &staged.new_peer,
            9443,
            64 * 1024 * 1024,
            second_sender,
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("replacement session bootstrap was blocked"))??;
    assert_ne!(second_session_id, first_session_id);
    let activation = receive_rotation_activation(
        &mut second_receiver,
        staged.rotation_id,
        std::time::Duration::from_secs(1),
    )
    .await?;
    assert_eq!(activation.rotation_id, staged.rotation_id.to_string());

    readiness.first_release.add_permits(1);
    if !first_bootstrap_was_nonblocking {
        let _ = timeout(std::time::Duration::from_secs(1), &mut first_bootstrap).await;
    }
    let mut old_session_received_activation = false;
    while let Ok(item) = first_receiver.try_recv() {
        if let Ok(envelope) = item {
            if matches!(
                envelope.payload,
                Some(
                    media_rpc::control_plane::core_envelope::Payload::ActivateCertificateRotation(
                        _
                    )
                )
            ) {
                old_session_received_activation = true;
            }
        }
    }
    let (rotation_state, activated_by): (String, Option<Uuid>) = sqlx::query_as(
        "select state, management_activated_by_session_id from agent_certificate_rotations where id = $1",
    )
    .bind(staged.rotation_id)
    .fetch_one(&db.pool)
    .await?;
    let probed_sessions = readiness.sessions.lock().unwrap().clone();
    service.close_session(node_id, second_session_id).await;

    assert!(
        first_bootstrap_was_nonblocking
            && !old_session_received_activation
            && rotation_state == "management_activated"
            && activated_by == Some(second_session_id)
            && probed_sessions.first() == Some(&first_session_id)
            && probed_sessions.contains(&second_session_id),
        "only the current takeover session may activate a late readiness result: first_bootstrap_nonblocking={first_bootstrap_was_nonblocking}, old_activation={old_session_received_activation}, state={rotation_state}, activated_by={activated_by:?}, probes={probed_sessions:?}"
    );
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn control_plane_rotation_probes_management_activates_and_completes_ack() -> anyhow::Result<()>
{
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let now = Utc::now();
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    sqlx::query("update agent_certificates set not_after = $1 where node_id = $2")
        .bind(now + chrono::Duration::days(20))
        .bind(node_id)
        .execute(&db.pool)
        .await?;
    sqlx::query("update agent_management_certificates set not_after = $1 where node_id = $2")
        .bind(now + chrono::Duration::days(20))
        .bind(node_id)
        .execute(&db.pool)
        .await?;
    let authority = control_plane_test_ca(now);
    let readiness = Arc::new(SequenceReadinessProbe::new([false, true, true]));
    let identity = AgentIdentityService::new(
        repository.clone(),
        authority,
        AgentEnrollmentPublicConfig {
            control_plane_server_ca_pem: control_plane_test_ca(now).certificate_pem().to_string(),
            management_client_ca_pem: control_plane_test_ca(now).certificate_pem().to_string(),
            capability_jwt_public_key_pem: "capability-public-key-pem".to_string(),
            capability_jwt_kid: "capability-kid".to_string(),
        },
    );
    let service = ControlPlaneService::new(repository.clone())
        .with_agent_identity_and_readiness(identity, readiness.clone())
        .with_rotation_activation_retry_interval_for_test(std::time::Duration::from_millis(100));
    let (old_sender, mut old_receiver) = mpsc::channel(8);
    let old_session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            old_sender,
        )
        .await?;
    let _probe_capabilities = old_receiver.recv().await.expect("bootstrap probe")?;
    let rotation_id = Uuid::now_v7();
    service
        .handle_payload(
            node_id,
            old_session_id,
            media_rpc::control_plane::agent_envelope::Payload::CertificateRotationRequest(
                media_rpc::control_plane::CertificateRotationRequest {
                    rotation_id: rotation_id.to_string(),
                    control_csr_pem: control_plane_rotation_csr()?,
                    management_csr_pem: control_plane_rotation_csr()?,
                },
            ),
        )
        .await?;
    let bundle = match old_receiver
        .recv()
        .await
        .expect("rotation bundle response")?
        .payload
        .expect("bundle payload")
    {
        media_rpc::control_plane::core_envelope::Payload::CertificateRotationBundle(bundle) => {
            bundle
        }
        other => anyhow::bail!("unexpected rotation response: {other:?}"),
    };
    assert_eq!(bundle.rotation_id, rotation_id.to_string());
    assert!(bundle.expires_at_ms > Utc::now().timestamp_millis());
    assert!(bundle.expires_at_ms <= (Utc::now() + chrono::Duration::minutes(5)).timestamp_millis());

    let (_, new_control) = parse_x509_pem(bundle.control_certificate_pem.as_bytes())
        .map_err(|error| anyhow::anyhow!("parse rotated control leaf: {error}"))?;
    let new_peer = parse_authenticated_agent_peer(&new_control.contents, peer.peer_ip, Utc::now())
        .map_err(|error| anyhow::anyhow!(error))?;
    let (new_sender, mut new_receiver) = mpsc::channel(8);
    let new_session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &new_peer,
            9443,
            64 * 1024 * 1024,
            new_sender,
        )
        .await?;
    let _probe_capabilities = new_receiver.recv().await.expect("new bootstrap probe")?;
    timeout(std::time::Duration::from_secs(1), async {
        while readiness.calls.load(std::sync::atomic::Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("initial management readiness probe did not run"))?;
    assert!(
        timeout(std::time::Duration::from_millis(50), new_receiver.recv())
            .await
            .is_err(),
        "failed readiness must not activate the rotating management leaf"
    );
    timeout(
        std::time::Duration::from_millis(150),
        service.handle_payload(
            node_id,
            new_session_id,
            media_rpc::control_plane::agent_envelope::Payload::CapabilitySnapshot(
                media_rpc::control_plane::CapabilitySnapshot::default(),
            ),
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("ordinary payload was blocked by management readiness"))??;
    let activate_message = timeout(std::time::Duration::from_secs(1), new_receiver.recv())
        .await
        .map_err(|_| anyhow::anyhow!("rotation activation was not retried"))?
        .expect("rotation activation")?;
    let activate = match activate_message.payload.expect("activation payload") {
        media_rpc::control_plane::core_envelope::Payload::ActivateCertificateRotation(activate) => {
            activate
        }
        other => anyhow::bail!("unexpected post-takeover command: {other:?}"),
    };
    assert_eq!(activate.rotation_id, rotation_id.to_string());
    assert!(readiness.calls.load(std::sync::atomic::Ordering::SeqCst) >= 2);

    assert!(
        timeout(std::time::Duration::from_millis(20), new_receiver.recv())
            .await
            .is_err(),
        "Activate must be rate-limited before its retry interval"
    );
    let retried_activate = timeout(std::time::Duration::from_secs(1), new_receiver.recv())
        .await
        .map_err(|_| anyhow::anyhow!("lost rotation activation was not resent"))?
        .expect("retried rotation activation")?;
    assert!(matches!(
        retried_activate.payload,
        Some(
            media_rpc::control_plane::core_envelope::Payload::ActivateCertificateRotation(
                ref command
            )
        ) if command.rotation_id == rotation_id.to_string()
    ));
    assert!(readiness.calls.load(std::sync::atomic::Ordering::SeqCst) >= 3);

    service
        .handle_payload(
            node_id,
            new_session_id,
            media_rpc::control_plane::agent_envelope::Payload::CertificateRotationActivated(
                media_rpc::control_plane::CertificateRotationActivated {
                    rotation_id: rotation_id.to_string(),
                    activated_at_ms: Utc::now().timestamp_millis(),
                    control_fingerprint_sha256: bundle.control_fingerprint_sha256,
                    management_fingerprint_sha256: bundle.management_fingerprint_sha256,
                },
            ),
        )
        .await?;
    let rotation_state: String =
        sqlx::query_scalar("select state from agent_certificate_rotations where id = $1")
            .bind(rotation_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(rotation_state, "completed");

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn zlm_debug_round_trip_uses_typed_session_bound_request_and_bounded_json()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository);
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let (sender, mut receiver) = mpsc::channel(8);
    let session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            sender,
        )
        .await?;
    let _probe_capabilities = receiver.recv().await.expect("bootstrap probe")?;

    let caller = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .zlm_debug(
                    node_id,
                    ZlmDebugCommand::ListMedia {
                        schema: Some("rtsp".to_string()),
                        vhost: Some("__defaultVhost__".to_string()),
                        app: Some("live".to_string()),
                        stream: Some("camera".to_string()),
                    },
                )
                .await
        })
    };
    let request = match receiver
        .recv()
        .await
        .expect("ZLM request")?
        .payload
        .expect("ZLM request payload")
    {
        media_rpc::control_plane::core_envelope::Payload::ZlmDebugRequest(request) => request,
        other => anyhow::bail!("unexpected ZLM command: {other:?}"),
    };
    assert_eq!(
        request.operation,
        media_rpc::control_plane::ZlmDebugOperation::ListMedia as i32
    );
    assert!(Uuid::parse_str(&request.request_id).is_ok());
    assert!(!format!("{request:?}").contains("secret"));

    service
        .handle_payload(
            node_id,
            session_id,
            media_rpc::control_plane::agent_envelope::Payload::ZlmDebugResponse(
                media_rpc::control_plane::ZlmDebugResponse {
                    request_id: request.request_id,
                    operation: request.operation,
                    status: media_rpc::control_plane::ZlmDebugResponseStatus::Succeeded as i32,
                    payload: Some(
                        media_rpc::control_plane::zlm_debug_response::Payload::JsonPayload(
                            r#"{"code":0,"data":[{"stream":"camera"}]}"#.to_string(),
                        ),
                    ),
                    truncated: false,
                },
            ),
        )
        .await?;
    assert_eq!(
        caller.await??,
        ZlmDebugResult::Json(json!({"code": 0, "data": [{"stream": "camera"}]}))
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn zlm_hook_round_trip_uses_authenticated_session_node_and_returns_json_response()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let handler = Arc::new(CapturingZlmHookHandler::new(ZlmHookHandlerResponse {
        http_status: 200,
        body: json!({"code": 0, "msg": "ok"}),
    }));
    let service = ControlPlaneService::new(repository).with_zlm_hook_handler(handler.clone());
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let (sender, mut receiver) = mpsc::channel(8);
    let session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            sender,
        )
        .await?;
    let _probe_capabilities = receiver.recv().await.expect("bootstrap probe")?;
    let request_id = Uuid::now_v7();

    service
        .handle_payload(
            node_id,
            session_id,
            media_rpc::control_plane::agent_envelope::Payload::ZlmHookRequest(
                media_rpc::control_plane::ZlmHookRequest {
                    request_id: request_id.to_string(),
                    hook_name: "on_server_started".to_string(),
                    body_json: "{}".to_string(),
                },
            ),
        )
        .await?;

    let response = timeout(std::time::Duration::from_secs(1), receiver.recv())
        .await
        .map_err(|_| anyhow::anyhow!("hook response was not returned on the control stream"))?
        .expect("hook response envelope")?;
    let response = match response.payload {
        Some(media_rpc::control_plane::core_envelope::Payload::ZlmHookResponse(response)) => {
            response
        }
        other => panic!("expected ZLM hook response, got {other:?}"),
    };
    assert_eq!(response.request_id, request_id.to_string());
    assert_eq!(response.http_status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&response.body_json)?,
        json!({"code": 0, "msg": "ok"})
    );
    let calls = handler.calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].node_id, node_id);
    assert_eq!(calls[0].hook_name, "on_server_started");
    assert_eq!(calls[0].body, json!({}));

    db.cleanup().await?;
    Ok(())
}

#[test]
fn zlm_hook_request_accepts_only_the_fixed_hook_allowlist() {
    let request_id = Uuid::now_v7().to_string();
    for hook_name in [
        "on_publish",
        "on_rtp_server_timeout",
        "on_record_mp4",
        "on_record_ts",
        "on_record_hls",
        "on_stream_none_reader",
        "on_stream_not_found",
        "on_server_keepalive",
        "on_server_started",
    ] {
        assert!(
            parse_zlm_hook_request(RpcZlmHookRequest {
                request_id: request_id.clone(),
                hook_name: hook_name.to_string(),
                body_json: "{}".to_string(),
            })
            .is_ok(),
            "allowlisted hook {hook_name} was rejected"
        );
    }
    for hook_name in [
        "",
        " on_publish",
        "on_publish ",
        "ON_PUBLISH",
        "on_http_access",
        "../../on_publish",
    ] {
        assert!(
            parse_zlm_hook_request(RpcZlmHookRequest {
                request_id: request_id.clone(),
                hook_name: hook_name.to_string(),
                body_json: "{}".to_string(),
            })
            .is_err(),
            "unallowlisted hook {hook_name:?} was accepted"
        );
    }
}

#[test]
fn zlm_hook_request_rejects_remote_secrets_server_identity_and_unbounded_bodies() {
    let request_id = Uuid::now_v7().to_string();
    for body in [
        json!({"secret": "must-stay-on-agent"}).to_string(),
        json!({"nested": {"secret": "must-stay-on-agent"}}).to_string(),
        json!({"mediaServerId": "attacker-selected"}).to_string(),
        json!({"server_id": "attacker-selected"}).to_string(),
        json!({"serverId": "attacker-selected"}).to_string(),
        json!({"api.secret": "must-stay-on-agent"}).to_string(),
        json!({
            "hook.on_publish": "http://127.0.0.1/hook?secret=must-stay-on-agent"
        })
        .to_string(),
        "[]".to_string(),
        "null".to_string(),
        "{".to_string(),
        json!({"data": "x".repeat(MAX_ZLM_HOOK_BODY_BYTES)}).to_string(),
    ] {
        assert!(
            parse_zlm_hook_request(RpcZlmHookRequest {
                request_id: request_id.clone(),
                hook_name: "on_server_started".to_string(),
                body_json: body,
            })
            .is_err()
        );
    }
    assert!(
        parse_zlm_hook_request(RpcZlmHookRequest {
            request_id: request_id.clone(),
            hook_name: "on_server_keepalive".to_string(),
            body_json: json!({"nested": {"safe": true}}).to_string(),
        })
        .is_ok()
    );
    assert!(
        parse_zlm_hook_request(RpcZlmHookRequest {
            request_id: request_id.clone(),
            hook_name: "on_server_started".to_string(),
            body_json: "{}".to_string(),
        })
        .is_ok()
    );
    assert!(
        parse_zlm_hook_request(RpcZlmHookRequest {
            request_id,
            hook_name: "on_server_started".to_string(),
            body_json: json!({"port": 1935}).to_string(),
        })
        .is_err(),
        "on_server_started must not carry the ZLM mINI configuration snapshot"
    );
}

#[tokio::test]
async fn invalid_zlm_hook_returns_bad_request_without_terminating_the_control_session()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let handler = Arc::new(CapturingZlmHookHandler::new(ZlmHookHandlerResponse {
        http_status: 200,
        body: json!({"code": 0}),
    }));
    let service = ControlPlaneService::new(repository).with_zlm_hook_handler(handler.clone());
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let (sender, mut receiver) = mpsc::channel(8);
    let session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            sender,
        )
        .await?;
    let _probe_capabilities = receiver.recv().await.expect("bootstrap probe")?;

    let invalid_id = Uuid::now_v7();
    service
        .handle_payload(
            node_id,
            session_id,
            media_rpc::control_plane::agent_envelope::Payload::ZlmHookRequest(RpcZlmHookRequest {
                request_id: invalid_id.to_string(),
                hook_name: "on_server_started".to_string(),
                body_json: json!({"secret": "must-not-cross-control-stream"}).to_string(),
            }),
        )
        .await
        .expect("invalid hook must not terminate the stream");
    let invalid = timeout(std::time::Duration::from_secs(1), receiver.recv())
        .await
        .map_err(|_| anyhow::anyhow!("invalid hook response timed out"))?
        .expect("invalid hook response")?;
    let invalid = match invalid.payload {
        Some(media_rpc::control_plane::core_envelope::Payload::ZlmHookResponse(response)) => {
            response
        }
        other => panic!("expected invalid hook response, got {other:?}"),
    };
    assert_eq!(invalid.request_id, invalid_id.to_string());
    assert_eq!(invalid.http_status, 400);
    assert!(serde_json::from_str::<Value>(&invalid.body_json).is_ok());
    assert!(handler.calls.lock().await.is_empty());

    let valid_id = Uuid::now_v7();
    service
        .handle_payload(
            node_id,
            session_id,
            media_rpc::control_plane::agent_envelope::Payload::ZlmHookRequest(RpcZlmHookRequest {
                request_id: valid_id.to_string(),
                hook_name: "on_server_started".to_string(),
                body_json: "{}".to_string(),
            }),
        )
        .await
        .expect("the session must accept a valid hook after a bad request");
    let valid = timeout(std::time::Duration::from_secs(1), receiver.recv())
        .await
        .map_err(|_| anyhow::anyhow!("valid hook response timed out"))?
        .expect("valid hook response")?;
    assert!(matches!(
        valid.payload,
        Some(
            media_rpc::control_plane::core_envelope::Payload::ZlmHookResponse(ZlmHookResponse {
                http_status: 200,
                ..
            })
        )
    ));
    assert_eq!(handler.calls.lock().await.len(), 1);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn duplicate_zlm_hook_request_id_replays_the_cached_response_without_reprocessing()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let handler = Arc::new(CapturingZlmHookHandler::new(ZlmHookHandlerResponse {
        http_status: 200,
        body: json!({"code": 0, "cached": true}),
    }));
    let service = ControlPlaneService::new(repository).with_zlm_hook_handler(handler.clone());
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let (sender, mut receiver) = mpsc::channel(8);
    let session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            sender,
        )
        .await?;
    let _probe_capabilities = receiver.recv().await.expect("bootstrap probe")?;
    let request = RpcZlmHookRequest {
        request_id: Uuid::now_v7().to_string(),
        hook_name: "on_server_started".to_string(),
        body_json: "{}".to_string(),
    };

    for _ in 0..2 {
        service
            .handle_payload(
                node_id,
                session_id,
                media_rpc::control_plane::agent_envelope::Payload::ZlmHookRequest(request.clone()),
            )
            .await?;
        let response = timeout(std::time::Duration::from_secs(1), receiver.recv())
            .await
            .map_err(|_| anyhow::anyhow!("idempotent hook response timed out"))?
            .expect("idempotent hook response")?;
        assert!(matches!(
            response.payload,
            Some(
                media_rpc::control_plane::core_envelope::Payload::ZlmHookResponse(
                    ZlmHookResponse {
                        http_status: 200,
                        ..
                    }
                )
            )
        ));
    }
    assert_eq!(handler.calls.lock().await.len(), 1);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn zlm_hook_processing_is_nonblocking_and_bounded_to_four_active_requests_per_session()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let handler = Arc::new(BlockingZlmHookHandler::new());
    let service = ControlPlaneService::new(repository).with_zlm_hook_handler(handler.clone());
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let (sender, mut receiver) = mpsc::channel(16);
    let session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            sender,
        )
        .await?;
    let _probe_capabilities = receiver.recv().await.expect("bootstrap probe")?;

    for sequence in 0..4 {
        timeout(
            std::time::Duration::from_millis(250),
            service.handle_payload(
                node_id,
                session_id,
                media_rpc::control_plane::agent_envelope::Payload::ZlmHookRequest(
                    RpcZlmHookRequest {
                        request_id: Uuid::now_v7().to_string(),
                        hook_name: "on_server_keepalive".to_string(),
                        body_json: json!({"sequence": sequence}).to_string(),
                    },
                ),
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("slow hook blocked the control stream"))??;
    }
    timeout(std::time::Duration::from_secs(1), async {
        while handler.started.load(std::sync::atomic::Ordering::SeqCst) < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("four hook workers did not start"))?;

    service
        .handle_payload(
            node_id,
            session_id,
            media_rpc::control_plane::agent_envelope::Payload::ZlmHookRequest(RpcZlmHookRequest {
                request_id: Uuid::now_v7().to_string(),
                hook_name: "on_server_keepalive".to_string(),
                body_json: json!({"sequence": 5}).to_string(),
            }),
        )
        .await?;
    let busy = timeout(std::time::Duration::from_secs(1), receiver.recv())
        .await
        .map_err(|_| anyhow::anyhow!("bounded hook admission did not return a response"))?
        .expect("busy hook response")?;
    assert!(matches!(
        busy.payload,
        Some(
            media_rpc::control_plane::core_envelope::Payload::ZlmHookResponse(ZlmHookResponse {
                http_status: 429,
                ..
            })
        )
    ));

    timeout(
        std::time::Duration::from_secs(1),
        service.handle_payload(
            node_id,
            session_id,
            media_rpc::control_plane::agent_envelope::Payload::CapabilitySnapshot(
                media_rpc::control_plane::CapabilitySnapshot::default(),
            ),
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("active hooks blocked ordinary control messages"))??;

    handler.release.add_permits(4);
    for _ in 0..4 {
        let completed = timeout(std::time::Duration::from_secs(1), receiver.recv())
            .await
            .map_err(|_| anyhow::anyhow!("completed hook response timed out"))?
            .expect("completed hook response")?;
        assert!(matches!(
            completed.payload,
            Some(
                media_rpc::control_plane::core_envelope::Payload::ZlmHookResponse(
                    ZlmHookResponse {
                        http_status: 200,
                        ..
                    }
                )
            )
        ));
    }
    assert_eq!(handler.started.load(std::sync::atomic::Ordering::SeqCst), 4);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn zlm_hook_worker_times_out_in_three_seconds_and_releases_its_active_slot()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let handler = Arc::new(BlockingZlmHookHandler::new());
    let service = ControlPlaneService::new(repository).with_zlm_hook_handler(handler.clone());
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let (sender, mut receiver) = mpsc::channel(8);
    let session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            sender,
        )
        .await?;
    let _probe_capabilities = receiver.recv().await.expect("bootstrap probe")?;

    let request = |sequence| RpcZlmHookRequest {
        request_id: Uuid::now_v7().to_string(),
        hook_name: "on_server_keepalive".to_string(),
        body_json: json!({"sequence": sequence}).to_string(),
    };
    service
        .handle_payload(
            node_id,
            session_id,
            media_rpc::control_plane::agent_envelope::Payload::ZlmHookRequest(request(1)),
        )
        .await?;
    let timed_out = timeout(std::time::Duration::from_secs(4), receiver.recv())
        .await
        .map_err(|_| anyhow::anyhow!("hung hook did not reach the three-second deadline"))?
        .expect("timeout hook response")?;
    assert!(matches!(
        timed_out.payload,
        Some(
            media_rpc::control_plane::core_envelope::Payload::ZlmHookResponse(ZlmHookResponse {
                http_status: 504,
                ..
            })
        )
    ));

    handler.release.add_permits(1);
    service
        .handle_payload(
            node_id,
            session_id,
            media_rpc::control_plane::agent_envelope::Payload::ZlmHookRequest(request(2)),
        )
        .await?;
    let recovered = timeout(std::time::Duration::from_secs(1), receiver.recv())
        .await
        .map_err(|_| anyhow::anyhow!("timed-out hook did not release its active slot"))?
        .expect("post-timeout hook response")?;
    assert!(matches!(
        recovered.payload,
        Some(
            media_rpc::control_plane::core_envelope::Payload::ZlmHookResponse(ZlmHookResponse {
                http_status: 200,
                ..
            })
        )
    ));

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn zlm_hook_takeover_after_initial_check_fences_old_business_side_effects()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let handler = Arc::new(AuditedCoreZlmHookHandler::new(repository.clone()));
    let service = ControlPlaneService::new(repository).with_zlm_hook_handler(handler.clone());
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let (old_sender, mut old_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let old_session_id = service
        .bootstrap_session(&sample_registration(node_id), &peer, old_sender)
        .await?;
    old_receiver
        .recv()
        .await
        .expect("old session bootstrap probe")?;

    let initial_activity = DateTime::<Utc>::UNIX_EPOCH;
    service
        .sessions
        .lock()
        .await
        .get_mut(&node_id)
        .expect("old session remains in memory")
        .identity
        .last_activity_at = initial_activity;

    // This mutex is the deterministic admission barrier: the old payload has
    // passed both in-memory current-session checks once last_activity_at moves,
    // but cannot yet create its per-session hook state.
    let hook_admission_barrier = service.zlm_hook_sessions.lock().await;
    let request_id = Uuid::now_v7();
    let old_service = service.clone();
    let mut old_payload = tokio::spawn(async move {
        old_service
            .handle_payload(
                node_id,
                old_session_id,
                media_rpc::control_plane::agent_envelope::Payload::ZlmHookRequest(
                    media_rpc::control_plane::ZlmHookRequest {
                        request_id: request_id.to_string(),
                        hook_name: "on_server_started".to_string(),
                        body_json: "{}".to_string(),
                    },
                ),
            )
            .await
    });
    timeout(std::time::Duration::from_secs(1), async {
        loop {
            let activity = service
                .sessions
                .lock()
                .await
                .get(&node_id)
                .expect("old session remains current before takeover")
                .identity
                .last_activity_at;
            if activity != initial_activity {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("old hook payload did not reach the admission barrier"))?;

    // The old payload has already observed a healthy durable lease. Expire it
    // only now so the replacement claim wins in the exact TOCTOU window.
    sqlx::query(
        r#"
        update agent_control_sessions
           set connected_at = clock_timestamp() - interval '31 seconds',
               last_activity_at = clock_timestamp() - interval '31 seconds',
               lease_expires_at = clock_timestamp() - interval '1 second'
         where node_id = $1 and session_id = $2
        "#,
    )
    .bind(node_id)
    .bind(old_session_id)
    .execute(&db.pool)
    .await?;

    let (new_sender, mut new_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let takeover_service = service.clone();
    let registration = sample_registration(node_id);
    let takeover_peer = peer.clone();
    let mut takeover = tokio::spawn(async move {
        takeover_service
            .bootstrap_session(&registration, &takeover_peer, new_sender)
            .await
    });
    timeout(std::time::Duration::from_secs(1), async {
        loop {
            if service
                .sessions
                .lock()
                .await
                .get(&node_id)
                .is_some_and(|session| session.session_id != old_session_id)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("takeover did not replace the in-memory session"))?;

    // The old admission waiter is queued before takeover cleanup. Releasing
    // the barrier reproduces the audited race where it can recreate state
    // after the new durable session has already won.
    drop(hook_admission_barrier);
    let new_session_id = timeout(std::time::Duration::from_secs(1), &mut takeover)
        .await
        .map_err(|_| anyhow::anyhow!("takeover did not finish after releasing the barrier"))???;
    assert_ne!(new_session_id, old_session_id);
    new_receiver
        .recv()
        .await
        .expect("new session bootstrap probe")?;
    let _ = timeout(std::time::Duration::from_secs(1), &mut old_payload).await;

    let stale_handler_completed = timeout(
        std::time::Duration::from_millis(500),
        handler.completed.acquire(),
    )
    .await
    .is_ok();
    let persisted: i64 = sqlx::query_scalar(
        "select count(*) from hook_events where server_id = $1 and hook_name = 'on_server_started'",
    )
    .bind(node_id.to_string())
    .fetch_one(&db.pool)
    .await?;
    let calls = handler.calls.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        !stale_handler_completed && calls == 0 && persisted == 0,
        "the fenced old session must not execute hook business logic or persist it: completed={stale_handler_completed}, calls={calls}, rows={persisted}"
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn zlm_debug_waiter_is_timed_out_bounded_and_canceled_with_its_session() -> anyhow::Result<()>
{
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository)
        .with_zlm_request_timeout_for_test(std::time::Duration::from_millis(25));
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let (sender, mut receiver) = mpsc::channel(8);
    let session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            sender,
        )
        .await?;
    let _probe_capabilities = receiver.recv().await.expect("bootstrap probe")?;

    let timed_out = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .zlm_debug(node_id, ZlmDebugCommand::GetStatistic)
                .await
        })
    };
    let _request = receiver.recv().await.expect("timed request")?;
    assert_eq!(timed_out.await?, Err(ZlmDebugCallError::DeadlineExceeded));

    let canceled = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .zlm_debug(node_id, ZlmDebugCommand::ListSessions)
                .await
        })
    };
    let _request = receiver.recv().await.expect("cancelable request")?;
    service.close_session(node_id, session_id).await;
    assert_eq!(canceled.await?, Err(ZlmDebugCallError::Disconnected));
    assert_eq!(service.pending_zlm_waiter_count().await, 0);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn aborted_zlm_debug_callers_release_pending_slots_without_exhausting_the_session()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository);
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let (sender, mut receiver) = mpsc::channel(8);
    let session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            sender,
        )
        .await?;
    let _probe_capabilities = receiver.recv().await.expect("bootstrap probe")?;

    for _ in 0..MAX_PENDING_ZLM_PER_SESSION {
        let caller = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .zlm_debug(node_id, ZlmDebugCommand::GetStatistic)
                    .await
            })
        };
        let _request = timeout(std::time::Duration::from_secs(1), receiver.recv())
            .await
            .map_err(|_| anyhow::anyhow!("aborted caller did not publish its ZLM request"))?
            .expect("abortable ZLM request")?;
        caller.abort();
        assert!(
            caller
                .await
                .expect_err("aborted caller unexpectedly completed")
                .is_cancelled()
        );
    }

    assert_eq!(service.pending_zlm_waiter_count().await, 0);

    let final_caller = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .zlm_debug(node_id, ZlmDebugCommand::GetStatistic)
                .await
        })
    };
    let _request = timeout(std::time::Duration::from_secs(1), receiver.recv())
        .await
        .map_err(|_| anyhow::anyhow!("canceled callers exhausted the session ZLM slots"))?
        .expect("post-cancel ZLM request")?;
    service.close_session(node_id, session_id).await;
    service.close_session(node_id, session_id).await;
    assert_eq!(final_caller.await?, Err(ZlmDebugCallError::Disconnected));
    assert_eq!(service.pending_zlm_waiter_count().await, 0);

    db.cleanup().await?;
    Ok(())
}

#[test]
fn zlm_debug_response_rejects_oversized_or_mismatched_payloads() {
    use media_rpc::control_plane::{
        ZlmDebugResponseStatus, ZlmSnapshotPayload, zlm_debug_response::Payload,
    };

    let oversized_json = parse_zlm_debug_response(
        ZlmDebugResponse {
            request_id: Uuid::now_v7().to_string(),
            operation: ZlmDebugOperation::GetStatistic as i32,
            status: ZlmDebugResponseStatus::Succeeded as i32,
            payload: Some(Payload::JsonPayload(
                "x".repeat(MAX_ZLM_JSON_RESPONSE_BYTES + 1),
            )),
            truncated: false,
        },
        false,
    );
    assert_eq!(oversized_json, Err(ZlmDebugCallError::ResponseTooLarge));

    let oversized_snapshot = parse_zlm_debug_response(
        ZlmDebugResponse {
            request_id: Uuid::now_v7().to_string(),
            operation: ZlmDebugOperation::Snapshot as i32,
            status: ZlmDebugResponseStatus::Succeeded as i32,
            payload: Some(Payload::Snapshot(ZlmSnapshotPayload {
                content_type: "image/jpeg".to_string(),
                data: vec![0; MAX_ZLM_SNAPSHOT_RESPONSE_BYTES + 1],
            })),
            truncated: false,
        },
        true,
    );
    assert_eq!(oversized_snapshot, Err(ZlmDebugCallError::ResponseTooLarge));

    let wrong_payload_kind = parse_zlm_debug_response(
        ZlmDebugResponse {
            request_id: Uuid::now_v7().to_string(),
            operation: ZlmDebugOperation::Snapshot as i32,
            status: ZlmDebugResponseStatus::Succeeded as i32,
            payload: Some(Payload::JsonPayload("{}".to_string())),
            truncated: false,
        },
        true,
    );
    assert_eq!(
        wrong_payload_kind,
        Err(ZlmDebugCallError::ProtocolViolation)
    );
}

#[tokio::test]
async fn zlm_debug_enforces_per_session_concurrency_and_preserves_cross_session_fencing()
-> anyhow::Result<()> {
    let Some(db) = TestDatabase::maybe_new(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository);
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;
    let (sender, mut receiver) = mpsc::channel(8);
    let session_id = service
        .bootstrap_session_with_management(
            &sample_registration(node_id),
            &peer,
            9443,
            64 * 1024 * 1024,
            sender,
        )
        .await?;
    let _probe_capabilities = receiver.recv().await.expect("bootstrap probe")?;

    let mut callers = Vec::new();
    let mut requests = Vec::new();
    for _ in 0..MAX_PENDING_ZLM_PER_SESSION {
        let service = service.clone();
        callers.push(tokio::spawn(async move {
            service
                .zlm_debug(node_id, ZlmDebugCommand::GetStatistic)
                .await
        }));
        let request = match receiver
            .recv()
            .await
            .expect("bounded ZLM request")?
            .payload
            .expect("bounded request payload")
        {
            media_rpc::control_plane::core_envelope::Payload::ZlmDebugRequest(request) => request,
            other => anyhow::bail!("unexpected bounded command: {other:?}"),
        };
        requests.push(request);
    }
    assert_eq!(
        service.pending_zlm_waiter_count().await,
        MAX_PENDING_ZLM_PER_SESSION
    );
    assert_eq!(
        service
            .zlm_debug(node_id, ZlmDebugCommand::GetStatistic)
            .await,
        Err(ZlmDebugCallError::Busy)
    );

    let first = requests.remove(0);
    let wrong_session = Uuid::now_v7();
    let error = service
        .handle_zlm_debug_response(
            node_id,
            wrong_session,
            ZlmDebugResponse {
                request_id: first.request_id,
                operation: first.operation,
                status: ZlmDebugResponseStatus::Succeeded as i32,
                payload: Some(
                    media_rpc::control_plane::zlm_debug_response::Payload::JsonPayload(
                        "{}".to_string(),
                    ),
                ),
                truncated: false,
            },
        )
        .await
        .expect_err("another session cannot resolve a pending waiter");
    assert_eq!(error.code(), tonic::Code::PermissionDenied);
    assert_eq!(
        service.pending_zlm_waiter_count().await,
        MAX_PENDING_ZLM_PER_SESSION
    );

    service.close_session(node_id, session_id).await;
    for caller in callers {
        assert_eq!(caller.await?, Err(ZlmDebugCallError::Disconnected));
    }
    assert_eq!(service.pending_zlm_waiter_count().await, 0);

    db.cleanup().await?;
    Ok(())
}

fn runtime_slot_load_for_usage(
    source_mode: SourceMode,
    running_tasks: u32,
    starting_tasks: u32,
    stopping_tasks: u32,
    orphaned_tasks: u32,
    slot_usage: f64,
) -> RuntimeSlotLoad {
    let occupied = running_tasks
        .saturating_add(starting_tasks)
        .saturating_add(stopping_tasks)
        .saturating_add(orphaned_tasks);
    let max_runtime_slots = if occupied == 0 || slot_usage <= 0.0 {
        0
    } else {
        ((occupied as f64 / slot_usage).ceil() as u32).max(1)
    };
    RuntimeSlotLoad {
        source_mode,
        max_runtime_slots,
        running_tasks,
        starting_tasks,
        stopping_tasks,
        orphaned_tasks,
        slot_usage,
    }
}

fn live_slot_load(
    running_tasks: u32,
    starting_tasks: u32,
    stopping_tasks: u32,
    orphaned_tasks: u32,
    slot_usage: f64,
) -> RuntimeSlotLoad {
    runtime_slot_load_for_usage(
        SourceMode::Live,
        running_tasks,
        starting_tasks,
        stopping_tasks,
        orphaned_tasks,
        slot_usage,
    )
}

fn online_live_session_load(running_tasks: u32, slot_usage: f64) -> SessionLoad {
    SessionLoad {
        running_tasks,
        runtime_slot_loads: vec![live_slot_load(running_tasks, 0, 0, 0, slot_usage)],
        zlm_alive: true,
        ffmpeg_alive: true,
        ..SessionLoad::default()
    }
}

fn sample_heartbeat(running_tasks: u32, slot_usage: f64) -> HeartbeatSnapshot {
    HeartbeatSnapshot {
        node_time: Utc::now(),
        cpu_percent: 0.0,
        mem_percent: 0.0,
        disk_percent: 0.0,
        upload_disk_total_bytes: 100,
        upload_disk_available_bytes: 80,
        upload_disk_used_percent: 20.0,
        running_tasks,
        starting_tasks: 0,
        stopping_tasks: 0,
        orphaned_tasks: 0,
        runtime_slot_loads: vec![live_slot_load(running_tasks, 0, 0, 0, slot_usage)],
        zlm_alive: true,
        ffmpeg_alive: true,
        artifact_cleanup_blocked: false,
        artifact_cleanup_block_reason: None,
        gpu_runtime: Vec::new(),
    }
}

fn sample_heartbeat_with_states(
    running_tasks: u32,
    starting_tasks: u32,
    stopping_tasks: u32,
    orphaned_tasks: u32,
    slot_usage: f64,
) -> HeartbeatSnapshot {
    HeartbeatSnapshot {
        node_time: Utc::now(),
        cpu_percent: 0.0,
        mem_percent: 0.0,
        disk_percent: 0.0,
        upload_disk_total_bytes: 100,
        upload_disk_available_bytes: 80,
        upload_disk_used_percent: 20.0,
        running_tasks,
        starting_tasks,
        stopping_tasks,
        orphaned_tasks,
        runtime_slot_loads: vec![live_slot_load(
            running_tasks,
            starting_tasks,
            stopping_tasks,
            orphaned_tasks,
            slot_usage,
        )],
        zlm_alive: true,
        ffmpeg_alive: true,
        artifact_cleanup_blocked: false,
        artifact_cleanup_block_reason: None,
        gpu_runtime: Vec::new(),
    }
}

fn sample_gpu_runtime(gpu_util: f64, encoder_util: f64, decoder_util: f64) -> Vec<GpuRuntimeStats> {
    vec![GpuRuntimeStats {
        index: 0,
        gpu_util_percent: gpu_util,
        memory_used_mb: 1024,
        memory_total_mb: 8192,
        encoder_util_percent: encoder_util,
        decoder_util_percent: decoder_util,
    }]
}

fn sample_gpu_capabilities() -> SessionCapabilities {
    SessionCapabilities {
        gpu_devices: vec![GpuDeviceInfo {
            index: 0,
            uuid: "GPU-00000000".to_string(),
            name: "NVIDIA Test GPU".to_string(),
            memory_total_mb: 8192,
        }],
    }
}

#[test]
fn task_source_affinity_uses_source_url_instead_of_local_interface_ip() {
    let spec = sample_spec(
        InputKind::Rtsp,
        Some("rtsp://10.10.10.20/live"),
        Some("192.168.10.8"),
    );

    assert_eq!(
        task_source_affinity_ip(&spec),
        Some(IpAddr::V4(Ipv4Addr::new(10, 10, 10, 20)))
    );
}

#[test]
fn task_source_affinity_uses_literal_url_host() {
    let spec = sample_spec(InputKind::Rtsp, Some("rtsp://192.168.20.15/live"), None);

    assert_eq!(
        task_source_affinity_ip(&spec),
        Some(IpAddr::V4(Ipv4Addr::new(192, 168, 20, 15)))
    );
}

#[test]
fn task_source_affinity_ignores_domain_hosts() {
    let spec = sample_spec(InputKind::Rtsp, Some("rtsp://camera.example/live"), None);

    assert_eq!(task_source_affinity_ip(&spec), None);
}

#[test]
fn parse_interface_network_accepts_named_cidr() {
    let network = parse_interface_network("eth0|192.168.10.7/24").expect("cidr should parse");

    assert_eq!(
        network,
        InterfaceNetwork {
            ip: IpAddr::V4(Ipv4Addr::new(192, 168, 10, 7)),
            prefix: 24,
        }
    );
}

#[test]
fn compare_dispatch_score_prefers_same_subnet_then_lower_load() {
    let better = DispatchScore {
        same_subnet: true,
        gpu_headroom: None,
        slot_usage: 0.9,
        occupied_tasks: 8,
        node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
    };
    let worse = DispatchScore {
        same_subnet: false,
        gpu_headroom: None,
        slot_usage: 0.1,
        occupied_tasks: 1,
        node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
    };

    assert_eq!(compare_dispatch_score(better, worse), CmpOrdering::Less);

    let lighter = DispatchScore {
        same_subnet: true,
        gpu_headroom: None,
        slot_usage: 0.2,
        occupied_tasks: 5,
        node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
    };

    assert_eq!(compare_dispatch_score(lighter, better), CmpOrdering::Less);
}

#[test]
fn compare_dispatch_score_falls_back_to_load_and_occupied_tasks() {
    let lighter = DispatchScore {
        same_subnet: false,
        gpu_headroom: None,
        slot_usage: 0.2,
        occupied_tasks: 3,
        node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
    };
    let heavier = DispatchScore {
        same_subnet: false,
        gpu_headroom: None,
        slot_usage: 0.8,
        occupied_tasks: 1,
        node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000004").unwrap(),
    };
    let same_load_more_tasks = DispatchScore {
        same_subnet: false,
        gpu_headroom: None,
        slot_usage: 0.2,
        occupied_tasks: 6,
        node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000005").unwrap(),
    };

    assert_eq!(compare_dispatch_score(lighter, heavier), CmpOrdering::Less);
    assert_eq!(
        compare_dispatch_score(lighter, same_load_more_tasks),
        CmpOrdering::Less
    );
}

#[test]
fn dispatch_reservation_waits_for_active_counts_instead_of_start_events() {
    assert!(!event_releases_dispatch_reservation("accepted"));
    assert!(!event_releases_dispatch_reservation("starting"));
    assert!(!event_releases_dispatch_reservation("recovering"));
    assert!(!event_releases_dispatch_reservation("running"));
    assert!(event_releases_dispatch_reservation("start_rejected"));
    assert!(event_releases_dispatch_reservation("failed"));
}

#[test]
fn effective_slot_usage_counts_starting_tasks_and_reservations() {
    let load = SessionLoad {
        running_tasks: 1,
        starting_tasks: 1,
        runtime_slot_loads: vec![live_slot_load(1, 1, 0, 0, 0.5)],
        ..SessionLoad::default()
    };

    assert_eq!(effective_occupied_tasks(&load, SourceMode::Live, 0), 2);
    assert_eq!(effective_slot_usage(&load, SourceMode::Live, 2), 1.0);
    assert!(session_is_saturated(&load, SourceMode::Live, 2));
}

#[test]
fn update_session_load_uses_starting_tasks_to_release_reservations() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    runtime.block_on(async {
        let service = ControlPlaneService::new(Arc::new(TaskRepository::new(
            PgPoolOptions::new()
                .connect_lazy("postgresql://postgres@127.0.0.1/postgres")
                .expect("lazy pool should build"),
        )));
        let node_id = Uuid::now_v7();
        service.sessions.lock().await.insert(
            node_id,
            SessionHandle {
                session_id: Uuid::from_u128(1),
                generation: Arc::default(),
                sender: mpsc::channel(CONTROL_STREAM_BUFFER).0,
                registration: sample_registration(node_id),
                identity: synthetic_session_identity(),
                capabilities: SessionCapabilities::default(),
                load: SessionLoad::default(),
                reservations: VecDeque::from([
                    DispatchReservation {
                        task_id: Uuid::now_v7(),
                        source_mode: SourceMode::Live,
                    },
                    DispatchReservation {
                        task_id: Uuid::now_v7(),
                        source_mode: SourceMode::Live,
                    },
                ]),
                management_port: 9443,
                management_upload_max_bytes: 64 * 1024 * 1024,
            },
        );

        service
            .update_session_load(
                node_id,
                Uuid::from_u128(1),
                &sample_heartbeat_with_states(0, 2, 0, 0, 0.5),
            )
            .await
            .expect("load update should succeed");

        let sessions = service.sessions.lock().await;
        let handle = sessions.get(&node_id).expect("session should exist");
        assert!(handle.reservations.is_empty());
        assert_eq!(handle.load.starting_tasks, 2);
    });
}

#[tokio::test]
async fn pick_best_session_skips_saturated_node_without_database() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
        .expect("lazy test pool should parse");
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
    let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000009").unwrap();
    let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

    service.sessions.lock().await.insert(
        node_id,
        SessionHandle {
            session_id: Uuid::from_u128(1),
            generation: Arc::default(),
            sender,
            registration: sample_registration(node_id),
            identity: synthetic_session_identity(),
            capabilities: SessionCapabilities::default(),
            load: SessionLoad {
                cpu_percent: 0.0,
                mem_percent: 0.0,
                disk_percent: 0.0,
                upload_disk_total_bytes: 100,
                upload_disk_available_bytes: 80,
                upload_disk_used_percent: 20.0,
                artifact_cleanup_blocked: false,
                gpu_runtime: Vec::new(),
                ..online_live_session_load(1, 1.0)
            },
            reservations: VecDeque::new(),
            management_port: 9443,
            management_upload_max_bytes: 64 * 1024 * 1024,
        },
    );

    assert!(
        pick_best_session_for_test(
            &service,
            None,
            &sample_immediate_task_spec(),
            ExecutionPreference::CpuOnly,
        )
        .await
        .is_none()
    );
}

#[tokio::test]
async fn pick_best_session_skips_artifact_cleanup_blocked_node_without_database() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
        .expect("lazy test pool should parse");
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
    let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000019").unwrap();
    let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

    service.sessions.lock().await.insert(
        node_id,
        SessionHandle {
            session_id: Uuid::from_u128(1),
            generation: Arc::default(),
            sender,
            registration: sample_registration(node_id),
            identity: synthetic_session_identity(),
            capabilities: SessionCapabilities::default(),
            load: SessionLoad {
                zlm_alive: true,
                ffmpeg_alive: true,
                artifact_cleanup_blocked: true,
                ..SessionLoad::default()
            },
            reservations: VecDeque::new(),
            management_port: 9443,
            management_upload_max_bytes: 64 * 1024 * 1024,
        },
    );

    assert!(
        pick_best_session_for_test(
            &service,
            None,
            &sample_immediate_task_spec(),
            ExecutionPreference::CpuOnly,
        )
        .await
        .is_none()
    );
}

#[tokio::test]
async fn claim_best_session_uses_reservations_to_spread_burst_dispatches() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
        .expect("lazy test pool should parse");
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
    let first_node = Uuid::parse_str("00000000-0000-0000-0000-000000000007").unwrap();
    let second_node = Uuid::parse_str("00000000-0000-0000-0000-000000000008").unwrap();
    let (first_sender, _first_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let (second_sender, _second_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

    {
        let mut sessions = service.sessions.lock().await;
        for (session_id, node_id, sender) in [
            (Uuid::from_u128(1), first_node, first_sender),
            (Uuid::from_u128(2), second_node, second_sender),
        ] {
            sessions.insert(
                node_id,
                SessionHandle {
                    session_id,
                    generation: Arc::default(),
                    sender,
                    registration: sample_registration(node_id),
                    identity: synthetic_session_identity(),
                    capabilities: SessionCapabilities::default(),
                    load: online_live_session_load(0, 0.0),
                    reservations: VecDeque::new(),
                    management_port: 9443,
                    management_upload_max_bytes: 64 * 1024 * 1024,
                },
            );
        }
    }

    let ClaimResult::Selected(first) = service
        .claim_best_session(
            None,
            Uuid::parse_str("00000000-0000-0000-0000-000000000101").unwrap(),
            &sample_immediate_task_spec(),
            ExecutionPreference::CpuOnly,
        )
        .await
    else {
        panic!("first dispatch should find a node");
    };
    let ClaimResult::Selected(second) = service
        .claim_best_session(
            None,
            Uuid::parse_str("00000000-0000-0000-0000-000000000102").unwrap(),
            &sample_immediate_task_spec(),
            ExecutionPreference::CpuOnly,
        )
        .await
    else {
        panic!("second dispatch should find a node");
    };

    assert_eq!(first.node_id, first_node);
    assert_eq!(second.node_id, second_node);
}

#[tokio::test]
async fn required_labels_filter_candidates_before_scoring() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
        .expect("lazy test pool should parse");
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
    let matching_node = Uuid::parse_str("00000000-0000-0000-0000-000000000025").unwrap();
    let other_node = Uuid::parse_str("00000000-0000-0000-0000-000000000026").unwrap();
    let (matching_sender, _matching_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let (other_sender, _other_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

    let mut matching_registration = sample_registration(matching_node);
    matching_registration.labels = vec!["archive".to_string(), "beijing-idc".to_string()];
    let mut other_registration = sample_registration(other_node);
    other_registration.labels = vec!["archive".to_string(), "shanghai".to_string()];

    let mut spec = sample_immediate_task_spec();
    spec.resource.required_labels = vec!["archive".to_string(), "beijing-idc".to_string()];

    let mut sessions = service.sessions.lock().await;
    sessions.insert(
        matching_node,
        SessionHandle {
            session_id: Uuid::from_u128(1),
            generation: Arc::default(),
            sender: matching_sender,
            registration: matching_registration,
            identity: synthetic_session_identity(),
            capabilities: SessionCapabilities::default(),
            load: online_live_session_load(9, 0.9),
            reservations: VecDeque::new(),
            management_port: 9443,
            management_upload_max_bytes: 64 * 1024 * 1024,
        },
    );
    sessions.insert(
        other_node,
        SessionHandle {
            session_id: Uuid::from_u128(2),
            generation: Arc::default(),
            sender: other_sender,
            registration: other_registration,
            identity: synthetic_session_identity(),
            capabilities: SessionCapabilities::default(),
            load: online_live_session_load(1, 0.1),
            reservations: VecDeque::new(),
            management_port: 9443,
            management_upload_max_bytes: 64 * 1024 * 1024,
        },
    );
    drop(sessions);

    let target = pick_best_session_for_test(&service, None, &spec, ExecutionPreference::CpuOnly)
        .await
        .expect("required label match should still find a node");

    assert_eq!(target.node_id, matching_node);
}

#[tokio::test]
async fn required_labels_return_none_when_no_online_node_matches() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
        .expect("lazy test pool should parse");
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
    let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000027").unwrap();
    let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

    let mut registration = sample_registration(node_id);
    registration.labels = vec!["archive".to_string()];

    let mut spec = sample_immediate_task_spec();
    spec.resource.required_labels = vec!["gpu".to_string()];

    let mut sessions = service.sessions.lock().await;
    sessions.insert(
        node_id,
        SessionHandle {
            session_id: Uuid::from_u128(1),
            generation: Arc::default(),
            sender,
            registration,
            identity: synthetic_session_identity(),
            capabilities: SessionCapabilities::default(),
            load: online_live_session_load(0, 0.0),
            reservations: VecDeque::new(),
            management_port: 9443,
            management_upload_max_bytes: 64 * 1024 * 1024,
        },
    );
    drop(sessions);

    let target =
        pick_best_session_for_test(&service, None, &spec, ExecutionPreference::CpuOnly).await;

    assert!(target.is_none());
}

#[tokio::test]
async fn required_labels_still_queue_when_matching_node_is_saturated() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
        .expect("lazy test pool should parse");
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
    let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000029").unwrap();
    let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

    let mut registration = sample_registration(node_id);
    registration.labels = vec!["archive".to_string()];

    let mut spec = sample_immediate_task_spec();
    spec.resource.required_labels = vec!["archive".to_string()];

    let mut sessions = service.sessions.lock().await;
    sessions.insert(
        node_id,
        SessionHandle {
            session_id: Uuid::from_u128(1),
            generation: Arc::default(),
            sender,
            registration,
            identity: synthetic_session_identity(),
            capabilities: SessionCapabilities::default(),
            load: online_live_session_load(1, 1.0),
            reservations: VecDeque::new(),
            management_port: 9443,
            management_upload_max_bytes: 64 * 1024 * 1024,
        },
    );
    drop(sessions);

    let target =
        pick_best_session_for_test(&service, None, &spec, ExecutionPreference::CpuOnly).await;

    assert!(target.is_none());
}

#[tokio::test]
async fn cpu_only_dispatch_still_prefers_lower_load_gpu_node_as_cpu_candidate() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
        .expect("lazy test pool should parse");
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
    let gpu_node = Uuid::parse_str("00000000-0000-0000-0000-000000000021").unwrap();
    let cpu_node = Uuid::parse_str("00000000-0000-0000-0000-000000000022").unwrap();
    let (gpu_sender, _gpu_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let (cpu_sender, _cpu_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

    let mut gpu_load = online_live_session_load(0, 0.0);
    gpu_load.gpu_runtime = sample_gpu_runtime(22.0, 18.0, 5.0);

    let mut sessions = service.sessions.lock().await;
    sessions.insert(
        gpu_node,
        SessionHandle {
            session_id: Uuid::from_u128(1),
            generation: Arc::default(),
            sender: gpu_sender,
            registration: sample_registration(gpu_node),
            identity: synthetic_session_identity(),
            capabilities: sample_gpu_capabilities(),
            load: gpu_load,
            reservations: VecDeque::new(),
            management_port: 9443,
            management_upload_max_bytes: 64 * 1024 * 1024,
        },
    );
    sessions.insert(
        cpu_node,
        SessionHandle {
            session_id: Uuid::from_u128(2),
            generation: Arc::default(),
            sender: cpu_sender,
            registration: sample_registration(cpu_node),
            identity: synthetic_session_identity(),
            capabilities: SessionCapabilities::default(),
            load: online_live_session_load(0, 0.0),
            reservations: VecDeque::new(),
            management_port: 9443,
            management_upload_max_bytes: 64 * 1024 * 1024,
        },
    );
    drop(sessions);

    let target = pick_best_session_for_test(
        &service,
        None,
        &sample_immediate_task_spec(),
        ExecutionPreference::CpuOnly,
    )
    .await
    .expect("cpu-only task should find a target");

    assert_eq!(target.node_id, gpu_node);
    assert!(!target.using_gpu_path);
}

#[tokio::test]
async fn gpu_nodes_remain_cpu_candidates_when_gpu_is_unavailable() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
        .expect("lazy test pool should parse");
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
    let gpu_node = Uuid::parse_str("00000000-0000-0000-0000-000000000023").unwrap();
    let cpu_node = Uuid::parse_str("00000000-0000-0000-0000-000000000024").unwrap();
    let (gpu_sender, _gpu_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let (cpu_sender, _cpu_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

    let mut overloaded_gpu_load = online_live_session_load(1, 0.1);
    overloaded_gpu_load.gpu_runtime = sample_gpu_runtime(99.0, 99.0, 10.0);

    let cpu_load = online_live_session_load(4, 0.7);

    let mut sessions = service.sessions.lock().await;
    sessions.insert(
        gpu_node,
        SessionHandle {
            session_id: Uuid::from_u128(1),
            generation: Arc::default(),
            sender: gpu_sender,
            registration: sample_registration(gpu_node),
            identity: synthetic_session_identity(),
            capabilities: sample_gpu_capabilities(),
            load: overloaded_gpu_load,
            reservations: VecDeque::new(),
            management_port: 9443,
            management_upload_max_bytes: 64 * 1024 * 1024,
        },
    );
    sessions.insert(
        cpu_node,
        SessionHandle {
            session_id: Uuid::from_u128(2),
            generation: Arc::default(),
            sender: cpu_sender,
            registration: sample_registration(cpu_node),
            identity: synthetic_session_identity(),
            capabilities: SessionCapabilities::default(),
            load: cpu_load,
            reservations: VecDeque::new(),
            management_port: 9443,
            management_upload_max_bytes: 64 * 1024 * 1024,
        },
    );
    drop(sessions);

    let target = pick_best_session_for_test(
        &service,
        None,
        &sample_immediate_task_spec(),
        ExecutionPreference::CpuOnly,
    )
    .await
    .expect("cpu-only task should fall back to a base-eligible node");

    assert_eq!(target.node_id, gpu_node);
    assert!(!target.using_gpu_path);
    assert!(target.has_gpu_devices);
}

#[test]
fn same_subnet_matches_ipv4_prefix() {
    assert!(same_subnet(
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 200)),
        24,
    ));
    assert!(!same_subnet(
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
        IpAddr::V4(Ipv4Addr::new(192, 168, 2, 10)),
        24,
    ));
}

#[tokio::test]
async fn dispatch_task_rolls_back_when_agent_channel_is_closed() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository.clone());
    let task = match repository
        .create_task(
            "dispatch-send-failure",
            "dispatch-send-failure-hash",
            sample_immediate_task_spec(),
        )
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    let task = repository.ensure_task_queued(task.id).await?;

    let node_id = Uuid::now_v7();
    let (sender, receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let session_id = provision_and_bootstrap_session(&service, &db.pool, node_id, sender).await?;
    service
        .update_session_load(node_id, session_id, &sample_heartbeat(0, 0.0))
        .await?;
    drop(receiver);

    let error = service
        .dispatch_task(task.id)
        .await
        .expect_err("closed channel should fail dispatch");
    assert!(matches!(error, ControlPlaneError::NodeDisconnected(id) if id == node_id));

    let summary = repository.get_task_summary(task.id).await?;
    assert_eq!(summary.status, TaskStatus::Queued);
    assert_eq!(summary.assigned_node_id, None);

    let active_lease_count: i64 =
        sqlx::query_scalar("select count(*) from task_leases where task_id = $1")
            .bind(task.id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(active_lease_count, 0);

    let attempt = sqlx::query(
        r#"
        select status::text as status, failure_code, failure_reason
          from task_attempts
         where task_id = $1
           and attempt_no = 1
        "#,
    )
    .bind(task.id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(attempt.try_get::<String, _>("status")?, "FAILED");
    assert_eq!(
        attempt.try_get::<Option<String>, _>("failure_code")?,
        Some("dispatch_send_failed".to_string())
    );
    assert!(
        attempt
            .try_get::<Option<String>, _>("failure_reason")?
            .unwrap_or_default()
            .contains("failed to send start_task to agent")
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn dispatch_task_returns_no_connected_node_when_only_node_is_full() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository.clone());
    let mut spec = sample_immediate_task_spec();
    spec.resource.required_labels = vec!["edge".to_string()];
    let task = match repository
        .create_task("full-node-dispatch", "full-node-dispatch-hash", spec)
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    let task = repository.ensure_task_queued(task.id).await?;

    let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000010")?;
    let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let session_id = provision_and_bootstrap_session(&service, &db.pool, node_id, sender).await?;
    service
        .update_session_load(node_id, session_id, &sample_heartbeat(1, 1.0))
        .await?;

    let error = service
        .dispatch_task(task.id)
        .await
        .expect_err("full node should be filtered out");
    assert!(matches!(error, ControlPlaneError::NoConnectedNode));

    let summary = repository.get_task_summary(task.id).await?;
    assert_eq!(summary.status, TaskStatus::Queued);
    assert_eq!(summary.assigned_node_id, None);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn stream_retry_after_disconnect_waits_for_original_node() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository.clone());
    let task = match repository
        .create_task(
            "stream-retry-affinity",
            "stream-retry-affinity-hash",
            sample_immediate_task_spec(),
        )
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    let task = repository.ensure_task_queued(task.id).await?;

    let original_node = Uuid::parse_str("00000000-0000-0000-0000-000000000041")?;
    let standby_node = Uuid::parse_str("00000000-0000-0000-0000-000000000042")?;
    let original_peer = provision_authenticated_peer(&db.pool, original_node).await?;
    let standby_peer = provision_authenticated_peer(&db.pool, standby_node).await?;
    let (original_sender, _original_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let original_session_id = service
        .bootstrap_session(
            &sample_registration(original_node),
            &original_peer,
            original_sender,
        )
        .await?;
    service
        .update_session_load(
            original_node,
            original_session_id,
            &sample_heartbeat(0, 0.0),
        )
        .await?;

    service.dispatch_task(task.id).await?;
    let dispatched = repository.get_task_summary(task.id).await?;
    let lease_token = current_attempt_lease_token(&db.pool, task.id).await?;
    assert_eq!(dispatched.assigned_node_id, Some(original_node));
    assert_eq!(dispatched.current_attempt_no, 1);

    let (standby_sender, _standby_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let standby_session_id = service
        .bootstrap_session(
            &sample_registration(standby_node),
            &standby_peer,
            standby_sender,
        )
        .await?;
    service
        .update_session_load(standby_node, standby_session_id, &sample_heartbeat(0, 0.0))
        .await?;

    service
        .close_session(original_node, original_session_id)
        .await;

    let reclaiming = repository.get_task_summary(task.id).await?;
    assert_eq!(reclaiming.status, TaskStatus::Reclaiming);
    assert_eq!(reclaiming.assigned_node_id, Some(original_node));
    assert_eq!(reclaiming.current_attempt_no, 1);

    let error = service
        .dispatch_task(task.id)
        .await
        .expect_err("reclaiming task should not redispatch");
    assert!(matches!(
        error,
        ControlPlaneError::Repository(RepoError::TaskNotDispatchable(TaskStatus::Reclaiming))
    ));

    let waiting = repository.get_task_summary(task.id).await?;
    assert_eq!(waiting.status, TaskStatus::Reclaiming);
    assert_eq!(waiting.assigned_node_id, Some(original_node));
    assert_eq!(waiting.current_attempt_no, 1);
    let reclaim_candidate = repository
        .list_reclaiming_tasks()
        .await?
        .into_iter()
        .find(|candidate| candidate.task_id == task.id)
        .expect("reclaiming task should be listed before adoption");

    let (reconnected_sender, _reconnected_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let reconnected_session_id = service
        .bootstrap_session(
            &sample_registration(original_node),
            &original_peer,
            reconnected_sender,
        )
        .await?;
    service
        .update_session_load(
            original_node,
            reconnected_session_id,
            &sample_heartbeat(0, 0.0),
        )
        .await?;

    repository
        .record_agent_task_event(
            original_node,
            AgentTaskEventRecord {
                task_id: task.id,
                attempt_no: 1,
                lease_token: lease_token.clone(),
                event_type: "adopted".to_string(),
                event_level: "info".to_string(),
                message: "runtime reattached".to_string(),
                payload: Value::Null,
            },
        )
        .await?;
    let recovering = repository.get_task_summary(task.id).await?;
    assert_eq!(recovering.status, TaskStatus::Recovering);
    assert_eq!(recovering.assigned_node_id, Some(original_node));
    assert_eq!(recovering.current_attempt_no, 1);
    assert!(
        !repository
            .finalize_reclaim_timeout(&reclaim_candidate)
            .await?,
        "stale reclaim timeout must not retry an adopted runtime"
    );
    let still_recovering = repository.get_task_summary(task.id).await?;
    assert_eq!(still_recovering.status, TaskStatus::Recovering);
    assert_eq!(still_recovering.current_attempt_no, 1);

    repository
        .record_agent_task_event(
            original_node,
            AgentTaskEventRecord {
                task_id: task.id,
                attempt_no: 1,
                lease_token,
                event_type: "running".to_string(),
                event_level: "info".to_string(),
                message: "runtime resumed".to_string(),
                payload: Value::Null,
            },
        )
        .await?;
    let resumed = repository.get_task_summary(task.id).await?;
    assert_eq!(resumed.status, TaskStatus::Running);
    assert_eq!(resumed.assigned_node_id, Some(original_node));
    assert_eq!(resumed.current_attempt_no, 1);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn dispatch_task_rewrites_live_http_source_through_gateway() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let (gateway_base, calls, _gateway) = spawn_source_gateway_stub(StatusCode::OK).await?;
    let service = ControlPlaneService::with_source_gateway(
        repository.clone(),
        crate::source_gateway::SourceGatewayClient::new_for_test(&gateway_base)?,
    );
    let mut spec = sample_immediate_task_spec();
    spec.input.kind = Some(InputKind::HttpFlv);
    spec.input.source_mode = Some(SourceMode::Live);
    spec.input.url = Some("http://customer.example/live.flv".to_string());
    let task = match repository
        .create_task("source-gateway-live", "source-gateway-live-hash", spec)
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    let task = repository.ensure_task_queued(task.id).await?;
    let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000051")?;
    let (sender, mut receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let session_id = provision_and_bootstrap_session(&service, &db.pool, node_id, sender).await?;
    assert!(matches!(
        receiver
            .recv()
            .await
            .and_then(Result::ok)
            .and_then(|value| value.payload),
        Some(media_rpc::control_plane::core_envelope::Payload::ProbeCapabilities(_))
    ));
    service
        .update_session_load(node_id, session_id, &sample_heartbeat(0, 0.0))
        .await?;

    service.dispatch_task(task.id).await?;

    let (_, stored_url) = resolved_spec_input(&db.pool, task.id).await?;
    assert!(stored_url.starts_with("http://media:18080/relay/"));
    let dispatched = receiver
        .recv()
        .await
        .expect("agent should receive start task")?;
    let media_rpc::control_plane::core_envelope::Payload::StartTask(command) = dispatched
        .payload
        .expect("start task payload should be sent")
    else {
        panic!("expected start task payload");
    };
    let sent_spec: TaskSpec = serde_json::from_str(&command.resolved_spec_json)?;
    assert_eq!(sent_spec.input.url.as_deref(), Some(stored_url.as_str()));
    assert_eq!(calls.lock().await.len(), 1);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn dispatch_task_returns_while_gateway_prefetch_is_pending_and_defers_the_next_poll()
-> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let (gateway_base, posts, gets, _gateway) = spawn_pending_prefetch_gateway_stub().await?;
    let service = ControlPlaneService::with_source_gateway(
        repository.clone(),
        crate::source_gateway::SourceGatewayClient::new_for_test(&gateway_base)?,
    );
    let mut spec = sample_immediate_task_spec();
    spec.input.kind = Some(InputKind::HttpMp4);
    spec.input.source_mode = Some(SourceMode::Vod);
    spec.input.url = Some("http://customer.example/archive.mp4".to_string());
    let task = match repository
        .create_task(
            "source-gateway-vod-pending",
            "source-gateway-vod-pending-hash",
            spec,
        )
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    repository.ensure_task_queued(task.id).await?;

    timeout(Duration::from_secs(1), service.dispatch_task(task.id)).await??;
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    assert_eq!(gets.load(Ordering::SeqCst), 0);
    assert_eq!(
        repository.get_task_summary(task.id).await?.status,
        TaskStatus::Queued
    );

    service.dispatch_task(task.id).await?;
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    assert_eq!(gets.load(Ordering::SeqCst), 0);

    tokio::time::sleep(Duration::from_millis(70)).await;
    assert!(matches!(
        service.dispatch_task(task.id).await,
        Err(ControlPlaneError::NoConnectedNode)
    ));
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    assert_eq!(gets.load(Ordering::SeqCst), 1);
    let (stored_kind, stored_url) = resolved_spec_input(&db.pool, task.id).await?;
    assert_eq!(stored_kind, "file");
    assert_eq!(stored_url, format!("imports/{}/source.mp4", task.id));

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn dispatch_task_fails_queued_task_when_gateway_relay_creation_fails() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let (gateway_base, _calls, _gateway) =
        spawn_source_gateway_stub(StatusCode::BAD_GATEWAY).await?;
    let service = ControlPlaneService::with_source_gateway(
        repository.clone(),
        crate::source_gateway::SourceGatewayClient::new_for_test(&gateway_base)?,
    );
    let mut spec = sample_immediate_task_spec();
    spec.input.kind = Some(InputKind::HttpFlv);
    spec.input.source_mode = Some(SourceMode::Live);
    spec.input.url = Some("http://customer.example/live.flv".to_string());
    let task = match repository
        .create_task(
            "source-gateway-live-fails",
            "source-gateway-live-fails-hash",
            spec,
        )
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    let task = repository.ensure_task_queued(task.id).await?;
    let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000052")?;
    let (sender, mut receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let session_id = provision_and_bootstrap_session(&service, &db.pool, node_id, sender).await?;
    assert!(matches!(
        receiver
            .recv()
            .await
            .and_then(Result::ok)
            .and_then(|value| value.payload),
        Some(media_rpc::control_plane::core_envelope::Payload::ProbeCapabilities(_))
    ));
    service
        .update_session_load(node_id, session_id, &sample_heartbeat(0, 0.0))
        .await?;

    service.dispatch_task(task.id).await?;

    let failed = repository.get_task_summary(task.id).await?;
    assert_eq!(failed.status, TaskStatus::Failed);
    assert!(receiver.try_recv().is_err());

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn dispatch_task_fails_when_no_online_node_matches_required_labels() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository.clone());
    let mut spec = sample_immediate_task_spec();
    spec.resource.required_labels = vec!["archive".to_string()];
    let task = match repository
        .create_task("required-labels-miss", "required-labels-miss-hash", spec)
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    let task = repository.ensure_task_queued(task.id).await?;

    let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000028")?;
    let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let _session_id = provision_and_bootstrap_session(&service, &db.pool, node_id, sender).await?;

    service.dispatch_task(task.id).await?;

    let summary = repository.get_task_summary(task.id).await?;
    assert_eq!(summary.status, TaskStatus::Failed);
    assert_eq!(summary.assigned_node_id, None);
    assert_eq!(summary.current_attempt_no, 1);

    let attempt = sqlx::query(
        r#"
        select status::text as status, failure_code, failure_reason
          from task_attempts
         where task_id = $1
           and attempt_no = 1
        "#,
    )
    .bind(task.id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(attempt.try_get::<String, _>("status")?, "FAILED");
    assert_eq!(
        attempt.try_get::<Option<String>, _>("failure_code")?,
        Some("required_labels_unmatched".to_string())
    );
    assert!(
        attempt
            .try_get::<Option<String>, _>("failure_reason")?
            .unwrap_or_default()
            .contains("archive")
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn dispatch_task_second_required_labels_failure_reuses_current_attempt() -> anyhow::Result<()>
{
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository.clone());
    let mut spec = sample_immediate_task_spec();
    spec.resource.required_labels = vec!["archive".to_string()];
    let task = match repository
        .create_task(
            "required-labels-miss-retry",
            "required-labels-miss-retry-hash",
            spec,
        )
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    let task = repository.ensure_task_queued(task.id).await?;

    let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000029")?;
    let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let _session_id = provision_and_bootstrap_session(&service, &db.pool, node_id, sender).await?;

    service.dispatch_task(task.id).await?;
    repository.retry_task(task.id).await?;
    service.dispatch_task(task.id).await?;

    let summary = repository.get_task_summary(task.id).await?;
    assert_eq!(summary.status, TaskStatus::Failed);
    assert_eq!(summary.current_attempt_no, 2);
    assert_eq!(summary.assigned_node_id, None);

    let first_attempt = sqlx::query(
        r#"
        select status::text as status, failure_code
          from task_attempts
         where task_id = $1
           and attempt_no = 1
        "#,
    )
    .bind(task.id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(first_attempt.try_get::<String, _>("status")?, "FAILED");
    assert_eq!(
        first_attempt.try_get::<Option<String>, _>("failure_code")?,
        Some("required_labels_unmatched".to_string())
    );

    let second_attempt = sqlx::query(
        r#"
        select status::text as status, failure_code, failure_reason
          from task_attempts
         where task_id = $1
           and attempt_no = 2
        "#,
    )
    .bind(task.id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(second_attempt.try_get::<String, _>("status")?, "FAILED");
    assert_eq!(
        second_attempt.try_get::<Option<String>, _>("failure_code")?,
        Some("required_labels_unmatched".to_string())
    );
    assert!(
        second_attempt
            .try_get::<Option<String>, _>("failure_reason")?
            .unwrap_or_default()
            .contains("archive")
    );

    let third_attempt_count: i64 = sqlx::query_scalar(
        r#"
        select count(*)
          from task_attempts
         where task_id = $1
           and attempt_no = 3
        "#,
    )
    .bind(task.id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(third_attempt_count, 0);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn fail_queued_task_returns_invariant_error_when_current_attempt_row_is_missing()
-> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let task = match repository
        .create_task(
            "queued-attempt-invariant",
            "queued-attempt-invariant-hash",
            sample_immediate_task_spec(),
        )
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    let task = repository.ensure_task_queued(task.id).await?;
    repository
        .fail_queued_task(task.id, "first_failure", "seed first failure")
        .await?;
    repository.retry_task(task.id).await?;

    sqlx::query(
        r#"
        delete from task_attempts
         where task_id = $1
           and attempt_no = 2
        "#,
    )
    .bind(task.id)
    .execute(&db.pool)
    .await?;

    let error = repository
        .fail_queued_task(
            task.id,
            "second_failure",
            "current pending attempt disappeared",
        )
        .await
        .expect_err("missing current attempt row should fail fast");
    assert!(matches!(
        error,
        RepoError::TaskAttemptInvariant {
            task_id,
            attempt_no: 2,
            ..
        } if task_id == task.id
    ));

    let summary = repository.get_task_summary(task.id).await?;
    assert_eq!(summary.status, TaskStatus::Queued);
    assert_eq!(summary.current_attempt_no, 2);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn dispatch_task_reserves_slots_to_reduce_burst_skew() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository.clone());

    let first_task = match repository
        .create_task(
            "burst-reservation-a",
            "burst-reservation-a-hash",
            sample_immediate_task_spec(),
        )
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    let first_task = repository.ensure_task_queued(first_task.id).await?;

    let second_task = match repository
        .create_task(
            "burst-reservation-b",
            "burst-reservation-b-hash",
            sample_immediate_task_spec(),
        )
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    let second_task = repository.ensure_task_queued(second_task.id).await?;

    let first_node = Uuid::parse_str("00000000-0000-0000-0000-000000000011")?;
    let second_node = Uuid::parse_str("00000000-0000-0000-0000-000000000012")?;
    let (first_sender, _first_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let (second_sender, _second_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let first_session =
        provision_and_bootstrap_session(&service, &db.pool, first_node, first_sender).await?;
    let second_session =
        provision_and_bootstrap_session(&service, &db.pool, second_node, second_sender).await?;
    service
        .update_session_load(first_node, first_session, &sample_heartbeat(0, 0.0))
        .await?;
    service
        .update_session_load(second_node, second_session, &sample_heartbeat(0, 0.0))
        .await?;

    service.dispatch_task(first_task.id).await?;
    service.dispatch_task(second_task.id).await?;

    let first_summary = repository.get_task_summary(first_task.id).await?;
    let second_summary = repository.get_task_summary(second_task.id).await?;
    assert_eq!(first_summary.assigned_node_id, Some(first_node));
    assert_eq!(second_summary.assigned_node_id, Some(second_node));

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn close_session_marks_dispatching_task_reclaiming_before_retry() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository.clone());
    let task = match repository
        .create_task(
            "disconnect-dispatching",
            "disconnect-dispatching-hash",
            sample_immediate_task_spec(),
        )
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    let task = repository.ensure_task_queued(task.id).await?;

    let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000013")?;
    let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let session_id = provision_and_bootstrap_session(&service, &db.pool, node_id, sender).await?;
    service
        .update_session_load(node_id, session_id, &sample_heartbeat(0, 0.0))
        .await?;

    service.dispatch_task(task.id).await?;
    service.close_session(node_id, session_id).await;

    let summary = repository.get_task_summary(task.id).await?;
    assert_eq!(summary.status, TaskStatus::Reclaiming);
    assert_eq!(summary.assigned_node_id, Some(node_id));
    assert_eq!(summary.current_attempt_no, 1);

    let attempt = sqlx::query(
        r#"
        select status::text as status, failure_code
          from task_attempts
         where task_id = $1
           and attempt_no = 1
        "#,
    )
    .bind(task.id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(attempt.try_get::<String, _>("status")?, "PENDING");
    assert_eq!(attempt.try_get::<Option<String>, _>("failure_code")?, None);

    let candidate = repository
        .list_reclaiming_tasks()
        .await?
        .into_iter()
        .find(|candidate| candidate.task_id == task.id)
        .expect("dispatching task should enter reclaiming");
    repository.finalize_reclaim_timeout(&candidate).await?;

    let retried = repository.get_task_summary(task.id).await?;
    assert_eq!(retried.status, TaskStatus::Queued);
    assert_eq!(retried.assigned_node_id, None);
    assert_eq!(retried.current_attempt_no, 2);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn close_session_marks_running_task_reclaiming_until_timeout_retry() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository.clone());
    let task = match repository
        .create_task(
            "disconnect-running",
            "disconnect-running-hash",
            sample_immediate_task_spec(),
        )
        .await?
    {
        crate::repository::CreateTaskResult::Fresh(task)
        | crate::repository::CreateTaskResult::Replay(task) => task,
    };
    let task = repository.ensure_task_queued(task.id).await?;

    let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000014")?;
    let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let session_id = provision_and_bootstrap_session(&service, &db.pool, node_id, sender).await?;
    service
        .update_session_load(node_id, session_id, &sample_heartbeat(0, 0.0))
        .await?;

    service.dispatch_task(task.id).await?;
    let lease_token = current_attempt_lease_token(&db.pool, task.id).await?;
    repository
        .record_agent_task_event(
            node_id,
            AgentTaskEventRecord {
                task_id: task.id,
                attempt_no: 1,
                lease_token,
                event_type: "running".to_string(),
                event_level: "info".to_string(),
                message: "task is running".to_string(),
                payload: Value::Null,
            },
        )
        .await?;

    service.close_session(node_id, session_id).await;

    let summary = repository.get_task_summary(task.id).await?;
    assert_eq!(summary.status, TaskStatus::Reclaiming);
    assert_eq!(summary.current_attempt_no, 1);
    assert_eq!(summary.assigned_node_id, Some(node_id));

    let before_retry = sqlx::query(
        r#"
        select attempt_no, status::text as status, failure_code, node_id
          from task_attempts
         where task_id = $1
         order by attempt_no asc
        "#,
    )
    .bind(task.id)
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(before_retry.len(), 1);
    assert_eq!(before_retry[0].try_get::<i32, _>("attempt_no")?, 1);
    assert_eq!(before_retry[0].try_get::<String, _>("status")?, "RUNNING");
    assert_eq!(
        before_retry[0].try_get::<Option<String>, _>("failure_code")?,
        None
    );

    let candidate = repository
        .list_reclaiming_tasks()
        .await?
        .into_iter()
        .find(|candidate| candidate.task_id == task.id)
        .expect("running task should enter reclaiming");
    repository.finalize_reclaim_timeout(&candidate).await?;

    let retried = repository.get_task_summary(task.id).await?;
    assert_eq!(retried.status, TaskStatus::Queued);
    assert_eq!(retried.current_attempt_no, 2);
    assert_eq!(retried.assigned_node_id, None);

    let attempts = sqlx::query(
        r#"
        select attempt_no, status::text as status, failure_code, node_id
          from task_attempts
         where task_id = $1
         order by attempt_no asc
        "#,
    )
    .bind(task.id)
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].try_get::<i32, _>("attempt_no")?, 1);
    assert_eq!(attempts[0].try_get::<String, _>("status")?, "FAILED");
    assert_eq!(
        attempts[0].try_get::<Option<String>, _>("failure_code")?,
        Some("node_disconnected".to_string())
    );
    assert_eq!(attempts[1].try_get::<i32, _>("attempt_no")?, 2);
    assert_eq!(attempts[1].try_get::<String, _>("status")?, "PENDING");
    assert_eq!(attempts[1].try_get::<Option<Uuid>, _>("node_id")?, None);

    db.cleanup().await?;
    Ok(())
}

fn probe_capabilities_envelope() -> CoreEnvelope {
    CoreEnvelope {
        payload: Some(
            media_rpc::control_plane::core_envelope::Payload::ProbeCapabilities(
                ProbeCapabilities {},
            ),
        ),
    }
}

async fn replace_durable_session_for_test(
    pool: &PgPool,
    node_id: Uuid,
    old_session_id: Uuid,
    new_session_id: Uuid,
) -> anyhow::Result<()> {
    let now = Utc::now();
    let replaced = sqlx::query(
        r#"
        update agent_control_sessions
           set session_id = $1,
               core_instance_id = $2,
               connected_at = $3,
               last_activity_at = $3,
               lease_expires_at = $4,
               disconnected_at = null,
               takeover_from_session_id = $5,
               takeover_reason = 'stale_timeout'
         where node_id = $6
           and session_id = $5
        "#,
    )
    .bind(new_session_id)
    .bind(Uuid::now_v7())
    .bind(now)
    .bind(now + chrono::Duration::seconds(30))
    .bind(old_session_id)
    .bind(node_id)
    .execute(pool)
    .await?;
    assert_eq!(replaced.rows_affected(), 1, "replace the expected session");
    Ok(())
}

#[tokio::test]
async fn bootstrap_unknown_certificate_emits_no_outbound_envelope() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository);
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;

    sqlx::query("delete from agent_management_certificates where node_id = $1")
        .bind(node_id)
        .execute(&db.pool)
        .await?;
    sqlx::query("delete from agent_certificates where node_id = $1")
        .bind(node_id)
        .execute(&db.pool)
        .await?;
    sqlx::query("delete from agent_identities where node_id = $1")
        .bind(node_id)
        .execute(&db.pool)
        .await?;

    let (sender, mut receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let error = service
        .bootstrap_session(&sample_registration(node_id), &peer, sender.clone())
        .await
        .expect_err("an unknown certificate must be rejected");
    assert_eq!(error.code(), tonic::Code::Unauthenticated);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    let durable_session_count: i64 =
        sqlx::query_scalar("select count(*) from agent_control_sessions where node_id = $1")
            .bind(node_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(durable_session_count, 0);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn bootstrap_healthy_duplicate_emits_no_outbound_envelope() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository);
    let node_id = Uuid::now_v7();
    let peer = provision_authenticated_peer(&db.pool, node_id).await?;

    let (current_sender, mut current_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let current_session_id = service
        .bootstrap_session(&sample_registration(node_id), &peer, current_sender)
        .await?;
    let initial = current_receiver
        .try_recv()
        .expect("accepted session should receive its bootstrap probe")?;
    assert!(matches!(
        initial.payload,
        Some(media_rpc::control_plane::core_envelope::Payload::ProbeCapabilities(_))
    ));
    assert!(matches!(
        current_receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));

    let (duplicate_sender, mut duplicate_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let error = service
        .bootstrap_session(
            &sample_registration(node_id),
            &peer,
            duplicate_sender.clone(),
        )
        .await
        .expect_err("a healthy session must reject a duplicate login");
    assert_eq!(error.code(), tonic::Code::AlreadyExists);
    assert!(matches!(
        duplicate_receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        current_receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    let durable_session_id: Uuid =
        sqlx::query_scalar("select session_id from agent_control_sessions where node_id = $1")
            .bind(node_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(durable_session_id, current_session_id);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn outbound_fence_takeover_first_rejects_without_sending_to_old_stream() -> anyhow::Result<()>
{
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository);
    let node_id = Uuid::now_v7();
    let (sender, mut receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let old_session_id =
        provision_and_bootstrap_session(&service, &db.pool, node_id, sender).await?;
    receiver
        .try_recv()
        .expect("accepted session should receive its bootstrap probe")?;
    let target = service
        .session_for_node(node_id)
        .await
        .expect("session should be present in memory");
    let new_session_id = Uuid::now_v7();
    replace_durable_session_for_test(&db.pool, node_id, old_session_id, new_session_id).await?;

    let error = service
        .send_to_current_session(&target, CoreEnvelope { payload: None })
        .await
        .expect_err("the old sender must be fenced after durable takeover");
    assert_eq!(error.code(), tonic::Code::PermissionDenied);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    let durable_session_id: Uuid =
        sqlx::query_scalar("select session_id from agent_control_sessions where node_id = $1")
            .bind(node_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(durable_session_id, new_session_id);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn outbound_closed_channel_does_not_hold_the_durable_session_lock() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository);
    let node_id = Uuid::now_v7();
    let (sender, receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let old_session_id =
        provision_and_bootstrap_session(&service, &db.pool, node_id, sender).await?;
    let target = service
        .session_for_node(node_id)
        .await
        .expect("session should be present in memory");
    drop(receiver);

    let error = service
        .send_to_current_session(&target, CoreEnvelope { payload: None })
        .await
        .expect_err("a closed response stream must reject outbound commands");
    assert_eq!(error.code(), tonic::Code::Unavailable);

    let new_session_id = Uuid::now_v7();
    timeout(
        std::time::Duration::from_secs(1),
        replace_durable_session_for_test(&db.pool, node_id, old_session_id, new_session_id),
    )
    .await
    .map_err(|_| anyhow::anyhow!("closed-channel send retained the durable session lock"))??;

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn outbound_full_channel_waits_without_holding_the_durable_session_lock() -> anyhow::Result<()>
{
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let service = ControlPlaneService::new(repository);
    let node_id = Uuid::now_v7();
    let (sender, mut receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let old_session_id =
        provision_and_bootstrap_session(&service, &db.pool, node_id, sender).await?;
    let target = service
        .session_for_node(node_id)
        .await
        .expect("session should be present in memory");
    loop {
        match target.sender.try_send(Ok(probe_capabilities_envelope())) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => break,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                panic!("response stream unexpectedly closed")
            }
        }
    }

    let send_service = service.clone();
    let send_target = target.clone();
    let mut send_task = tokio::spawn(async move {
        send_service
            .send_to_current_session(&send_target, CoreEnvelope { payload: None })
            .await
    });
    assert!(
        timeout(std::time::Duration::from_millis(100), &mut send_task)
            .await
            .is_err(),
        "a full response channel should leave the sender waiting for capacity"
    );

    let new_session_id = Uuid::now_v7();
    timeout(
        std::time::Duration::from_secs(1),
        replace_durable_session_for_test(&db.pool, node_id, old_session_id, new_session_id),
    )
    .await
    .map_err(|_| anyhow::anyhow!("full-channel send held the durable session lock"))??;

    receiver
        .recv()
        .await
        .expect("free one response-channel slot")?;
    let error = timeout(std::time::Duration::from_secs(1), send_task)
        .await
        .map_err(|_| {
            anyhow::anyhow!("fenced send did not finish after capacity became available")
        })??
        .expect_err("takeover must fence the old sender after it acquires capacity");
    assert_eq!(error.code(), tonic::Code::PermissionDenied);
    while let Ok(item) = receiver.try_recv() {
        let envelope = item?;
        assert!(
            envelope.payload.is_some(),
            "the sentinel command reached the old response stream"
        );
    }

    db.cleanup().await?;
    Ok(())
}

struct TestGrpcPki {
    ca_pem: String,
    server_certificate_pem: String,
    server_private_key_pem: String,
    agent_certificate_pem: String,
    agent_private_key_pem: String,
    agent_certificate_der: Vec<u8>,
}

fn test_grpc_pki(node_id: Uuid, now: DateTime<Utc>) -> anyhow::Result<TestGrpcPki> {
    use rcgen::{ExtendedKeyUsagePurpose, SanType};

    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    ca_params.not_before =
        time::OffsetDateTime::from_unix_timestamp((now - chrono::Duration::days(1)).timestamp())?;
    ca_params.not_after =
        time::OffsetDateTime::from_unix_timestamp((now + chrono::Duration::days(30)).timestamp())?;
    let ca_certificate = ca_params.self_signed(&ca_key)?;

    let server_key = KeyPair::generate()?;
    let mut server_params = CertificateParams::new(vec!["localhost".to_string()])?;
    server_params.is_ca = IsCa::NoCa;
    server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    server_params.not_before = time::OffsetDateTime::from_unix_timestamp(
        (now - chrono::Duration::minutes(5)).timestamp(),
    )?;
    server_params.not_after =
        time::OffsetDateTime::from_unix_timestamp((now + chrono::Duration::days(1)).timestamp())?;
    let server_certificate = server_params.signed_by(&server_key, &ca_certificate, &ca_key)?;

    let agent_key = KeyPair::generate()?;
    let mut agent_params = CertificateParams::default();
    agent_params.subject_alt_names = vec![SanType::URI(
        format!("spiffe://streamserver/agent/{node_id}").try_into()?,
    )];
    agent_params.is_ca = IsCa::NoCa;
    agent_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    agent_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    agent_params.not_before = time::OffsetDateTime::from_unix_timestamp(
        (now - chrono::Duration::minutes(5)).timestamp(),
    )?;
    agent_params.not_after =
        time::OffsetDateTime::from_unix_timestamp((now + chrono::Duration::days(1)).timestamp())?;
    let agent_certificate = agent_params.signed_by(&agent_key, &ca_certificate, &ca_key)?;

    Ok(TestGrpcPki {
        ca_pem: ca_certificate.pem(),
        server_certificate_pem: server_certificate.pem(),
        server_private_key_pem: server_key.serialize_pem(),
        agent_certificate_pem: agent_certificate.pem(),
        agent_private_key_pem: agent_key.serialize_pem(),
        agent_certificate_der: agent_certificate.der().to_vec(),
    })
}

async fn seed_transport_agent_identity(
    pool: &PgPool,
    node_id: Uuid,
    pki: &TestGrpcPki,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    use sha2::{Digest as _, Sha256};
    use x509_parser::prelude::FromDer;

    let (_, certificate) =
        x509_parser::certificate::X509Certificate::from_der(&pki.agent_certificate_der)
            .map_err(|error| anyhow::anyhow!("parse transport Agent certificate: {error}"))?;
    sqlx::query(
        r#"
        insert into agent_identities (node_id, status, created_at, updated_at)
        values ($1, 'active', $2, $2)
        "#,
    )
    .bind(node_id)
    .bind(now)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        insert into agent_certificates (
          id, node_id, serial_number, fingerprint_sha256, public_key_sha256,
          certificate_pem, state, not_before, not_after, issued_at, activated_at, issued_via
        ) values ($1, $2, $3, $4, $5, 'test-certificate-pem', 'active', $6, $7, $8, $8, 'enrollment')
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(node_id)
    .bind(Uuid::now_v7().simple().to_string())
    .bind(agent_certificate_fingerprint_sha256(
        &pki.agent_certificate_der,
    ))
    .bind(Sha256::digest(certificate.public_key().raw).as_slice())
    .bind(DateTime::from_timestamp(
        certificate.validity().not_before.timestamp(),
        0,
    ))
    .bind(DateTime::from_timestamp(
        certificate.validity().not_after.timestamp(),
        0,
    ))
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

#[allow(deprecated)]
fn rpc_register_for_transport(node_id: Uuid) -> RpcRegister {
    RpcRegister {
        node_id: node_id.to_string(),
        node_name: "transport-agent".to_string(),
        agent_version: "test".to_string(),
        hostname: "transport-agent".to_string(),
        labels: Vec::new(),
        interfaces: Vec::new(),
        zlm_api_base: String::new(),
        zlm_api_secret: String::new(),
        agent_stream_addr: "127.0.0.1:19091".to_string(),
        network_mode: "host".to_string(),
        ffmpeg_bin: "ffmpeg".to_string(),
        ffprobe_bin: "ffprobe".to_string(),
        zlm_server_id: "transport-agent".to_string(),
        output_mount_relative_prefix_mp4: String::new(),
        output_mount_relative_prefix_hls: String::new(),
        zlm_rtmp_port: 1935,
        zlm_rtsp_port: 554,
        agent_http_base_url: "http://127.0.0.1:8081".to_string(),
        management_port: 9443,
        management_upload_max_bytes: 64 * 1024 * 1024,
    }
}

#[allow(deprecated)]
#[test]
fn registration_ignores_legacy_self_reported_management_addresses_and_secret() {
    let node_id = Uuid::now_v7();
    let mut rpc = rpc_register_for_transport(node_id);
    rpc.zlm_api_base = "http://169.254.169.254/latest/meta-data".to_string();
    rpc.zlm_api_secret = "must-not-enter-core".to_string();
    rpc.agent_http_base_url = "http://127.0.0.1:1/legacy-management".to_string();

    let authenticated =
        authenticated_registration_from_rpc(rpc).expect("otherwise valid registration");
    let registration = authenticated.registration;

    assert!(registration.zlm_api_base.is_empty());
    assert!(registration.zlm_api_secret.is_empty());
    assert!(registration.agent_http_base_url.is_empty());
    assert_eq!(authenticated.management_port, 9443);
    assert_eq!(authenticated.management_upload_max_bytes, 64 * 1024 * 1024);
}

#[test]
fn registration_requires_authenticated_management_port_and_upload_limit() {
    let node_id = Uuid::now_v7();
    let mut zero_port = rpc_register_for_transport(node_id);
    zero_port.management_port = 0;
    assert!(authenticated_registration_from_rpc(zero_port).is_err());

    let mut oversized_port = rpc_register_for_transport(node_id);
    oversized_port.management_port = u32::from(u16::MAX) + 1;
    assert!(authenticated_registration_from_rpc(oversized_port).is_err());

    let mut zero_limit = rpc_register_for_transport(node_id);
    zero_limit.management_upload_max_bytes = 0;
    assert!(authenticated_registration_from_rpc(zero_limit).is_err());
}

fn register_envelope(node_id: Uuid) -> AgentEnvelope {
    AgentEnvelope {
        payload: Some(media_rpc::control_plane::agent_envelope::Payload::Register(
            rpc_register_for_transport(node_id),
        )),
    }
}

async fn spawn_transport_server(
    service: ControlPlaneService,
    tls: Option<ServerTlsConfig>,
) -> anyhow::Result<(
    std::net::SocketAddr,
    oneshot::Sender<()>,
    JoinHandle<Result<(), tonic::transport::Error>>,
)> {
    let probe = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = probe.local_addr()?;
    drop(probe);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let mut builder = Server::builder();
    if let Some(tls) = tls {
        builder = builder.tls_config(tls)?;
    }
    let server = builder
        .add_service(service.into_server())
        .serve_with_shutdown(address, async move {
            let _ = shutdown_rx.await;
        });
    Ok((address, shutdown_tx, tokio::spawn(server)))
}

async fn connect_transport(endpoint: Endpoint) -> anyhow::Result<Channel> {
    let mut last_error = None;
    for _ in 0..40 {
        match endpoint.clone().connect().await {
            Ok(channel) => return Ok(channel),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
    }
    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("transport endpoint did not start")))
}

async fn connect_mtls_transport(
    address: std::net::SocketAddr,
    pki: &TestGrpcPki,
) -> anyhow::Result<media_rpc::control_plane::control_plane_client::ControlPlaneClient<Channel>> {
    let endpoint = Endpoint::from_shared(format!("https://{address}"))?.tls_config(
        ClientTlsConfig::new()
            .ca_certificate(TonicCertificate::from_pem(&pki.ca_pem))
            .identity(TonicIdentity::from_pem(
                &pki.agent_certificate_pem,
                &pki.agent_private_key_pem,
            ))
            .domain_name("localhost"),
    )?;
    Ok(
        media_rpc::control_plane::control_plane_client::ControlPlaneClient::new(
            connect_transport(endpoint).await?,
        ),
    )
}

#[tokio::test]
async fn transport_plaintext_request_has_no_peer_identity_fallback() -> anyhow::Result<()> {
    let pool =
        PgPoolOptions::new().connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")?;
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
    let (address, shutdown, server) = spawn_transport_server(service, None).await?;
    let channel = connect_transport(Endpoint::from_shared(format!("http://{address}"))?).await?;
    let mut client =
        media_rpc::control_plane::control_plane_client::ControlPlaneClient::new(channel);
    let (sender, receiver) = mpsc::channel(1);
    sender.send(register_envelope(Uuid::now_v7())).await?;
    let error = client
        .stream_connect(ReceiverStream::new(receiver))
        .await
        .expect_err("plaintext transport must not have a peer certificate identity");
    assert_eq!(error.code(), tonic::Code::Unauthenticated);
    let _ = shutdown.send(());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn transport_mtls_unknown_fingerprint_is_rejected_after_handshake() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let node_id = Uuid::now_v7();
    let now = Utc::now();
    let pki = test_grpc_pki(node_id, now)?;
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(db.pool.clone())));
    let tls = ServerTlsConfig::new()
        .identity(TonicIdentity::from_pem(
            &pki.server_certificate_pem,
            &pki.server_private_key_pem,
        ))
        .client_ca_root(TonicCertificate::from_pem(&pki.ca_pem));
    let (address, shutdown, server) = spawn_transport_server(service, Some(tls)).await?;
    let mut client = connect_mtls_transport(address, &pki).await?;
    let (sender, receiver) = mpsc::channel(1);
    sender.send(register_envelope(node_id)).await?;
    let error = client
        .stream_connect(ReceiverStream::new(receiver))
        .await
        .expect_err("transport-trusted but unregistered fingerprint must fail");
    assert_eq!(error.code(), tonic::Code::Unauthenticated);
    let _ = shutdown.send(());
    server.await??;
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn transport_mtls_rejects_agent_supplied_intermediate_chain() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let node_id = Uuid::now_v7();
    let now = Utc::now();
    let pki = test_grpc_pki(node_id, now)?;
    seed_transport_agent_identity(&db.pool, node_id, &pki, now).await?;
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(db.pool.clone())));
    let tls = ServerTlsConfig::new()
        .identity(TonicIdentity::from_pem(
            &pki.server_certificate_pem,
            &pki.server_private_key_pem,
        ))
        .client_ca_root(TonicCertificate::from_pem(&pki.ca_pem));
    let (address, shutdown, server) = spawn_transport_server(service, Some(tls)).await?;
    let endpoint = Endpoint::from_shared(format!("https://{address}"))?.tls_config(
        ClientTlsConfig::new()
            .ca_certificate(TonicCertificate::from_pem(&pki.ca_pem))
            .identity(TonicIdentity::from_pem(
                format!("{}{}", pki.agent_certificate_pem, pki.ca_pem),
                &pki.agent_private_key_pem,
            ))
            .domain_name("localhost"),
    )?;
    let channel = connect_transport(endpoint).await?;
    let mut client =
        media_rpc::control_plane::control_plane_client::ControlPlaneClient::new(channel);
    let (sender, receiver) = mpsc::channel(1);
    sender.send(register_envelope(node_id)).await?;
    let error = client
        .stream_connect(ReceiverStream::new(receiver))
        .await
        .expect_err("Agent-provided certificate chain must be rejected");
    assert_eq!(error.code(), tonic::Code::Unauthenticated);
    let audited: bool = sqlx::query_scalar(
        r#"
        select exists (
          select 1 from security_audit_events
           where event_type = 'agent_peer_identity_rejected'
             and subject = $1
             and payload->>'reason' = 'unexpected_certificate_chain'
        )
        "#,
    )
    .bind(node_id.to_string())
    .fetch_one(&db.pool)
    .await?;
    assert!(audited);
    drop(sender);
    let _ = shutdown.send(());
    server.await??;
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn transport_mtls_rejects_missing_or_wrong_ca_client_identity() -> anyhow::Result<()> {
    let now = Utc::now();
    let pki = test_grpc_pki(Uuid::now_v7(), now)?;
    let wrong_pki = test_grpc_pki(Uuid::now_v7(), now)?;
    let pool =
        PgPoolOptions::new().connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")?;
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
    let tls = ServerTlsConfig::new()
        .identity(TonicIdentity::from_pem(
            &pki.server_certificate_pem,
            &pki.server_private_key_pem,
        ))
        .client_ca_root(TonicCertificate::from_pem(&pki.ca_pem));
    let (address, shutdown, server) = spawn_transport_server(service, Some(tls)).await?;

    let without_identity = Endpoint::from_shared(format!("https://{address}"))?.tls_config(
        ClientTlsConfig::new()
            .ca_certificate(TonicCertificate::from_pem(&pki.ca_pem))
            .domain_name("localhost"),
    )?;
    if let Ok(channel) = connect_transport(without_identity).await {
        let mut client =
            media_rpc::control_plane::control_plane_client::ControlPlaneClient::new(channel);
        let (sender, receiver) = mpsc::channel(1);
        sender.send(register_envelope(Uuid::now_v7())).await?;
        assert!(
            client
                .stream_connect(ReceiverStream::new(receiver))
                .await
                .is_err(),
            "mTLS server must reject an RPC from a client without an identity"
        );
    }

    let wrong_identity = Endpoint::from_shared(format!("https://{address}"))?.tls_config(
        ClientTlsConfig::new()
            .ca_certificate(TonicCertificate::from_pem(&pki.ca_pem))
            .identity(TonicIdentity::from_pem(
                &wrong_pki.agent_certificate_pem,
                &wrong_pki.agent_private_key_pem,
            ))
            .domain_name("localhost"),
    )?;
    if let Ok(channel) = connect_transport(wrong_identity).await {
        let mut client =
            media_rpc::control_plane::control_plane_client::ControlPlaneClient::new(channel);
        let (sender, receiver) = mpsc::channel(1);
        sender.send(register_envelope(Uuid::now_v7())).await?;
        assert!(
            client
                .stream_connect(ReceiverStream::new(receiver))
                .await
                .is_err(),
            "mTLS server must reject an Agent RPC signed by the wrong CA"
        );
    }

    let _ = shutdown.send(());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn transport_mtls_uses_certificate_node_and_persists_peer_metadata() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let node_id = Uuid::now_v7();
    let now = Utc::now();
    let pki = test_grpc_pki(node_id, now)?;
    seed_transport_agent_identity(&db.pool, node_id, &pki, now).await?;
    let service = ControlPlaneService::new(Arc::new(TaskRepository::new(db.pool.clone())));
    let tls = ServerTlsConfig::new()
        .identity(TonicIdentity::from_pem(
            &pki.server_certificate_pem,
            &pki.server_private_key_pem,
        ))
        .client_ca_root(TonicCertificate::from_pem(&pki.ca_pem));
    let (address, shutdown, server) = spawn_transport_server(service, Some(tls)).await?;
    let mut client = connect_mtls_transport(address, &pki).await?;

    let (mismatch_sender, mismatch_receiver) = mpsc::channel(1);
    mismatch_sender
        .send(register_envelope(Uuid::now_v7()))
        .await?;
    let mismatch = client
        .stream_connect(ReceiverStream::new(mismatch_receiver))
        .await
        .expect_err("certificate/Register node mismatch must fail");
    assert_eq!(mismatch.code(), tonic::Code::PermissionDenied);

    let (sender, receiver) = mpsc::channel(2);
    sender.send(register_envelope(node_id)).await?;
    let mut response = client
        .stream_connect(ReceiverStream::new(receiver))
        .await?
        .into_inner();
    let first = timeout(std::time::Duration::from_secs(2), response.message())
        .await??
        .expect("authorized stream must receive bootstrap envelope");
    assert!(matches!(
        first.payload,
        Some(media_rpc::control_plane::core_envelope::Payload::ProbeCapabilities(_))
    ));
    let durable = sqlx::query(
        "select peer_ip::text as peer_ip, session_id from agent_control_sessions where node_id = $1",
    )
    .bind(node_id)
    .fetch_one(&db.pool)
    .await?;
    assert!(
        durable
            .try_get::<String, _>("peer_ip")?
            .starts_with("127.0.0.1")
    );
    assert!(!durable.try_get::<Uuid, _>("session_id")?.is_nil());
    let mismatch_audits: i64 = sqlx::query_scalar(
        "select count(*) from security_audit_events where event_type = 'agent_peer_identity_rejected' and subject = $1",
    )
    .bind(node_id.to_string())
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(mismatch_audits, 1);

    drop(sender);
    let _ = shutdown.send(());
    server.await??;
    db.cleanup().await?;
    Ok(())
}
