use super::*;
use crate::config::{AuthMode, CoreSettings};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use media_domain::{AgentRegistration, HeartbeatSnapshot, NetworkMode};
use serde_json::json;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use tower::util::ServiceExt;

const TEST_RSA_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDRNk+CElS+M3My1DbTUInl9aeU\nYCLza8Uftij7kPTApECFQcy1em6CZwb+PDHjjtFB2i8Ncfbx+dt2S6CbJHSF0dDB\n+GoiaVaYolB9XoQODqA7LXTy/D4e9jdNJQgDVXlzXsTm4k3v1CnC1As7RfUkgdM/\npsbfsbeai7RULN2NnQIDAQAB\n-----END PUBLIC KEY-----";

fn disabled_auth_config() -> AuthConfig {
    AuthConfig::from_settings(&CoreSettings::default()).expect("disabled auth config")
}

fn auth_config_from_public_key(enabled: bool, pem: &str) -> anyhow::Result<AuthConfig> {
    if enabled {
        let settings = CoreSettings {
            auth_mode: AuthMode::ExternalJwt,
            jwt_public_key: pem.to_string(),
            ..CoreSettings::default()
        };
        AuthConfig::from_settings(&settings)
    } else {
        Ok(disabled_auth_config())
    }
}

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

#[test]
fn parse_cli_command_accepts_top_level_help() {
    let command = parse_cli_command_from(["--help".to_string()]).unwrap();
    assert_eq!(command, Some(CliCommand::Help { auth_only: false }));
}

#[test]
fn parse_cli_command_accepts_auth_help() {
    let command = parse_cli_command_from(["auth".to_string(), "--help".to_string()]).unwrap();
    assert_eq!(command, Some(CliCommand::Help { auth_only: true }));
}

#[test]
fn parse_cli_command_accepts_auth_subcommand_help() {
    let command = parse_cli_command_from([
        "auth".to_string(),
        "bootstrap-admin".to_string(),
        "--help".to_string(),
    ])
    .unwrap();
    assert_eq!(command, Some(CliCommand::Help { auth_only: true }));
}

#[test]
fn parse_cli_command_parses_auth_command() {
    let command = parse_cli_command_from([
        "auth".to_string(),
        "bootstrap-admin".to_string(),
        "--username".to_string(),
        "admin".to_string(),
        "--password-stdin".to_string(),
    ])
    .unwrap();
    assert_eq!(
        command,
        Some(CliCommand::BootstrapAdmin {
            username: "admin".to_string(),
        })
    );
}

fn play_url_test_node(agent_stream_addr: &str) -> repository::NodeSummary {
    let now = Utc::now();
    repository::NodeSummary {
        id: Uuid::now_v7(),
        node_name: "node-a".to_string(),
        hostname: "node-a".to_string(),
        labels: Vec::new(),
        zlm_api_base: "http://127.0.0.1".to_string(),
        agent_stream_addr: agent_stream_addr.to_string(),
        agent_http_base_url: "http://127.0.0.1:8081".to_string(),
        zlm_rtmp_port: 2935,
        zlm_rtsp_port: 9554,
        network_mode: "bridge".to_string(),
        interfaces: Vec::new(),
        healthy: true,
        control_connected: true,
        media_alive: true,
        last_seen_at: Some(now),
        control_last_seen_at: Some(now),
        media_last_seen_at: Some(now),
        created_at: now,
        updated_at: now,
        ffmpeg_protocols: Vec::new(),
        ffmpeg_formats: Vec::new(),
        ffmpeg_encoders: Vec::new(),
        ffmpeg_decoders: Vec::new(),
        zlm_api_list: Vec::new(),
        zlm_version: None,
        gpu: Vec::new(),
        gpu_devices: Vec::new(),
        capability_captured_at: None,
        slot_usage: None,
        running_tasks: None,
        starting_tasks: None,
        stopping_tasks: None,
        orphaned_tasks: None,
        connected: None,
        cpu_percent: None,
        mem_percent: None,
        disk_percent: None,
        upload_disk_total_bytes: None,
        upload_disk_available_bytes: None,
        upload_disk_used_percent: None,
        zlm_alive: None,
        ffmpeg_alive: None,
        gpu_runtime: None,
    }
}

#[test]
fn build_play_urls_returns_http_flv_when_rtmp_schema_is_online() {
    let node = play_url_test_node("http://stream.example:18080");
    let schemas = ["rtmp".to_string()].into_iter().collect::<BTreeSet<_>>();

    let urls = build_play_urls(&node, &schemas, "live", "camera01");

    assert_eq!(
        urls,
        vec![
            "rtmp://stream.example:2935/live/camera01",
            "http://stream.example:18080/live/camera01.live.flv",
        ]
    );
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
        auth: disabled_auth_config(),
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
        auth_config_from_public_key(true, TEST_RSA_PUBLIC_KEY).expect("rsa key should load");
    state
}

async fn upsert_test_node(
    repository: &TaskRepository,
    node_id: Uuid,
    zlm_api_base: &str,
    agent_stream_addr: &str,
) -> anyhow::Result<()> {
    upsert_test_node_with_ports(
        repository,
        node_id,
        zlm_api_base,
        agent_stream_addr,
        1935,
        554,
    )
    .await
}

async fn upsert_test_node_with_ports(
    repository: &TaskRepository,
    node_id: Uuid,
    zlm_api_base: &str,
    agent_stream_addr: &str,
    zlm_rtmp_port: u16,
    zlm_rtsp_port: u16,
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
                agent_http_base_url: "http://127.0.0.1:8081".to_string(),
                zlm_rtmp_port,
                zlm_rtsp_port,
                network_mode: NetworkMode::Bridge,
                ffmpeg_bin: "ffmpeg".to_string(),
                ffprobe_bin: "ffprobe".to_string(),
                zlm_server_id: format!("zlm-{node_id}"),
                output_mount_relative_prefix_mp4: "output".to_string(),
                output_mount_relative_prefix_hls: "output".to_string(),
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
          checkpoint_json, started_at, ended_at, created_at, lease_token
        ) values (
          $1, $2, 1, $3, 'zlm_proxy'::worker_kind, 'RUNNING'::attempt_status,
          null, null, 'rtsp', '__defaultVhost__', $4, $5,
          null, null, null, null,
          null, $6, null, $6, 'lease-1'
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
          id, task_id, attempt_id, server_id, node_id, schema, vhost, app, stream,
          zlm_proxy_key, zlm_pusher_key, rtp_stream_id, created_at
        ) values (
          $1, $2, $3, $4, $5, 'rtsp', '__defaultVhost__', $6, $7, null, null, null, $8
        )
        on conflict (server_id, schema, vhost, app, stream) do update
          set task_id = excluded.task_id,
              attempt_id = excluded.attempt_id,
              node_id = excluded.node_id,
              zlm_proxy_key = excluded.zlm_proxy_key,
              zlm_pusher_key = excluded.zlm_pusher_key,
              rtp_stream_id = excluded.rtp_stream_id,
              created_at = excluded.created_at
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(task_id)
    .bind(attempt_id)
    .bind(format!("zlm-{node_id}"))
    .bind(node_id)
    .bind(app)
    .bind(stream)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(task_id)
}

