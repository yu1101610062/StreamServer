use super::*;
use crate::test_database::{config_from_env, finish_setup};
use std::net::Ipv4Addr;

use axum::{Json, Router, http::StatusCode};
use media_domain::{
    CommonSpec, ExposeSpec, InputSpec, PublishSpec, RecordSpec, RecoverySpec, ResourceSpec,
    RuntimeSlotLoad, ScheduleSpec, SourceMode, StreamSpec, TaskStatus, TaskType,
};
use serde_json::json;
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use tokio::{net::TcpStream, sync::mpsc, task::JoinHandle, time::timeout};

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

#[test]
fn source_gateway_rewrites_live_http_input_to_relay_url() -> anyhow::Result<()> {
    let task_id = Uuid::parse_str("00000000-0000-0000-0000-000000000111")?;
    let mut spec = sample_spec(
        InputKind::HttpFlv,
        Some("http://customer.example/live.flv"),
        None,
    )
    .resolved();

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
    assert_eq!(
        spec.input.url.as_deref(),
        Some("http://media:18080/relay/00000000-0000-0000-0000-000000000111?token=t")
    );
    Ok(())
}

#[test]
fn source_gateway_rewrites_vod_http_input_to_shared_file_path() -> anyhow::Result<()> {
    let task_id = Uuid::parse_str("00000000-0000-0000-0000-000000000222")?;
    let mut spec = sample_spec(
        InputKind::HttpMp4,
        Some("http://customer.example/archive.mp4"),
        None,
    )
    .resolved();

    let action = crate::source_gateway::plan_gateway_action(&spec, task_id)
        .expect("vod http input should use media prefetch");
    crate::source_gateway::apply_gateway_result(
        &mut spec,
        action,
        crate::source_gateway::GatewayActionResult::Prefetch {
            source_url: "imports/00000000-0000-0000-0000-000000000222/source.mp4".to_string(),
        },
    )?;

    assert_eq!(spec.input.kind, Some(InputKind::File));
    assert_eq!(spec.input.source_mode, Some(SourceMode::Vod));
    assert_eq!(
        spec.input.url.as_deref(),
        Some("imports/00000000-0000-0000-0000-000000000222/source.mp4")
    );
    Ok(())
}

struct TestDatabase {
    admin_pool: PgPool,
    pool: PgPool,
    database_name: String,
}

