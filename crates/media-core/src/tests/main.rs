use super::*;
#[cfg(unix)]
use crate::config::Settings;
use crate::config::{AuthMode, CoreSettings};
use crate::test_database::{acquire_test_database_slot, config_from_env, finish_setup};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use media_domain::{
    AgentRegistration, HeartbeatSnapshot, NetworkMode, RuntimeSlotLoad, SourceMode,
};
use serde_json::json;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use tower::util::ServiceExt;

#[cfg(unix)]
static CORE_CONFIG_ENVIRONMENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const TEST_RSA_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDRNk+CElS+M3My1DbTUInl9aeU\nYCLza8Uftij7kPTApECFQcy1em6CZwb+PDHjjtFB2i8Ncfbx+dt2S6CbJHSF0dDB\n+GoiaVaYolB9XoQODqA7LXTy/D4e9jdNJQgDVXlzXsTm4k3v1CnC1As7RfUkgdM/\npsbfsbeai7RULN2NnQIDAQAB\n-----END PUBLIC KEY-----";
const TEST_ED25519_PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIMAlSI3/XdPzRT72Rw08g6NnTnJ2eaq1JoJoW5Vlbm/T\n-----END PRIVATE KEY-----";
const TEST_ED25519_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAA5Q5gilpT0f2fcLhC7l30Wou7Ng/gESlFWWx8z6TGJw=\n-----END PUBLIC KEY-----";

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

#[test]
fn persisted_user_agent_is_capped() {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_str(&"x".repeat(2_048)).unwrap(),
    );
    let persisted = user_agent_from_headers(&headers).expect("user agent");
    assert_eq!(persisted.len(), 256);
    let unicode = truncate_utf8_for_storage(&"界".repeat(100), 256);
    assert!(unicode.len() <= 256);
    assert!(unicode.is_char_boundary(unicode.len()));
}

#[test]
fn http_handlers_cannot_bypass_database_validated_authentication() {
    let main_source = include_str!("../main.rs");
    let route_modules = [
        main_source,
        include_str!("../ui.rs"),
        include_str!("../upload.rs"),
    ];

    assert_eq!(
        main_source.matches(".auth.verify_session_claims(").count(),
        1,
        "only authenticated_session may call the claim verifier directly"
    );
    assert_eq!(
        route_modules
            .iter()
            .map(|source| source.matches(".auth.verify_session_claims(").count())
            .sum::<usize>(),
        1,
        "route modules must not introduce another direct claim-verifier call"
    );
    for source in route_modules {
        assert!(
            !source.contains(".auth.authorize(")
                && !source.contains(".auth.session(")
                && source.matches(".auth.verify_session_claims(").count() <= 1,
            "HTTP handlers must use authenticated_session/authorize_api_request"
        );
    }
}

fn live_runtime_slot_load(running_tasks: u32, slot_usage: f64) -> Vec<RuntimeSlotLoad> {
    let max_runtime_slots = if running_tasks == 0 || slot_usage <= 0.0 {
        0
    } else {
        ((running_tasks as f64 / slot_usage).ceil() as u32).max(1)
    };
    vec![RuntimeSlotLoad {
        source_mode: SourceMode::Live,
        max_runtime_slots,
        running_tasks,
        starting_tasks: 0,
        stopping_tasks: 0,
        orphaned_tasks: 0,
        slot_usage,
    }]
}