async fn insert_running_stream_task_with_times(
    pool: &sqlx::PgPool,
    node_id: Uuid,
    name: &str,
    stream: &str,
    task_created_at: DateTime<Utc>,
    task_updated_at: DateTime<Utc>,
    binding_created_at: DateTime<Utc>,
) -> anyhow::Result<Uuid> {
    let task_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": name,
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "publish": {},
        "record": {"enabled": false},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });

    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
        ) values (
          $1, $2, 'stream_ingest'::task_type, 'RUNNING'::task_status, $3,
          50, $4, $4, 'tester', $5,
          1, 'immediate', $6, $7, $6, null
        )
        "#,
    )
    .bind(task_id)
    .bind(name)
    .bind(format!("stream-order-{task_id}"))
    .bind(&resolved_spec)
    .bind(node_id)
    .bind(task_created_at)
    .bind(task_updated_at)
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
          null, null, 'rtsp', '__defaultVhost__', 'live', $4,
          null, null, null, null,
          null, $5, null, $5
        )
        "#,
    )
    .bind(attempt_id)
    .bind(task_id)
    .bind(node_id)
    .bind(stream)
    .bind(task_created_at)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        insert into stream_bindings (
          id, task_id, attempt_id, server_id, node_id, schema, vhost, app, stream,
          zlm_proxy_key, zlm_pusher_key, rtp_stream_id, created_at
        ) values (
          $1, $2, $3, $4, $5, 'rtsp', '__defaultVhost__', 'live', $6, null, null, null, $7
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(task_id)
    .bind(attempt_id)
    .bind(format!("zlm-{node_id}"))
    .bind(node_id)
    .bind(stream)
    .bind(binding_created_at)
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
          checkpoint_json, started_at, ended_at, created_at, lease_token
        ) values (
          $1, $2, 1, $3, 'zlm_proxy'::worker_kind, 'STARTING'::attempt_status,
          null, null, 'rtsp', '__defaultVhost__', $4, $5,
          null, null, null, null,
          null, $6, null, $6, 'lease-1'
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

async fn pending_callback_deliver_after(
    pool: &sqlx::PgPool,
    task_id: Uuid,
    attempt_no: i32,
    reason: &str,
) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
    sqlx::query_scalar(
        r#"
        select deliver_after
          from task_callback_outbox
         where task_id = $1
           and attempt_no = $2
           and event_type = 'task.completed'
           and reason = $3
           and status in ('pending', 'retrying')
         order by created_at desc
         limit 1
        "#,
    )
    .bind(task_id)
    .bind(attempt_no)
    .bind(reason)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
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
          checkpoint_json, started_at, ended_at, created_at, lease_token
        ) values (
          $1, $2, 1, $3, 'ffmpeg'::worker_kind, 'RUNNING'::attempt_status,
          null, null, null, null, null, null,
          null, null, null, null,
          null, $4, null, $4, 'lease-1'
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
          checkpoint_json, started_at, ended_at, created_at, lease_token
        ) values (
          $1, $2, 1, $3, 'ffmpeg'::worker_kind, 'RUNNING'::attempt_status,
          null, null, null, null, null, null,
          null, null, null, null,
          null, $4, null, $4, 'lease-1'
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
          checkpoint_json, started_at, ended_at, created_at, lease_token
        ) values (
          $1, $2, 1, $3, 'ffmpeg'::worker_kind, 'RUNNING'::attempt_status,
          null, null, null, null, null, null,
          null, null, null, null,
          null, $4, null, $4, 'lease-1'
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
    let task_status_type: bool =
        sqlx::query_scalar("select exists (select 1 from pg_type where typname = 'task_status')")
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
    let agent_http_base_url_exists: bool = sqlx::query_scalar(
        r#"
        select exists (
          select 1
            from information_schema.columns
           where table_schema = 'public'
             and table_name = 'media_nodes'
             and column_name = 'agent_http_base_url'
        )
        "#,
    )
    .fetch_one(&db.pool)
    .await?;

    assert_eq!(tasks.as_deref(), Some("tasks"));
    assert_eq!(media_nodes.as_deref(), Some("media_nodes"));
    assert!(task_status_type);
    assert!(!node_name_unique_exists);
    assert!(agent_http_base_url_exists);

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
async fn stop_task_allows_starting_state_via_api() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let node_id = Uuid::now_v7();
    upsert_test_node(
        &repository,
        node_id,
        "http://127.0.0.1:18080",
        "http://127.0.0.1:8081",
    )
    .await?;
    let task_id = insert_starting_stream_task(
        &db.pool,
        node_id,
        sample_create_task_payload("immediate"),
        "live",
        "camera01",
    )
    .await?;

    let app = build_app(test_app_state(db.pool.clone()));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/tasks/{task_id}/stop"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = json_body(response).await;
    assert_eq!(body["id"], json!(task_id));
    assert_eq!(body["status"], json!("STOPPING"));

    let row = sqlx::query(
        r#"
        select
          t.status::text as task_status,
          ta.status::text as attempt_status,
          ta.stop_requested_at,
          ta.stop_reason,
          ta.desired_terminal_status::text as desired_terminal_status
        from tasks t
        join task_attempts ta on ta.task_id = t.id and ta.attempt_no = t.current_attempt_no
        where t.id = $1
        "#,
    )
    .bind(task_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(row.try_get::<String, _>("task_status")?, "STOPPING");
    assert_eq!(row.try_get::<String, _>("attempt_status")?, "STOPPING");
    assert!(
        row.try_get::<Option<DateTime<Utc>>, _>("stop_requested_at")?
            .is_some()
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("stop_reason")?.as_deref(),
        Some("user_requested")
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("desired_terminal_status")?
            .as_deref(),
        Some("CANCELED")
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn stop_task_is_idempotent_when_task_is_already_stopping_via_api() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let now = Utc::now();
    let task_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let node_id = Uuid::now_v7();
    let payload = sample_create_task_payload("manual");
    let repository = TaskRepository::new(db.pool.clone());
    upsert_test_node(
        &repository,
        node_id,
        "http://127.0.0.1:65535",
        "http://stream.example",
    )
    .await?;

    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
        ) values (
          $1, 'stopping-task', 'stream_ingest'::task_type, 'STOPPING'::task_status, $2,
          50, $3, $3, 'tester', $4,
          1, 'manual', $5, $5, $5, null
        )
        "#,
    )
    .bind(task_id)
    .bind(format!("stopping-task-{task_id}"))
    .bind(&payload)
    .bind(node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;

    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
          rtp_port, exit_code, failure_code, failure_reason,
          checkpoint_json, started_at, ended_at, created_at,
          lease_token, stop_requested_at, stop_reason, desired_terminal_status
        ) values (
          $1, $2, 1, $3, 'hybrid'::worker_kind, 'STOPPING'::attempt_status,
          4321, null, null, null, null, null,
          null, null, null, null,
          null, $4, null, $4,
          'lease-1', $5, 'user_requested', 'CANCELED'::task_status
        )
        "#,
    )
    .bind(attempt_id)
    .bind(task_id)
    .bind(node_id)
    .bind(now)
    .bind(now - chrono::Duration::seconds(10))
    .execute(&db.pool)
    .await?;

    let app = build_app(test_app_state(db.pool.clone()));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/tasks/{task_id}/stop"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = json_body(response).await;
    assert_eq!(body["id"], json!(task_id));
    assert_eq!(body["status"], json!("STOPPING"));

    let stop_request_events: i64 = sqlx::query_scalar(
        r#"
        select count(*)
          from task_events
         where task_id = $1
           and event_type = 'task_stop_requested'
        "#,
    )
    .bind(task_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(stop_request_events, 0);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn delete_task_allows_lost_state_without_assignment_or_lease_via_api() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let now = Utc::now();
    let task_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let payload = sample_create_task_payload("manual");

    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
        ) values (
          $1, 'lost-task-delete', 'stream_ingest'::task_type, 'LOST'::task_status, $2,
          50, $3, $3, 'tester', null,
          1, 'manual', $4, $4, $4, $4
        )
        "#,
    )
    .bind(task_id)
    .bind(format!("lost-task-delete-{task_id}"))
    .bind(&payload)
    .bind(now)
    .execute(&db.pool)
    .await?;

    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
          rtp_port, exit_code, failure_code, failure_reason,
          checkpoint_json, started_at, ended_at, created_at
        ) values (
          $1, $2, 1, null, 'ffmpeg'::worker_kind, 'FAILED'::attempt_status,
          null, null, null, null, null, null,
          null, null, 'node_disconnected', 'runtime may still be reclaimable',
          null, $3, $3, $3
        )
        "#,
    )
    .bind(attempt_id)
    .bind(task_id)
    .bind(now)
    .execute(&db.pool)
    .await?;

    let repository = TaskRepository::new(db.pool.clone());
    let app = build_app(test_app_state(db.pool.clone()));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/tasks/{task_id}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["id"], json!(task_id));
    assert!(matches!(
        repository.get_task_summary(task_id).await,
        Err(repository::RepoError::TaskNotFound(id)) if id == task_id
    ));

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn delete_task_rejects_lost_state_with_live_lease_via_api() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let now = Utc::now();
    let task_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let payload = sample_create_task_payload("manual");

    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
        ) values (
          $1, 'lost-task-delete-with-lease', 'stream_ingest'::task_type, 'LOST'::task_status, $2,
          50, $3, $3, 'tester', null,
          1, 'manual', $4, $4, $4, $4
        )
        "#,
    )
    .bind(task_id)
    .bind(format!("lost-task-delete-with-lease-{task_id}"))
    .bind(&payload)
    .bind(now)
    .execute(&db.pool)
    .await?;

    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
          rtp_port, exit_code, failure_code, failure_reason,
          checkpoint_json, started_at, ended_at, created_at
        ) values (
          $1, $2, 1, null, 'ffmpeg'::worker_kind, 'FAILED'::attempt_status,
          null, null, null, null, null, null,
          null, null, 'node_disconnected', 'runtime may still be reclaimable',
          null, $3, $3, $3
        )
        "#,
    )
    .bind(attempt_id)
    .bind(task_id)
    .bind(now)
    .execute(&db.pool)
    .await?;

    sqlx::query(
        r#"
        insert into task_leases (task_id, holder, lease_token, node_id, expires_at, updated_at)
        values ($1, 'agent', 'lease-1', null, $2, $2)
        "#,
    )
    .bind(task_id)
    .bind(now + chrono::Duration::minutes(5))
    .execute(&db.pool)
    .await?;

    let repository = TaskRepository::new(db.pool.clone());
    let app = build_app(test_app_state(db.pool.clone()));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/tasks/{task_id}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = json_body(response).await;
    assert_eq!(body["code"], json!("TASK_DELETE_FORBIDDEN"));
    assert_eq!(
        repository.get_task_summary(task_id).await?.status,
        media_domain::TaskStatus::Lost
    );

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
async fn preview_live_stream_ingest_falls_back_to_http_fmp4_when_expose_is_all_disabled()
-> anyhow::Result<()> {
    let pool = PgPoolOptions::new().connect_lazy("postgresql://postgres@127.0.0.1/postgres")?;
    let app = build_app(test_app_state(pool));
    let mut payload = sample_create_task_payload("manual");
    payload["expose"] = json!({
        "enable_rtsp": false,
        "enable_rtmp": false,
        "enable_http_ts": false,
        "enable_http_fmp4": false,
        "enable_hls": false
    });
    payload["record"] = json!({
        "enabled": false
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
    let expose = &body["resolved_spec"]["expose"];
    assert_eq!(expose["enable_rtsp"], json!(false));
    assert_eq!(expose["enable_rtmp"], json!(false));
    assert_eq!(expose["enable_http_ts"], json!(false));
    assert_eq!(expose["enable_http_fmp4"], json!(true));
    assert_eq!(expose["enable_hls"], json!(false));
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
    if tasks.status() == StatusCode::SERVICE_UNAVAILABLE {
        let html = to_bytes(tasks.into_body(), usize::MAX).await?;
        let html = String::from_utf8(html.to_vec())?;
        assert!(html.contains("控制台静态资源不可用"));
        return Ok(());
    }

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
        folder: Some("/data/zlm/www/record/live/camera01".to_string()),
        start_time: None,
        time_len: None,
        url: None,
        m3u8_url: None,
    };

    assert_eq!(
        resolve_record_hls_file_path(&hook).as_deref(),
        Some("/data/zlm/www/record/live/camera01/index.m3u8")
    );
}

#[test]
fn build_publish_hook_response_uses_expose_policy_without_auto_recording() {
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
    assert_eq!(response["add_mute_audio"], json!(false));
    assert_eq!(response["enable_mp4"], json!(false));
    assert_eq!(response["auto_close"], json!(true));
    assert_eq!(response["mp4_as_player"], json!(true));
}

#[test]
fn build_publish_hook_response_uses_documented_defaults_without_task_spec() {
    let response = build_publish_hook_response(None, true);

    assert_eq!(response["enable_rtsp"], json!(true));
    assert_eq!(response["enable_rtmp"], json!(true));
    assert_eq!(response["enable_ts"], json!(true));
    assert_eq!(response["add_mute_audio"], json!(false));
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
                upload_disk_total_bytes: 1_000,
                upload_disk_available_bytes: 390,
                upload_disk_used_percent: 61.0,
                running_tasks: 2,
                starting_tasks: 0,
                stopping_tasks: 0,
                orphaned_tasks: 0,
                slot_usage: 0.4,
                zlm_alive: true,
                ffmpeg_alive: true,
                artifact_cleanup_blocked: false,
                artifact_cleanup_block_reason: None,
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
                upload_disk_total_bytes: 1_000,
                upload_disk_available_bytes: 370,
                upload_disk_used_percent: 63.0,
                running_tasks: 3,
                starting_tasks: 0,
                stopping_tasks: 0,
                orphaned_tasks: 0,
                slot_usage: 0.55,
                zlm_alive: true,
                ffmpeg_alive: false,
                artifact_cleanup_blocked: false,
                artifact_cleanup_block_reason: None,
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
async fn node_heartbeat_does_not_refresh_media_last_seen_at() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = TaskRepository::new(db.pool.clone());
    let node_id = Uuid::now_v7();
    let server_id = format!("zlm-{node_id}");
    upsert_test_node(
        &repository,
        node_id,
        "http://127.0.0.1:65535",
        "http://stream.example",
    )
    .await?;
    let media_seen_at = Utc::now() - chrono::Duration::seconds(40);
    let stored_media_seen_at =
        DateTime::<Utc>::from_timestamp_micros(media_seen_at.timestamp_micros())
            .expect("test timestamp should be representable");
    repository
        .record_media_server_seen(node_id, &server_id, media_seen_at)
        .await?;
    repository
        .record_node_heartbeat(
            node_id,
            &HeartbeatSnapshot {
                node_time: Utc::now(),
                cpu_percent: 10.0,
                mem_percent: 20.0,
                disk_percent: 30.0,
                upload_disk_total_bytes: 1_000,
                upload_disk_available_bytes: 700,
                upload_disk_used_percent: 30.0,
                running_tasks: 1,
                starting_tasks: 0,
                stopping_tasks: 0,
                orphaned_tasks: 0,
                slot_usage: 0.2,
                zlm_alive: true,
                ffmpeg_alive: true,
                artifact_cleanup_blocked: false,
                artifact_cleanup_block_reason: None,
                gpu_runtime: Vec::new(),
            },
        )
        .await?;

    let nodes = repository.list_nodes().await?;
    let node = nodes
        .into_iter()
        .find(|candidate| candidate.id == node_id)
        .expect("node should exist");
    assert_eq!(node.media_last_seen_at, Some(stored_media_seen_at));
    assert!(!node.media_alive);
    assert!(node.control_connected);
    assert!(node.healthy);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn node_heartbeat_marks_current_control_session_connected() -> anyhow::Result<()> {
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
    repository.update_node_health(node_id, false, None).await?;

    let heartbeat_time = Utc::now();
    let stored_heartbeat_time =
        DateTime::<Utc>::from_timestamp_micros(heartbeat_time.timestamp_micros())
            .expect("test timestamp should be representable");
    repository
        .record_node_heartbeat(
            node_id,
            &HeartbeatSnapshot {
                node_time: heartbeat_time,
                cpu_percent: 10.0,
                mem_percent: 20.0,
                disk_percent: 30.0,
                upload_disk_total_bytes: 1_000,
                upload_disk_available_bytes: 700,
                upload_disk_used_percent: 30.0,
                running_tasks: 1,
                starting_tasks: 0,
                stopping_tasks: 0,
                orphaned_tasks: 0,
                slot_usage: 0.2,
                zlm_alive: true,
                ffmpeg_alive: true,
                artifact_cleanup_blocked: false,
                artifact_cleanup_block_reason: None,
                gpu_runtime: Vec::new(),
            },
        )
        .await?;

    let nodes = repository.list_nodes().await?;
    let node = nodes
        .into_iter()
        .find(|candidate| candidate.id == node_id)
        .expect("node should exist");
    assert!(node.control_connected);
    assert!(node.healthy);
    assert_eq!(node.control_last_seen_at, Some(stored_heartbeat_time));

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn publish_lookup_requires_binding_when_node_has_multiple_media_servers() -> anyhow::Result<()>
{
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = TaskRepository::new(db.pool.clone());
    let node_id = Uuid::now_v7();
    let primary_server_id = format!("zlm-{node_id}");
    let secondary_server_id = format!("zlm-secondary-{node_id}");
    upsert_test_node(
        &repository,
        node_id,
        "http://127.0.0.1:65535",
        "http://stream.example",
    )
    .await?;
    repository
        .record_media_server_seen(node_id, &secondary_server_id, Utc::now())
        .await?;

    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
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
    .bind(format!("publish-{task_id}"))
    .bind(&resolved_spec)
    .bind(node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
          rtp_port, exit_code, failure_code, failure_reason,
          checkpoint_json, started_at, ended_at, created_at, lease_token
        ) values (
          $1, $2, 1, $3, 'zlm_proxy'::worker_kind, 'STARTING'::attempt_status,
          null, null, 'rtsp', '__defaultVhost__', 'live', 'camera01',
          null, null, null, null,
          null, $4, null, $4, 'lease-1'
        )
        "#,
    )
    .bind(attempt_id)
    .bind(task_id)
    .bind(node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;

    let without_binding = repository
        .find_task_for_publish_stream(&secondary_server_id, "__defaultVhost__", "live", "camera01")
        .await?;
    assert!(without_binding.is_none());

    sqlx::query(
        r#"
        insert into stream_bindings (
          id, task_id, attempt_id, server_id, node_id, schema, vhost, app, stream,
          zlm_proxy_key, zlm_pusher_key, rtp_stream_id, created_at
        ) values (
          $1, $2, $3, $4, $5, 'rtsp', '__defaultVhost__', 'live', 'camera01',
          null, null, null, $6
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(task_id)
    .bind(attempt_id)
    .bind(&secondary_server_id)
    .bind(node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;

    let with_binding = repository
        .find_task_for_publish_stream(&secondary_server_id, "__defaultVhost__", "live", "camera01")
        .await?;
    assert_eq!(with_binding.map(|target| target.task_id), Some(task_id));

    let primary_lookup = repository
        .find_task_for_publish_stream(&primary_server_id, "__defaultVhost__", "live", "camera01")
        .await?;
    assert!(primary_lookup.is_none());

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
        "type": "stream_ingest",
        "name": "relay-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
        "expose": {
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
            .any(|value| value == "rtsp://stream.example:554/live/camera01")
    );
    assert!(
        play_urls
            .iter()
            .any(|value| value == "rtmp://stream.example:1935/live/camera01")
    );
    assert!(
        play_urls
            .iter()
            .any(|value| value == "http://stream.example/live/camera01.live.flv")
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
async fn list_streams_orders_by_stream_or_task_created_at_desc() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = TaskRepository::new(db.pool.clone());
    let node_id = Uuid::now_v7();
    upsert_test_node(
        &repository,
        node_id,
        "http://127.0.0.1:8081",
        "http://stream.example",
    )
    .await?;

    let now = Utc::now();
    insert_running_stream_task_with_times(
        &db.pool,
        node_id,
        "older-stream",
        "camera-old",
        now - chrono::Duration::minutes(30),
        now + chrono::Duration::minutes(30),
        now - chrono::Duration::minutes(20),
    )
    .await?;
    insert_running_stream_task_with_times(
        &db.pool,
        node_id,
        "newer-stream",
        "camera-new",
        now - chrono::Duration::minutes(5),
        now - chrono::Duration::minutes(25),
        now - chrono::Duration::minutes(10),
    )
    .await?;

    let streams = repository
        .list_streams(StreamListFilter {
            schema: None,
            app: None,
            stream: None,
            task_id: None,
            node_id: None,
            has_viewer: None,
        })
        .await?;

    assert_eq!(
        streams
            .iter()
            .map(|stream| stream.stream.as_str())
            .collect::<Vec<_>>(),
        vec!["camera-new", "camera-old"]
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn list_streams_uses_current_node_stream_ports() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let (zlm_base, zlm_handle) = spawn_zlm_stub().await?;
    let repository = TaskRepository::new(db.pool.clone());
    let node_id = Uuid::now_v7();
    upsert_test_node_with_ports(
        &repository,
        node_id,
        &zlm_base,
        "http://stream.example:18080",
        2935,
        9554,
    )
    .await?;
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-camera-ports",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
        "expose": {
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
    let play_urls = items[0]["play_urls"]
        .as_array()
        .expect("play_urls should be a list");
    assert!(
        play_urls
            .iter()
            .any(|value| value == "rtsp://stream.example:9554/live/camera01")
    );
    assert!(
        play_urls
            .iter()
            .any(|value| value == "rtmp://stream.example:2935/live/camera01")
    );
    assert!(
        play_urls
            .iter()
            .any(|value| value == "http://stream.example:18080/live/camera01.live.flv")
    );
    assert!(
        play_urls
            .iter()
            .any(|value| value == "http://stream.example:18080/live/camera01/hls.m3u8")
    );

    zlm_handle.abort();
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn list_streams_collapses_duplicate_bindings_for_same_logical_stream() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let (zlm_base, zlm_handle) = spawn_zlm_stub().await?;
    let repository = TaskRepository::new(db.pool.clone());
    let node_id = Uuid::now_v7();
    upsert_test_node(&repository, node_id, &zlm_base, "http://stream.example").await?;
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
        "expose": {
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
    let task_id =
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;
    let attempt_id: Uuid = sqlx::query_scalar(
        r#"
        select id
          from task_attempts
         where task_id = $1
           and attempt_no = 1
        "#,
    )
    .bind(task_id)
    .fetch_one(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into stream_bindings (
          id, task_id, attempt_id, server_id, node_id, schema, vhost, app, stream,
          zlm_proxy_key, zlm_pusher_key, rtp_stream_id, created_at
        ) values (
          $1, $2, $3, $4, $5, 'rtmp', '__defaultVhost__', 'live', 'camera01',
          null, null, null, $6
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(task_id)
    .bind(attempt_id)
    .bind(format!("zlm-{node_id}"))
    .bind(node_id)
    .bind(Utc::now())
    .execute(&db.pool)
    .await?;

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
    assert_eq!(items[0]["schema"], json!("rtsp"));
    assert_eq!(items[0]["stream"], json!("camera01"));

    zlm_handle.abort();
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn recording_control_command_allows_running_realtime_stream_ingest() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = TaskRepository::new(db.pool.clone());
    let node_id = Uuid::now_v7();
    upsert_test_node(
        &repository,
        node_id,
        "http://127.0.0.1:8081",
        "http://stream.example",
    )
    .await?;
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01", "vhost": "__defaultVhost__"},
        "expose": {"enable_rtsp": true},
        "record": {"enabled": false},
        "recovery": {"policy": "auto"},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;
    sqlx::query("update task_attempts set lease_token = 'lease-recording' where task_id = $1")
        .bind(task_id)
        .execute(&db.pool)
        .await?;

    let command = repository.build_recording_control_command(task_id).await?;

    assert_eq!(command.task_id, task_id);
    assert_eq!(command.attempt_no, 1);
    assert_eq!(command.node_id, node_id);
    assert_eq!(command.lease_token, "lease-recording");

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn list_streams_returns_fallback_entries_when_runtime_lookup_fails() -> anyhow::Result<()> {
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
        "name": "relay-camera-runtime-down",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
        "expose": {},
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
    assert_eq!(items[0]["stream"], json!("camera01"));
    assert!(items[0].get("viewer_count").is_none());
    assert_eq!(items[0]["has_viewer"], Value::Null);
    assert_eq!(
        items[0]["play_urls"],
        json!(["rtsp://stream.example:554/live/camera01"])
    );

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
        "type": "stream_ingest",
        "priority": 50,
        "common": {
            "created_by": "alice",
            "callback_url": "not-a-url"
        },
        "input": {
            "kind": "rtsp",
            "url": "rtsp://camera.example/live"
        },
        "expose": {
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
async fn callback_dispatcher_waits_for_record_artifact_before_first_terminal_callback()
-> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::with_callback_delays(
        db.pool.clone(),
        chrono::Duration::zero(),
        chrono::Duration::seconds(30),
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
        "name": "relay-camera-01",
        "common": {"created_by": "tester", "callback_url": callback_url},
        "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
        "expose": {
            "enable_rtsp": true,
            "enable_http_ts": true
        },
        "record": {"enabled": true, "format": "mp4"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;
    repository
        .record_agent_task_event(
            node_id,
            repository::AgentTaskEventRecord {
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                event_type: "succeeded".to_string(),
                event_level: "info".to_string(),
                message: "finished".to_string(),
                payload: json!({}),
            },
        )
        .await?;

    let initial_deliver_after =
        pending_callback_deliver_after(&db.pool, task_id, 1, "terminal_state")
            .await?
            .expect("terminal callback should be enqueued");
    assert!(
        initial_deliver_after >= Utc::now() + chrono::Duration::seconds(25),
        "record-producing tasks should hold terminal callbacks for artifact wait"
    );

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
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert_eq!(calls.lock().await.len(), 0);

    repository
        .record_zlm_record_file_hook(
            &format!("zlm-{node_id}"),
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

    let expedited_deliver_after =
        pending_callback_deliver_after(&db.pool, task_id, 1, "terminal_state")
            .await?
            .expect("terminal callback should remain pending until delivery");
    assert!(expedited_deliver_after <= Utc::now());

    let delivered_calls = wait_for_callback_count(&calls, 1).await?;
    assert_eq!(delivered_calls.len(), 1);
    assert_eq!(delivered_calls[0].1["event_type"], json!("task.completed"));
    assert_eq!(delivered_calls[0].1["reason"], json!("terminal_state"));
    assert_eq!(delivered_calls[0].1["task"]["status"], json!("SUCCEEDED"));
    assert!(
        delivered_calls[0].1["streams"][0]["play_urls"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|value| value == "rtsp://stream.example:554/live/camera01")
    );
    assert_eq!(
        delivered_calls[0]
            .0
            .get("X-StreamServer-Signature")
            .and_then(|value| value.to_str().ok())
            .is_some(),
        true
    );
    assert_eq!(
        delivered_calls[0].1["records"][0]["http_url"],
        json!("http://stream.example/record/live/camera01/clip.mp4")
    );
    assert_eq!(
        delivered_calls[0].1["records"][0]["file_path"],
        json!("/record/live/camera01/clip.mp4")
    );
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert_eq!(calls.lock().await.len(), 1);

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
        "type": "stream_ingest",
        "name": "relay-camera-01",
        "common": {"created_by": "tester", "callback_url": callback_url},
        "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
        "expose": {
            "enable_rtsp": true,
            "enable_http_ts": true
        },
        "record": {"enabled": false},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_starting_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;
    repository
        .record_agent_task_event(
            node_id,
            repository::AgentTaskEventRecord {
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
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
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;

    repository
        .record_zlm_record_file_hook(
            &format!("zlm-{node_id}"),
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
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "expose": {
            "enable_rtsp": false,
            "enable_rtmp": false,
            "enable_http_ts": false,
            "enable_http_fmp4": true,
            "enable_hls": false
        },
        "process": {"mode": "copy_or_transcode"},
        "record": {"enabled": true, "format": "hls"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;

    repository
        .record_zlm_record_file_hook(
            &format!("zlm-{node_id}"),
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
                url: Some("http://stream.example/record/live/camera01/index-00001.ts".to_string()),
            },
        )
        .await?;
    repository
        .record_zlm_record_file_hook(
            &format!("zlm-{node_id}"),
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
    assert_eq!(records[0].file_path, "/record/live/camera01/index.m3u8");
    assert_eq!(
        records[0].http_url.as_deref(),
        Some("http://stream.example/record/live/camera01/index.m3u8")
    );
    let stored_http_url: Option<String> = sqlx::query_scalar(
        "select http_url from record_files where task_id = $1 and file_path like '%index.m3u8'",
    )
    .bind(task_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        stored_http_url.as_deref(),
        Some("/record/live/camera01/index.m3u8")
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn record_file_http_url_uses_latest_node_stream_addr() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = TaskRepository::new(db.pool.clone());
    let node_id = Uuid::now_v7();
    upsert_test_node_with_ports(
        &repository,
        node_id,
        "http://127.0.0.1:65535",
        "http://stream.example:18080",
        1935,
        554,
    )
    .await?;
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "record-mp4-current-node-url",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "process": {"mode": "copy_or_transcode"},
        "record": {"enabled": true, "format": "mp4"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;

    repository
        .record_zlm_record_file_hook(
            &format!("zlm-{node_id}"),
            "on_record_mp4",
            "record-http-url-rebind",
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

    let first_records = repository.list_task_record_files(task_id).await?;
    assert_eq!(first_records.len(), 1);
    assert_eq!(
        first_records[0].http_url.as_deref(),
        Some("http://stream.example:18080/record/live/camera01/clip.mp4")
    );
    let stored_http_url: Option<String> =
        sqlx::query_scalar("select http_url from record_files where task_id = $1 limit 1")
            .bind(task_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        stored_http_url.as_deref(),
        Some("/record/live/camera01/clip.mp4")
    );

    upsert_test_node_with_ports(
        &repository,
        node_id,
        "http://127.0.0.1:65535",
        "http://stream-new.example:19090",
        1935,
        554,
    )
    .await?;

    let second_records = repository.list_task_record_files(task_id).await?;
    assert_eq!(second_records.len(), 1);
    assert_eq!(
        second_records[0].http_url.as_deref(),
        Some("http://stream-new.example:19090/record/live/camera01/clip.mp4")
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn task_events_endpoint_externalizes_record_file_paths() -> anyhow::Result<()> {
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
        "name": "record-mp4",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "process": {"mode": "copy_or_transcode"},
        "record": {"enabled": true, "format": "mp4"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;

    repository
        .record_zlm_record_file_hook(
            &format!("zlm-{node_id}"),
            "on_record_mp4",
            "record-event-paths",
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
    let attempt_id: Uuid =
        sqlx::query_scalar("select id from task_attempts where task_id = $1 and attempt_no = 1")
            .bind(task_id)
            .fetch_one(&db.pool)
            .await?;
    sqlx::query(
        r#"
        insert into task_events (
          id, task_id, attempt_id, attempt_no, source, event_type, event_level,
          payload, created_at
        ) values (
          $1, $2, $3, 1, 'zlm_hook'::event_source, 'record_file_persisted', 'info',
          $4, $5
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(task_id)
    .bind(attempt_id)
    .bind(json!({
        "file_path": "/data/zlm/www/record/live/camera01/clip.mp4",
        "folder": "/data/zlm/www/record/live/camera01"
    }))
    .bind(Utc::now() + chrono::Duration::milliseconds(1))
    .execute(&db.pool)
    .await?;

    let app = build_app(test_app_state(db.pool.clone()));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/tasks/{task_id}/events?page=1&page_size=10"
                ))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(
        body["items"][0]["payload"]["file_path"],
        json!("/record/live/camera01/clip.mp4")
    );
    assert_eq!(
        body["items"][0]["payload"]["folder"],
        json!("/record/live/camera01")
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
        "type": "stream_ingest",
        "name": "relay-camera-01",
        "common": {"created_by": "tester", "callback_url": callback_url},
        "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
        "expose": {
            "enable_rtsp": true
        },
        "record": {"enabled": false},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_starting_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;

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
                lease_token: "lease-1".to_string(),
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
                lease_token: "lease-1".to_string(),
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
                lease_token: "lease-1".to_string(),
                worker_kind: "ffmpeg".to_string(),
                pid: Some(1234),
                state: "RUNNING".to_string(),
                command_line: Some("ffmpeg ...".to_string()),
                outputs: vec!["/data/zlm/www/artifacts/transcode/verify/output.mp4".to_string()],
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
                lease_token: "lease-1".to_string(),
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
        delivered[0].1["file_artifacts"][0]["file_path"],
        json!("/artifacts/transcode/verify/output.mp4")
    );
    assert_eq!(
        delivered[0].1["file_artifacts"][0]["artifact_kind"],
        json!("transcode_output")
    );
    let stored_http_url: String =
        sqlx::query_scalar("select http_url from transcode_artifacts where task_id = $1 limit 1")
            .bind(task_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(stored_http_url, "/artifacts/transcode/verify/output.mp4");

    let _ = shutdown_tx.send(true);
    dispatcher.abort();
    callback_handle.abort();
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn orphaned_event_with_stop_intent_reconciles_to_canceled() -> anyhow::Result<()> {
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

    let task_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let stop_requested_at = Utc::now() - chrono::Duration::seconds(10);
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });

    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
        ) values (
          $1, 'relay-camera-01', 'stream_ingest'::task_type, 'LOST'::task_status, $2,
          50, $3, $3, 'tester', null,
          1, 'immediate', $4, $4, $4, $4
        )
        "#,
    )
    .bind(task_id)
    .bind(format!("orphaned-stop-{task_id}"))
    .bind(&resolved_spec)
    .bind(stop_requested_at)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
          rtp_port, exit_code, failure_code, failure_reason,
          checkpoint_json, started_at, ended_at, created_at,
          lease_token, stop_requested_at, stop_reason, desired_terminal_status
        ) values (
          $1, $2, 1, $3, 'zlm_proxy'::worker_kind, 'STOPPING'::attempt_status,
          null, null, 'rtsp', '__defaultVhost__', 'live', 'camera01',
          null, null, 'node_disconnected', 'control-plane session closed before task completed',
          null, $4, null, $4,
          'lease-1', $5, 'user_requested', 'CANCELED'::task_status
        )
        "#,
    )
    .bind(attempt_id)
    .bind(task_id)
    .bind(node_id)
    .bind(stop_requested_at)
    .bind(stop_requested_at)
    .execute(&db.pool)
    .await?;

    repository
        .record_agent_task_event(
            node_id,
            repository::AgentTaskEventRecord {
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                event_type: "orphaned".to_string(),
                event_level: "warn".to_string(),
                message: "runtime missing".to_string(),
                payload: json!({"reason": "runtime_not_found"}),
            },
        )
        .await?;

    let summary = repository.get_task_summary(task_id).await?;
    assert_eq!(summary.status, media_domain::TaskStatus::Stopping);

    let candidates = repository.list_stopping_reconcile_tasks().await?;
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].attempt_status,
        media_domain::AttemptStatus::Orphaned
    );
    assert!(repository.complete_stopping_task(&candidates[0]).await?);

    let completed = repository.get_task_summary(task_id).await?;
    assert_eq!(completed.status, media_domain::TaskStatus::Canceled);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn orphaned_running_attempt_marks_lost_and_auto_retries() -> anyhow::Result<()> {
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

    let task_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let started_at = Utc::now() - chrono::Duration::seconds(30);
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });

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
    .bind(format!("orphaned-running-{task_id}"))
    .bind(&resolved_spec)
    .bind(node_id)
    .bind(started_at)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
          rtp_port, exit_code, failure_code, failure_reason,
          checkpoint_json, started_at, ended_at, created_at, lease_token
        ) values (
          $1, $2, 1, $3, 'zlm_proxy'::worker_kind, 'RUNNING'::attempt_status,
          1234, null, 'rtsp', '__defaultVhost__', 'live', 'camera01',
          null, null, null, null,
          null, $4, null, $4, 'lease-1'
        )
        "#,
    )
    .bind(attempt_id)
    .bind(task_id)
    .bind(node_id)
    .bind(started_at)
    .execute(&db.pool)
    .await?;

    repository
        .record_agent_task_event(
            node_id,
            repository::AgentTaskEventRecord {
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                event_type: "orphaned".to_string(),
                event_level: "warn".to_string(),
                message: "runtime missing".to_string(),
                payload: json!({"reason": "runtime_not_found"}),
            },
        )
        .await?;

    let summary = repository.get_task_summary(task_id).await?;
    assert_eq!(summary.status, media_domain::TaskStatus::Queued);
    assert_eq!(summary.current_attempt_no, 2);
    assert_eq!(summary.assigned_node_id, None);

    let attempts = sqlx::query(
        r#"
        select attempt_no, status::text as status, failure_code, node_id
          from task_attempts
         where task_id = $1
         order by attempt_no asc
        "#,
    )
    .bind(task_id)
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].try_get::<i32, _>("attempt_no")?, 1);
    assert_eq!(attempts[0].try_get::<String, _>("status")?, "FAILED");
    assert_eq!(
        attempts[0].try_get::<Option<String>, _>("failure_code")?,
        Some("runtime_not_found".to_string())
    );
    assert_eq!(attempts[1].try_get::<i32, _>("attempt_no")?, 2);
    assert_eq!(attempts[1].try_get::<String, _>("status")?, "PENDING");
    assert_eq!(attempts[1].try_get::<Option<Uuid>, _>("node_id")?, None);

    let event_count: i64 = sqlx::query_scalar(
        "select count(*) from task_events where task_id = $1 and event_type = 'task_lost_after_reclaim_orphaned'",
    )
    .bind(task_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(event_count, 1);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn orphaned_running_attempt_with_retry_disabled_stays_lost() -> anyhow::Result<()> {
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

    let task_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let started_at = Utc::now() - chrono::Duration::seconds(30);
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-camera-02",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera02"},
        "recovery": {"policy": "never"},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });

    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
        ) values (
          $1, 'relay-camera-02', 'stream_ingest'::task_type, 'RUNNING'::task_status, $2,
          50, $3, $3, 'tester', $4,
          1, 'immediate', $5, $5, $5, null
        )
        "#,
    )
    .bind(task_id)
    .bind(format!("orphaned-never-{task_id}"))
    .bind(&resolved_spec)
    .bind(node_id)
    .bind(started_at)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
          rtp_port, exit_code, failure_code, failure_reason,
          checkpoint_json, started_at, ended_at, created_at, lease_token
        ) values (
          $1, $2, 1, $3, 'zlm_proxy'::worker_kind, 'RUNNING'::attempt_status,
          1234, null, 'rtsp', '__defaultVhost__', 'live', 'camera02',
          null, null, null, null,
          null, $4, null, $4, 'lease-1'
        )
        "#,
    )
    .bind(attempt_id)
    .bind(task_id)
    .bind(node_id)
    .bind(started_at)
    .execute(&db.pool)
    .await?;

    repository
        .record_agent_task_event(
            node_id,
            repository::AgentTaskEventRecord {
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                event_type: "orphaned".to_string(),
                event_level: "warn".to_string(),
                message: "runtime missing".to_string(),
                payload: json!({"reason": "runtime_not_found"}),
            },
        )
        .await?;

    let summary = repository.get_task_summary(task_id).await?;
    assert_eq!(summary.status, media_domain::TaskStatus::Lost);
    assert_eq!(summary.current_attempt_no, 1);
    assert_eq!(summary.assigned_node_id, None);

    let attempts = sqlx::query(
        r#"
        select attempt_no, status::text as status, failure_code
          from task_attempts
         where task_id = $1
         order by attempt_no asc
        "#,
    )
    .bind(task_id)
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].try_get::<i32, _>("attempt_no")?, 1);
    assert_eq!(attempts[0].try_get::<String, _>("status")?, "FAILED");
    assert_eq!(
        attempts[0].try_get::<Option<String>, _>("failure_code")?,
        Some("runtime_not_found".to_string())
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn dispatch_reuses_pending_retry_attempt() -> anyhow::Result<()> {
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

    let task_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let now = Utc::now();
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "retry-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "retry-camera01"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });

    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
        ) values (
          $1, 'retry-camera-01', 'stream_ingest'::task_type, 'FAILED'::task_status, $2,
          50, $3, $3, 'tester', null,
          1, 'immediate', $4, $4, $4, $4
        )
        "#,
    )
    .bind(task_id)
    .bind(format!("retry-dispatch-{task_id}"))
    .bind(&resolved_spec)
    .bind(now)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
          rtp_port, exit_code, failure_code, failure_reason,
          checkpoint_json, started_at, ended_at, created_at
        ) values (
          $1, $2, 1, null, 'zlm_proxy'::worker_kind, 'FAILED'::attempt_status,
          null, null, null, null, null, null,
          null, null, 'agent_failed', 'failed before retry',
          null, $3, $3, $3
        )
        "#,
    )
    .bind(attempt_id)
    .bind(task_id)
    .bind(now)
    .execute(&db.pool)
    .await?;

    let retry = repository.retry_task(task_id).await?;
    assert_eq!(retry.attempt_no, 2);
    let command = repository
        .prepare_task_dispatch(task_id, node_id, "test-holder")
        .await?;
    assert_eq!(command.attempt_no, 2);

    let attempts = sqlx::query(
        r#"
        select attempt_no, status::text as status, node_id, nullif(lease_token, '') as lease_token
          from task_attempts
         where task_id = $1
         order by attempt_no asc
        "#,
    )
    .bind(task_id)
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].try_get::<i32, _>("attempt_no")?, 2);
    assert_eq!(attempts[1].try_get::<String, _>("status")?, "PENDING");
    assert_eq!(
        attempts[1].try_get::<Option<Uuid>, _>("node_id")?,
        Some(node_id)
    );
    assert!(
        attempts[1]
            .try_get::<Option<String>, _>("lease_token")?
            .is_some()
    );

    let summary = repository.get_task_summary(task_id).await?;
    assert_eq!(summary.status, media_domain::TaskStatus::Dispatching);
    assert_eq!(summary.current_attempt_no, 2);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn record_agent_snapshot_ignores_missing_attempt_without_sql_error() -> anyhow::Result<()> {
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

    let task_id = Uuid::now_v7();
    let now = Utc::now();
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "snapshot-missing-attempt",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });

    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at
        ) values (
          $1, 'snapshot-missing-attempt', 'stream_ingest'::task_type, 'RUNNING'::task_status, $2,
          50, $3, $3, 'tester', $4,
          1, 'immediate', $5, $5, $5
        )
        "#,
    )
    .bind(task_id)
    .bind(format!("snapshot-missing-attempt-{task_id}"))
    .bind(&resolved_spec)
    .bind(node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;

    repository
        .record_agent_snapshot(
            node_id,
            repository::TaskSnapshotRecord {
                runtime_id: Uuid::now_v7(),
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                worker_kind: "ffmpeg".to_string(),
                pid: Some(1234),
                state: "RUNNING".to_string(),
                command_line: Some("ffmpeg ...".to_string()),
                outputs: Vec::new(),
                metadata: json!({}),
            },
        )
        .await?;

    let snapshot_event_count: i64 = sqlx::query_scalar(
        "select count(*) from task_events where task_id = $1 and event_type = 'task_snapshot'",
    )
    .bind(task_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(snapshot_event_count, 0);
    let stale_event_count: i64 = sqlx::query_scalar(
        "select count(*) from task_events where task_id = $1 and event_type = 'stale_agent_message'",
    )
    .bind(task_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(stale_event_count, 1);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn exited_snapshot_does_not_override_terminal_success() -> anyhow::Result<()> {
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
        "name": "snapshot-after-success",
        "common": {"created_by": "tester"},
        "input": {"kind": "file", "source_mode": "vod", "url": "input.ts"},
        "stream": {"app": "live", "name": "snapshot-after-success"},
        "process": {"mode": "transcode"},
        "record": {"enabled": true, "format": "mp4"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id = insert_running_ingest_task(&db.pool, node_id, resolved_spec).await?;

    repository
        .record_agent_task_event(
            node_id,
            repository::AgentTaskEventRecord {
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                event_type: "succeeded".to_string(),
                event_level: "info".to_string(),
                message: "finished".to_string(),
                payload: json!({
                    "exit_code": 0
                }),
            },
        )
        .await?;

    repository
        .record_agent_snapshot(
            node_id,
            repository::TaskSnapshotRecord {
                runtime_id: Uuid::now_v7(),
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                worker_kind: "ffmpeg".to_string(),
                pid: Some(1234),
                state: "EXITED".to_string(),
                command_line: Some("ffmpeg ...".to_string()),
                outputs: Vec::new(),
                metadata: json!({}),
            },
        )
        .await?;

    let detail = repository.get_task(task_id).await?;
    assert_eq!(detail.task.status, media_domain::TaskStatus::Succeeded);
    assert_eq!(
        detail
            .current_attempt
            .as_ref()
            .map(|attempt| attempt.status),
        Some(media_domain::AttemptStatus::Succeeded)
    );
    assert_eq!(
        detail
            .current_attempt
            .as_ref()
            .and_then(|attempt| attempt.failure_code.as_deref()),
        None
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn list_reclaim_runtimes_includes_dispatching_attempts_with_leases() -> anyhow::Result<()> {
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

    let task_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let now = Utc::now();
    let lease_token = "lease-dispatching-reclaim";
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "dispatching-reclaim",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });

    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at
        ) values (
          $1, 'dispatching-reclaim', 'stream_ingest'::task_type, 'DISPATCHING'::task_status, $2,
          50, $3, $3, 'tester', $4,
          1, 'immediate', $5, $5
        )
        "#,
    )
    .bind(task_id)
    .bind(format!("dispatching-reclaim-{task_id}"))
    .bind(&resolved_spec)
    .bind(node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;

    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          created_at, lease_token
        ) values (
          $1, $2, 1, $3, 'hybrid'::worker_kind, 'PENDING'::attempt_status,
          $4, $5
        )
        "#,
    )
    .bind(attempt_id)
    .bind(task_id)
    .bind(node_id)
    .bind(now)
    .bind(lease_token)
    .execute(&db.pool)
    .await?;

    let reclaim = repository.list_reclaim_runtimes(node_id).await?;
    assert!(reclaim.iter().any(|item| {
        item.task_id == task_id
            && item.attempt_no == 1
            && item.lease_token == lease_token
            && item.worker_kind == media_domain::WorkerKind::Hybrid
    }));

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn callback_payload_includes_file_artifact_http_url_for_bridge_output() -> anyhow::Result<()>
{
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
                lease_token: "lease-1".to_string(),
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
                lease_token: "lease-1".to_string(),
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
        delivered[0].1["file_artifacts"][0]["file_path"],
        json!("/artifacts/bridge/verify/output.mp4")
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
                lease_token: "lease-1".to_string(),
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
                lease_token: "lease-1".to_string(),
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
        delivered[0].1["file_artifacts"][0]["file_path"],
        json!("/artifacts/stream-ingest-record/verify/output.mp4")
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
async fn callback_dispatcher_falls_back_to_terminal_callback_when_artifact_wait_times_out()
-> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::with_callback_delays(
        db.pool.clone(),
        chrono::Duration::zero(),
        chrono::Duration::milliseconds(200),
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
                lease_token: "lease-1".to_string(),
                event_type: "succeeded".to_string(),
                event_level: "info".to_string(),
                message: "finished".to_string(),
                payload: json!({}),
            },
        )
        .await?;

    let initial_deliver_after =
        pending_callback_deliver_after(&db.pool, task_id, 1, "terminal_state")
            .await?
            .expect("terminal callback should be enqueued");
    assert!(initial_deliver_after > Utc::now());

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

    let delivered_calls = wait_for_callback_count(&calls, 1).await?;
    assert_eq!(delivered_calls[0].1["reason"], json!("terminal_state"));
    assert_eq!(delivered_calls[0].1["records"], json!([]));
    assert_eq!(delivered_calls[0].1["file_artifacts"], json!([]));

    let _ = shutdown_tx.send(true);
    dispatcher.abort();
    callback_handle.abort();
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn terminal_callback_uses_normal_settle_delay_for_tasks_without_expected_artifacts()
-> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::with_callback_delays(
        db.pool.clone(),
        chrono::Duration::zero(),
        chrono::Duration::seconds(30),
    ));
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
        "name": "relay-camera-01",
        "common": {"created_by": "tester", "callback_url": "http://example.invalid/callback"},
        "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
        "expose": {
            "enable_rtsp": true,
            "enable_http_ts": true
        },
        "record": {"enabled": false},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;
    repository
        .record_agent_task_event(
            node_id,
            repository::AgentTaskEventRecord {
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                event_type: "succeeded".to_string(),
                event_level: "info".to_string(),
                message: "finished".to_string(),
                payload: json!({}),
            },
        )
        .await?;

    let deliver_after = pending_callback_deliver_after(&db.pool, task_id, 1, "terminal_state")
        .await?
        .expect("terminal callback should be enqueued");
    assert!(deliver_after <= Utc::now());

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn terminal_callback_does_not_wait_when_artifacts_already_exist_before_terminal_state()
-> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::with_callback_delays(
        db.pool.clone(),
        chrono::Duration::zero(),
        chrono::Duration::seconds(30),
    ));
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
        "common": {"created_by": "tester", "callback_url": "http://example.invalid/callback"},
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
                lease_token: "lease-1".to_string(),
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
                lease_token: "lease-1".to_string(),
                event_type: "succeeded".to_string(),
                event_level: "info".to_string(),
                message: "finished".to_string(),
                payload: json!({}),
            },
        )
        .await?;

    let deliver_after = pending_callback_deliver_after(&db.pool, task_id, 1, "terminal_state")
        .await?
        .expect("terminal callback should be enqueued");
    assert!(deliver_after <= Utc::now());

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn callback_dispatcher_delivers_bridge_artifact_update_callback_for_late_artifacts()
-> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::with_callback_delays(
        db.pool.clone(),
        chrono::Duration::zero(),
        chrono::Duration::milliseconds(200),
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
                lease_token: "lease-1".to_string(),
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
                lease_token: "lease-1".to_string(),
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
        second_calls[1].1["file_artifacts"][0]["file_path"],
        json!("/artifacts/bridge/late/output.mp4")
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
async fn late_record_hook_without_stream_binding_backfills_record_and_artifact_callback()
-> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::with_callback_delays(
        db.pool.clone(),
        chrono::Duration::zero(),
        chrono::Duration::milliseconds(200),
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
        "name": "record-only-live",
        "common": {"created_by": "tester", "callback_url": callback_url},
        "input": {
            "kind": "http_ts",
            "source_mode": "live",
            "url": "http://camera.example/live.ts"
        },
        "stream": {"app": "objective", "name": "objective-1"},
        "expose": {
            "enable_rtsp": false,
            "enable_rtmp": false,
            "enable_http_ts": false,
            "enable_http_fmp4": true,
            "enable_hls": false
        },
        "process": {"mode": "copy_or_transcode"},
        "record": {"enabled": true, "format": "mp4"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "objective", "objective-1")
            .await?;
    let record_started_at = Utc::now();
    repository
        .record_agent_task_event(
            node_id,
            repository::AgentTaskEventRecord {
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                event_type: "canceled".to_string(),
                event_level: "info".to_string(),
                message: "stopped".to_string(),
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
    sqlx::query("delete from stream_bindings where task_id = $1")
        .bind(task_id)
        .execute(&db.pool)
        .await?;
    let binding_count: i64 =
        sqlx::query_scalar("select count(*) from stream_bindings where task_id = $1")
            .bind(task_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(binding_count, 0);

    repository
        .record_zlm_record_file_hook(
            &format!("zlm-{node_id}"),
            "on_record_mp4",
            "late-record-hook-without-binding",
            json!({}),
            repository::ZlmRecordFileRecord {
                record_format: Some("mp4".to_string()),
                schema: Some("rtmp".to_string()),
                vhost: "__defaultVhost__".to_string(),
                app: "objective".to_string(),
                stream: "objective-1".to_string(),
                file_path: format!(
                    "/data/zlm/www/output/mp4/node-stream_example-mp4/{task_id}/record/objective/objective-1/2026-04-16/clip.mp4"
                ),
                file_size: 4096,
                time_len_sec: Some(12),
                start_time: Some(record_started_at),
                file_name: Some("clip.mp4".to_string()),
                folder: Some(format!(
                    "/data/zlm/www/output/mp4/node-stream_example-mp4/{task_id}/record/objective/objective-1/2026-04-16"
                )),
                url: None,
            },
        )
        .await?;

    let second_calls = wait_for_callback_count(&calls, 2).await?;
    assert_eq!(second_calls[1].1["reason"], json!("artifact_update"));
    assert_eq!(
        second_calls[1].1["records"].as_array().map(Vec::len),
        Some(1)
    );

    let records = repository.list_task_record_files(task_id).await?;
    assert_eq!(records.len(), 1);
    assert!(records[0].file_path.contains(&task_id.to_string()));
    assert!(
        records[0]
            .http_url
            .as_deref()
            .is_some_and(|value| value.contains(&task_id.to_string()))
    );

    let _ = shutdown_tx.send(true);
    dispatcher.abort();
    callback_handle.abort();
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn record_hook_prefers_task_id_from_managed_output_path_over_active_binding()
-> anyhow::Result<()> {
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
        "name": "record-only-live",
        "common": {"created_by": "tester"},
        "input": {
            "kind": "http_ts",
            "source_mode": "live",
            "url": "http://camera.example/live.ts"
        },
        "stream": {"app": "objective", "name": "objective-1"},
        "expose": {
            "enable_rtsp": false,
            "enable_rtmp": false,
            "enable_http_ts": false,
            "enable_http_fmp4": true,
            "enable_hls": false
        },
        "process": {"mode": "copy_or_transcode"},
        "record": {"enabled": true, "format": "mp4"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });

    let first_task_id = insert_running_stream_task(
        &db.pool,
        node_id,
        resolved_spec.clone(),
        "objective",
        "objective-1",
    )
    .await?;
    let second_task_id =
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "objective", "objective-1")
            .await?;

    let active_binding_task_id: Uuid = sqlx::query_scalar(
        "select task_id from stream_bindings where server_id = $1 and schema = 'rtsp' and vhost = '__defaultVhost__' and app = 'objective' and stream = 'objective-1'",
    )
    .bind(format!("zlm-{node_id}"))
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(active_binding_task_id, second_task_id);

    repository
        .record_zlm_record_file_hook(
            &format!("zlm-{node_id}"),
            "on_record_mp4",
            "record-hook-prefers-path-task-id",
            json!({}),
            repository::ZlmRecordFileRecord {
                record_format: Some("mp4".to_string()),
                schema: Some("rtmp".to_string()),
                vhost: "__defaultVhost__".to_string(),
                app: "objective".to_string(),
                stream: "objective-1".to_string(),
                file_path: format!(
                    "/data/zlm/www/output/mp4/node-172_17_13_196-mp4/{first_task_id}/record/objective/objective-1/2026-04-16/clip.mp4"
                ),
                file_size: 4096,
                time_len_sec: Some(12),
                start_time: None,
                file_name: Some("clip.mp4".to_string()),
                folder: Some(format!(
                    "/data/zlm/www/output/mp4/node-172_17_13_196-mp4/{first_task_id}/record/objective/objective-1/2026-04-16"
                )),
                url: None,
            },
        )
        .await?;

    let first_records = repository.list_task_record_files(first_task_id).await?;
    let second_records = repository.list_task_record_files(second_task_id).await?;
    assert_eq!(first_records.len(), 1);
    assert!(second_records.is_empty());
    assert!(
        first_records[0]
            .file_path
            .contains(&first_task_id.to_string())
    );
    assert!(
        first_records[0]
            .http_url
            .as_deref()
            .is_some_and(|value| value.contains(&first_task_id.to_string()))
    );

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
                lease_token: "lease-1".to_string(),
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
        json!("/artifacts/bridge/verify/output.mp4")
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
                lease_token: "lease-1".to_string(),
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
                .uri(
                    "/api/v1/file-artifacts?artifact_kind=stream_ingest_record&page=1&page_size=10",
                )
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
        json!("/artifacts/stream-ingest-record/verify/output.mp4")
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
        "type": "stream_ingest",
        "name": "relay-camera",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
        "expose": {},
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
async fn list_streams_omits_terminal_and_non_current_attempt_bindings() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let (zlm_base, zlm_handle) = spawn_zlm_stub().await?;
    let repository = TaskRepository::new(db.pool.clone());
    let terminal_node_id = Uuid::now_v7();
    let stale_node_id = Uuid::now_v7();
    upsert_test_node(
        &repository,
        terminal_node_id,
        &zlm_base,
        "http://stream-terminal.example",
    )
    .await?;
    upsert_test_node(
        &repository,
        stale_node_id,
        &zlm_base,
        "http://stream-stale.example",
    )
    .await?;
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-camera-stale",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
        "expose": {},
        "record": {"enabled": false},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let now = Utc::now();

    let terminal_task_id = Uuid::now_v7();
    let terminal_attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
        ) values (
          $1, 'terminal-stream', 'stream_ingest'::task_type, 'SUCCEEDED'::task_status, $2,
          50, $3, $3, 'tester', $4,
          1, 'immediate', $5, $5, $5, $5
        )
        "#,
    )
    .bind(terminal_task_id)
    .bind(format!("terminal-{terminal_task_id}"))
    .bind(&resolved_spec)
    .bind(terminal_node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
          rtp_port, exit_code, failure_code, failure_reason,
          checkpoint_json, started_at, ended_at, created_at
        ) values (
          $1, $2, 1, $3, 'zlm_proxy'::worker_kind, 'SUCCEEDED'::attempt_status,
          null, null, 'rtsp', '__defaultVhost__', 'live', 'camera01',
          null, 0, null, null,
          null, $4, $4, $4
        )
        "#,
    )
    .bind(terminal_attempt_id)
    .bind(terminal_task_id)
    .bind(terminal_node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into stream_bindings (
          id, task_id, attempt_id, server_id, node_id, schema, vhost, app, stream,
          zlm_proxy_key, zlm_pusher_key, rtp_stream_id, created_at
        ) values (
          $1, $2, $3, $4, $5, 'rtsp', '__defaultVhost__', 'live', 'camera01',
          null, null, null, $6
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(terminal_task_id)
    .bind(terminal_attempt_id)
    .bind(format!("zlm-{terminal_node_id}"))
    .bind(terminal_node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;

    let stale_task_id = Uuid::now_v7();
    let stale_attempt_id = Uuid::now_v7();
    let current_attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
        ) values (
          $1, 'stale-attempt-stream', 'stream_ingest'::task_type, 'STARTING'::task_status, $2,
          50, $3, $3, 'tester', $4,
          2, 'immediate', $5, $5, $5, null
        )
        "#,
    )
    .bind(stale_task_id)
    .bind(format!("stale-{stale_task_id}"))
    .bind(&resolved_spec)
    .bind(stale_node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
          rtp_port, exit_code, failure_code, failure_reason,
          checkpoint_json, started_at, ended_at, created_at
        ) values
          ($1, $2, 1, $3, 'zlm_proxy'::worker_kind, 'SUCCEEDED'::attempt_status,
           null, null, 'rtsp', '__defaultVhost__', 'live', 'camera01',
           null, 0, null, null,
           null, $4, $4, $4),
          ($5, $2, 2, $3, 'zlm_proxy'::worker_kind, 'STARTING'::attempt_status,
           null, null, 'rtsp', '__defaultVhost__', 'live', 'camera01',
           null, null, null, null,
           null, $4, null, $4)
        "#,
    )
    .bind(stale_attempt_id)
    .bind(stale_task_id)
    .bind(stale_node_id)
    .bind(now)
    .bind(current_attempt_id)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into stream_bindings (
          id, task_id, attempt_id, server_id, node_id, schema, vhost, app, stream,
          zlm_proxy_key, zlm_pusher_key, rtp_stream_id, created_at
        ) values (
          $1, $2, $3, $4, $5, 'rtsp', '__defaultVhost__', 'live', 'camera01',
          null, null, null, $6
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(stale_task_id)
    .bind(stale_attempt_id)
    .bind(format!("zlm-{stale_node_id}"))
    .bind(stale_node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;

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
    assert!(items.is_empty());

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
          ($1, $2, 'on_publish', 'hook-node-a', '{"app":"live","file_path":"/data/zlm/www/live/camera01/hls.m3u8","folder":"/data/zlm/www/live/camera01"}'::jsonb, $3, $3),
          ($4, $5, 'on_record_mp4', 'hook-node-b', '{"app":"archive"}'::jsonb, $3, $3)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(format!("zlm-{node_id}"))
    .bind(Utc::now())
    .bind(Uuid::now_v7())
    .bind(format!("zlm-{other_node_id}"))
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
    assert_eq!(items[0]["server_id"], json!(format!("zlm-{node_id}")));
    assert_eq!(items[0]["hook_name"], json!("on_publish"));
    assert_eq!(
        items[0]["payload"]["file_path"],
        json!("/live/camera01/hls.m3u8")
    );
    assert_eq!(items[0]["payload"]["folder"], json!("/live/camera01"));

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

#[tokio::test]
async fn start_rejected_requeues_before_failure_limit() -> anyhow::Result<()> {
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
        "name": "relay-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "record": {"enabled": false},
        "recovery": {"policy": "auto", "max_consecutive_failures": 3},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let now = Utc::now();
    let task_id = Uuid::now_v7();
    let attempt_1 = Uuid::now_v7();
    let attempt_2 = Uuid::now_v7();
    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
        ) values (
          $1, 'relay-camera-01', 'stream_ingest'::task_type, 'STARTING'::task_status, $2,
          50, $3, $3, 'tester', $4,
          2, 'immediate', $5, $5, $5, null
        )
        "#,
    )
    .bind(task_id)
    .bind(format!("start-rejected-requeue-{task_id}"))
    .bind(&resolved_spec)
    .bind(node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
          rtp_port, exit_code, failure_code, failure_reason,
          checkpoint_json, started_at, ended_at, created_at, lease_token
        ) values
          (
            $1, $2, 1, $3, 'zlm_proxy'::worker_kind, 'FAILED'::attempt_status,
            null, null, 'rtsp', '__defaultVhost__', 'live', 'camera01',
            null, null, 'agent_start_rejected', 'previous rejection',
            null, $4, $4, $4, 'lease-1'
          ),
          (
            $5, $2, 2, $3, 'zlm_proxy'::worker_kind, 'STARTING'::attempt_status,
            null, null, 'rtsp', '__defaultVhost__', 'live', 'camera01',
            null, null, null, null,
            null, $4, null, $4, 'lease-2'
          )
        "#,
    )
    .bind(attempt_1)
    .bind(task_id)
    .bind(node_id)
    .bind(now)
    .bind(attempt_2)
    .execute(&db.pool)
    .await?;

    repository
        .record_agent_task_event(
            node_id,
            repository::AgentTaskEventRecord {
                task_id,
                attempt_no: 2,
                lease_token: "lease-2".to_string(),
                event_type: "start_rejected".to_string(),
                event_level: "error".to_string(),
                message: "proxy create rejected".to_string(),
                payload: json!({}),
            },
        )
        .await?;

    let summary = repository.get_task_summary(task_id).await?;
    assert_eq!(summary.status, media_domain::TaskStatus::Queued);
    assert_eq!(summary.current_attempt_no, 2);
    assert_eq!(summary.assigned_node_id, None);

    let attempt_row = sqlx::query(
        r#"
        select status::text as status, failure_code, failure_reason
          from task_attempts
         where task_id = $1
           and attempt_no = 2
        "#,
    )
    .bind(task_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        attempt_row.try_get::<String, _>("status")?,
        media_domain::AttemptStatus::Failed.as_str()
    );
    assert_eq!(
        attempt_row.try_get::<Option<String>, _>("failure_code")?,
        Some("agent_start_rejected".to_string())
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn start_rejected_hits_default_failure_limit_and_cleans_bindings() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = TaskRepository::new(db.pool.clone());
    let node_id = Uuid::now_v7();
    let server_id = format!("zlm-{node_id}");
    upsert_test_node(
        &repository,
        node_id,
        "http://127.0.0.1:65535",
        "http://stream.example",
    )
    .await?;

    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "record": {"enabled": false},
        "recovery": {"policy": "auto"},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let now = Utc::now();
    let task_id = Uuid::now_v7();
    let attempt_1 = Uuid::now_v7();
    let attempt_2 = Uuid::now_v7();
    let attempt_3 = Uuid::now_v7();
    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
        ) values (
          $1, 'relay-camera-01', 'stream_ingest'::task_type, 'STARTING'::task_status, $2,
          50, $3, $3, 'tester', $4,
          3, 'immediate', $5, $5, $5, null
        )
        "#,
    )
    .bind(task_id)
    .bind(format!("start-rejected-limit-{task_id}"))
    .bind(&resolved_spec)
    .bind(node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
          rtp_port, exit_code, failure_code, failure_reason,
          checkpoint_json, started_at, ended_at, created_at, lease_token
        ) values
          (
            $1, $2, 1, $3, 'zlm_proxy'::worker_kind, 'FAILED'::attempt_status,
            null, null, 'rtsp', '__defaultVhost__', 'live', 'camera01',
            null, null, 'agent_start_rejected', 'first rejection',
            null, $4, $4, $4, 'lease-1'
          ),
          (
            $5, $2, 2, $3, 'zlm_proxy'::worker_kind, 'FAILED'::attempt_status,
            null, null, 'rtsp', '__defaultVhost__', 'live', 'camera01',
            null, null, 'agent_start_rejected', 'second rejection',
            null, $4, $4, $4, 'lease-2'
          ),
          (
            $6, $2, 3, $3, 'zlm_proxy'::worker_kind, 'STARTING'::attempt_status,
            null, null, 'rtsp', '__defaultVhost__', 'live', 'camera01',
            null, null, null, null,
            null, $4, null, $4, 'lease-3'
          )
        "#,
    )
    .bind(attempt_1)
    .bind(task_id)
    .bind(node_id)
    .bind(now)
    .bind(attempt_2)
    .bind(attempt_3)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into stream_bindings (
          id, task_id, attempt_id, server_id, node_id, schema, vhost, app, stream,
          zlm_proxy_key, zlm_pusher_key, rtp_stream_id, created_at
        ) values (
          $1, $2, $3, $4, $5, 'rtsp', '__defaultVhost__', 'live', 'camera01',
          'proxy-stale', null, null, $6
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(task_id)
    .bind(attempt_3)
    .bind(&server_id)
    .bind(node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;

    repository
        .record_agent_task_event(
            node_id,
            repository::AgentTaskEventRecord {
                task_id,
                attempt_no: 3,
                lease_token: "lease-3".to_string(),
                event_type: "start_rejected".to_string(),
                event_level: "error".to_string(),
                message: "proxy create rejected".to_string(),
                payload: json!({}),
            },
        )
        .await?;

    let detail = repository.get_task(task_id).await?;
    assert_eq!(detail.task.status, media_domain::TaskStatus::Failed);
    assert_eq!(detail.task.assigned_node_id, None);
    assert_eq!(detail.task.current_attempt_no, 3);
    assert_eq!(
        detail
            .current_attempt
            .as_ref()
            .and_then(|attempt| attempt.failure_code.as_deref()),
        Some("agent_start_rejected")
    );
    assert!(
        detail
            .current_attempt
            .as_ref()
            .and_then(|attempt| attempt.failure_reason.as_deref())
            .unwrap_or_default()
            .contains("reached 3/3")
    );

    let binding_count: i64 =
        sqlx::query_scalar("select count(*) from stream_bindings where task_id = $1")
            .bind(task_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(binding_count, 0);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn startup_timeout_snapshot_cleans_stream_bindings() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = TaskRepository::new(db.pool.clone());
    let node_id = Uuid::now_v7();
    let server_id = format!("zlm-{node_id}");
    upsert_test_node(
        &repository,
        node_id,
        "http://127.0.0.1:65535",
        "http://stream.example",
    )
    .await?;

    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "record": {"enabled": false},
        "recovery": {"policy": "never"},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_starting_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;
    let attempt_id: Uuid = sqlx::query_scalar(
        r#"
        select id
          from task_attempts
         where task_id = $1
           and attempt_no = 1
        "#,
    )
    .bind(task_id)
    .fetch_one(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into stream_bindings (
          id, task_id, attempt_id, server_id, node_id, schema, vhost, app, stream,
          zlm_proxy_key, zlm_pusher_key, rtp_stream_id, created_at
        ) values (
          $1, $2, $3, $4, $5, 'rtsp', '__defaultVhost__', 'live', 'camera01',
          'proxy-startup-timeout', null, null, $6
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(task_id)
    .bind(attempt_id)
    .bind(&server_id)
    .bind(node_id)
    .bind(Utc::now())
    .execute(&db.pool)
    .await?;

    repository
        .record_agent_snapshot(
            node_id,
            repository::TaskSnapshotRecord {
                runtime_id: Uuid::now_v7(),
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                worker_kind: "zlm_proxy".to_string(),
                pid: None,
                state: "exited".to_string(),
                command_line: Some("zlm addStreamProxy ...".to_string()),
                outputs: Vec::new(),
                metadata: json!({
                    "startup_timeout": true,
                    "stream_binding": {
                        "schema": "rtsp",
                        "vhost": "__defaultVhost__",
                        "app": "live",
                        "stream": "camera01"
                    },
                    "zlm_server_id": server_id,
                    "zlm_proxy_key": "proxy-startup-timeout"
                }),
            },
        )
        .await?;

    let detail = repository.get_task(task_id).await?;
    assert_eq!(detail.task.status, media_domain::TaskStatus::Failed);
    assert_eq!(
        detail
            .current_attempt
            .as_ref()
            .and_then(|attempt| attempt.failure_code.as_deref()),
        Some("snapshot_exited")
    );
    assert_eq!(
        detail
            .current_attempt
            .as_ref()
            .and_then(|attempt| attempt.failure_reason.as_deref()),
        Some("runtime exited after startup timeout")
    );

    let binding_count: i64 =
        sqlx::query_scalar("select count(*) from stream_bindings where task_id = $1")
            .bind(task_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(binding_count, 0);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn sticky_live_ingest_startup_timeout_snapshot_stays_starting() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = TaskRepository::new(db.pool.clone());
    let node_id = Uuid::now_v7();
    let server_id = format!("zlm-{node_id}");
    upsert_test_node(
        &repository,
        node_id,
        "http://127.0.0.1:65535",
        "http://stream.example",
    )
    .await?;

    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "record": {"enabled": false},
        "recovery": {"policy": "auto"},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_starting_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;
    let attempt_id: Uuid = sqlx::query_scalar(
        r#"
        select id
          from task_attempts
         where task_id = $1
           and attempt_no = 1
        "#,
    )
    .bind(task_id)
    .fetch_one(&db.pool)
    .await?;
    sqlx::query(
        r#"
        insert into stream_bindings (
          id, task_id, attempt_id, server_id, node_id, schema, vhost, app, stream,
          zlm_proxy_key, zlm_pusher_key, rtp_stream_id, created_at
        ) values (
          $1, $2, $3, $4, $5, 'rtsp', '__defaultVhost__', 'live', 'camera01',
          'proxy-sticky-startup-timeout', null, null, $6
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(task_id)
    .bind(attempt_id)
    .bind(&server_id)
    .bind(node_id)
    .bind(Utc::now())
    .execute(&db.pool)
    .await?;

    repository
        .record_agent_snapshot(
            node_id,
            repository::TaskSnapshotRecord {
                runtime_id: Uuid::now_v7(),
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                worker_kind: "zlm_proxy".to_string(),
                pid: None,
                state: "exited".to_string(),
                command_line: Some("zlm addStreamProxy ...".to_string()),
                outputs: Vec::new(),
                metadata: json!({
                    "startup_timeout": true,
                    "stream_binding": {
                        "schema": "rtsp",
                        "vhost": "__defaultVhost__",
                        "app": "live",
                        "stream": "camera01"
                    },
                    "zlm_server_id": server_id,
                    "zlm_proxy_key": "proxy-sticky-startup-timeout"
                }),
            },
        )
        .await?;

    let detail = repository.get_task(task_id).await?;
    assert_eq!(detail.task.status, media_domain::TaskStatus::Starting);
    assert_eq!(
        detail
            .current_attempt
            .as_ref()
            .map(|attempt| attempt.status),
        Some(media_domain::AttemptStatus::Starting)
    );

    let binding_count: i64 =
        sqlx::query_scalar("select count(*) from stream_bindings where task_id = $1")
            .bind(task_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(binding_count, 1);

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn sticky_live_ingest_failed_event_keeps_running_status() -> anyhow::Result<()> {
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
        "name": "relay-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "record": {"enabled": true},
        "recovery": {"policy": "auto"},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;
    sqlx::query(
        r#"
        update task_attempts
           set lease_token = 'lease-1'
         where task_id = $1
           and attempt_no = 1
        "#,
    )
    .bind(task_id)
    .execute(&db.pool)
    .await?;

    repository
        .record_agent_task_event(
            node_id,
            repository::AgentTaskEventRecord {
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                event_type: "failed".to_string(),
                event_level: "error".to_string(),
                message: "live_relay stream went offline unexpectedly".to_string(),
                payload: json!({"reason": "unexpected_offline"}),
            },
        )
        .await?;

    let detail = repository.get_task(task_id).await?;
    assert_eq!(detail.task.status, media_domain::TaskStatus::Running);
    assert_eq!(
        detail
            .current_attempt
            .as_ref()
            .map(|attempt| attempt.status),
        Some(media_domain::AttemptStatus::Running)
    );
    assert_eq!(
        detail
            .current_attempt
            .as_ref()
            .and_then(|attempt| attempt.failure_code.as_deref()),
        None
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn disk_threshold_failed_event_completes_task_as_failed() -> anyhow::Result<()> {
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
        "name": "relay-camera-01",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://camera/live"},
        "stream": {"app": "live", "name": "camera01"},
        "record": {"enabled": true},
        "recovery": {"policy": "auto"},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;
    sqlx::query(
        r#"
        update task_attempts
           set lease_token = 'lease-1'
         where task_id = $1
           and attempt_no = 1
        "#,
    )
    .bind(task_id)
    .execute(&db.pool)
    .await?;

    repository
        .record_agent_task_event(
            node_id,
            repository::AgentTaskEventRecord {
                task_id,
                attempt_no: 1,
                lease_token: "lease-1".to_string(),
                event_type: "failed".to_string(),
                event_level: "error".to_string(),
                message: "child process stopped after disk threshold was exceeded".to_string(),
                payload: json!({"reason": "disk_threshold_exceeded"}),
            },
        )
        .await?;

    let detail = repository.get_task(task_id).await?;
    assert_eq!(detail.task.status, media_domain::TaskStatus::Failed);
    let attempt = detail.current_attempt.expect("current attempt");
    assert_eq!(attempt.status, media_domain::AttemptStatus::Failed);
    assert_eq!(
        attempt.failure_code.as_deref(),
        Some("disk_threshold_exceeded")
    );
    assert_eq!(
        attempt.failure_reason.as_deref(),
        Some("disk_threshold_exceeded")
    );

    db.cleanup().await?;
    Ok(())
}