impl TestDatabase {
    async fn new(admin_url: &str, run_migrations: bool) -> anyhow::Result<Self> {
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
                session_id: 1,
                sender: mpsc::channel(CONTROL_STREAM_BUFFER).0,
                registration: sample_registration(node_id),
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
            },
        );

        service
            .update_session_load(node_id, &sample_heartbeat_with_states(0, 2, 0, 0, 0.5))
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
            session_id: 1,
            sender,
            registration: sample_registration(node_id),
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
            session_id: 1,
            sender,
            registration: sample_registration(node_id),
            capabilities: SessionCapabilities::default(),
            load: SessionLoad {
                zlm_alive: true,
                ffmpeg_alive: true,
                artifact_cleanup_blocked: true,
                ..SessionLoad::default()
            },
            reservations: VecDeque::new(),
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
            (1, first_node, first_sender),
            (2, second_node, second_sender),
        ] {
            sessions.insert(
                node_id,
                SessionHandle {
                    session_id,
                    sender,
                    registration: sample_registration(node_id),
                    capabilities: SessionCapabilities::default(),
                    load: online_live_session_load(0, 0.0),
                    reservations: VecDeque::new(),
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
            session_id: 1,
            sender: matching_sender,
            registration: matching_registration,
            capabilities: SessionCapabilities::default(),
            load: online_live_session_load(9, 0.9),
            reservations: VecDeque::new(),
        },
    );
    sessions.insert(
        other_node,
        SessionHandle {
            session_id: 2,
            sender: other_sender,
            registration: other_registration,
            capabilities: SessionCapabilities::default(),
            load: online_live_session_load(1, 0.1),
            reservations: VecDeque::new(),
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
            session_id: 1,
            sender,
            registration,
            capabilities: SessionCapabilities::default(),
            load: online_live_session_load(0, 0.0),
            reservations: VecDeque::new(),
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
            session_id: 1,
            sender,
            registration,
            capabilities: SessionCapabilities::default(),
            load: online_live_session_load(1, 1.0),
            reservations: VecDeque::new(),
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
            session_id: 1,
            sender: gpu_sender,
            registration: sample_registration(gpu_node),
            capabilities: sample_gpu_capabilities(),
            load: gpu_load,
            reservations: VecDeque::new(),
        },
    );
    sessions.insert(
        cpu_node,
        SessionHandle {
            session_id: 2,
            sender: cpu_sender,
            registration: sample_registration(cpu_node),
            capabilities: SessionCapabilities::default(),
            load: online_live_session_load(0, 0.0),
            reservations: VecDeque::new(),
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
            session_id: 1,
            sender: gpu_sender,
            registration: sample_registration(gpu_node),
            capabilities: sample_gpu_capabilities(),
            load: overloaded_gpu_load,
            reservations: VecDeque::new(),
        },
    );
    sessions.insert(
        cpu_node,
        SessionHandle {
            session_id: 2,
            sender: cpu_sender,
            registration: sample_registration(cpu_node),
            capabilities: SessionCapabilities::default(),
            load: cpu_load,
            reservations: VecDeque::new(),
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
    let _session_id = service
        .bootstrap_session(&sample_registration(node_id), sender)
        .await?;
    service
        .update_session_load(node_id, &sample_heartbeat(0, 0.0))
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
    let _session_id = service
        .bootstrap_session(&sample_registration(node_id), sender)
        .await?;
    service
        .update_session_load(node_id, &sample_heartbeat(1, 1.0))
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
    let (original_sender, _original_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let original_session_id = service
        .bootstrap_session(&sample_registration(original_node), original_sender)
        .await?;
    service
        .update_session_load(original_node, &sample_heartbeat(0, 0.0))
        .await?;

    service.dispatch_task(task.id).await?;
    let dispatched = repository.get_task_summary(task.id).await?;
    let lease_token = current_attempt_lease_token(&db.pool, task.id).await?;
    assert_eq!(dispatched.assigned_node_id, Some(original_node));
    assert_eq!(dispatched.current_attempt_no, 1);

    let (standby_sender, _standby_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
    let _standby_session_id = service
        .bootstrap_session(&sample_registration(standby_node), standby_sender)
        .await?;
    service
        .update_session_load(standby_node, &sample_heartbeat(0, 0.0))
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
    let _reconnected_session_id = service
        .bootstrap_session(&sample_registration(original_node), reconnected_sender)
        .await?;
    service
        .update_session_load(original_node, &sample_heartbeat(0, 0.0))
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
    service
        .bootstrap_session(&sample_registration(node_id), sender)
        .await?;
    service
        .update_session_load(node_id, &sample_heartbeat(0, 0.0))
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
    service
        .bootstrap_session(&sample_registration(node_id), sender)
        .await?;
    service
        .update_session_load(node_id, &sample_heartbeat(0, 0.0))
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
    let _session_id = service
        .bootstrap_session(&sample_registration(node_id), sender)
        .await?;

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
    let _session_id = service
        .bootstrap_session(&sample_registration(node_id), sender)
        .await?;

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
    let _first_session = service
        .bootstrap_session(&sample_registration(first_node), first_sender)
        .await?;
    let _second_session = service
        .bootstrap_session(&sample_registration(second_node), second_sender)
        .await?;
    service
        .update_session_load(first_node, &sample_heartbeat(0, 0.0))
        .await?;
    service
        .update_session_load(second_node, &sample_heartbeat(0, 0.0))
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
    let session_id = service
        .bootstrap_session(&sample_registration(node_id), sender)
        .await?;
    service
        .update_session_load(node_id, &sample_heartbeat(0, 0.0))
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
    let session_id = service
        .bootstrap_session(&sample_registration(node_id), sender)
        .await?;
    service
        .update_session_load(node_id, &sample_heartbeat(0, 0.0))
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