struct TestDatabase {
    _slot: tokio::sync::OwnedSemaphorePermit,
    admin_pool: sqlx::PgPool,
    pool: sqlx::PgPool,
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

#[test]
fn parse_cli_command_accepts_top_level_help() {
    let command = parse_cli_command_from(["--help".to_string()]).unwrap();
    assert_eq!(command, Some(CliCommand::Help { auth_only: false }));
}

#[test]
fn parse_cli_command_defaults_to_secure_server_mode() {
    let command = parse_cli_command_from(Vec::<String>::new()).unwrap();
    assert_eq!(
        command,
        Some(CliCommand::Serve {
            insecure_dev: false,
        })
    );
}

#[test]
fn parse_cli_command_accepts_explicit_insecure_dev_mode() {
    let command = parse_cli_command_from(["--insecure-dev".to_string()]).unwrap();
    assert_eq!(command, Some(CliCommand::Serve { insecure_dev: true }));
    assert!(CLI_HELP_TEXT.contains("--insecure-dev"));
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
fn parse_cli_command_accepts_agent_enrollment_help_and_warns_about_sensitive_stdout() {
    for args in [
        vec!["agent".to_string(), "--help".to_string()],
        vec![
            "agent".to_string(),
            "create-enrollment".to_string(),
            "--help".to_string(),
        ],
    ] {
        assert_eq!(
            parse_cli_command_from(args).unwrap(),
            Some(CliCommand::AgentHelp)
        );
    }
    assert!(CLI_HELP_TEXT.contains("agent create-enrollment"));
    assert!(AGENT_CLI_HELP_TEXT.contains("only stdout line"));
    assert!(AGENT_CLI_HELP_TEXT.contains("sensitive"));
}

#[test]
fn parse_cli_command_requires_an_exact_node_and_explicit_token_stdout() {
    let node_id = Uuid::parse_str("0190d8d4-31d2-7b23-b27e-8b9b28a2ed11").unwrap();
    assert_eq!(
        parse_cli_command_from([
            "agent".to_string(),
            "create-enrollment".to_string(),
            "--node-id".to_string(),
            node_id.to_string(),
            "--token-stdout".to_string(),
        ])
        .unwrap(),
        Some(CliCommand::AgentCreateEnrollment { node_id })
    );

    for (name, args) in [
        (
            "missing token stdout opt-in",
            vec![
                "agent",
                "create-enrollment",
                "--node-id",
                "0190d8d4-31d2-7b23-b27e-8b9b28a2ed11",
            ],
        ),
        (
            "noncanonical uppercase UUID",
            vec![
                "agent",
                "create-enrollment",
                "--node-id",
                "0190D8D4-31D2-7B23-B27E-8B9B28A2ED11",
                "--token-stdout",
            ],
        ),
        (
            "nil UUID",
            vec![
                "agent",
                "create-enrollment",
                "--node-id",
                "00000000-0000-0000-0000-000000000000",
                "--token-stdout",
            ],
        ),
        (
            "extra argument",
            vec![
                "agent",
                "create-enrollment",
                "--node-id",
                "0190d8d4-31d2-7b23-b27e-8b9b28a2ed11",
                "--token-stdout",
                "extra",
            ],
        ),
        (
            "duplicate token option",
            vec![
                "agent",
                "create-enrollment",
                "--node-id",
                "0190d8d4-31d2-7b23-b27e-8b9b28a2ed11",
                "--token-stdout",
                "--token-stdout",
            ],
        ),
    ] {
        let error = parse_cli_command_from(args.into_iter().map(str::to_string)).expect_err(name);
        assert!(!error.to_string().is_empty(), "{name}");
    }
}

#[test]
fn agent_enrollment_cli_writes_only_the_sensitive_token_line_and_redacts_debug() {
    let enrollment = agent_identity::CreatedAgentEnrollment {
        enrollment_id: Uuid::now_v7(),
        node_id: Uuid::now_v7(),
        token: zeroize::Zeroizing::new("one-time-sensitive-token".to_string()),
        expires_at: Utc::now() + chrono::Duration::minutes(10),
    };
    let mut output = Vec::new();
    write_agent_enrollment_token(&mut output, &enrollment).unwrap();
    assert_eq!(output, b"one-time-sensitive-token\n");
    let debug = format!("{enrollment:?}");
    assert!(!debug.contains("one-time-sensitive-token"));
    assert!(debug.contains("[REDACTED]"));
}

#[tokio::test]
async fn agent_enrollment_cli_loads_pki_runs_migrations_and_persists_only_a_token_hash()
-> anyhow::Result<()> {
    use sha2::Digest as _;

    let Some(db) = require_test_database(false).await? else {
        return Ok(());
    };
    let config = config_from_env()?;
    let directory = tempfile::tempdir()?;
    let now = Utc::now();
    let mut settings = write_test_agent_enrollment_materials(directory.path(), now)?;
    settings.database_url = test_database_url(&config.admin_url, &db.database_name)?;
    let node_id = Uuid::now_v7();

    let enrollment = create_agent_enrollment_for_cli(&settings, node_id, now).await?;
    assert_eq!(enrollment.node_id, node_id);
    let persisted_hash: Vec<u8> =
        sqlx::query_scalar("select token_hash from agent_enrollment_tokens where node_id = $1")
            .bind(node_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        persisted_hash,
        sha2::Sha256::digest(enrollment.token.as_bytes()).as_slice()
    );
    let persisted_token: bool = sqlx::query_scalar(
        "select exists (select 1 from security_audit_events where payload::text like '%' || $1 || '%')",
    )
    .bind(enrollment.token.as_str())
    .fetch_one(&db.pool)
    .await?;
    assert!(!persisted_token);

    db.cleanup().await?;
    Ok(())
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

#[test]
fn parse_cli_command_parses_read_only_admin_check() {
    let command = parse_cli_command_from(["auth".to_string(), "check-admin".to_string()]).unwrap();
    assert_eq!(command, Some(CliCommand::CheckAdmin));

    let error = parse_cli_command_from([
        "auth".to_string(),
        "check-admin".to_string(),
        "--password-stdin".to_string(),
    ])
    .unwrap_err();
    assert!(error.to_string().contains("does not accept arguments"));
}

#[test]
fn parse_cli_command_parses_read_only_auth_config_check() {
    let command = parse_cli_command_from(["auth".to_string(), "check-config".to_string()]).unwrap();
    assert_eq!(command, Some(CliCommand::CheckAuthConfig));

    let error = parse_cli_command_from([
        "auth".to_string(),
        "check-config".to_string(),
        "--password-stdin".to_string(),
    ])
    .unwrap_err();
    assert!(error.to_string().contains("does not accept arguments"));
}

#[test]
fn parse_cli_command_parses_bootstrap_status_probe() {
    let command = parse_cli_command_from([
        "auth".to_string(),
        "bootstrap-status".to_string(),
        "--username".to_string(),
        "admin".to_string(),
        "--handoff-id".to_string(),
        "0190d8d4-31d2-7b23-b27e-8b9b28a2ed11".to_string(),
    ])
    .unwrap();
    assert_eq!(
        command,
        Some(CliCommand::BootstrapStatus {
            username: "admin".to_string(),
            handoff_id: "0190d8d4-31d2-7b23-b27e-8b9b28a2ed11".to_string(),
        })
    );

    let error = parse_cli_command_from([
        "auth".to_string(),
        "bootstrap-status".to_string(),
        "--username".to_string(),
        "admin".to_string(),
        "--password-stdin".to_string(),
    ])
    .unwrap_err();
    assert!(error.to_string().contains("requires"));

    let recover = parse_cli_command_from([
        "auth".to_string(),
        "recover-bootstrap-admin".to_string(),
        "--username".to_string(),
        "admin".to_string(),
        "--handoff-id".to_string(),
        "0190d8d4-31d2-7b23-b27e-8b9b28a2ed11".to_string(),
        "--expected-version".to_string(),
        "7".to_string(),
        "--password-stdin".to_string(),
    ])
    .unwrap();
    assert_eq!(
        recover,
        Some(CliCommand::RecoverBootstrapAdmin {
            username: "admin".to_string(),
            handoff_id: "0190d8d4-31d2-7b23-b27e-8b9b28a2ed11".to_string(),
            expected_version: "7".to_string(),
        })
    );
}

#[test]
fn bootstrap_admin_cli_requires_password_change() {
    assert!(std::hint::black_box(BOOTSTRAP_ADMIN_MUST_CHANGE_PASSWORD));
}

fn production_security_settings() -> CoreSettings {
    CoreSettings {
        http_addr: "0.0.0.0:8080".to_string(),
        http_tls_cert_path: "http-server.pem".to_string(),
        http_tls_key_path: "http-server.key".to_string(),
        grpc_addr: "0.0.0.0:50051".to_string(),
        grpc_tls_cert_path: "grpc-server.pem".to_string(),
        grpc_tls_key_path: "grpc-server.key".to_string(),
        grpc_tls_client_ca_path: "grpc-client-ca.pem".to_string(),
        grpc_tls_server_ca_path: "grpc-server-ca.pem".to_string(),
        agent_ca_cert_path: "agent-ca.pem".to_string(),
        agent_ca_key_path: "agent-ca.key".to_string(),
        agent_capability_jwt_private_key_path: "agent-capability-private.pem".to_string(),
        agent_capability_jwt_public_key_path: "agent-capability-public.pem".to_string(),
        agent_capability_ttl_sec: 60,
        core_instance_id: "0190d8d4-31d2-7b23-b27e-8b9b28a2ed11".to_string(),
        agent_management_client_cert_path: "management-client.pem".to_string(),
        agent_management_client_key_path: "management-client-key.pem".to_string(),
        agent_management_ca_path: "management-client-ca.pem".to_string(),
        auth_mode: AuthMode::LocalPassword,
        ..CoreSettings::default()
    }
}

fn write_test_agent_enrollment_materials(
    directory: &FsPath,
    now: DateTime<Utc>,
) -> anyhow::Result<CoreSettings> {
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
        IsCa, KeyPair, KeyUsagePurpose, PKCS_ED25519, SanType,
    };

    fn root_params(now: DateTime<Utc>, name: &str) -> CertificateParams {
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.not_before = time::OffsetDateTime::from_unix_timestamp(
            (now - chrono::Duration::days(1)).timestamp(),
        )
        .unwrap();
        params.not_after = time::OffsetDateTime::from_unix_timestamp(
            (now + chrono::Duration::days(365)).timestamp(),
        )
        .unwrap();
        params
    }

    let agent_ca_key = KeyPair::generate()?;
    let agent_ca = root_params(now, "Agent Root").self_signed(&agent_ca_key)?;
    let server_ca_key = KeyPair::generate()?;
    let server_ca = root_params(now, "Core Server Root").self_signed(&server_ca_key)?;
    let server_key = KeyPair::generate()?;
    let mut server_params = CertificateParams::default();
    server_params.distinguished_name = DistinguishedName::new();
    server_params
        .distinguished_name
        .push(DnType::CommonName, "Core gRPC");
    server_params.subject_alt_names = vec![SanType::DnsName(
        "core.streamserver.internal".try_into().unwrap(),
    )];
    server_params.is_ca = IsCa::NoCa;
    server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    server_params.not_before = time::OffsetDateTime::from_unix_timestamp(
        (now - chrono::Duration::minutes(5)).timestamp(),
    )?;
    server_params.not_after =
        time::OffsetDateTime::from_unix_timestamp((now + chrono::Duration::days(90)).timestamp())?;
    let server = server_params.signed_by(&server_key, &server_ca, &server_ca_key)?;
    let core_instance_id = Uuid::parse_str("0190d8d4-31d2-7b23-b27e-8b9b28a2ed11")?;
    let management_ca_key = KeyPair::generate()?;
    let management_ca =
        root_params(now, "Core Management Client Root").self_signed(&management_ca_key)?;
    let management_client_key = KeyPair::generate_for(&PKCS_ED25519)?;
    let mut management_client_params = CertificateParams::default();
    management_client_params.distinguished_name = DistinguishedName::new();
    management_client_params
        .distinguished_name
        .push(DnType::CommonName, "StreamServer Core Management Client");
    management_client_params.subject_alt_names = vec![SanType::URI(
        format!("spiffe://streamserver/core/{core_instance_id}")
            .try_into()
            .unwrap(),
    )];
    management_client_params.is_ca = IsCa::NoCa;
    management_client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    management_client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    management_client_params.not_before = time::OffsetDateTime::from_unix_timestamp(
        (now - chrono::Duration::minutes(5)).timestamp(),
    )?;
    management_client_params.not_after =
        time::OffsetDateTime::from_unix_timestamp((now + chrono::Duration::days(90)).timestamp())?;
    let management_client = management_client_params.signed_by(
        &management_client_key,
        &management_ca,
        &management_ca_key,
    )?;

    let agent_ca_path = directory.join("agent-ca.pem");
    let agent_ca_key_path = directory.join("agent-ca-key.pem");
    let client_ca_path = directory.join("client-ca.pem");
    let server_ca_path = directory.join("server-ca.pem");
    let server_path = directory.join("server.pem");
    let server_key_path = directory.join("server-key.pem");
    let capability_private_path = directory.join("capability-private.pem");
    let capability_public_path = directory.join("capability-public.pem");
    let management_client_path = directory.join("management-client.pem");
    let management_client_key_path = directory.join("management-client-key.pem");
    let management_ca_path = directory.join("management-client-ca.pem");
    fs::write(&agent_ca_path, agent_ca.pem())?;
    fs::write(&agent_ca_key_path, agent_ca_key.serialize_pem())?;
    fs::write(&client_ca_path, agent_ca.pem())?;
    fs::write(&server_ca_path, server_ca.pem())?;
    fs::write(&server_path, server.pem())?;
    fs::write(&server_key_path, server_key.serialize_pem())?;
    fs::write(&capability_private_path, TEST_ED25519_PRIVATE_KEY)?;
    fs::write(&capability_public_path, TEST_ED25519_PUBLIC_KEY)?;
    fs::write(&management_client_path, management_client.pem())?;
    fs::write(
        &management_client_key_path,
        management_client_key.serialize_pem(),
    )?;
    fs::write(&management_ca_path, management_ca.pem())?;

    Ok(CoreSettings {
        grpc_tls_cert_path: server_path.to_string_lossy().to_string(),
        grpc_tls_key_path: server_key_path.to_string_lossy().to_string(),
        grpc_tls_client_ca_path: client_ca_path.to_string_lossy().to_string(),
        grpc_tls_server_ca_path: server_ca_path.to_string_lossy().to_string(),
        agent_ca_cert_path: agent_ca_path.to_string_lossy().to_string(),
        agent_ca_key_path: agent_ca_key_path.to_string_lossy().to_string(),
        agent_capability_jwt_private_key_path: capability_private_path
            .to_string_lossy()
            .to_string(),
        agent_capability_jwt_public_key_path: capability_public_path.to_string_lossy().to_string(),
        core_instance_id: core_instance_id.to_string(),
        agent_management_client_cert_path: management_client_path.to_string_lossy().to_string(),
        agent_management_client_key_path: management_client_key_path.to_string_lossy().to_string(),
        agent_management_ca_path: management_ca_path.to_string_lossy().to_string(),
        ..CoreSettings::default()
    })
}

#[test]
fn agent_enrollment_startup_loads_distinct_server_ca_and_capability_metadata() -> anyhow::Result<()>
{
    let now = Utc::now();
    let directory = tempfile::tempdir()?;
    let mut settings = write_test_agent_enrollment_materials(directory.path(), now)?;
    let agent_ca_pem = fs::read_to_string(&settings.agent_ca_cert_path)?;
    let expected_server_ca = fs::read_to_string(&settings.grpc_tls_server_ca_path)?;
    let expected_management_ca = fs::read_to_string(&settings.agent_management_ca_path)?;

    let (_, public_config, loaded_core_instance_id) =
        load_agent_certificate_authority(&settings, now)?
            .expect("Agent enrollment material must be configured");
    assert_eq!(
        loaded_core_instance_id.to_string(),
        settings.core_instance_id
    );

    assert_eq!(
        public_config.control_plane_server_ca_pem,
        expected_server_ca
    );
    assert_ne!(public_config.control_plane_server_ca_pem, agent_ca_pem);
    assert_eq!(
        public_config.management_client_ca_pem,
        expected_management_ca
    );
    assert_ne!(public_config.management_client_ca_pem, agent_ca_pem);
    assert_ne!(
        public_config.management_client_ca_pem,
        public_config.control_plane_server_ca_pem
    );
    assert_eq!(
        public_config.capability_jwt_public_key_pem,
        TEST_ED25519_PUBLIC_KEY
    );
    let (_, public_block) =
        x509_parser::pem::parse_x509_pem(TEST_ED25519_PUBLIC_KEY.as_bytes()).unwrap();
    assert_eq!(
        public_config.capability_jwt_kid,
        bytes_to_lower_hex(&Sha256::digest(public_block.contents))
    );
    settings.agent_capability_ttl_sec = 10;
    let management_client =
        load_agent_management_client(&settings, public_config.capability_jwt_kid.as_str())?;
    let management_private = fs::read_to_string(&settings.agent_management_client_key_path)?;
    let debug = format!("{management_client:?}");
    assert!(!debug.contains(TEST_ED25519_PRIVATE_KEY));
    assert!(!debug.contains(&management_private));
    assert!(debug.contains("[REDACTED]"));
    assert!(debug.contains("ttl_seconds: 10"));
    Ok(())
}

#[test]
fn agent_enrollment_startup_rejects_wrong_server_ca_and_capability_pair() -> anyhow::Result<()> {
    use rcgen::{
        BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose, PKCS_ED25519,
    };

    let now = Utc::now();
    let directory = tempfile::tempdir()?;
    let settings = write_test_agent_enrollment_materials(directory.path(), now)?;
    let other_ca_key = KeyPair::generate()?;
    let mut other_ca_params = CertificateParams::default();
    other_ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    other_ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    other_ca_params.not_before =
        time::OffsetDateTime::from_unix_timestamp((now - chrono::Duration::days(1)).timestamp())?;
    other_ca_params.not_after =
        time::OffsetDateTime::from_unix_timestamp((now + chrono::Duration::days(365)).timestamp())?;
    let other_ca = other_ca_params.self_signed(&other_ca_key)?;
    fs::write(&settings.grpc_tls_server_ca_path, other_ca.pem())?;
    let error = load_agent_certificate_authority(&settings, now)
        .expect_err("unrelated server CA must fail");
    assert!(error.to_string().contains("not directly issued"));

    let settings = write_test_agent_enrollment_materials(directory.path(), now)?;
    let different_capability_key = KeyPair::generate_for(&PKCS_ED25519)?;
    fs::write(
        &settings.agent_capability_jwt_public_key_path,
        different_capability_key.public_key_pem(),
    )?;
    let error = load_agent_certificate_authority(&settings, now)
        .expect_err("mismatched capability key pair must fail");
    assert!(error.to_string().contains("do not match"));

    let mut settings = write_test_agent_enrollment_materials(directory.path(), now)?;
    let instance = Uuid::parse_str(&settings.core_instance_id)?;
    settings.core_instance_id = Uuid::from_u128(instance.as_u128() ^ 1).to_string();
    let error = load_agent_certificate_authority(&settings, now)
        .expect_err("management client SAN must match CORE_INSTANCE_ID");
    assert!(error.to_string().contains("CORE_INSTANCE_ID"));

    let settings = write_test_agent_enrollment_materials(directory.path(), now)?;
    let management_private = fs::read_to_string(&settings.agent_management_client_key_path)?;
    let management_key = KeyPair::from_pem(&management_private)?;
    fs::write(
        &settings.agent_capability_jwt_private_key_path,
        management_private,
    )?;
    fs::write(
        &settings.agent_capability_jwt_public_key_path,
        management_key.public_key_pem(),
    )?;
    let error = load_agent_certificate_authority(&settings, now)
        .expect_err("capability key must not reuse management client key");
    assert!(error.to_string().contains("dedicated"));

    let mut settings = write_test_agent_enrollment_materials(directory.path(), now)?;
    settings.auth_mode = AuthMode::LocalPassword;
    settings.auth_jwt_private_key_path = settings.agent_capability_jwt_private_key_path.clone();
    settings.auth_jwt_public_key_path = settings.agent_capability_jwt_public_key_path.clone();
    let error = load_agent_certificate_authority(&settings, now)
        .expect_err("capability key must not reuse local auth signing key");
    assert!(error.to_string().contains("local auth"));
    Ok(())
}

#[test]
fn security_policy_covers_production_tls_and_insecure_development_matrix() {
    struct Case {
        name: &'static str,
        environment: &'static str,
        settings: CoreSettings,
        expected_error: Option<&'static str>,
    }

    let mut cases = Vec::new();

    let mut settings = production_security_settings();
    settings.auth_mode = AuthMode::Disabled;
    cases.push(Case {
        name: "production auth disabled",
        environment: "production",
        settings,
        expected_error: Some("production requires AUTH_MODE other than disabled"),
    });

    let mut settings = production_security_settings();
    settings.auth_mode = AuthMode::Disabled;
    cases.push(Case {
        name: "production environment is normalized",
        environment: " Production ",
        settings,
        expected_error: Some("production requires AUTH_MODE other than disabled"),
    });

    for environment in ["prod", "staging", "production-typo"] {
        cases.push(Case {
            name: "unknown environment fails closed",
            environment,
            settings: production_security_settings(),
            expected_error: Some("STREAMSERVER_ENV must be development or production"),
        });
    }

    let mut settings = production_security_settings();
    settings.http_tls_cert_path.clear();
    settings.http_tls_key_path.clear();
    cases.push(Case {
        name: "production non-loopback without HTTP TLS",
        environment: "production",
        settings,
        expected_error: Some("non-loopback CORE_HTTP_ADDR requires HTTP TLS"),
    });

    let mut settings = production_security_settings();
    settings.http_tls_cert_path.clear();
    settings.http_tls_key_path.clear();
    cases.push(Case {
        name: "development plaintext HTTP on non-loopback",
        environment: "development",
        settings,
        expected_error: Some("non-loopback CORE_HTTP_ADDR requires HTTP TLS"),
    });

    let mut settings = production_security_settings();
    settings.http_tls_key_path.clear();
    cases.push(Case {
        name: "partial HTTP TLS",
        environment: "production",
        settings,
        expected_error: Some(
            "CORE_HTTP_TLS_CERT_PATH and CORE_HTTP_TLS_KEY_PATH must be set together",
        ),
    });

    let mut settings = production_security_settings();
    settings.grpc_tls_cert_path.clear();
    settings.grpc_tls_key_path.clear();
    settings.grpc_tls_client_ca_path.clear();
    cases.push(Case {
        name: "production without gRPC mTLS",
        environment: "production",
        settings,
        expected_error: Some("production requires gRPC mTLS"),
    });

    let mut settings = production_security_settings();
    settings.grpc_tls_client_ca_path.clear();
    cases.push(Case {
        name: "partial gRPC mTLS",
        environment: "production",
        settings,
        expected_error: Some(
            "CORE_GRPC_TLS_CERT_PATH, CORE_GRPC_TLS_KEY_PATH and CORE_GRPC_TLS_CLIENT_CA_PATH must all be set together",
        ),
    });

    let mut settings = production_security_settings();
    settings.agent_ca_key_path.clear();
    cases.push(Case {
        name: "partial Agent signing CA",
        environment: "production",
        settings,
        expected_error: Some(
            "CORE_AGENT_CA_CERT_PATH and CORE_AGENT_CA_KEY_PATH must be set together",
        ),
    });

    let mut settings = production_security_settings();
    settings.grpc_tls_server_ca_path.clear();
    cases.push(Case {
        name: "Agent enrollment without explicit Core server CA",
        environment: "production",
        settings,
        expected_error: Some("Agent enrollment requires CORE_GRPC_TLS_SERVER_CA_PATH"),
    });

    let mut settings = production_security_settings();
    settings.agent_capability_jwt_public_key_path.clear();
    cases.push(Case {
        name: "partial Agent capability signer",
        environment: "production",
        settings,
        expected_error: Some(
            "CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH and CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH must be set together",
        ),
    });

    let mut settings = production_security_settings();
    settings.agent_management_ca_path.clear();
    cases.push(Case {
        name: "partial Agent management client identity",
        environment: "production",
        settings,
        expected_error: Some(
            "CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH, CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH and CORE_AGENT_MANAGEMENT_CA_PATH must all be set together",
        ),
    });

    let mut settings = production_security_settings();
    settings.core_instance_id = Uuid::nil().to_string();
    cases.push(Case {
        name: "nil Core instance identity",
        environment: "production",
        settings,
        expected_error: Some("CORE_INSTANCE_ID must be a non-nil canonical UUID"),
    });

    for ttl in [9, 121] {
        let mut settings = production_security_settings();
        settings.agent_capability_ttl_sec = ttl;
        cases.push(Case {
            name: "Agent capability TTL outside policy",
            environment: "production",
            settings,
            expected_error: Some("CORE_AGENT_CAPABILITY_TTL_SEC must be between 10 and 120"),
        });
    }

    let mut settings = production_security_settings();
    settings.agent_ca_cert_path.clear();
    settings.agent_ca_key_path.clear();
    cases.push(Case {
        name: "production without Agent signing CA",
        environment: "production",
        settings,
        expected_error: Some("production requires the Agent signing CA"),
    });

    let mut settings = production_security_settings();
    settings.http_addr = "127.0.0.1:8080".to_string();
    settings.grpc_addr = "127.0.0.1:50051".to_string();
    let error = crate::config::validate_security_policy("production", &settings, true)
        .expect_err("insecure dev in production")
        .to_string();
    assert!(error.contains("--insecure-dev is allowed only in development"));

    let settings = production_security_settings();
    let error = crate::config::validate_security_policy("development", &settings, true)
        .expect_err("insecure dev on non-loopback")
        .to_string();
    assert!(error.contains("--insecure-dev requires loopback HTTP and gRPC addresses"));

    cases.push(Case {
        name: "valid production TLS and mTLS",
        environment: "production",
        settings: production_security_settings(),
        expected_error: None,
    });

    let mut settings = production_security_settings();
    settings.http_addr = "127.0.0.1:8080".to_string();
    settings.http_tls_cert_path.clear();
    settings.http_tls_key_path.clear();
    cases.push(Case {
        name: "valid production loopback HTTP with gRPC mTLS",
        environment: "production",
        settings,
        expected_error: None,
    });

    let mut settings = CoreSettings {
        http_addr: "127.0.0.1:8080".to_string(),
        grpc_addr: "127.0.0.1:50051".to_string(),
        auth_mode: AuthMode::Disabled,
        ..CoreSettings::default()
    };
    settings.http_tls_cert_path.clear();
    settings.http_tls_key_path.clear();
    settings.grpc_tls_cert_path.clear();
    settings.grpc_tls_key_path.clear();
    settings.grpc_tls_client_ca_path.clear();
    crate::config::validate_security_policy("development", &settings, true)
        .expect("valid development loopback insecure");

    cases.push(Case {
        name: "development plaintext gRPC without explicit insecure dev",
        environment: "development",
        settings,
        expected_error: Some("development plaintext gRPC requires --insecure-dev"),
    });

    for case in cases {
        let result =
            crate::config::validate_security_policy(case.environment, &case.settings, false);
        match case.expected_error {
            Some(expected) => {
                let error = result.expect_err(case.name).to_string();
                assert!(
                    error.contains(expected),
                    "{}: expected error containing {expected:?}, got {error:?}",
                    case.name
                );
            }
            None => result.unwrap_or_else(|error| panic!("{}: {error:#}", case.name)),
        }
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_streamserver_environment_fails_closed() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let _guard = CORE_CONFIG_ENVIRONMENT_LOCK.lock().unwrap();
    let previous = std::env::var_os("STREAMSERVER_ENV");
    unsafe {
        std::env::set_var("STREAMSERVER_ENV", OsString::from_vec(vec![0xff]));
    }
    let error = Settings::load_with_insecure_dev(false)
        .expect_err("non-Unicode STREAMSERVER_ENV must not default to development");
    match previous {
        Some(value) => unsafe { std::env::set_var("STREAMSERVER_ENV", value) },
        None => unsafe { std::env::remove_var("STREAMSERVER_ENV") },
    }
    assert!(error.to_string().contains("valid Unicode"));
}

#[test]
fn insecure_dev_config_key_is_rejected() -> anyhow::Result<()> {
    let result = ::config::Config::builder()
        .add_source(::config::File::from_str(
            "insecure_dev = true",
            ::config::FileFormat::Toml,
        ))
        .build()?
        .try_deserialize::<CoreSettings>();

    let error = result.expect_err("core.insecure_dev must not replace the CLI flag");
    assert!(error.to_string().contains("insecure_dev"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn insecure_dev_environment_override_is_rejected() {
    let _guard = CORE_CONFIG_ENVIRONMENT_LOCK.lock().unwrap();
    let previous_environment = std::env::var_os("STREAMSERVER_ENV");
    let previous_database_url = std::env::var_os("DATABASE_URL");
    let previous_insecure_dev = std::env::var_os("CORE_INSECURE_DEV");
    unsafe {
        std::env::set_var("STREAMSERVER_ENV", "development");
        std::env::set_var(
            "DATABASE_URL",
            "postgresql://unused:unused@127.0.0.1:1/unused",
        );
        std::env::set_var("CORE_INSECURE_DEV", "true");
    }

    let result = Settings::load_with_insecure_dev(false);

    unsafe {
        match previous_environment {
            Some(value) => std::env::set_var("STREAMSERVER_ENV", value),
            None => std::env::remove_var("STREAMSERVER_ENV"),
        }
        match previous_database_url {
            Some(value) => std::env::set_var("DATABASE_URL", value),
            None => std::env::remove_var("DATABASE_URL"),
        }
        match previous_insecure_dev {
            Some(value) => std::env::set_var("CORE_INSECURE_DEV", value),
            None => std::env::remove_var("CORE_INSECURE_DEV"),
        }
    }

    let error = result.expect_err("CORE_INSECURE_DEV must not replace the CLI flag");
    assert!(
        error
            .to_string()
            .contains("CORE_INSECURE_DEV is unsupported")
    );
}

async fn tls_peer_address(PeerAddress(peer): PeerAddress) -> String {
    peer.map(|value| value.ip().to_string())
        .unwrap_or_else(|| "missing".to_string())
}

#[tokio::test]
async fn core_http_tls_listener_accepts_https_and_preserves_peer_address() -> anyhow::Result<()> {
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    let temp_dir = tempfile::tempdir()?;
    let cert_path = temp_dir.path().join("server.pem");
    let key_path = temp_dir.path().join("server.key");
    tokio::fs::write(&cert_path, cert.pem()).await?;
    tokio::fs::write(&key_path, key_pair.serialize_pem()).await?;

    let settings = CoreSettings {
        http_tls_cert_path: cert_path.to_string_lossy().into_owned(),
        http_tls_key_path: key_path.to_string_lossy().into_owned(),
        ..CoreSettings::default()
    };
    let tls = load_http_tls_config(&settings)
        .await?
        .expect("HTTP TLS must be configured");
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let listener = listener.into_std()?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(serve_http(
        listener,
        Router::new().route("/peer", get(tls_peer_address)),
        Some(tls),
        shutdown_rx,
    ));

    let response = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?
        .get(format!("https://localhost:{}/peer", address.port()))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.text().await?, "127.0.0.1");

    shutdown_tx.send(true)?;
    timeout(std::time::Duration::from_secs(5), server).await???;
    Ok(())
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
        runtime_slot_loads: None,
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
        agent_identity: None,
        agent_management: None,
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

fn test_app_state_with_local_auth(pool: sqlx::PgPool) -> anyhow::Result<AppState> {
    let key_dir = tempfile::tempdir()?;
    let private_key_path = key_dir.path().join("jwt-private.pem");
    let public_key_path = key_dir.path().join("jwt-public.pem");
    fs::write(&private_key_path, TEST_ED25519_PRIVATE_KEY)?;
    fs::write(&public_key_path, TEST_ED25519_PUBLIC_KEY)?;
    let settings = CoreSettings {
        auth_mode: AuthMode::LocalPassword,
        auth_jwt_private_key_path: private_key_path.to_string_lossy().to_string(),
        auth_jwt_public_key_path: public_key_path.to_string_lossy().to_string(),
        ..CoreSettings::default()
    };
    let mut state = test_app_state(pool);
    state.auth = AuthConfig::from_settings(&settings)?;
    Ok(state)
}

async fn authenticated_get_status(
    app: &Router,
    uri: &str,
    access_token: &str,
) -> anyhow::Result<StatusCode> {
    Ok(app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {access_token}"))
                .body(Body::empty())?,
        )
        .await?
        .status())
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

#[tokio::test]
async fn authenticated_zlm_hook_handler_uses_session_node_id_as_server_id_and_real_business_logic()
-> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    let repository = Arc::new(TaskRepository::new(db.pool.clone()));
    let node_id = Uuid::now_v7();
    upsert_test_node(
        &repository,
        node_id,
        "http://127.0.0.1:65535",
        "http://stream.example",
    )
    .await?;
    let handler = CoreZlmHookHandler::new(repository, false, Vec::new());

    let response = control_plane::ZlmHookHandler::handle(
        &handler,
        control_plane::AuthenticatedZlmHook {
            node_id,
            hook_name: "on_server_started".to_string(),
            body: json!({}),
        },
    )
    .await;
    assert_eq!(response.http_status, 200);
    assert_eq!(response.body["code"], json!(0));

    let row = sqlx::query(
        "select server_id, payload from hook_events where hook_name = 'on_server_started'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(row.try_get::<String, _>("server_id")?, node_id.to_string());
    assert_eq!(row.try_get::<Value, _>("payload")?, json!({}));

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn production_removes_direct_zlm_hook_routes_while_development_keeps_compatibility()
-> anyhow::Result<()> {
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

    let request = || {
        Request::builder()
            .method("POST")
            .uri(format!("/internal/hooks/zlm/{server_id}/on_server_started"))
            .header(header::CONTENT_TYPE, "application/json")
            .extension(ConnectInfo(
                "127.0.0.1:19350"
                    .parse::<SocketAddr>()
                    .expect("loopback hook peer"),
            ))
            .body(Body::from("{}"))
            .expect("hook request")
    };

    let mut production = test_app_state(db.pool.clone());
    production.environment = "production".to_string();
    let production_response = build_app(production).oneshot(request()).await?;
    assert_eq!(production_response.status(), StatusCode::NOT_FOUND);

    let mut development = test_app_state(db.pool.clone());
    development.environment = "development".to_string();
    let development_response = build_app(development).oneshot(request()).await?;
    assert_eq!(development_response.status(), StatusCode::OK);
    assert_eq!(json_body(development_response).await["code"], json!(0));

    db.cleanup().await?;
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
async fn external_jwt_ignores_local_claims_without_database_lookup() -> anyhow::Result<()> {
    #[derive(serde::Serialize)]
    struct ExternalClaims<'a> {
        sub: &'a str,
        role: auth::ApiRole,
        #[serde(rename = "urn:streamserver:credential_version")]
        credential_version: i64,
        #[serde(rename = "urn:streamserver:must_change_password")]
        must_change_password: bool,
        jti: &'a str,
        iat: i64,
        nbf: i64,
        exp: i64,
    }

    let now = Utc::now().timestamp();
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA),
        &ExternalClaims {
            sub: "external-admin",
            role: auth::ApiRole::Admin,
            credential_version: 999,
            must_change_password: true,
            jti: "external-local-claim-collision",
            iat: now,
            nbf: now,
            exp: now + 300,
        },
        &jsonwebtoken::EncodingKey::from_ed_pem(TEST_ED25519_PRIVATE_KEY.as_bytes())?,
    )?;
    let pool = PgPoolOptions::new().connect_lazy("postgresql://postgres@127.0.0.1:1/postgres")?;
    let mut state = test_app_state(pool);
    state.auth = auth_config_from_public_key(true, TEST_ED25519_PUBLIC_KEY)?;
    let response = build_app(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/me")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["must_change_password"], json!(false));
    assert_eq!(body["permissions"][0], json!("task_read"));
    Ok(())
}

#[tokio::test]
async fn bootstrap_status_reports_missing_for_a_truly_empty_database() -> anyhow::Result<()> {
    let Some(db) = require_test_database(false).await? else {
        return Ok(());
    };
    let repository = TaskRepository::new(db.pool.clone());
    let probe = repository
        .bootstrap_admin_password_state("fresh-admin", Uuid::now_v7())
        .await?;
    assert_eq!(
        probe.state,
        repository::BootstrapAdminPasswordState::Missing
    );
    assert_eq!(probe.expected_version, Some(0));
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn bootstrap_status_handles_legacy_schema_and_fails_closed_on_partial_schema()
-> anyhow::Result<()> {
    let Some(db) = require_test_database(false).await? else {
        return Ok(());
    };
    sqlx::query(
        r#"
        create table auth_users (
          username text primary key,
          role text not null,
          enabled boolean not null
        )
        "#,
    )
    .execute(&db.pool)
    .await?;
    let repository = TaskRepository::new(db.pool.clone());
    let handoff_id = Uuid::now_v7();
    let empty_legacy = repository
        .bootstrap_admin_password_state("legacy-admin", handoff_id)
        .await?;
    assert_eq!(
        empty_legacy.state,
        repository::BootstrapAdminPasswordState::Missing
    );
    sqlx::query("insert into auth_users (username, role, enabled) values ($1, 'viewer', false)")
        .bind("legacy-admin")
        .execute(&db.pool)
        .await?;
    let occupied_target = repository
        .bootstrap_admin_password_state("legacy-admin", handoff_id)
        .await?;
    assert_eq!(
        occupied_target.state,
        repository::BootstrapAdminPasswordState::Conflict
    );
    sqlx::query("delete from auth_users")
        .execute(&db.pool)
        .await?;
    sqlx::query(
        "insert into auth_users (username, role, enabled) values ('other-admin', 'admin', true)",
    )
    .execute(&db.pool)
    .await?;
    let occupied_admin = repository
        .bootstrap_admin_password_state("legacy-admin", handoff_id)
        .await?;
    assert_eq!(
        occupied_admin.state,
        repository::BootstrapAdminPasswordState::Conflict
    );

    sqlx::query("alter table auth_users add column bootstrap_handoff_id uuid")
        .execute(&db.pool)
        .await?;
    assert!(
        repository
            .bootstrap_admin_password_state("legacy-admin", handoff_id)
            .await
            .is_err(),
        "partially applied handoff schema must fail closed"
    );
    sqlx::query("drop table auth_users")
        .execute(&db.pool)
        .await?;
    sqlx::query("create table auth_users (username text primary key)")
        .execute(&db.pool)
        .await?;
    assert!(
        repository
            .bootstrap_admin_password_state("legacy-admin", handoff_id)
            .await
            .is_err(),
        "unexpected legacy schemas must propagate their database error"
    );
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn bootstrap_login_requires_change_and_password_change_revokes_refresh() -> anyhow::Result<()>
{
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    const USERNAME: &str = "bootstrap-admin";
    const INITIAL_PASSWORD: &str = "Initial-password-1";
    const RECOVERED_PASSWORD_A: &str = "Recovered-password-A2";
    const RECOVERED_PASSWORD_B: &str = "Recovered-password-B2";
    const BARRIER_RECOVERY_PASSWORD: &str = "Barrier-recovery-3";
    const NEXT_PASSWORD: &str = "Changed-password-2";
    const RACING_RECOVERY_PASSWORD: &str = "Must-not-overwrite-3";
    const CONFLICT_USERNAME: &str = "unrelated-admin";

    let repository = TaskRepository::new(db.pool.clone());
    let handoff_id = Uuid::now_v7();
    let missing_probe = repository
        .bootstrap_admin_password_state(USERNAME, handoff_id)
        .await?;
    assert_eq!(
        missing_probe.state,
        repository::BootstrapAdminPasswordState::Missing
    );
    assert_eq!(missing_probe.expected_version, Some(0));
    assert_eq!(
        repository
            .reconcile_bootstrap_admin_password(
                USERNAME,
                handoff_id,
                0,
                &hash_password(INITIAL_PASSWORD)?,
            )
            .await?,
        repository::BootstrapAdminReconcileOutcome::Created
    );
    let pending_probe = repository
        .bootstrap_admin_password_state(USERNAME, handoff_id)
        .await?;
    assert_eq!(
        pending_probe.state,
        repository::BootstrapAdminPasswordState::PendingPasswordChange
    );
    assert_eq!(pending_probe.expected_version, Some(1));
    let user_before_recovery = repository
        .find_auth_user_by_username(USERNAME)
        .await?
        .expect("created handoff administrator");
    assert_eq!(user_before_recovery.credential_version, 1);

    let repository_a = repository.clone();
    let repository_b = repository.clone();
    let hash_a = hash_password(RECOVERED_PASSWORD_A)?;
    let hash_b = hash_password(RECOVERED_PASSWORD_B)?;
    let (outcome_a, outcome_b) = tokio::join!(
        repository_a.reconcile_bootstrap_admin_password(USERNAME, handoff_id, 1, &hash_a),
        repository_b.reconcile_bootstrap_admin_password(USERNAME, handoff_id, 1, &hash_b),
    );
    let outcome_a = outcome_a?;
    let outcome_b = outcome_b?;
    assert!(matches!(
        (outcome_a, outcome_b),
        (
            repository::BootstrapAdminReconcileOutcome::Recovered,
            repository::BootstrapAdminReconcileOutcome::Stale
        ) | (
            repository::BootstrapAdminReconcileOutcome::Stale,
            repository::BootstrapAdminReconcileOutcome::Recovered
        )
    ));
    let recovered_password = if outcome_a == repository::BootstrapAdminReconcileOutcome::Recovered {
        RECOVERED_PASSWORD_A
    } else {
        RECOVERED_PASSWORD_B
    };
    let app = build_app(test_app_state_with_local_auth(db.pool.clone())?);
    let pre_barrier_login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&json!({
                    "username": USERNAME,
                    "password": recovered_password,
                }))?))?,
        )
        .await?;
    assert_eq!(pre_barrier_login.status(), StatusCode::OK);
    let pre_barrier_login = json_body(pre_barrier_login).await;
    let pre_barrier_access = pre_barrier_login["access_token"]
        .as_str()
        .expect("pre-recovery access token")
        .to_string();
    let pre_barrier_refresh = pre_barrier_login["refresh_token"]
        .as_str()
        .expect("pre-recovery refresh token")
        .to_string();
    assert_eq!(
        authenticated_get_status(&app, "/api/v1/me", &pre_barrier_access).await?,
        StatusCode::OK,
        "a current must-change token may inspect its own session"
    );
    assert_eq!(
        authenticated_get_status(&app, "/api/v1/tasks", &pre_barrier_access).await?,
        StatusCode::FORBIDDEN,
        "a must-change token may not call business APIs"
    );
    let pending_logout = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/logout")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&json!({
                    "refresh_token": pre_barrier_refresh,
                }))?))?,
        )
        .await?;
    assert_eq!(
        pending_logout.status(),
        StatusCode::NO_CONTENT,
        "must-change users must remain able to log out"
    );

    let stale_login_inserted = repository
        .insert_login_refresh_session(
            repository::NewRefreshSession {
                id: Uuid::now_v7(),
                user_id: user_before_recovery.id,
                token_hash: hash_refresh_token("stale-login-after-recovery"),
                expires_at: Utc::now() + chrono::Duration::minutes(5),
                created_at: Utc::now(),
                client_ip: None,
                user_agent: Some("stale-login-race".to_string()),
            },
            &user_before_recovery.password_hash,
        )
        .await?;
    assert!(!stale_login_inserted);

    let pending_probe = repository
        .bootstrap_admin_password_state(USERNAME, handoff_id)
        .await?;
    assert_eq!(pending_probe.expected_version, Some(2));
    let current_user = repository
        .find_auth_user_by_username(USERNAME)
        .await?
        .expect("recovered handoff administrator");
    assert_eq!(current_user.credential_version, 2);
    let barrier_session_id = Uuid::now_v7();
    let barrier_session_hash = hash_refresh_token("login-linearization-barrier");
    let mut login_tx = db.pool.begin().await?;
    sqlx::query_scalar::<_, Uuid>(
        "select id from auth_users where id = $1 and password_hash = $2 for share",
    )
    .bind(current_user.id)
    .bind(&current_user.password_hash)
    .fetch_one(&mut *login_tx)
    .await?;
    sqlx::query(
        r#"
        insert into auth_refresh_sessions (
          id, user_id, token_hash, expires_at, revoked_at, created_at,
          updated_at, last_used_at, client_ip, user_agent
        ) values ($1, $2, $3, $4, null, $5, $5, null, null, 'barrier')
        "#,
    )
    .bind(barrier_session_id)
    .bind(current_user.id)
    .bind(&barrier_session_hash)
    .bind(Utc::now() + chrono::Duration::minutes(5))
    .bind(Utc::now())
    .execute(&mut *login_tx)
    .await?;
    let barrier_repository = repository.clone();
    let barrier_hash = hash_password(BARRIER_RECOVERY_PASSWORD)?;
    let recovery_task = tokio::spawn(async move {
        barrier_repository
            .reconcile_bootstrap_admin_password(USERNAME, handoff_id, 2, &barrier_hash)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !recovery_task.is_finished(),
        "recovery must wait for the login transaction's user row lock"
    );
    login_tx.commit().await?;
    let interleaved_login_access = test_app_state_with_local_auth(db.pool.clone())?
        .auth
        .issue_access_token(
            USERNAME,
            auth::ApiRole::Admin,
            current_user.credential_version,
            current_user.must_change_password,
        )?
        .token;
    assert_eq!(
        recovery_task.await??,
        repository::BootstrapAdminReconcileOutcome::Recovered
    );
    assert_eq!(
        authenticated_get_status(&app, "/api/v1/me", &pre_barrier_access).await?,
        StatusCode::FORBIDDEN,
        "bootstrap recovery must invalidate access tokens issued for the prior credential version"
    );
    assert_eq!(
        authenticated_get_status(&app, "/api/v1/me", &interleaved_login_access).await?,
        StatusCode::FORBIDDEN,
        "a token signed after the login transaction commits must still be stale if recovery wins before authorization"
    );
    assert!(
        repository
            .find_refresh_session(&barrier_session_hash)
            .await?
            .expect("barrier session remains auditable")
            .revoked_at
            .is_some()
    );

    let login_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&json!({
                    "username": USERNAME,
                    "password": BARRIER_RECOVERY_PASSWORD,
                }))?))?,
        )
        .await?;
    assert_eq!(login_response.status(), StatusCode::OK);
    let login_body = json_body(login_response).await;
    assert_eq!(login_body["must_change_password"], json!(true));
    let access_token = login_body["access_token"]
        .as_str()
        .expect("login response access token")
        .to_string();
    let refresh_token = login_body["refresh_token"]
        .as_str()
        .expect("login response refresh token")
        .to_string();

    let change_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/change-password")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {access_token}"))
                .body(Body::from(serde_json::to_vec(&json!({
                    "current_password": BARRIER_RECOVERY_PASSWORD,
                    "new_password": NEXT_PASSWORD,
                }))?))?,
        )
        .await?;
    assert_eq!(change_response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        authenticated_get_status(&app, "/api/v1/me", &access_token).await?,
        StatusCode::FORBIDDEN,
        "changing the password must invalidate the access token used for the change"
    );
    assert_eq!(
        authenticated_get_status(&app, "/api/v1/tasks", &access_token).await?,
        StatusCode::FORBIDDEN
    );

    let user = repository
        .find_auth_user_by_username(USERNAME)
        .await?
        .expect("bootstrap administrator must exist");
    assert!(!user.must_change_password);
    assert_eq!(user.credential_version, 4);
    let complete_probe = repository
        .bootstrap_admin_password_state(USERNAME, handoff_id)
        .await?;
    assert_eq!(
        complete_probe.state,
        repository::BootstrapAdminPasswordState::Complete
    );
    assert_eq!(complete_probe.expected_version, None);
    assert_eq!(
        repository
            .reconcile_bootstrap_admin_password(
                USERNAME,
                handoff_id,
                user.bootstrap_handoff_version,
                &hash_password(RACING_RECOVERY_PASSWORD)?,
            )
            .await?,
        repository::BootstrapAdminReconcileOutcome::AlreadyComplete
    );
    let completed_user = repository
        .find_auth_user_by_username(USERNAME)
        .await?
        .expect("completed administrator must remain present");
    assert!(verify_password(
        &completed_user.password_hash,
        NEXT_PASSWORD
    )?);
    assert!(!verify_password(
        &completed_user.password_hash,
        RACING_RECOVERY_PASSWORD
    )?);
    let unrelated_handoff_id = Uuid::now_v7();
    let conflict_probe = repository
        .bootstrap_admin_password_state(CONFLICT_USERNAME, unrelated_handoff_id)
        .await?;
    assert_eq!(
        conflict_probe.state,
        repository::BootstrapAdminPasswordState::Conflict
    );
    assert_eq!(
        repository
            .reconcile_bootstrap_admin_password(
                CONFLICT_USERNAME,
                unrelated_handoff_id,
                0,
                &hash_password(RACING_RECOVERY_PASSWORD)?,
            )
            .await?,
        repository::BootstrapAdminReconcileOutcome::Conflict
    );
    assert!(
        repository
            .find_auth_user_by_username(CONFLICT_USERNAME)
            .await?
            .is_none()
    );
    assert!(
        repository
            .find_refresh_session(&hash_refresh_token(&refresh_token))
            .await?
            .expect("initial refresh session must remain auditable")
            .revoked_at
            .is_some()
    );

    let stale_refresh_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/refresh")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&json!({
                    "refresh_token": refresh_token,
                }))?))?,
        )
        .await?;
    assert_eq!(stale_refresh_response.status(), StatusCode::FORBIDDEN);

    let next_login_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&json!({
                    "username": USERNAME,
                    "password": NEXT_PASSWORD,
                }))?))?,
        )
        .await?;
    assert_eq!(next_login_response.status(), StatusCode::OK);
    let next_login_body = json_body(next_login_response).await;
    assert_eq!(next_login_body["must_change_password"], json!(false));
    let next_access_token = next_login_body["access_token"]
        .as_str()
        .expect("post-change access token")
        .to_string();
    assert_eq!(
        authenticated_get_status(&app, "/api/v1/tasks", &next_access_token).await?,
        StatusCode::OK,
        "a current post-change token may call business APIs"
    );

    const RESET_PASSWORD: &str = "Reset-password-4";
    repository
        .reset_user_password(
            USERNAME,
            &hash_password(RESET_PASSWORD)?,
            true,
            "test-cli",
            "password_reset",
            None,
            Some("credential-version-test"),
        )
        .await?;
    assert_eq!(
        repository
            .find_auth_user_by_username(USERNAME)
            .await?
            .expect("reset administrator")
            .credential_version,
        5
    );
    assert_eq!(
        authenticated_get_status(&app, "/api/v1/me", &next_access_token).await?,
        StatusCode::FORBIDDEN,
        "a password reset must invalidate already-issued access tokens"
    );

    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn change_password_and_bootstrap_recovery_are_linearized() -> anyhow::Result<()> {
    let Some(db) = require_test_database(true).await? else {
        return Ok(());
    };
    const USERNAME: &str = "handoff-change-race";
    const INITIAL_PASSWORD: &str = "Handoff-initial-1";
    const CHANGED_PASSWORD: &str = "User-selected-2";
    const RECOVERED_PASSWORD: &str = "Installer-recovered-3";

    let repository = TaskRepository::new(db.pool.clone());
    let handoff_id = Uuid::now_v7();
    assert_eq!(
        repository
            .reconcile_bootstrap_admin_password(
                USERNAME,
                handoff_id,
                0,
                &hash_password(INITIAL_PASSWORD)?,
            )
            .await?,
        repository::BootstrapAdminReconcileOutcome::Created
    );
    let user = repository
        .find_auth_user_by_username(USERNAME)
        .await?
        .expect("race user");
    let session_token_hash = hash_refresh_token("handoff-change-race-session");
    assert!(
        repository
            .insert_login_refresh_session(
                repository::NewRefreshSession {
                    id: Uuid::now_v7(),
                    user_id: user.id,
                    token_hash: session_token_hash.clone(),
                    expires_at: Utc::now() + chrono::Duration::minutes(5),
                    created_at: Utc::now(),
                    client_ip: None,
                    user_agent: Some("handoff-change-race".to_string()),
                },
                &user.password_hash,
            )
            .await?
    );

    let change_repository = repository.clone();
    let recovery_repository = repository.clone();
    let changed_hash = hash_password(CHANGED_PASSWORD)?;
    let recovered_hash = hash_password(RECOVERED_PASSWORD)?;
    let expected_hash = user.password_hash.clone();
    let (change_outcome, recovery_outcome) = tokio::join!(
        change_repository.change_user_password(
            USERNAME,
            &expected_hash,
            Some(handoff_id),
            1,
            &changed_hash,
            USERNAME,
            None,
            Some("race-test"),
        ),
        recovery_repository.reconcile_bootstrap_admin_password(
            USERNAME,
            handoff_id,
            1,
            &recovered_hash,
        ),
    );
    let change_outcome = change_outcome?;
    let recovery_outcome = recovery_outcome?;
    let final_user = repository
        .find_auth_user_by_username(USERNAME)
        .await?
        .expect("linearized race user");
    let final_probe = repository
        .bootstrap_admin_password_state(USERNAME, handoff_id)
        .await?;
    match (change_outcome, recovery_outcome) {
        (true, repository::BootstrapAdminReconcileOutcome::AlreadyComplete) => {
            assert!(verify_password(
                &final_user.password_hash,
                CHANGED_PASSWORD
            )?);
            assert_eq!(
                final_probe.state,
                repository::BootstrapAdminPasswordState::Complete
            );
        }
        (false, repository::BootstrapAdminReconcileOutcome::Recovered) => {
            assert!(verify_password(
                &final_user.password_hash,
                RECOVERED_PASSWORD
            )?);
            assert_eq!(
                final_probe.state,
                repository::BootstrapAdminPasswordState::PendingPasswordChange
            );
            assert_eq!(final_probe.expected_version, Some(2));
        }
        outcomes => panic!("password change/recovery race was not linearized: {outcomes:?}"),
    }
    assert!(
        repository
            .find_refresh_session(&session_token_hash)
            .await?
            .expect("race session remains auditable")
            .revoked_at
            .is_some()
    );

    db.cleanup().await?;
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
async fn preview_task_preserves_vod_start_offset_sec() -> anyhow::Result<()> {
    let pool = PgPoolOptions::new().connect_lazy("postgresql://postgres@127.0.0.1/postgres")?;
    let app = build_app(test_app_state(pool));
    let mut payload = sample_create_task_payload("manual");
    payload["input"] = json!({
        "kind": "http_mp4",
        "source_mode": "vod",
        "loop_enabled": false,
        "start_offset_sec": 600,
        "url": "http://vod.example.com/archive.mp4"
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
    assert_eq!(
        body["requested_spec"]["input"]["start_offset_sec"],
        json!(600)
    );
    assert_eq!(
        body["resolved_spec"]["input"]["start_offset_sec"],
        json!(600)
    );
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
                runtime_slot_loads: live_runtime_slot_load(2, 0.4),
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
                runtime_slot_loads: live_runtime_slot_load(3, 0.55),
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
                runtime_slot_loads: live_runtime_slot_load(1, 0.2),
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
                runtime_slot_loads: live_runtime_slot_load(1, 0.2),
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
async fn list_streams_does_not_use_self_reported_zlm_control_address() -> anyhow::Result<()> {
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
    assert!(items[0].get("viewer_count").is_none());
    assert!(items[0].get("bitrate_kbps").is_none());
    assert_ne!(items[0]["has_viewer"], json!(true));

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
async fn list_streams_does_not_probe_self_reported_zlm_for_play_schemas() -> anyhow::Result<()> {
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
    assert!(items[0].get("viewer_count").is_none());
    assert!(items[0].get("bitrate_kbps").is_none());

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
    assert!(
        delivered_calls[0]
            .0
            .get("X-StreamServer-Signature")
            .and_then(|value| value.to_str().ok())
            .is_some()
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

    let lost_task_id = Uuid::now_v7();
    let lost_attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        insert into tasks (
          id, name, type, status, idempotency_key,
          priority, requested_spec, resolved_spec, created_by, assigned_node_id,
          current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
        ) values (
          $1, 'lost-reclaim-stale-runtime', 'stream_ingest'::task_type, 'LOST'::task_status, $2,
          50, $3, $3, 'tester', null,
          1, 'immediate', $4, $4, $4, $4
        )
        "#,
    )
    .bind(lost_task_id)
    .bind(format!("lost-reclaim-stale-runtime-{lost_task_id}"))
    .bind(&resolved_spec)
    .bind(now)
    .execute(&db.pool)
    .await?;

    sqlx::query(
        r#"
        insert into task_attempts (
          id, task_id, attempt_no, node_id, worker_kind, status,
          created_at, lease_token, ended_at
        ) values (
          $1, $2, 1, $3, 'hybrid'::worker_kind, 'FAILED'::attempt_status,
          $4, 'lease-lost-stale-runtime', $4
        )
        "#,
    )
    .bind(lost_attempt_id)
    .bind(lost_task_id)
    .bind(node_id)
    .bind(now)
    .execute(&db.pool)
    .await?;

    let reclaim = repository.list_reclaim_runtimes(node_id).await?;
    assert!(reclaim.iter().any(|item| {
        item.task_id == task_id
            && item.attempt_no == 1
            && item.lease_token == lease_token
            && item.worker_kind == media_domain::WorkerKind::Hybrid
    }));
    assert!(
        reclaim.iter().all(|item| item.task_id != lost_task_id),
        "LOST tasks must not be advertised for runtime reclaim"
    );

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
async fn list_streams_keeps_database_entries_without_authenticated_runtime_lookup()
-> anyhow::Result<()> {
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
    assert_eq!(items.len(), 2);
    let streams = items
        .iter()
        .map(|item| item["stream"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(streams, BTreeSet::from(["camera01", "camera02"]));

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
async fn debug_zlm_snap_rejects_legacy_self_reported_control_address() -> anyhow::Result<()> {
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
    assert_eq!(response.status(), StatusCode::CONFLICT);

    zlm_handle.abort();
    db.cleanup().await?;
    Ok(())
}

#[test]
fn core_zlm_debug_has_no_direct_url_secret_or_repository_target_path() {
    let main_source = include_str!("../main.rs");
    let repository_source = include_str!("../repository_nodes.rs");
    for forbidden in [
        "fn build_zlm_debug_url",
        "async fn call_zlm_binary_api",
        ".get_node_debug_target(",
        "zlm_api_secret.trim()",
    ] {
        assert!(
            !main_source.contains(forbidden),
            "Core ZLM debug path must not contain {forbidden}"
        );
    }
    assert!(
        !repository_source.contains("pub async fn get_node_debug_target"),
        "legacy self-reported ZLM target lookup must be deleted"
    );
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
async fn reclaim_timeout_lost_task_cleans_stream_bindings() -> anyhow::Result<()> {
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
        "recovery": {"policy": "never"},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let task_id =
        insert_running_stream_task(&db.pool, node_id, resolved_spec, "live", "camera01").await?;
    let deadline = Utc::now() - chrono::Duration::seconds(1);
    sqlx::query(
        r#"
        update tasks
           set status = 'RECLAIMING'::task_status,
               reclaim_deadline_at = $1,
               updated_at = $1
         where id = $2
        "#,
    )
    .bind(deadline)
    .bind(task_id)
    .execute(&db.pool)
    .await?;

    let before_count: i64 =
        sqlx::query_scalar("select count(*) from stream_bindings where task_id = $1")
            .bind(task_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(before_count, 1);

    let candidate = repository
        .list_reclaiming_tasks()
        .await?
        .into_iter()
        .find(|candidate| candidate.task_id == task_id)
        .expect("task should be reclaiming before timeout");
    assert!(repository.finalize_reclaim_timeout(&candidate).await?);

    let detail = repository.get_task(task_id).await?;
    assert_eq!(detail.task.status, media_domain::TaskStatus::Lost);
    assert_eq!(detail.task.assigned_node_id, None);

    let after_count: i64 =
        sqlx::query_scalar("select count(*) from stream_bindings where task_id = $1")
            .bind(task_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(after_count, 0);

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
