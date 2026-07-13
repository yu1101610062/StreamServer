use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, anyhow, bail, ensure};
use chrono::{DateTime, Duration, Utc};
use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType, KeyPair,
    PublicKeyData,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use x509_parser::{
    extensions::GeneralName,
    parse_x509_certificate,
    pem::{Pem, parse_x509_pem},
    prelude::FromDer,
};
use zeroize::{Zeroize, Zeroizing};

const LEGACY_IDENTITY_METADATA_VERSION: u32 = 1;
const IDENTITY_METADATA_VERSION: u32 = 2;
const PENDING_IDENTITY_METADATA_VERSION: u32 = 2;
const PENDING_ROTATION_METADATA_VERSION: u32 = 1;
const ROTATION_AUDIT_VERSION: u32 = 1;
const ROTATION_RESET_AUDIT_VERSION: u32 = 1;
const ROTATION_WINDOW: Duration = Duration::days(30);
const ROTATION_BUNDLE_MAX_LIFETIME: Duration = Duration::minutes(5);
const MAX_IDENTITY_FILE_BYTES: u64 = 1024 * 1024;
const MAX_ENROLLMENT_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub(crate) struct AgentIdentityStore {
    root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentIdentityLoadError {
    #[error("Agent identity is not enrolled")]
    NotEnrolled,
    #[error(transparent)]
    Invalid(#[from] anyhow::Error),
}

impl fmt::Debug for AgentIdentityStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentIdentityStore")
            .field("root", &self.root)
            .finish()
    }
}

pub(crate) struct PendingIdentity {
    node_id: Uuid,
    generation_id: Uuid,
    csr_pem: String,
    private_key_pem: Zeroizing<String>,
    management_csr_pem: String,
    management_private_key_pem: Zeroizing<String>,
}

pub(crate) enum EnrollmentPreparation {
    Pending(PendingIdentity),
    Recovered(Box<LoadedIdentity>),
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CertificateRotationRequestData {
    rotation_id: Uuid,
    control_csr_pem: String,
    management_csr_pem: String,
}

impl fmt::Debug for CertificateRotationRequestData {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CertificateRotationRequestData")
            .field("rotation_id", &self.rotation_id)
            .field("control_csr", &"[REDACTED]")
            .field("management_csr", &"[REDACTED]")
            .finish()
    }
}

impl CertificateRotationRequestData {
    pub(crate) fn rotation_id(&self) -> Uuid {
        self.rotation_id
    }

    pub(crate) fn control_csr_pem(&self) -> &str {
        &self.control_csr_pem
    }

    pub(crate) fn management_csr_pem(&self) -> &str {
        &self.management_csr_pem
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthenticatedRotationAction {
    None,
    SendRequest(CertificateRotationRequestData),
    RestartRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RotationCommitOutcome {
    RestartRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CertificateRotationActivatedData {
    rotation_id: Uuid,
    activated_at_ms: i64,
    control_fingerprint_sha256: String,
    management_fingerprint_sha256: String,
}

impl CertificateRotationActivatedData {
    pub(crate) fn rotation_id(&self) -> Uuid {
        self.rotation_id
    }

    pub(crate) fn activated_at_ms(&self) -> i64 {
        self.activated_at_ms
    }

    pub(crate) fn control_fingerprint_sha256(&self) -> &str {
        &self.control_fingerprint_sha256
    }

    pub(crate) fn management_fingerprint_sha256(&self) -> &str {
        &self.management_fingerprint_sha256
    }
}

struct IdentityRootLock {
    root: PathBuf,
    file: File,
    #[cfg(unix)]
    root_device: u64,
    #[cfg(unix)]
    root_inode: u64,
    #[cfg(unix)]
    lock_device: u64,
    #[cfg(unix)]
    lock_inode: u64,
}

impl Drop for IdentityRootLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

impl fmt::Debug for PendingIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingIdentity")
            .field("node_id", &self.node_id)
            .field("generation_id", &self.generation_id)
            .field("csr", &"[REDACTED]")
            .field("private_key", &"[REDACTED]")
            .field("management_csr", &"[REDACTED]")
            .field("management_private_key", &"[REDACTED]")
            .finish()
    }
}

impl PendingIdentity {
    pub(crate) fn node_id(&self) -> Uuid {
        self.node_id
    }

    pub(crate) fn csr_pem(&self) -> &str {
        &self.csr_pem
    }

    pub(crate) fn management_csr_pem(&self) -> &str {
        &self.management_csr_pem
    }

    #[cfg(test)]
    fn private_key_pem_for_test(&self) -> &str {
        self.private_key_pem.as_str()
    }

    #[cfg(test)]
    fn control_public_key_der_for_test(&self) -> Vec<u8> {
        KeyPair::from_pem(self.private_key_pem.as_str())
            .expect("valid pending control key")
            .public_key_der()
    }

    #[cfg(test)]
    fn management_public_key_der_for_test(&self) -> Vec<u8> {
        KeyPair::from_pem(self.management_private_key_pem.as_str())
            .expect("valid pending management key")
            .public_key_der()
    }
}

pub(crate) struct LoadedIdentity {
    metadata: IdentityMetadata,
    #[cfg(test)]
    generation_dir: PathBuf,
    certificate_pem: String,
    private_key_pem: Zeroizing<String>,
    management_certificate_pem: Option<String>,
    management_private_key_pem: Option<Zeroizing<String>>,
    agent_client_issuer_ca_pem: Option<String>,
    control_plane_server_ca_pem: Option<String>,
    management_client_ca_pem: Option<String>,
    capability_jwt_public_key_pem: Option<String>,
}

impl fmt::Debug for LoadedIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadedIdentity")
            .field("node_id", &self.metadata.node_id)
            .field("generation_id", &self.metadata.generation_id)
            .field("fingerprint_sha256", &self.metadata.fingerprint_sha256)
            .field("not_after", &self.metadata.not_after)
            .field("certificate", &"[REDACTED]")
            .field("private_key", &"[REDACTED]")
            .finish()
    }
}

impl LoadedIdentity {
    pub(crate) fn node_id(&self) -> Uuid {
        self.metadata.node_id
    }

    pub(crate) fn generation_id(&self) -> Uuid {
        self.metadata.generation_id
    }

    #[cfg(test)]
    fn fingerprint_sha256(&self) -> &str {
        &self.metadata.fingerprint_sha256
    }

    pub(crate) fn not_after(&self) -> DateTime<Utc> {
        self.metadata.not_after
    }

    pub(crate) fn rotation_not_after(&self) -> anyhow::Result<DateTime<Utc>> {
        self.ensure_production_complete()?;
        Ok(self
            .metadata
            .management_not_after
            .expect("complete identity checked")
            .min(self.metadata.not_after))
    }

    pub(crate) fn tonic_identity(&self) -> tonic::transport::Identity {
        tonic::transport::Identity::from_pem(
            self.certificate_pem.as_bytes(),
            self.private_key_pem.as_bytes(),
        )
    }

    pub(crate) fn ensure_production_complete(&self) -> anyhow::Result<()> {
        ensure!(
            self.metadata.version == IDENTITY_METADATA_VERSION
                && self.management_certificate_pem.is_some()
                && self.management_private_key_pem.is_some()
                && self.agent_client_issuer_ca_pem.is_some()
                && self.control_plane_server_ca_pem.is_some()
                && self.management_client_ca_pem.is_some()
                && self.capability_jwt_public_key_pem.is_some()
                && self.metadata.management_dns_name.is_some()
                && self.metadata.capability_jwt_kid.is_some(),
            "Agent identity bundle is incomplete and cannot be used in production"
        );
        Ok(())
    }

    pub(crate) fn management_certificate_pem(&self) -> anyhow::Result<&str> {
        self.ensure_production_complete()?;
        Ok(self
            .management_certificate_pem
            .as_deref()
            .expect("complete identity checked"))
    }

    pub(crate) fn management_private_key_pem(&self) -> anyhow::Result<&str> {
        self.ensure_production_complete()?;
        Ok(self
            .management_private_key_pem
            .as_deref()
            .expect("complete identity checked"))
    }

    #[cfg(test)]
    pub(crate) fn agent_client_issuer_ca_pem(&self) -> anyhow::Result<&str> {
        self.ensure_production_complete()?;
        Ok(self
            .agent_client_issuer_ca_pem
            .as_deref()
            .expect("complete identity checked"))
    }

    pub(crate) fn control_plane_server_ca_pem(&self) -> anyhow::Result<&str> {
        self.ensure_production_complete()?;
        Ok(self
            .control_plane_server_ca_pem
            .as_deref()
            .expect("complete identity checked"))
    }

    pub(crate) fn management_client_ca_pem(&self) -> anyhow::Result<&str> {
        self.ensure_production_complete()?;
        Ok(self
            .management_client_ca_pem
            .as_deref()
            .expect("complete identity checked"))
    }

    pub(crate) fn capability_jwt_public_key_pem(&self) -> anyhow::Result<&str> {
        self.ensure_production_complete()?;
        Ok(self
            .capability_jwt_public_key_pem
            .as_deref()
            .expect("complete identity checked"))
    }

    pub(crate) fn capability_jwt_kid(&self) -> anyhow::Result<&str> {
        self.ensure_production_complete()?;
        Ok(self
            .metadata
            .capability_jwt_kid
            .as_deref()
            .expect("complete identity checked"))
    }

    #[cfg(test)]
    pub(crate) fn management_dns_name(&self) -> anyhow::Result<&str> {
        self.ensure_production_complete()?;
        Ok(self
            .metadata
            .management_dns_name
            .as_deref()
            .expect("complete identity checked"))
    }

    #[cfg(test)]
    fn private_key_path(&self) -> PathBuf {
        self.generation_dir.join("identity-key.pem")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdentityMetadata {
    version: u32,
    generation_id: Uuid,
    node_id: Uuid,
    fingerprint_sha256: String,
    #[serde(default)]
    serial_number: Option<String>,
    not_before: DateTime<Utc>,
    not_after: DateTime<Utc>,
    #[serde(default)]
    management_fingerprint_sha256: Option<String>,
    #[serde(default)]
    management_serial_number: Option<String>,
    #[serde(default)]
    management_not_before: Option<DateTime<Utc>>,
    #[serde(default)]
    management_not_after: Option<DateTime<Utc>>,
    #[serde(default)]
    management_dns_name: Option<String>,
    #[serde(default)]
    agent_client_issuer_ca_sha256: Option<String>,
    #[serde(default)]
    control_plane_server_ca_sha256: Option<String>,
    #[serde(default)]
    management_client_ca_sha256: Option<String>,
    #[serde(default)]
    capability_jwt_kid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingIdentityMetadata {
    version: u32,
    generation_id: Uuid,
    node_id: Uuid,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RotationStage {
    Requested,
    SwitchAuthorized,
    Consumed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingRotationMetadata {
    version: u32,
    rotation_id: Uuid,
    node_id: Uuid,
    source_generation_id: Uuid,
    created_at: DateTime<Utc>,
    stage: RotationStage,
    #[serde(default)]
    target_generation_id: Option<Uuid>,
    #[serde(default)]
    bundle_expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    new_session_consumed_at: Option<DateTime<Utc>>,
}

struct PendingRotation {
    identity: PendingIdentity,
    metadata: PendingRotationMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RotationAuditStatus {
    Activated,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RotationAudit {
    version: u32,
    status: RotationAuditStatus,
    rotation_id: Uuid,
    node_id: Uuid,
    source_generation_id: Uuid,
    target_generation_id: Uuid,
    created_at: DateTime<Utc>,
    bundle_expires_at: DateTime<Utc>,
    #[serde(default)]
    new_session_consumed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    activated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    previous_identity_expires_at: Option<DateTime<Utc>>,
    control_fingerprint_sha256: String,
    management_fingerprint_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RotationResetAudit {
    version: u32,
    rotation_id: Uuid,
    node_id: Uuid,
    source_generation_id: Uuid,
    reset_at: DateTime<Utc>,
}

impl RotationAudit {
    fn activated_ack(&self) -> anyhow::Result<CertificateRotationActivatedData> {
        ensure!(
            self.status == RotationAuditStatus::Activated,
            "certificate rotation was not activated"
        );
        let activated_at = self
            .activated_at
            .ok_or_else(|| anyhow!("activated rotation audit is missing its timestamp"))?;
        Ok(CertificateRotationActivatedData {
            rotation_id: self.rotation_id,
            activated_at_ms: activated_at.timestamp_millis(),
            control_fingerprint_sha256: self.control_fingerprint_sha256.clone(),
            management_fingerprint_sha256: self.management_fingerprint_sha256.clone(),
        })
    }
}

impl PendingRotation {
    fn request_data(&self) -> CertificateRotationRequestData {
        CertificateRotationRequestData {
            rotation_id: self.metadata.rotation_id,
            control_csr_pem: self.identity.csr_pem.clone(),
            management_csr_pem: self.identity.management_csr_pem.clone(),
        }
    }
}

struct ValidatedCertificate {
    fingerprint_sha256: String,
    serial_number: String,
    not_before: DateTime<Utc>,
    not_after: DateTime<Utc>,
}

struct ValidatedEnrollmentBundle {
    control: ValidatedCertificate,
    management: ValidatedCertificate,
    management_dns_name: String,
    agent_client_issuer_ca_sha256: String,
    control_plane_server_ca_sha256: String,
    management_client_ca_sha256: String,
    capability_jwt_kid: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallFailpoint {
    None,
    AfterPrivateKey,
    AfterCertificate,
    BeforeCurrentPointer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RotationInstallFailpoint {
    None,
    AfterGenerationPublished,
    AfterSwitchAuthorized,
    AfterCurrentPointer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RotationActivationFailpoint {
    None,
    AfterAudit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationRetirementFailpoint {
    None,
    AfterRename,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingCleanupFailpoint {
    None,
    AfterRetiredRename,
    AfterFirstRetiredEntry,
    AfterRetiredDirectoryRemoval,
}

#[derive(Clone)]
struct EnrollArgs {
    node_id: Uuid,
    core_url: reqwest::Url,
    server_ca: Option<PathBuf>,
    identity_dir: PathBuf,
}

#[derive(Clone)]
struct IdentityCheckArgs {
    node_id: Uuid,
    identity_dir: PathBuf,
}

impl fmt::Debug for EnrollArgs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnrollArgs")
            .field("node_id", &self.node_id)
            .field("core_url", &self.core_url)
            .field("server_ca", &self.server_ca)
            .field("identity_dir", &self.identity_dir)
            .field("enrollment_token", &"[STDIN/REDACTED]")
            .finish()
    }
}

#[derive(Serialize)]
struct AgentEnrollRequest<'a> {
    node_id: Uuid,
    csr_pem: &'a str,
    management_csr_pem: &'a str,
}

#[derive(Deserialize)]
struct AgentEnrollResponse {
    node_id: Uuid,
    certificate_pem: String,
    ca_certificate_pem: String,
    fingerprint_sha256: String,
    serial_number: String,
    not_before: DateTime<Utc>,
    not_after: DateTime<Utc>,
    agent_client_issuer_ca_pem: String,
    control_plane_server_ca_pem: String,
    management_client_ca_pem: String,
    management_certificate_pem: String,
    management_fingerprint_sha256: String,
    management_serial_number: String,
    management_not_before: DateTime<Utc>,
    management_not_after: DateTime<Utc>,
    capability_jwt_public_key_pem: String,
    capability_jwt_kid: String,
}

fn rotation_due(not_after: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    not_after <= now + ROTATION_WINDOW
}

fn parse_canonical_uuid(value: &str, name: &str) -> anyhow::Result<Uuid> {
    let parsed = Uuid::parse_str(value).with_context(|| format!("{name} must be a UUID"))?;
    ensure!(
        !parsed.is_nil() && value == parsed.to_string(),
        "{name} must be a canonical non-nil UUID"
    );
    Ok(parsed)
}

fn timestamp_millis(value: i64, name: &str) -> anyhow::Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(value)
        .ok_or_else(|| anyhow!("{name} is outside the supported timestamp range"))
}

fn rotation_bundle_as_enrollment_response(
    pending: &PendingIdentity,
    bundle: &media_rpc::control_plane::CertificateRotationBundle,
) -> anyhow::Result<AgentEnrollResponse> {
    Ok(AgentEnrollResponse {
        node_id: pending.node_id,
        certificate_pem: bundle.control_certificate_pem.clone(),
        ca_certificate_pem: bundle.agent_client_issuer_ca_pem.clone(),
        fingerprint_sha256: bundle.control_fingerprint_sha256.clone(),
        serial_number: bundle.control_serial_number.clone(),
        not_before: timestamp_millis(
            bundle.control_not_before_ms,
            "rotation control certificate not-before",
        )?,
        not_after: timestamp_millis(
            bundle.control_not_after_ms,
            "rotation control certificate not-after",
        )?,
        agent_client_issuer_ca_pem: bundle.agent_client_issuer_ca_pem.clone(),
        control_plane_server_ca_pem: bundle.control_plane_server_ca_pem.clone(),
        management_client_ca_pem: bundle.management_client_ca_pem.clone(),
        management_certificate_pem: bundle.management_certificate_pem.clone(),
        management_fingerprint_sha256: bundle.management_fingerprint_sha256.clone(),
        management_serial_number: bundle.management_serial_number.clone(),
        management_not_before: timestamp_millis(
            bundle.management_not_before_ms,
            "rotation management certificate not-before",
        )?,
        management_not_after: timestamp_millis(
            bundle.management_not_after_ms,
            "rotation management certificate not-after",
        )?,
        capability_jwt_public_key_pem: bundle.capability_jwt_public_key_pem.clone(),
        capability_jwt_kid: bundle.capability_jwt_kid.clone(),
    })
}

pub(crate) async fn run_enrollment_cli_if_requested() -> anyhow::Result<bool> {
    let mut process_args = std::env::args().skip(1);
    let Some(command) = process_args.next() else {
        return Ok(false);
    };
    if command == "identity" {
        ensure!(
            process_args.next().as_deref() == Some("check"),
            "identity requires the `check` subcommand"
        );
        let arguments = parse_identity_check_args(process_args)?;
        let identity = AgentIdentityStore::new(&arguments.identity_dir)
            .check_current_read_only(arguments.node_id, Utc::now())?;
        println!(
            "Agent identity check passed for node {} generation {}; earliest leaf expiry {}",
            identity.node_id(),
            identity.generation_id(),
            identity.rotation_not_after()?.to_rfc3339()
        );
        return Ok(true);
    }
    ensure!(
        command == "enroll",
        "unknown media-agent command; expected `enroll`, `identity check`, or no command"
    );
    let arguments = parse_enroll_args(process_args)?;
    let store = AgentIdentityStore::new(&arguments.identity_dir);
    let lock = store.acquire_enrollment_lock()?;
    let pending = match store.prepare_enrollment(&lock, arguments.node_id, Utc::now())? {
        EnrollmentPreparation::Pending(pending) => pending,
        EnrollmentPreparation::Recovered(identity) => {
            println!(
                "Agent {} enrollment recovered; identity expires at {}",
                identity.node_id(),
                identity.not_after().to_rfc3339()
            );
            return Ok(true);
        }
    };
    let token = read_enrollment_token_from_stdin()?;
    let endpoint = arguments
        .core_url
        .join("/api/v1/agent-enroll")
        .context("failed to construct Core enrollment endpoint")?;

    let mut builder = enrollment_client_builder(true, arguments.server_ca.is_none());
    if let Some(ca_path) = &arguments.server_ca {
        let ca_bundle = read_regular_file_limited(ca_path, MAX_IDENTITY_FILE_BYTES)?;
        let certificates = reqwest::Certificate::from_pem_bundle(&ca_bundle)
            .context("explicit Core HTTPS CA bundle is invalid")?;
        ensure!(
            !certificates.is_empty(),
            "explicit Core HTTPS CA bundle contains no certificates"
        );
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }
    let client = builder
        .build()
        .context("failed to build the enrollment HTTPS client")?;
    let authorization_header = enrollment_authorization_header(token.as_str())?;

    let response = client
        .post(endpoint)
        .header(reqwest::header::AUTHORIZATION, authorization_header)
        .json(&AgentEnrollRequest {
            node_id: pending.node_id(),
            csr_pem: pending.csr_pem(),
            management_csr_pem: pending.management_csr_pem(),
        })
        .send()
        .await
        .context("Core enrollment HTTPS request failed")?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "Core rejected Agent enrollment with HTTP {}",
        response.status()
    );
    let body = read_bounded_response(response, MAX_ENROLLMENT_RESPONSE_BYTES).await?;
    let issued: AgentEnrollResponse =
        serde_json::from_slice(&body).context("Core enrollment response is invalid")?;
    let installed = store.commit_enrollment_response(&lock, &pending, &issued, Utc::now())?;
    println!(
        "Agent {} enrolled; identity expires at {}",
        installed.node_id(),
        installed.not_after().to_rfc3339()
    );
    Ok(true)
}

fn parse_identity_check_args(
    arguments: impl IntoIterator<Item = String>,
) -> anyhow::Result<IdentityCheckArgs> {
    let mut arguments = arguments.into_iter();
    let mut node_id = None;
    let mut identity_dir = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--node-id" => {
                let value = required_argument_value(&mut arguments, "--node-id")?;
                set_once(
                    &mut node_id,
                    parse_canonical_uuid(&value, "--node-id")?,
                    "--node-id",
                )?;
            }
            "--identity-dir" => set_once(
                &mut identity_dir,
                PathBuf::from(required_argument_value(&mut arguments, "--identity-dir")?),
                "--identity-dir",
            )?,
            _ => bail!("unsupported identity check argument"),
        }
    }
    let node_id = node_id.ok_or_else(|| anyhow!("identity check requires --node-id"))?;
    let identity_dir =
        identity_dir.ok_or_else(|| anyhow!("identity check requires --identity-dir"))?;
    ensure!(
        identity_dir.is_absolute(),
        "--identity-dir must be an absolute path"
    );
    Ok(IdentityCheckArgs {
        node_id,
        identity_dir,
    })
}

fn enrollment_client_builder(
    require_https: bool,
    use_builtin_roots: bool,
) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .tls_built_in_root_certs(use_builtin_roots)
        .user_agent("streamserver-media-agent-enrollment/1");
    if require_https {
        builder.https_only(true)
    } else {
        builder
    }
}

fn enrollment_authorization_header(token: &str) -> anyhow::Result<reqwest::header::HeaderValue> {
    let mut authorization = Zeroizing::new(format!("Bearer {token}"));
    let mut header = reqwest::header::HeaderValue::from_str(authorization.as_str())
        .context("enrollment token cannot be represented as an Authorization header")?;
    authorization.zeroize();
    header.set_sensitive(true);
    Ok(header)
}

fn parse_enroll_args(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<EnrollArgs> {
    let mut arguments = arguments.into_iter();
    let mut node_id = None;
    let mut core_url = None;
    let mut server_ca = None;
    let mut identity_dir = None;
    let mut token_stdin = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--node-id" => set_once(
                &mut node_id,
                Uuid::parse_str(&required_argument_value(&mut arguments, "--node-id")?)
                    .context("--node-id must be a UUID")?,
                "--node-id",
            )?,
            "--core-url" => set_once(
                &mut core_url,
                reqwest::Url::parse(&required_argument_value(&mut arguments, "--core-url")?)
                    .context("--core-url must be an absolute HTTPS URL")?,
                "--core-url",
            )?,
            "--server-ca" => set_once(
                &mut server_ca,
                PathBuf::from(required_argument_value(&mut arguments, "--server-ca")?),
                "--server-ca",
            )?,
            "--identity-dir" => set_once(
                &mut identity_dir,
                PathBuf::from(required_argument_value(&mut arguments, "--identity-dir")?),
                "--identity-dir",
            )?,
            "--token-stdin" => {
                ensure!(!token_stdin, "--token-stdin may appear only once");
                token_stdin = true;
            }
            _ => bail!(
                "unsupported enrollment argument; the token is accepted only via --token-stdin"
            ),
        }
    }

    let node_id = node_id.ok_or_else(|| anyhow!("enroll requires --node-id"))?;
    ensure!(!node_id.is_nil(), "--node-id must not be nil");
    let core_url = core_url.ok_or_else(|| anyhow!("enroll requires --core-url"))?;
    ensure!(
        core_url.scheme() == "https"
            && core_url.host_str().is_some()
            && core_url.username().is_empty()
            && core_url.password().is_none()
            && core_url.query().is_none()
            && core_url.fragment().is_none()
            && matches!(core_url.path(), "" | "/"),
        "--core-url must be an origin-only HTTPS URL without credentials, query, or fragment"
    );
    let identity_dir = identity_dir.ok_or_else(|| anyhow!("enroll requires --identity-dir"))?;
    ensure!(
        identity_dir.is_absolute(),
        "--identity-dir must be an absolute path"
    );
    if let Some(path) = &server_ca {
        ensure!(path.is_absolute(), "--server-ca must be an absolute path");
    }
    ensure!(
        token_stdin,
        "enroll requires --token-stdin; enrollment tokens are never accepted in argv or the environment"
    );
    Ok(EnrollArgs {
        node_id,
        core_url,
        server_ca,
        identity_dir,
    })
}

fn required_argument_value(
    arguments: &mut impl Iterator<Item = String>,
    name: &str,
) -> anyhow::Result<String> {
    let value = arguments
        .next()
        .ok_or_else(|| anyhow!("{name} requires a value"))?;
    ensure!(!value.is_empty(), "{name} must not be empty");
    Ok(value)
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> anyhow::Result<()> {
    ensure!(slot.is_none(), "{name} may appear only once");
    *slot = Some(value);
    Ok(())
}

fn read_enrollment_token_from_stdin() -> anyhow::Result<Zeroizing<String>> {
    let mut bytes = Zeroizing::new(Vec::new());
    std::io::stdin()
        .lock()
        .take(4097)
        .read_to_end(&mut bytes)
        .context("failed to read enrollment token from stdin")?;
    ensure!(
        bytes.len() <= 4096,
        "enrollment token on stdin exceeds the size limit"
    );
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    ensure!(
        is_valid_enrollment_token_wire(&bytes),
        "enrollment token on stdin has an invalid format"
    );
    let token =
        String::from_utf8(bytes.to_vec()).context("enrollment token on stdin is not UTF-8")?;
    Ok(Zeroizing::new(token))
}

fn is_valid_enrollment_token_wire(token: &[u8]) -> bool {
    token.len() == 146
        && token.starts_with(b"ssae1.")
        && token[102] == b'.'
        && token[6..102]
            .iter()
            .chain(token[103..].iter())
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_'))
}

async fn read_bounded_response(
    mut response: reqwest::Response,
    limit: usize,
) -> anyhow::Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        ensure!(
            length <= limit as u64,
            "Core enrollment response exceeds the size limit"
        );
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read Core enrollment response")?
    {
        ensure!(
            body.len().saturating_add(chunk.len()) <= limit,
            "Core enrollment response exceeds the size limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl AgentIdentityStore {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub(crate) fn on_authenticated_session(
        &self,
        current_generation_id: Uuid,
        now: DateTime<Utc>,
    ) -> anyhow::Result<AuthenticatedRotationAction> {
        let lock = self.acquire_enrollment_lock()?;
        self.validate_enrollment_lock(&lock)?;
        self.cleanup_stale_staging_directories()?;
        let current = self
            .load_current(now)
            .map_err(anyhow::Error::new)
            .context("failed to load current Agent identity for rotation")?;
        current.ensure_production_complete()?;
        let pending_path = self.root.join("pending-rotation");
        let pending = if path_exists_no_follow(&pending_path)? {
            self.cleanup_pending_rotation_metadata_temps()?;
            Some(self.load_pending_rotation()?)
        } else {
            None
        };
        if current.metadata.generation_id != current_generation_id {
            if pending.as_ref().is_some_and(|pending| {
                pending.metadata.stage == RotationStage::SwitchAuthorized
                    && pending.metadata.source_generation_id == current_generation_id
                    && pending.metadata.target_generation_id == Some(current.metadata.generation_id)
            }) {
                return Ok(AuthenticatedRotationAction::RestartRequired);
            }
            bail!("authenticated Agent identity generation does not match the current pointer");
        }
        self.retire_previous_generation_if_overlap_ended(current_generation_id, now)?;

        if let Some(pending) = pending {
            ensure!(
                pending.metadata.node_id == current.node_id(),
                "pending certificate rotation belongs to another Agent"
            );
            let requested_was_reset = pending.metadata.stage == RotationStage::Requested
                && self
                    .read_rotation_reset_audit(pending.metadata.rotation_id)?
                    .is_some();
            if pending.metadata.stage == RotationStage::Requested
                && (requested_was_reset
                    || now >= pending.metadata.created_at + ROTATION_BUNDLE_MAX_LIFETIME)
            {
                self.retire_requested_rotation(&pending, now)?;
            } else {
                return match pending.metadata.stage {
                    RotationStage::Requested => {
                        ensure!(
                            pending.metadata.source_generation_id == current_generation_id,
                            "pending certificate rotation belongs to another source generation"
                        );
                        let orphan_target = self
                            .root
                            .join("generations")
                            .join(pending.metadata.rotation_id.to_string());
                        if path_exists_no_follow(&orphan_target)? {
                            remove_secure_identity_tree(&orphan_target)?;
                            sync_directory(&self.root.join("generations"))?;
                        }
                        Ok(AuthenticatedRotationAction::SendRequest(
                            pending.request_data(),
                        ))
                    }
                    RotationStage::SwitchAuthorized => {
                        let target_generation_id =
                            pending.metadata.target_generation_id.ok_or_else(|| {
                                anyhow!("authorized rotation has no target generation")
                            })?;
                        ensure!(
                            target_generation_id == pending.metadata.rotation_id,
                            "authorized rotation target is inconsistent"
                        );
                        if current_generation_id == pending.metadata.source_generation_id {
                            return Ok(AuthenticatedRotationAction::RestartRequired);
                        }
                        ensure!(
                            target_generation_id == current_generation_id,
                            "authenticated session did not use the rotated certificate generation"
                        );
                        ensure!(
                            pending.metadata.bundle_expires_at.is_some(),
                            "authorized rotation has no expiry"
                        );
                        // A successful StreamConnect response is returned only after Core has
                        // authenticated and durably claimed this new certificate generation.
                        // Core owns the authorization deadline; the response may cross that
                        // boundary in flight, so rolling back here could resurrect a certificate
                        // Core has already fenced as replaced.
                        let mut metadata = pending.metadata.clone();
                        metadata.stage = RotationStage::Consumed;
                        metadata.new_session_consumed_at = Some(now);
                        self.replace_pending_rotation_metadata(&metadata)?;
                        Ok(AuthenticatedRotationAction::None)
                    }
                    RotationStage::Consumed => {
                        ensure!(
                            pending.metadata.target_generation_id == Some(current_generation_id)
                                && pending.metadata.new_session_consumed_at.is_some(),
                            "consumed rotation state does not match the authenticated generation"
                        );
                        Ok(AuthenticatedRotationAction::None)
                    }
                };
            }
        }

        if !rotation_due(current.rotation_not_after()?, now) {
            return Ok(AuthenticatedRotationAction::None);
        }

        let pending = Self::prepare(current.node_id())?;
        let new_control_key = KeyPair::from_pem(pending.private_key_pem.as_str())
            .context("new rotation control key is invalid")?;
        let new_management_key = KeyPair::from_pem(pending.management_private_key_pem.as_str())
            .context("new rotation management key is invalid")?;
        let current_control_key = KeyPair::from_pem(current.private_key_pem.as_str())
            .context("current Agent control key is invalid")?;
        let current_management_key = KeyPair::from_pem(
            current
                .management_private_key_pem
                .as_deref()
                .expect("complete identity checked"),
        )
        .context("current Agent management key is invalid")?;
        for new_key in [&new_control_key, &new_management_key] {
            ensure!(
                new_key.public_key_der() != current_control_key.public_key_der()
                    && new_key.public_key_der() != current_management_key.public_key_der(),
                "certificate rotation must generate fresh private keys"
            );
        }
        let metadata = PendingRotationMetadata {
            version: PENDING_ROTATION_METADATA_VERSION,
            rotation_id: pending.generation_id,
            node_id: pending.node_id,
            source_generation_id: current_generation_id,
            created_at: now,
            stage: RotationStage::Requested,
            target_generation_id: None,
            bundle_expires_at: None,
            new_session_consumed_at: None,
        };
        self.persist_pending_rotation(&pending, &metadata)?;
        Ok(AuthenticatedRotationAction::SendRequest(
            PendingRotation {
                identity: pending,
                metadata,
            }
            .request_data(),
        ))
    }

    pub(crate) fn commit_rotation_bundle(
        &self,
        current_generation_id: Uuid,
        bundle: &media_rpc::control_plane::CertificateRotationBundle,
        now: DateTime<Utc>,
    ) -> anyhow::Result<RotationCommitOutcome> {
        self.commit_rotation_bundle_with_failpoint(
            current_generation_id,
            bundle,
            now,
            RotationInstallFailpoint::None,
        )
    }

    fn commit_rotation_bundle_with_failpoint(
        &self,
        current_generation_id: Uuid,
        bundle: &media_rpc::control_plane::CertificateRotationBundle,
        now: DateTime<Utc>,
        failpoint: RotationInstallFailpoint,
    ) -> anyhow::Result<RotationCommitOutcome> {
        let lock = self.acquire_enrollment_lock()?;
        self.validate_enrollment_lock(&lock)?;
        self.cleanup_stale_staging_directories()?;
        self.cleanup_pending_rotation_metadata_temps()?;
        let current = self
            .load_current(now)
            .map_err(anyhow::Error::new)
            .context("failed to load current identity before certificate rotation")?;
        current.ensure_production_complete()?;
        ensure!(
            current.metadata.generation_id == current_generation_id,
            "certificate rotation bundle was delivered to a stale Agent generation"
        );
        let mut pending = self.load_pending_rotation()?;
        ensure!(
            pending.metadata.stage == RotationStage::Requested
                && pending.metadata.source_generation_id == current_generation_id,
            "certificate rotation bundle is stale or was already consumed"
        );
        let rotation_id = parse_canonical_uuid(&bundle.rotation_id, "rotation_id")?;
        ensure!(
            rotation_id == pending.metadata.rotation_id
                && rotation_id == pending.identity.generation_id,
            "certificate rotation bundle does not match the pending rotation"
        );
        let expires_at = timestamp_millis(bundle.expires_at_ms, "rotation bundle expiry")?;
        ensure!(
            expires_at > now && expires_at <= now + ROTATION_BUNDLE_MAX_LIFETIME,
            "certificate rotation bundle expiry is outside the allowed five-minute window"
        );
        let response = rotation_bundle_as_enrollment_response(&pending.identity, bundle)?;
        let validated = validate_complete_enrollment_response(&pending.identity, &response, now)?;
        ensure!(
            current.agent_client_issuer_ca_pem.as_deref()
                == Some(bundle.agent_client_issuer_ca_pem.as_str())
                && current.control_plane_server_ca_pem.as_deref()
                    == Some(bundle.control_plane_server_ca_pem.as_str())
                && current.management_client_ca_pem.as_deref()
                    == Some(bundle.management_client_ca_pem.as_str())
                && current.capability_jwt_public_key_pem.as_deref()
                    == Some(bundle.capability_jwt_public_key_pem.as_str())
                && current.metadata.capability_jwt_kid.as_deref()
                    == Some(bundle.capability_jwt_kid.as_str()),
            "certificate rotation bundle attempted to replace pinned trust material"
        );
        ensure!(
            validated.control.fingerprint_sha256 != current.metadata.fingerprint_sha256
                && Some(validated.control.serial_number.as_str())
                    != current.metadata.serial_number.as_deref()
                && Some(validated.management.fingerprint_sha256.as_str())
                    != current.metadata.management_fingerprint_sha256.as_deref()
                && Some(validated.management.serial_number.as_str())
                    != current.metadata.management_serial_number.as_deref(),
            "certificate rotation must issue new certificates and serials"
        );

        self.publish_rotation_generation(&pending.identity, &response, &validated, rotation_id)?;
        fail_rotation_install_at(
            failpoint,
            RotationInstallFailpoint::AfterGenerationPublished,
        )?;
        pending.metadata.stage = RotationStage::SwitchAuthorized;
        pending.metadata.target_generation_id = Some(rotation_id);
        pending.metadata.bundle_expires_at = Some(expires_at);
        self.replace_pending_rotation_metadata(&pending.metadata)?;
        fail_rotation_install_at(failpoint, RotationInstallFailpoint::AfterSwitchAuthorized)?;
        self.write_current_pointer(rotation_id)?;
        fail_rotation_install_at(failpoint, RotationInstallFailpoint::AfterCurrentPointer)?;
        Ok(RotationCommitOutcome::RestartRequired)
    }

    pub(crate) fn load_current_for_startup(
        &self,
        now: DateTime<Utc>,
    ) -> Result<LoadedIdentity, AgentIdentityLoadError> {
        if !path_exists_no_follow(&self.root)? {
            return Err(AgentIdentityLoadError::NotEnrolled);
        }
        self.recover_rotation_for_startup(now)
            .map_err(AgentIdentityLoadError::Invalid)
    }

    pub(crate) fn check_current_read_only(
        &self,
        expected_node_id: Uuid,
        now: DateTime<Utc>,
    ) -> anyhow::Result<LoadedIdentity> {
        ensure!(
            self.root.is_absolute(),
            "Agent identity directory must be absolute"
        );
        ensure!(
            !expected_node_id.is_nil(),
            "Agent identity node UUID must not be nil"
        );
        let identity = self
            .load_current(now)
            .map_err(anyhow::Error::new)
            .context("failed to validate current Agent identity")?;
        identity.ensure_production_complete()?;
        ensure!(
            identity.node_id() == expected_node_id,
            "Agent identity belongs to another node"
        );
        Ok(identity)
    }

    pub(crate) fn reset_requested_rotation(&self, rotation_id: &str) -> anyhow::Result<()> {
        let rotation_id = parse_canonical_uuid(rotation_id, "rotation reset ID")?;
        let lock = self.acquire_enrollment_lock()?;
        self.validate_enrollment_lock(&lock)?;
        if self.read_rotation_reset_audit(rotation_id)?.is_some() {
            let pending_path = self.root.join("pending-rotation");
            if path_exists_no_follow(&pending_path)? {
                let pending = self.load_pending_rotation()?;
                if pending.metadata.rotation_id == rotation_id {
                    return self.retire_requested_rotation(&pending, Utc::now());
                }
            }
            return Ok(());
        }
        let pending = self.load_pending_rotation()?;
        ensure!(
            pending.metadata.rotation_id == rotation_id
                && pending.metadata.stage == RotationStage::Requested,
            "certificate rotation reset does not match the requested rotation"
        );
        self.retire_requested_rotation(&pending, Utc::now())
    }

    pub(crate) fn replayable_activation_ack(
        &self,
        current_generation_id: Uuid,
    ) -> anyhow::Result<Option<CertificateRotationActivatedData>> {
        let lock = self.acquire_enrollment_lock()?;
        self.validate_enrollment_lock(&lock)?;
        ensure!(
            self.read_current_generation_id()? == current_generation_id,
            "cannot replay a certificate activation for a stale generation"
        );
        let Some(audit) = self.read_rotation_audit(current_generation_id)? else {
            return Ok(None);
        };
        if audit.status == RotationAuditStatus::RolledBack {
            return Ok(None);
        }
        audit.activated_ack().map(Some)
    }

    pub(crate) fn activate_rotation(
        &self,
        current_generation_id: Uuid,
        command: &media_rpc::control_plane::ActivateCertificateRotation,
        now: DateTime<Utc>,
    ) -> anyhow::Result<CertificateRotationActivatedData> {
        self.activate_rotation_with_failpoint(
            current_generation_id,
            command,
            now,
            RotationActivationFailpoint::None,
        )
    }

    fn activate_rotation_with_failpoint(
        &self,
        current_generation_id: Uuid,
        command: &media_rpc::control_plane::ActivateCertificateRotation,
        now: DateTime<Utc>,
        failpoint: RotationActivationFailpoint,
    ) -> anyhow::Result<CertificateRotationActivatedData> {
        let rotation_id = parse_canonical_uuid(&command.rotation_id, "rotation_id")?;
        let lock = self.acquire_enrollment_lock()?;
        self.validate_enrollment_lock(&lock)?;
        self.cleanup_stale_staging_directories()?;
        if let Some(audit) = self.read_rotation_audit(rotation_id)? {
            ensure!(
                audit.status == RotationAuditStatus::Activated
                    && audit.target_generation_id == current_generation_id
                    && self.read_current_generation_id()? == current_generation_id,
                "certificate rotation activation replay conflicts with local state"
            );
            let pending_path = self.root.join("pending-rotation");
            if path_exists_no_follow(&pending_path)? {
                self.cleanup_pending_rotation_metadata_temps()?;
                let pending = self.load_pending_rotation()?;
                ensure!(
                    pending.metadata.rotation_id == rotation_id
                        && pending.metadata.target_generation_id == Some(rotation_id),
                    "persisted activation audit conflicts with pending rotation"
                );
                self.clear_pending_rotation(&pending)?;
            }
            return audit.activated_ack();
        }

        ensure!(
            rotation_id == current_generation_id
                && self.read_current_generation_id()? == current_generation_id,
            "certificate rotation activation targets a stale Agent generation"
        );
        let pending = self.load_pending_rotation()?;
        ensure!(
            pending.metadata.rotation_id == rotation_id
                && pending.metadata.target_generation_id == Some(rotation_id)
                && pending.metadata.stage == RotationStage::Consumed
                && pending.metadata.new_session_consumed_at.is_some(),
            "certificate rotation cannot activate before the new identity session is consumed"
        );
        let previous_identity_expires_at = timestamp_millis(
            command.previous_identity_expires_at_ms,
            "previous identity expiry",
        )?;
        ensure!(
            previous_identity_expires_at > now
                && previous_identity_expires_at <= now + ROTATION_BUNDLE_MAX_LIFETIME,
            "previous identity expiry is outside the allowed five-minute overlap"
        );
        let current = self
            .load_generation(current_generation_id, now)
            .context("failed to load rotated identity for activation")?;
        let audit = RotationAudit {
            version: ROTATION_AUDIT_VERSION,
            status: RotationAuditStatus::Activated,
            rotation_id,
            node_id: pending.metadata.node_id,
            source_generation_id: pending.metadata.source_generation_id,
            target_generation_id: rotation_id,
            created_at: pending.metadata.created_at,
            bundle_expires_at: pending
                .metadata
                .bundle_expires_at
                .ok_or_else(|| anyhow!("consumed rotation is missing its bundle expiry"))?,
            new_session_consumed_at: pending.metadata.new_session_consumed_at,
            activated_at: Some(now),
            previous_identity_expires_at: Some(previous_identity_expires_at),
            control_fingerprint_sha256: current.metadata.fingerprint_sha256.clone(),
            management_fingerprint_sha256: current
                .metadata
                .management_fingerprint_sha256
                .clone()
                .ok_or_else(|| {
                anyhow!("rotated management certificate fingerprint is missing")
            })?,
        };
        self.write_rotation_audit(&audit)?;
        fail_rotation_activation_at(failpoint, RotationActivationFailpoint::AfterAudit)?;
        self.clear_pending_rotation(&pending)?;
        audit.activated_ack()
    }

    fn recover_rotation_for_startup(&self, now: DateTime<Utc>) -> anyhow::Result<LoadedIdentity> {
        let lock = self.acquire_enrollment_lock()?;
        self.validate_enrollment_lock(&lock)?;
        self.cleanup_stale_staging_directories()?;
        let current_generation_id = self.read_current_generation_id()?;
        self.cleanup_retired_rotation_generations(current_generation_id)?;
        self.cleanup_audited_rotation_orphans(current_generation_id)?;
        self.retire_previous_generation_if_overlap_ended(current_generation_id, now)?;
        let pending_path = self.root.join("pending-rotation");
        if !path_exists_no_follow(&pending_path)? {
            return self.load_current(now).map_err(anyhow::Error::new);
        }
        self.cleanup_pending_rotation_metadata_temps()?;
        let pending = self.load_pending_rotation()?;
        ensure!(
            pending.metadata.rotation_id == pending.identity.generation_id
                && pending.metadata.rotation_id != pending.metadata.source_generation_id,
            "pending certificate rotation generation metadata is inconsistent"
        );
        let source_generation_id = pending.metadata.source_generation_id;
        let target_generation_id = pending.metadata.rotation_id;
        let target_path = self
            .root
            .join("generations")
            .join(target_generation_id.to_string());
        if let Some(audit) = self.read_rotation_audit(target_generation_id)? {
            ensure!(
                audit.node_id == pending.metadata.node_id
                    && audit.source_generation_id == source_generation_id,
                "certificate rotation audit conflicts with pending state"
            );
            return match audit.status {
                RotationAuditStatus::Activated => {
                    ensure!(
                        current_generation_id == target_generation_id,
                        "activated certificate rotation does not own the current pointer"
                    );
                    let target = self.load_generation(target_generation_id, now)?;
                    self.clear_pending_rotation(&pending)?;
                    Ok(target)
                }
                RotationAuditStatus::RolledBack => {
                    ensure!(
                        current_generation_id == source_generation_id,
                        "rolled-back certificate rotation does not own the current pointer"
                    );
                    let source = self.load_generation(source_generation_id, now)?;
                    if path_exists_no_follow(&target_path)? {
                        remove_secure_identity_tree(&target_path)?;
                        sync_directory(&self.root.join("generations"))?;
                    }
                    self.clear_pending_rotation(&pending)?;
                    Ok(source)
                }
            };
        }

        match pending.metadata.stage {
            RotationStage::Requested => {
                ensure!(
                    current_generation_id == source_generation_id,
                    "requested certificate rotation conflicts with the current generation"
                );
                if path_exists_no_follow(&target_path)? {
                    remove_secure_identity_tree(&target_path)?;
                    sync_directory(&self.root.join("generations"))?;
                }
                self.load_generation(source_generation_id, now)
            }
            RotationStage::SwitchAuthorized => {
                ensure!(
                    pending.metadata.target_generation_id == Some(target_generation_id),
                    "authorized certificate rotation target is inconsistent"
                );
                let expires_at = pending
                    .metadata
                    .bundle_expires_at
                    .ok_or_else(|| anyhow!("authorized certificate rotation has no expiry"))?;
                ensure!(
                    current_generation_id == source_generation_id
                        || current_generation_id == target_generation_id,
                    "authorized certificate rotation conflicts with the current pointer"
                );
                let target = self
                    .load_generation(target_generation_id, now)
                    .context("rotated Agent generation is incomplete")?;
                ensure!(
                    target.node_id() == pending.metadata.node_id,
                    "rotated Agent generation belongs to another node"
                );
                if now >= expires_at {
                    self.rollback_pending_rotation(&pending, &target, now)?;
                    return self.load_generation(source_generation_id, now);
                }
                if current_generation_id == source_generation_id {
                    self.write_current_pointer(target_generation_id)?;
                }
                Ok(target)
            }
            RotationStage::Consumed => {
                ensure!(
                    pending.metadata.target_generation_id == Some(target_generation_id)
                        && pending.metadata.new_session_consumed_at.is_some()
                        && current_generation_id == target_generation_id,
                    "consumed certificate rotation state is inconsistent"
                );
                self.load_generation(target_generation_id, now)
            }
        }
    }

    fn read_current_generation_id(&self) -> anyhow::Result<Uuid> {
        let current_path = self.root.join("current");
        assert_regular_file_no_symlink(&current_path)?;
        let current = read_limited(&current_path, 128)?;
        let value = std::str::from_utf8(&current)
            .context("Agent identity current pointer is not UTF-8")?
            .trim();
        parse_canonical_uuid(value, "Agent identity current pointer")
    }

    fn cleanup_pending_rotation_metadata_temps(&self) -> anyhow::Result<()> {
        let pending_dir = self.root.join("pending-rotation");
        assert_secure_directory(&pending_dir)?;
        for entry in
            fs::read_dir(&pending_dir).context("failed to inspect pending certificate rotation")?
        {
            let entry = entry.context("failed to inspect pending rotation entry")?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow!("pending rotation contains a non-UTF-8 entry"))?;
            if name
                .strip_prefix(".metadata-")
                .is_some_and(|suffix| Uuid::parse_str(suffix).is_ok())
            {
                let metadata = fs::symlink_metadata(entry.path())
                    .context("failed to inspect stale rotation metadata")?;
                ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "stale rotation metadata path is not a regular file"
                );
                assert_private_identity_file_metadata(&entry.path(), &metadata)?;
                fs::remove_file(entry.path())
                    .context("failed to remove stale rotation metadata")?;
            }
        }
        sync_directory(&pending_dir)
    }

    fn clear_pending_rotation(&self, expected: &PendingRotation) -> anyhow::Result<()> {
        let current = self.load_pending_rotation()?;
        ensure!(
            current.metadata.rotation_id == expected.metadata.rotation_id,
            "pending certificate rotation changed before cleanup"
        );
        let pending_path = self.root.join("pending-rotation");
        let retired_path = self.root.join(format!(
            ".retired-rotation-{}",
            expected.metadata.rotation_id
        ));
        ensure!(
            !path_exists_no_follow(&retired_path)?,
            "retired certificate rotation path already exists"
        );
        fs::rename(&pending_path, &retired_path)
            .context("failed to retire pending certificate rotation")?;
        sync_directory(&self.root)?;
        remove_secure_identity_tree(&retired_path)?;
        sync_directory(&self.root)
    }

    fn rotation_audit_path(&self, rotation_id: Uuid) -> PathBuf {
        self.root.join(format!("rotation-audit-{rotation_id}.json"))
    }

    fn rotation_reset_audit_path(&self, rotation_id: Uuid) -> PathBuf {
        self.root.join(format!("rotation-reset-{rotation_id}.json"))
    }

    fn read_rotation_reset_audit(
        &self,
        rotation_id: Uuid,
    ) -> anyhow::Result<Option<RotationResetAudit>> {
        let path = self.rotation_reset_audit_path(rotation_id);
        if !path_exists_no_follow(&path)? {
            return Ok(None);
        }
        assert_regular_file_no_symlink(&path)?;
        let audit: RotationResetAudit =
            serde_json::from_slice(&read_limited(&path, MAX_IDENTITY_FILE_BYTES)?)
                .context("certificate rotation reset audit is invalid")?;
        ensure!(
            audit.version == ROTATION_RESET_AUDIT_VERSION
                && audit.rotation_id == rotation_id
                && !audit.node_id.is_nil()
                && !audit.source_generation_id.is_nil()
                && audit.source_generation_id != rotation_id,
            "certificate rotation reset audit metadata is inconsistent"
        );
        Ok(Some(audit))
    }

    fn write_rotation_reset_audit(&self, audit: &RotationResetAudit) -> anyhow::Result<()> {
        ensure!(
            self.read_rotation_reset_audit(audit.rotation_id)?.is_none(),
            "certificate rotation reset audit already exists"
        );
        let encoded = serde_json::to_vec_pretty(audit)
            .context("failed to serialize certificate rotation reset audit")?;
        write_new_secure_file(&self.rotation_reset_audit_path(audit.rotation_id), &encoded)?;
        sync_directory(&self.root)
    }

    fn retire_requested_rotation(
        &self,
        pending: &PendingRotation,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        ensure!(
            pending.metadata.stage == RotationStage::Requested,
            "only an uncommitted certificate rotation can be reset"
        );
        let audit = RotationResetAudit {
            version: ROTATION_RESET_AUDIT_VERSION,
            rotation_id: pending.metadata.rotation_id,
            node_id: pending.metadata.node_id,
            source_generation_id: pending.metadata.source_generation_id,
            reset_at: now,
        };
        if let Some(existing) = self.read_rotation_reset_audit(audit.rotation_id)? {
            ensure!(
                existing.node_id == audit.node_id
                    && existing.source_generation_id == audit.source_generation_id,
                "certificate rotation reset audit conflicts with pending state"
            );
        } else {
            self.write_rotation_reset_audit(&audit)?;
        }
        let orphan_target = self
            .root
            .join("generations")
            .join(pending.metadata.rotation_id.to_string());
        if path_exists_no_follow(&orphan_target)? {
            ensure!(
                self.read_current_generation_id()? != pending.metadata.rotation_id,
                "refusing to remove the current Agent identity as a reset orphan"
            );
            remove_secure_identity_tree(&orphan_target)?;
            sync_directory(&self.root.join("generations"))?;
        }
        self.clear_pending_rotation(pending)?;
        Ok(())
    }

    fn read_rotation_audit(&self, rotation_id: Uuid) -> anyhow::Result<Option<RotationAudit>> {
        let path = self.rotation_audit_path(rotation_id);
        if !path_exists_no_follow(&path)? {
            return Ok(None);
        }
        assert_regular_file_no_symlink(&path)?;
        let audit: RotationAudit =
            serde_json::from_slice(&read_limited(&path, MAX_IDENTITY_FILE_BYTES)?)
                .context("certificate rotation audit is invalid")?;
        ensure!(
            audit.version == ROTATION_AUDIT_VERSION
                && audit.rotation_id == rotation_id
                && !audit.node_id.is_nil()
                && !audit.source_generation_id.is_nil()
                && audit.target_generation_id == rotation_id
                && audit.source_generation_id != audit.target_generation_id
                && audit.control_fingerprint_sha256.len() == 64
                && audit.management_fingerprint_sha256.len() == 64,
            "certificate rotation audit metadata is inconsistent"
        );
        match audit.status {
            RotationAuditStatus::Activated => ensure!(
                audit.new_session_consumed_at.is_some()
                    && audit.activated_at.is_some()
                    && audit.previous_identity_expires_at.is_some(),
                "activated rotation audit is incomplete"
            ),
            RotationAuditStatus::RolledBack => ensure!(
                audit.activated_at.is_none() && audit.previous_identity_expires_at.is_none(),
                "rolled-back rotation audit is inconsistent"
            ),
        }
        Ok(Some(audit))
    }

    fn write_rotation_audit(&self, audit: &RotationAudit) -> anyhow::Result<()> {
        ensure!(
            self.read_rotation_audit(audit.rotation_id)?.is_none(),
            "certificate rotation audit already exists"
        );
        let encoded = serde_json::to_vec_pretty(audit)
            .context("failed to serialize certificate rotation audit")?;
        write_new_secure_file(&self.rotation_audit_path(audit.rotation_id), &encoded)?;
        sync_directory(&self.root)
    }

    fn cleanup_retired_rotation_generations(
        &self,
        current_generation_id: Uuid,
    ) -> anyhow::Result<()> {
        for entry in fs::read_dir(&self.root)
            .context("failed to inspect retired Agent identity generations")?
        {
            let entry = entry.context("failed to inspect retired Agent identity generation")?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow!("identity root contains a non-UTF-8 entry"))?;
            let Some(generation) = name.strip_prefix(".retired-generation-") else {
                continue;
            };
            let generation_id = parse_canonical_uuid(generation, "retired generation ID")?;
            ensure!(
                generation_id != current_generation_id,
                "refusing to remove a retired path for the current Agent identity"
            );
            assert_secure_directory(&entry.path())?;
            remove_secure_identity_tree(&entry.path())?;
        }
        sync_directory(&self.root)
    }

    fn cleanup_audited_rotation_orphans(&self, current_generation_id: Uuid) -> anyhow::Result<()> {
        let generations_dir = self.root.join("generations");
        let mut removed = false;
        for entry in fs::read_dir(&generations_dir)
            .context("failed to inspect Agent identity generations for audited orphans")?
        {
            let entry = entry.context("failed to inspect Agent identity generation")?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let Ok(generation_id) = Uuid::parse_str(&name) else {
                continue;
            };
            if generation_id == current_generation_id {
                continue;
            }

            let reset_requires_cleanup = self
                .read_rotation_reset_audit(generation_id)?
                .map(|audit| {
                    ensure!(
                        audit.source_generation_id == current_generation_id,
                        "rotation reset audit does not belong to the current Agent identity"
                    );
                    Ok::<_, anyhow::Error>(true)
                })
                .transpose()?
                .unwrap_or(false);
            let rollback_requires_cleanup = self
                .read_rotation_audit(generation_id)?
                .map(|audit| {
                    if audit.status != RotationAuditStatus::RolledBack {
                        return Ok(false);
                    }
                    ensure!(
                        audit.source_generation_id == current_generation_id,
                        "rotation rollback audit does not belong to the current Agent identity"
                    );
                    Ok::<_, anyhow::Error>(true)
                })
                .transpose()?
                .unwrap_or(false);
            if !reset_requires_cleanup && !rollback_requires_cleanup {
                continue;
            }

            assert_secure_directory(&entry.path())?;
            remove_secure_identity_tree(&entry.path())?;
            removed = true;
        }
        if removed {
            sync_directory(&generations_dir)?;
        }
        Ok(())
    }

    fn retire_previous_generation_if_overlap_ended(
        &self,
        current_generation_id: Uuid,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        self.retire_previous_generation_with_failpoint(
            current_generation_id,
            now,
            GenerationRetirementFailpoint::None,
        )
    }

    fn retire_previous_generation_with_failpoint(
        &self,
        current_generation_id: Uuid,
        now: DateTime<Utc>,
        failpoint: GenerationRetirementFailpoint,
    ) -> anyhow::Result<()> {
        let Some(audit) = self.read_rotation_audit(current_generation_id)? else {
            return Ok(());
        };
        if audit.status != RotationAuditStatus::Activated {
            return Ok(());
        }
        let overlap_ends = audit
            .previous_identity_expires_at
            .ok_or_else(|| anyhow!("activated rotation audit has no overlap expiry"))?;
        if now < overlap_ends {
            return Ok(());
        }
        ensure!(
            self.read_current_generation_id()? == current_generation_id
                && audit.target_generation_id == current_generation_id
                && audit.source_generation_id != current_generation_id,
            "refusing to retire an Agent identity from a stale rotation audit"
        );
        let source_path = self
            .root
            .join("generations")
            .join(audit.source_generation_id.to_string());
        if !path_exists_no_follow(&source_path)? {
            return Ok(());
        }
        assert_secure_directory(&source_path)?;
        let retired_path = self.root.join(format!(
            ".retired-generation-{}",
            audit.source_generation_id
        ));
        ensure!(
            !path_exists_no_follow(&retired_path)?,
            "retired Agent identity generation path already exists"
        );
        fs::rename(&source_path, &retired_path)
            .context("failed to atomically retire the previous Agent identity generation")?;
        sync_directory(&self.root.join("generations"))?;
        sync_directory(&self.root)?;
        if failpoint == GenerationRetirementFailpoint::AfterRename {
            bail!("injected previous Agent identity retirement failure");
        }
        remove_secure_identity_tree(&retired_path)?;
        sync_directory(&self.root)
    }

    fn rollback_pending_rotation(
        &self,
        pending: &PendingRotation,
        target: &LoadedIdentity,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        ensure!(
            pending.metadata.stage == RotationStage::SwitchAuthorized
                && pending.metadata.target_generation_id == Some(target.metadata.generation_id)
                && pending.metadata.rotation_id == target.metadata.generation_id,
            "certificate rotation cannot be safely rolled back"
        );
        let source = self
            .load_generation(pending.metadata.source_generation_id, now)
            .context("cannot safely roll back to the previous Agent generation")?;
        ensure!(
            source.node_id() == pending.metadata.node_id
                && target.node_id() == pending.metadata.node_id,
            "certificate rotation rollback identity mismatch"
        );
        self.write_current_pointer(pending.metadata.source_generation_id)?;
        let audit = RotationAudit {
            version: ROTATION_AUDIT_VERSION,
            status: RotationAuditStatus::RolledBack,
            rotation_id: pending.metadata.rotation_id,
            node_id: pending.metadata.node_id,
            source_generation_id: pending.metadata.source_generation_id,
            target_generation_id: pending.metadata.rotation_id,
            created_at: pending.metadata.created_at,
            bundle_expires_at: pending
                .metadata
                .bundle_expires_at
                .ok_or_else(|| anyhow!("rotation rollback is missing the bundle expiry"))?,
            new_session_consumed_at: None,
            activated_at: None,
            previous_identity_expires_at: None,
            control_fingerprint_sha256: target.metadata.fingerprint_sha256.clone(),
            management_fingerprint_sha256: target
                .metadata
                .management_fingerprint_sha256
                .clone()
                .ok_or_else(|| anyhow!("rotation rollback management fingerprint is missing"))?,
        };
        self.write_rotation_audit(&audit)?;
        let target_path = self
            .root
            .join("generations")
            .join(pending.metadata.rotation_id.to_string());
        remove_secure_identity_tree(&target_path)?;
        sync_directory(&self.root.join("generations"))?;
        self.clear_pending_rotation(pending)
    }

    fn publish_rotation_generation(
        &self,
        pending: &PendingIdentity,
        response: &AgentEnrollResponse,
        validated: &ValidatedEnrollmentBundle,
        generation_id: Uuid,
    ) -> anyhow::Result<()> {
        ensure!(
            pending.generation_id == generation_id,
            "rotation generation does not match pending key material"
        );
        let generations_dir = self.root.join("generations");
        assert_secure_directory(&generations_dir)?;
        let generation_dir = generations_dir.join(generation_id.to_string());
        ensure!(
            !path_exists_no_follow(&generation_dir)?,
            "certificate rotation generation already exists"
        );
        let staging_dir = self
            .root
            .join(format!(".rotation-generation-{generation_id}"));
        ensure!(
            !path_exists_no_follow(&staging_dir)?,
            "certificate rotation generation staging path already exists"
        );
        create_secure_directory(&staging_dir)?;
        let result = (|| -> anyhow::Result<()> {
            write_new_secure_file(
                &staging_dir.join("control-client-key.pem"),
                pending.private_key_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("control-client-cert.pem"),
                response.certificate_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("management-server-key.pem"),
                pending.management_private_key_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("management-server-cert.pem"),
                response.management_certificate_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("agent-client-issuer-ca.pem"),
                response.agent_client_issuer_ca_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("control-plane-server-ca.pem"),
                response.control_plane_server_ca_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("management-client-ca.pem"),
                response.management_client_ca_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("capability-jwt-public-key.pem"),
                response.capability_jwt_public_key_pem.as_bytes(),
            )?;
            let metadata = IdentityMetadata {
                version: IDENTITY_METADATA_VERSION,
                generation_id,
                node_id: pending.node_id,
                fingerprint_sha256: validated.control.fingerprint_sha256.clone(),
                serial_number: Some(validated.control.serial_number.clone()),
                not_before: validated.control.not_before,
                not_after: validated.control.not_after,
                management_fingerprint_sha256: Some(
                    validated.management.fingerprint_sha256.clone(),
                ),
                management_serial_number: Some(validated.management.serial_number.clone()),
                management_not_before: Some(validated.management.not_before),
                management_not_after: Some(validated.management.not_after),
                management_dns_name: Some(validated.management_dns_name.clone()),
                agent_client_issuer_ca_sha256: Some(
                    validated.agent_client_issuer_ca_sha256.clone(),
                ),
                control_plane_server_ca_sha256: Some(
                    validated.control_plane_server_ca_sha256.clone(),
                ),
                management_client_ca_sha256: Some(validated.management_client_ca_sha256.clone()),
                capability_jwt_kid: Some(validated.capability_jwt_kid.clone()),
            };
            let encoded = serde_json::to_vec_pretty(&metadata)
                .context("failed to serialize rotated Agent identity metadata")?;
            write_new_secure_file(&staging_dir.join("metadata.json"), &encoded)?;
            sync_directory(&staging_dir)?;
            fs::rename(&staging_dir, &generation_dir)
                .context("failed to publish rotated Agent identity generation")?;
            sync_directory(&generations_dir)
        })();
        if result.is_err() && path_exists_no_follow(&staging_dir).unwrap_or(false) {
            let _ = remove_secure_identity_tree(&staging_dir);
        }
        result
    }

    fn replace_pending_rotation_metadata(
        &self,
        metadata: &PendingRotationMetadata,
    ) -> anyhow::Result<()> {
        let pending_dir = self.root.join("pending-rotation");
        assert_secure_directory(&pending_dir)?;
        let metadata_path = pending_dir.join("metadata.json");
        assert_regular_file_no_symlink(&metadata_path)?;
        let temporary_path = pending_dir.join(format!(".metadata-{}", Uuid::now_v7()));
        let encoded = serde_json::to_vec_pretty(metadata)
            .context("failed to serialize certificate rotation metadata")?;
        write_new_secure_file(&temporary_path, &encoded)?;
        #[cfg(windows)]
        fs::remove_file(&metadata_path)
            .context("failed to replace certificate rotation metadata")?;
        fs::rename(&temporary_path, &metadata_path)
            .context("failed to publish certificate rotation metadata")?;
        sync_directory(&pending_dir)
    }

    fn persist_pending_rotation(
        &self,
        pending: &PendingIdentity,
        metadata: &PendingRotationMetadata,
    ) -> anyhow::Result<()> {
        ensure!(
            pending.generation_id == metadata.rotation_id
                && pending.node_id == metadata.node_id
                && metadata.stage == RotationStage::Requested,
            "pending certificate rotation metadata does not match its key material"
        );
        let pending_dir = self.root.join("pending-rotation");
        ensure!(
            !path_exists_no_follow(&pending_dir)?,
            "pending certificate rotation already exists"
        );
        let staging_dir = self
            .root
            .join(format!(".rotation-staging-{}", metadata.rotation_id));
        ensure!(
            !path_exists_no_follow(&staging_dir)?,
            "certificate rotation staging directory already exists"
        );
        create_secure_directory(&staging_dir)?;
        let result = (|| -> anyhow::Result<()> {
            write_new_secure_file(
                &staging_dir.join("control-client-key.pem"),
                pending.private_key_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("control-client.csr.pem"),
                pending.csr_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("management-server-key.pem"),
                pending.management_private_key_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("management-server.csr.pem"),
                pending.management_csr_pem.as_bytes(),
            )?;
            let encoded = serde_json::to_vec_pretty(metadata)
                .context("failed to serialize pending certificate rotation metadata")?;
            write_new_secure_file(&staging_dir.join("metadata.json"), &encoded)?;
            sync_directory(&staging_dir)?;
            fs::rename(&staging_dir, &pending_dir)
                .context("failed to publish pending certificate rotation")?;
            sync_directory(&self.root)
        })();
        if result.is_err() && path_exists_no_follow(&staging_dir).unwrap_or(false) {
            let _ = remove_secure_identity_tree(&staging_dir);
        }
        result
    }

    fn load_pending_rotation(&self) -> anyhow::Result<PendingRotation> {
        let pending_dir = self.root.join("pending-rotation");
        assert_secure_directory(&pending_dir)?;
        let key_path = pending_dir.join("control-client-key.pem");
        let csr_path = pending_dir.join("control-client.csr.pem");
        let management_key_path = pending_dir.join("management-server-key.pem");
        let management_csr_path = pending_dir.join("management-server.csr.pem");
        let metadata_path = pending_dir.join("metadata.json");
        for path in [
            &key_path,
            &csr_path,
            &management_key_path,
            &management_csr_path,
            &metadata_path,
        ] {
            assert_regular_file_no_symlink(path)?;
        }
        ensure_directory_contains_exactly(
            &pending_dir,
            &[
                "control-client-key.pem",
                "control-client.csr.pem",
                "management-server-key.pem",
                "management-server.csr.pem",
                "metadata.json",
            ],
        )?;
        let private_key_pem = Zeroizing::new(
            String::from_utf8(read_limited(&key_path, MAX_IDENTITY_FILE_BYTES)?)
                .context("pending rotation control key is not UTF-8 PEM")?,
        );
        let csr_pem = String::from_utf8(read_limited(&csr_path, MAX_IDENTITY_FILE_BYTES)?)
            .context("pending rotation control CSR is not UTF-8 PEM")?;
        let management_private_key_pem = Zeroizing::new(
            String::from_utf8(read_limited(&management_key_path, MAX_IDENTITY_FILE_BYTES)?)
                .context("pending rotation management key is not UTF-8 PEM")?,
        );
        let management_csr_pem =
            String::from_utf8(read_limited(&management_csr_path, MAX_IDENTITY_FILE_BYTES)?)
                .context("pending rotation management CSR is not UTF-8 PEM")?;
        let metadata: PendingRotationMetadata =
            serde_json::from_slice(&read_limited(&metadata_path, MAX_IDENTITY_FILE_BYTES)?)
                .context("pending certificate rotation metadata is invalid")?;
        ensure!(
            metadata.version == PENDING_ROTATION_METADATA_VERSION
                && !metadata.rotation_id.is_nil()
                && !metadata.node_id.is_nil()
                && !metadata.source_generation_id.is_nil(),
            "pending certificate rotation metadata is invalid"
        );
        let control_key = KeyPair::from_pem(private_key_pem.as_str())
            .context("pending rotation control key is invalid")?;
        let control_csr = CertificateSigningRequestParams::from_pem(&csr_pem)
            .context("pending rotation control CSR is invalid")?;
        let management_key = KeyPair::from_pem(management_private_key_pem.as_str())
            .context("pending rotation management key is invalid")?;
        let management_csr = CertificateSigningRequestParams::from_pem(&management_csr_pem)
            .context("pending rotation management CSR is invalid")?;
        ensure!(
            control_csr.public_key.der_bytes() == control_key.public_key_raw()
                && control_csr.params.subject_alt_names.is_empty(),
            "pending rotation control CSR does not match its key or contains a SAN"
        );
        ensure!(
            management_csr.public_key.der_bytes() == management_key.public_key_raw()
                && management_csr.params.subject_alt_names.is_empty(),
            "pending rotation management CSR does not match its key or contains a SAN"
        );
        ensure!(
            control_csr.public_key.der_bytes() != management_csr.public_key.der_bytes(),
            "pending rotation control and management keys must be distinct"
        );
        Ok(PendingRotation {
            identity: PendingIdentity {
                node_id: metadata.node_id,
                generation_id: metadata.rotation_id,
                csr_pem,
                private_key_pem,
                management_csr_pem,
                management_private_key_pem,
            },
            metadata,
        })
    }

    pub(crate) fn prepare(node_id: Uuid) -> anyhow::Result<PendingIdentity> {
        ensure!(
            !node_id.is_nil(),
            "Agent identity node UUID must not be nil"
        );
        let key_pair = KeyPair::generate().context("failed to generate Agent control key")?;
        let mut parameters = CertificateParams::default();
        parameters.subject_alt_names.clear();
        parameters.distinguished_name = DistinguishedName::new();
        parameters
            .distinguished_name
            .push(DnType::CommonName, format!("StreamServer Agent {node_id}"));
        let request = parameters
            .serialize_request(&key_pair)
            .context("failed to build Agent identity CSR")?;
        let csr_pem = request
            .pem()
            .context("failed to encode Agent identity CSR")?;
        let management_key_pair =
            KeyPair::generate().context("failed to generate Agent management key")?;
        ensure!(
            key_pair.public_key_der() != management_key_pair.public_key_der(),
            "Agent control and management public keys must be distinct"
        );
        let mut management_parameters = CertificateParams::default();
        management_parameters.subject_alt_names.clear();
        management_parameters.distinguished_name = DistinguishedName::new();
        management_parameters.distinguished_name.push(
            DnType::CommonName,
            format!("StreamServer Agent Management {node_id}"),
        );
        let management_request = management_parameters
            .serialize_request(&management_key_pair)
            .context("failed to build Agent management CSR")?;
        let management_csr_pem = management_request
            .pem()
            .context("failed to encode Agent management CSR")?;
        Ok(PendingIdentity {
            node_id,
            generation_id: Uuid::now_v7(),
            csr_pem,
            private_key_pem: Zeroizing::new(key_pair.serialize_pem()),
            management_csr_pem,
            management_private_key_pem: Zeroizing::new(management_key_pair.serialize_pem()),
        })
    }

    fn acquire_enrollment_lock(&self) -> anyhow::Result<IdentityRootLock> {
        ensure_secure_directory(&self.root)?;
        ensure_secure_directory(&self.root.join("generations"))?;
        let root_metadata = fs::symlink_metadata(&self.root)
            .context("failed to inspect Agent identity root before locking")?;
        let lock_path = self.root.join("enrollment.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&lock_path)
            .context("failed to open Agent identity enrollment lock")?;
        assert_private_identity_file_metadata(&lock_path, &file.metadata()?)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            loop {
                let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
                if result == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error).context("failed to lock Agent identity root");
                }
            }
        }
        let lock = IdentityRootLock {
            root: self.root.clone(),
            #[cfg(unix)]
            root_device: {
                use std::os::unix::fs::MetadataExt;
                root_metadata.dev()
            },
            #[cfg(unix)]
            root_inode: {
                use std::os::unix::fs::MetadataExt;
                root_metadata.ino()
            },
            #[cfg(unix)]
            lock_device: {
                use std::os::unix::fs::MetadataExt;
                file.metadata()?.dev()
            },
            #[cfg(unix)]
            lock_inode: {
                use std::os::unix::fs::MetadataExt;
                file.metadata()?.ino()
            },
            file,
        };
        self.validate_enrollment_lock(&lock)?;
        Ok(lock)
    }

    fn validate_enrollment_lock(&self, lock: &IdentityRootLock) -> anyhow::Result<()> {
        // Every mutable ancestor is restricted to root/the service UID and the
        // identity root itself is 0700.  Re-fencing both the root and lock-file
        // inodes after flock therefore closes cross-UID rename/symlink races;
        // the service UID is the identity's own security principal.
        ensure!(
            lock.root == self.root,
            "Agent identity lock belongs to another root"
        );
        assert_no_symlink_ancestors(&self.root)?;
        assert_trusted_directory_ancestors(&self.root)?;
        assert_secure_directory(&self.root)?;
        assert_secure_directory(&self.root.join("generations"))?;
        let root_metadata = fs::symlink_metadata(&self.root)
            .context("failed to revalidate Agent identity root under lock")?;
        let lock_path = self.root.join("enrollment.lock");
        assert_regular_file_no_symlink(&lock_path)?;
        let lock_metadata = lock
            .file
            .metadata()
            .context("failed to revalidate Agent identity lock file")?;
        assert_private_identity_file_metadata(&lock_path, &lock_metadata)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            ensure!(
                root_metadata.dev() == lock.root_device && root_metadata.ino() == lock.root_inode,
                "Agent identity root changed after its enrollment lock was acquired"
            );
            let named_lock = fs::symlink_metadata(&lock_path)
                .context("failed to inspect the named Agent enrollment lock")?;
            ensure!(
                named_lock.dev() == lock.lock_device
                    && named_lock.ino() == lock.lock_inode
                    && lock_metadata.dev() == lock.lock_device
                    && lock_metadata.ino() == lock.lock_inode,
                "Agent identity enrollment lock path changed while locked"
            );
        }
        Ok(())
    }

    fn prepare_enrollment(
        &self,
        lock: &IdentityRootLock,
        node_id: Uuid,
        now: DateTime<Utc>,
    ) -> anyhow::Result<EnrollmentPreparation> {
        self.validate_enrollment_lock(lock)?;
        ensure!(
            !node_id.is_nil(),
            "Agent identity node UUID must not be nil"
        );
        self.cleanup_stale_staging_directories()?;

        let pending_dir = self.root.join("pending-enrollment");
        let current_path = self.root.join("current");
        if path_exists_no_follow(&pending_dir)? {
            let pending = self.load_pending_identity()?;
            ensure!(
                pending.node_id == node_id,
                "pending Agent enrollment belongs to another node"
            );
            if path_exists_no_follow(&current_path)? {
                let loaded = self
                    .load_current(now)
                    .map_err(anyhow::Error::new)
                    .context("failed to reconcile pending Agent enrollment")?;
                ensure!(
                    loaded.metadata.generation_id == pending.generation_id
                        && loaded.node_id() == pending.node_id,
                    "pending Agent enrollment conflicts with the current identity"
                );
                self.clear_pending_identity(&pending)?;
                return Ok(EnrollmentPreparation::Recovered(Box::new(loaded)));
            }

            let generation_dir = self
                .root
                .join("generations")
                .join(pending.generation_id.to_string());
            if path_exists_no_follow(&generation_dir)? {
                let loaded = self.load_generation(pending.generation_id, now)?;
                ensure!(
                    loaded.node_id() == pending.node_id,
                    "published Agent generation conflicts with pending enrollment"
                );
                self.write_current_pointer(pending.generation_id)?;
                self.clear_pending_identity(&pending)?;
                return Ok(EnrollmentPreparation::Recovered(Box::new(loaded)));
            }
            return Ok(EnrollmentPreparation::Pending(pending));
        }

        if path_exists_no_follow(&current_path)? {
            let loaded = self
                .load_current(now)
                .map_err(anyhow::Error::new)
                .context("failed to recover the current Agent identity")?;
            ensure!(
                loaded.node_id() == node_id,
                "current Agent identity belongs to another node"
            );
            return Ok(EnrollmentPreparation::Recovered(Box::new(loaded)));
        }

        let pending = Self::prepare(node_id)?;
        self.persist_pending_identity(&pending, now)?;
        Ok(EnrollmentPreparation::Pending(pending))
    }

    fn persist_pending_identity(
        &self,
        pending: &PendingIdentity,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let pending_dir = self.root.join("pending-enrollment");
        ensure!(
            !path_exists_no_follow(&pending_dir)?,
            "pending Agent enrollment already exists"
        );
        let staging_dir = self
            .root
            .join(format!(".pending-staging-{}", Uuid::now_v7()));
        create_secure_directory(&staging_dir)?;
        let result = (|| -> anyhow::Result<()> {
            write_new_secure_file(
                &staging_dir.join("control-client-key.pem"),
                pending.private_key_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("control-client.csr.pem"),
                pending.csr_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("management-server-key.pem"),
                pending.management_private_key_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("management-server.csr.pem"),
                pending.management_csr_pem.as_bytes(),
            )?;
            let metadata = PendingIdentityMetadata {
                version: PENDING_IDENTITY_METADATA_VERSION,
                generation_id: pending.generation_id,
                node_id: pending.node_id,
                created_at: now,
            };
            let encoded = serde_json::to_vec_pretty(&metadata)
                .context("failed to serialize pending Agent identity metadata")?;
            write_new_secure_file(&staging_dir.join("metadata.json"), &encoded)?;
            sync_directory(&staging_dir)?;
            fs::rename(&staging_dir, &pending_dir)
                .context("failed to publish pending Agent identity")?;
            sync_directory(&self.root)
        })();
        if result.is_err() && path_exists_no_follow(&staging_dir).unwrap_or(false) {
            let _ = remove_secure_identity_tree(&staging_dir);
        }
        result
    }

    fn load_pending_identity(&self) -> anyhow::Result<PendingIdentity> {
        let pending_dir = self.root.join("pending-enrollment");
        assert_secure_directory(&pending_dir)?;
        let key_path = pending_dir.join("control-client-key.pem");
        let csr_path = pending_dir.join("control-client.csr.pem");
        let management_key_path = pending_dir.join("management-server-key.pem");
        let management_csr_path = pending_dir.join("management-server.csr.pem");
        let metadata_path = pending_dir.join("metadata.json");
        for path in [
            &key_path,
            &csr_path,
            &management_key_path,
            &management_csr_path,
            &metadata_path,
        ] {
            assert_regular_file_no_symlink(path)?;
        }
        ensure_directory_contains_exactly(
            &pending_dir,
            &[
                "control-client-key.pem",
                "control-client.csr.pem",
                "management-server-key.pem",
                "management-server.csr.pem",
                "metadata.json",
            ],
        )?;

        let private_key_pem = Zeroizing::new(
            String::from_utf8(read_limited(&key_path, MAX_IDENTITY_FILE_BYTES)?)
                .context("pending Agent identity key is not UTF-8 PEM")?,
        );
        let csr_pem = String::from_utf8(read_limited(&csr_path, MAX_IDENTITY_FILE_BYTES)?)
            .context("pending Agent identity CSR is not UTF-8 PEM")?;
        let management_private_key_pem = Zeroizing::new(
            String::from_utf8(read_limited(&management_key_path, MAX_IDENTITY_FILE_BYTES)?)
                .context("pending Agent management key is not UTF-8 PEM")?,
        );
        let management_csr_pem =
            String::from_utf8(read_limited(&management_csr_path, MAX_IDENTITY_FILE_BYTES)?)
                .context("pending Agent management CSR is not UTF-8 PEM")?;
        let metadata: PendingIdentityMetadata =
            serde_json::from_slice(&read_limited(&metadata_path, MAX_IDENTITY_FILE_BYTES)?)
                .context("pending Agent identity metadata is invalid")?;
        ensure!(
            metadata.version == PENDING_IDENTITY_METADATA_VERSION && !metadata.node_id.is_nil(),
            "pending Agent identity metadata version or node is invalid"
        );
        let key_pair = KeyPair::from_pem(private_key_pem.as_str())
            .context("pending Agent identity key is invalid")?;
        let csr = CertificateSigningRequestParams::from_pem(&csr_pem)
            .context("pending Agent identity CSR is invalid")?;
        let management_key_pair = KeyPair::from_pem(management_private_key_pem.as_str())
            .context("pending Agent management key is invalid")?;
        let management_csr = CertificateSigningRequestParams::from_pem(&management_csr_pem)
            .context("pending Agent management CSR is invalid")?;
        ensure!(
            csr.public_key.der_bytes() == key_pair.public_key_raw()
                && csr.params.subject_alt_names.is_empty(),
            "pending Agent identity CSR does not match its private key or contains a SAN"
        );
        ensure!(
            management_csr.public_key.der_bytes() == management_key_pair.public_key_raw()
                && management_csr.params.subject_alt_names.is_empty(),
            "pending Agent management CSR does not match its private key or contains a SAN"
        );
        ensure!(
            csr.public_key.der_bytes() != management_csr.public_key.der_bytes(),
            "pending Agent control and management public keys must be distinct"
        );
        Ok(PendingIdentity {
            node_id: metadata.node_id,
            generation_id: metadata.generation_id,
            csr_pem,
            private_key_pem,
            management_csr_pem,
            management_private_key_pem,
        })
    }

    fn clear_pending_identity(&self, expected: &PendingIdentity) -> anyhow::Result<()> {
        self.clear_pending_identity_with_failpoint(expected, PendingCleanupFailpoint::None)
    }

    fn clear_pending_identity_with_failpoint(
        &self,
        expected: &PendingIdentity,
        failpoint: PendingCleanupFailpoint,
    ) -> anyhow::Result<()> {
        let loaded = self.load_pending_identity()?;
        ensure!(
            loaded.node_id == expected.node_id && loaded.generation_id == expected.generation_id,
            "pending Agent identity changed before cleanup"
        );
        let pending_path = self.root.join("pending-enrollment");
        let retired_path = self
            .root
            .join(format!(".retired-pending-{}", expected.generation_id));
        ensure!(
            !path_exists_no_follow(&retired_path)?,
            "retired Agent enrollment path already exists"
        );
        fs::rename(&pending_path, &retired_path)
            .context("failed to retire pending Agent enrollment")?;
        sync_directory(&self.root)?;
        fail_pending_cleanup_at(failpoint, PendingCleanupFailpoint::AfterRetiredRename)?;

        let mut removed_entries = 0_usize;
        remove_secure_identity_tree_with_failpoint(&retired_path, failpoint, &mut removed_entries)?;
        fail_pending_cleanup_at(
            failpoint,
            PendingCleanupFailpoint::AfterRetiredDirectoryRemoval,
        )?;
        sync_directory(&self.root)
    }

    fn commit_enrollment_response(
        &self,
        lock: &IdentityRootLock,
        pending: &PendingIdentity,
        response: &AgentEnrollResponse,
        now: DateTime<Utc>,
    ) -> anyhow::Result<LoadedIdentity> {
        self.validate_enrollment_lock(lock)?;
        let validated = validate_complete_enrollment_response(pending, response, now)?;
        let installed = self.install_complete_enrollment(pending, response, &validated, now)?;
        self.clear_pending_identity(pending)?;
        Ok(installed)
    }

    fn install_complete_enrollment(
        &self,
        pending: &PendingIdentity,
        response: &AgentEnrollResponse,
        validated: &ValidatedEnrollmentBundle,
        now: DateTime<Utc>,
    ) -> anyhow::Result<LoadedIdentity> {
        self.install_complete_enrollment_with_failpoint(
            pending,
            response,
            validated,
            now,
            InstallFailpoint::None,
        )
    }

    fn install_complete_enrollment_with_failpoint(
        &self,
        pending: &PendingIdentity,
        response: &AgentEnrollResponse,
        validated: &ValidatedEnrollmentBundle,
        now: DateTime<Utc>,
        failpoint: InstallFailpoint,
    ) -> anyhow::Result<LoadedIdentity> {
        ensure_secure_directory(&self.root)?;
        let generations_dir = self.root.join("generations");
        ensure_secure_directory(&generations_dir)?;
        let staging_dir = self
            .root
            .join(format!(".staging-{}", pending.generation_id));
        let generation_dir = generations_dir.join(pending.generation_id.to_string());
        ensure!(
            !path_exists_no_follow(&staging_dir)?,
            "Agent identity staging generation already exists"
        );
        ensure!(
            !path_exists_no_follow(&generation_dir)?,
            "Agent identity generation already exists"
        );
        create_secure_directory(&staging_dir)?;
        let result = (|| -> anyhow::Result<()> {
            write_new_secure_file(
                &staging_dir.join("control-client-key.pem"),
                pending.private_key_pem.as_bytes(),
            )?;
            fail_at(failpoint, InstallFailpoint::AfterPrivateKey)?;
            write_new_secure_file(
                &staging_dir.join("control-client-cert.pem"),
                response.certificate_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("management-server-key.pem"),
                pending.management_private_key_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("management-server-cert.pem"),
                response.management_certificate_pem.as_bytes(),
            )?;
            fail_at(failpoint, InstallFailpoint::AfterCertificate)?;
            write_new_secure_file(
                &staging_dir.join("agent-client-issuer-ca.pem"),
                response.agent_client_issuer_ca_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("control-plane-server-ca.pem"),
                response.control_plane_server_ca_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("management-client-ca.pem"),
                response.management_client_ca_pem.as_bytes(),
            )?;
            write_new_secure_file(
                &staging_dir.join("capability-jwt-public-key.pem"),
                response.capability_jwt_public_key_pem.as_bytes(),
            )?;
            let metadata = IdentityMetadata {
                version: IDENTITY_METADATA_VERSION,
                generation_id: pending.generation_id,
                node_id: pending.node_id,
                fingerprint_sha256: validated.control.fingerprint_sha256.clone(),
                serial_number: Some(validated.control.serial_number.clone()),
                not_before: validated.control.not_before,
                not_after: validated.control.not_after,
                management_fingerprint_sha256: Some(
                    validated.management.fingerprint_sha256.clone(),
                ),
                management_serial_number: Some(validated.management.serial_number.clone()),
                management_not_before: Some(validated.management.not_before),
                management_not_after: Some(validated.management.not_after),
                management_dns_name: Some(validated.management_dns_name.clone()),
                agent_client_issuer_ca_sha256: Some(
                    validated.agent_client_issuer_ca_sha256.clone(),
                ),
                control_plane_server_ca_sha256: Some(
                    validated.control_plane_server_ca_sha256.clone(),
                ),
                management_client_ca_sha256: Some(validated.management_client_ca_sha256.clone()),
                capability_jwt_kid: Some(validated.capability_jwt_kid.clone()),
            };
            let metadata_json = serde_json::to_vec_pretty(&metadata)
                .context("failed to serialize complete Agent identity metadata")?;
            write_new_secure_file(&staging_dir.join("metadata.json"), &metadata_json)?;
            sync_directory(&staging_dir)?;
            fs::rename(&staging_dir, &generation_dir)
                .context("failed to publish complete Agent identity generation")?;
            sync_directory(&generations_dir)?;
            fail_at(failpoint, InstallFailpoint::BeforeCurrentPointer)?;
            self.write_current_pointer(pending.generation_id)
        })();
        if result.is_err() && path_exists_no_follow(&staging_dir).unwrap_or(false) {
            let _ = remove_secure_identity_tree(&staging_dir);
        }
        if result.is_err()
            && !self.current_pointer_matches(pending.generation_id)
            && path_exists_no_follow(&generation_dir).unwrap_or(false)
        {
            let _ = remove_secure_identity_tree(&generation_dir);
            let _ = sync_directory(&generations_dir);
        }
        result?;
        Ok(self.load_current(now)?)
    }

    fn cleanup_stale_staging_directories(&self) -> anyhow::Result<()> {
        for entry in fs::read_dir(&self.root).context("failed to inspect Agent identity root")? {
            let entry = entry.context("failed to inspect Agent identity root entry")?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow!("Agent identity root contains a non-UTF-8 entry"))?;
            let suffix = name
                .strip_prefix(".pending-staging-")
                .or_else(|| name.strip_prefix(".staging-"))
                .or_else(|| name.strip_prefix(".rotation-staging-"))
                .or_else(|| name.strip_prefix(".rotation-generation-"))
                .or_else(|| name.strip_prefix(".retired-pending-"))
                .or_else(|| name.strip_prefix(".retired-rotation-"))
                .or_else(|| name.strip_prefix(".current-"));
            if suffix.is_some_and(|value| Uuid::parse_str(value).is_ok()) {
                remove_secure_identity_tree(&entry.path())?;
            }
        }
        sync_directory(&self.root)
    }

    #[cfg(test)]
    pub(crate) fn install(
        &self,
        pending: &PendingIdentity,
        certificate_pem: &str,
        ca_certificate_pem: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<LoadedIdentity> {
        self.install_with_failpoint(
            pending,
            certificate_pem,
            ca_certificate_pem,
            now,
            InstallFailpoint::None,
        )
    }

    #[cfg(test)]
    fn install_with_failpoint(
        &self,
        pending: &PendingIdentity,
        certificate_pem: &str,
        ca_certificate_pem: &str,
        now: DateTime<Utc>,
        failpoint: InstallFailpoint,
    ) -> anyhow::Result<LoadedIdentity> {
        let certificate =
            validate_issued_identity(pending, certificate_pem, ca_certificate_pem, now)?;
        ensure_secure_directory(&self.root)?;
        let generations_dir = self.root.join("generations");
        ensure_secure_directory(&generations_dir)?;

        let staging_dir = self
            .root
            .join(format!(".staging-{}", pending.generation_id));
        let generation_dir = generations_dir.join(pending.generation_id.to_string());
        ensure!(
            !path_exists_no_follow(&staging_dir)?,
            "Agent identity staging generation already exists"
        );
        ensure!(
            !path_exists_no_follow(&generation_dir)?,
            "Agent identity generation already exists"
        );
        create_secure_directory(&staging_dir)?;

        let result = (|| -> anyhow::Result<()> {
            write_new_secure_file(
                &staging_dir.join("identity-key.pem"),
                pending.private_key_pem.as_bytes(),
            )?;
            fail_at(failpoint, InstallFailpoint::AfterPrivateKey)?;
            write_new_secure_file(
                &staging_dir.join("identity-cert.pem"),
                certificate_pem.as_bytes(),
            )?;
            fail_at(failpoint, InstallFailpoint::AfterCertificate)?;
            write_new_secure_file(
                &staging_dir.join("agent-ca.pem"),
                ca_certificate_pem.as_bytes(),
            )?;
            let metadata = IdentityMetadata {
                version: LEGACY_IDENTITY_METADATA_VERSION,
                generation_id: pending.generation_id,
                node_id: pending.node_id,
                fingerprint_sha256: certificate.fingerprint_sha256.clone(),
                serial_number: None,
                not_before: certificate.not_before,
                not_after: certificate.not_after,
                management_fingerprint_sha256: None,
                management_serial_number: None,
                management_not_before: None,
                management_not_after: None,
                management_dns_name: None,
                agent_client_issuer_ca_sha256: None,
                control_plane_server_ca_sha256: None,
                management_client_ca_sha256: None,
                capability_jwt_kid: None,
            };
            let metadata_json = serde_json::to_vec_pretty(&metadata)
                .context("failed to serialize Agent identity metadata")?;
            write_new_secure_file(&staging_dir.join("metadata.json"), &metadata_json)?;
            sync_directory(&staging_dir)?;
            fs::rename(&staging_dir, &generation_dir)
                .context("failed to publish Agent identity generation")?;
            sync_directory(&generations_dir)?;
            fail_at(failpoint, InstallFailpoint::BeforeCurrentPointer)?;
            self.write_current_pointer(pending.generation_id)?;
            Ok(())
        })();

        if result.is_err() && path_exists_no_follow(&staging_dir).unwrap_or(false) {
            let _ = remove_secure_identity_tree(&staging_dir);
        }
        if result.is_err()
            && !self.current_pointer_matches(pending.generation_id)
            && path_exists_no_follow(&generation_dir).unwrap_or(false)
        {
            let _ = remove_secure_identity_tree(&generation_dir);
            let _ = sync_directory(&generations_dir);
        }
        result?;
        Ok(self.load_current(now)?)
    }

    pub(crate) fn load_current(
        &self,
        now: DateTime<Utc>,
    ) -> Result<LoadedIdentity, AgentIdentityLoadError> {
        if !path_exists_no_follow(&self.root)? {
            return Err(AgentIdentityLoadError::NotEnrolled);
        }
        assert_no_symlink_ancestors(&self.root)?;
        assert_secure_directory(&self.root)?;
        let current_path = self.root.join("current");
        if !path_exists_no_follow(&current_path)? {
            return Err(AgentIdentityLoadError::NotEnrolled);
        }
        assert_regular_file_no_symlink(&current_path)?;
        let current = read_limited(&current_path, 128)?;
        let generation_id = Uuid::parse_str(
            std::str::from_utf8(&current)
                .context("Agent identity current pointer is not UTF-8")?
                .trim(),
        )
        .context("Agent identity current pointer is not a UUID")?;
        self.load_generation(generation_id, now)
            .map_err(AgentIdentityLoadError::Invalid)
    }

    fn load_generation(
        &self,
        generation_id: Uuid,
        now: DateTime<Utc>,
    ) -> anyhow::Result<LoadedIdentity> {
        let generations_dir = self.root.join("generations");
        assert_secure_directory(&generations_dir)?;
        let generation_dir = generations_dir.join(generation_id.to_string());
        assert_secure_directory(&generation_dir)?;
        let metadata_path = generation_dir.join("metadata.json");
        assert_regular_file_no_symlink(&metadata_path)?;
        let metadata: IdentityMetadata =
            serde_json::from_slice(&read_limited(&metadata_path, MAX_IDENTITY_FILE_BYTES)?)
                .context("Agent identity metadata is invalid")?;
        ensure!(
            metadata.generation_id == generation_id,
            "Agent identity generation metadata mismatch"
        );
        if metadata.version == IDENTITY_METADATA_VERSION {
            return self.load_complete_generation(generation_dir, metadata, now);
        }
        ensure!(
            metadata.version == LEGACY_IDENTITY_METADATA_VERSION,
            "unsupported Agent identity metadata version"
        );
        let key_path = generation_dir.join("identity-key.pem");
        let cert_path = generation_dir.join("identity-cert.pem");
        let ca_path = generation_dir.join("agent-ca.pem");
        for path in [&key_path, &cert_path, &ca_path, &metadata_path] {
            assert_regular_file_no_symlink(path)?;
        }
        ensure_directory_contains_exactly(
            &generation_dir,
            &[
                "identity-key.pem",
                "identity-cert.pem",
                "agent-ca.pem",
                "metadata.json",
            ],
        )?;

        let private_key_pem = Zeroizing::new(
            String::from_utf8(read_limited(&key_path, MAX_IDENTITY_FILE_BYTES)?)
                .context("Agent identity key is not UTF-8 PEM")?,
        );
        let certificate_pem = String::from_utf8(read_limited(&cert_path, MAX_IDENTITY_FILE_BYTES)?)
            .context("Agent identity certificate is not UTF-8 PEM")?;
        let ca_certificate_pem =
            String::from_utf8(read_limited(&ca_path, MAX_IDENTITY_FILE_BYTES)?)
                .context("Agent CA certificate is not UTF-8 PEM")?;
        let key_pair = KeyPair::from_pem(private_key_pem.as_str())
            .context("Agent identity private key is invalid")?;
        let pending = PendingIdentity {
            node_id: metadata.node_id,
            generation_id,
            csr_pem: String::new(),
            private_key_pem: Zeroizing::new(key_pair.serialize_pem()),
            management_csr_pem: String::new(),
            management_private_key_pem: Zeroizing::new(String::new()),
        };
        let validated =
            validate_issued_identity(&pending, &certificate_pem, &ca_certificate_pem, now)?;
        ensure!(
            metadata.fingerprint_sha256 == validated.fingerprint_sha256
                && metadata.not_before == validated.not_before
                && metadata.not_after == validated.not_after,
            "Agent identity metadata does not match certificate"
        );
        Ok(LoadedIdentity {
            metadata,
            #[cfg(test)]
            generation_dir,
            certificate_pem,
            private_key_pem,
            management_certificate_pem: None,
            management_private_key_pem: None,
            agent_client_issuer_ca_pem: None,
            control_plane_server_ca_pem: None,
            management_client_ca_pem: None,
            capability_jwt_public_key_pem: None,
        })
    }

    fn load_complete_generation(
        &self,
        generation_dir: PathBuf,
        metadata: IdentityMetadata,
        now: DateTime<Utc>,
    ) -> anyhow::Result<LoadedIdentity> {
        let control_key_path = generation_dir.join("control-client-key.pem");
        let control_cert_path = generation_dir.join("control-client-cert.pem");
        let management_key_path = generation_dir.join("management-server-key.pem");
        let management_cert_path = generation_dir.join("management-server-cert.pem");
        let issuer_ca_path = generation_dir.join("agent-client-issuer-ca.pem");
        let server_ca_path = generation_dir.join("control-plane-server-ca.pem");
        let management_client_ca_path = generation_dir.join("management-client-ca.pem");
        let capability_key_path = generation_dir.join("capability-jwt-public-key.pem");
        let metadata_path = generation_dir.join("metadata.json");
        for path in [
            &control_key_path,
            &control_cert_path,
            &management_key_path,
            &management_cert_path,
            &issuer_ca_path,
            &server_ca_path,
            &management_client_ca_path,
            &capability_key_path,
            &metadata_path,
        ] {
            assert_regular_file_no_symlink(path)?;
        }
        ensure_directory_contains_exactly(
            &generation_dir,
            &[
                "control-client-key.pem",
                "control-client-cert.pem",
                "management-server-key.pem",
                "management-server-cert.pem",
                "agent-client-issuer-ca.pem",
                "control-plane-server-ca.pem",
                "management-client-ca.pem",
                "capability-jwt-public-key.pem",
                "metadata.json",
            ],
        )?;
        let read_utf8 = |path: &Path, description: &str| -> anyhow::Result<String> {
            String::from_utf8(read_limited(path, MAX_IDENTITY_FILE_BYTES)?)
                .with_context(|| format!("{description} is not UTF-8 PEM"))
        };
        let control_private_key_pem =
            Zeroizing::new(read_utf8(&control_key_path, "Agent control private key")?);
        let control_certificate_pem = read_utf8(&control_cert_path, "Agent control certificate")?;
        let management_private_key_pem = Zeroizing::new(read_utf8(
            &management_key_path,
            "Agent management private key",
        )?);
        let management_certificate_pem =
            read_utf8(&management_cert_path, "Agent management certificate")?;
        let agent_client_issuer_ca_pem = read_utf8(&issuer_ca_path, "Agent issuer CA")?;
        let control_plane_server_ca_pem = read_utf8(&server_ca_path, "control-plane server CA")?;
        let management_client_ca_pem =
            read_utf8(&management_client_ca_path, "management client CA")?;
        let capability_jwt_public_key_pem =
            read_utf8(&capability_key_path, "capability JWT public key")?;
        let required = |value: Option<String>, name: &str| {
            value.ok_or_else(|| anyhow!("complete Agent identity metadata is missing {name}"))
        };
        let pending = PendingIdentity {
            node_id: metadata.node_id,
            generation_id: metadata.generation_id,
            csr_pem: String::new(),
            private_key_pem: Zeroizing::new(control_private_key_pem.to_string()),
            management_csr_pem: String::new(),
            management_private_key_pem: Zeroizing::new(management_private_key_pem.to_string()),
        };
        let response = AgentEnrollResponse {
            node_id: metadata.node_id,
            certificate_pem: control_certificate_pem.clone(),
            ca_certificate_pem: agent_client_issuer_ca_pem.clone(),
            fingerprint_sha256: metadata.fingerprint_sha256.clone(),
            serial_number: required(metadata.serial_number.clone(), "control serial")?,
            not_before: metadata.not_before,
            not_after: metadata.not_after,
            agent_client_issuer_ca_pem: agent_client_issuer_ca_pem.clone(),
            control_plane_server_ca_pem: control_plane_server_ca_pem.clone(),
            management_client_ca_pem: management_client_ca_pem.clone(),
            management_certificate_pem: management_certificate_pem.clone(),
            management_fingerprint_sha256: required(
                metadata.management_fingerprint_sha256.clone(),
                "management fingerprint",
            )?,
            management_serial_number: required(
                metadata.management_serial_number.clone(),
                "management serial",
            )?,
            management_not_before: metadata.management_not_before.ok_or_else(|| {
                anyhow!("complete Agent identity metadata is missing management not-before")
            })?,
            management_not_after: metadata.management_not_after.ok_or_else(|| {
                anyhow!("complete Agent identity metadata is missing management expiry")
            })?,
            capability_jwt_public_key_pem: capability_jwt_public_key_pem.clone(),
            capability_jwt_kid: required(
                metadata.capability_jwt_kid.clone(),
                "capability JWT kid",
            )?,
        };
        let validated = validate_complete_enrollment_response(&pending, &response, now)?;
        ensure!(
            metadata.management_dns_name.as_deref() == Some(validated.management_dns_name.as_str())
                && metadata.agent_client_issuer_ca_sha256.as_deref()
                    == Some(validated.agent_client_issuer_ca_sha256.as_str())
                && metadata.control_plane_server_ca_sha256.as_deref()
                    == Some(validated.control_plane_server_ca_sha256.as_str())
                && metadata.management_client_ca_sha256.as_deref()
                    == Some(validated.management_client_ca_sha256.as_str()),
            "complete Agent identity metadata does not match enrolled trust material"
        );
        Ok(LoadedIdentity {
            metadata,
            #[cfg(test)]
            generation_dir,
            certificate_pem: control_certificate_pem,
            private_key_pem: control_private_key_pem,
            management_certificate_pem: Some(management_certificate_pem),
            management_private_key_pem: Some(management_private_key_pem),
            agent_client_issuer_ca_pem: Some(agent_client_issuer_ca_pem),
            control_plane_server_ca_pem: Some(control_plane_server_ca_pem),
            management_client_ca_pem: Some(management_client_ca_pem),
            capability_jwt_public_key_pem: Some(capability_jwt_public_key_pem),
        })
    }

    fn write_current_pointer(&self, generation_id: Uuid) -> anyhow::Result<()> {
        let current_path = self.root.join("current");
        if path_exists_no_follow(&current_path)? {
            assert_regular_file_no_symlink(&current_path)?;
        }
        let temporary_path = self.root.join(format!(".current-{}", Uuid::now_v7()));
        write_new_secure_file(&temporary_path, format!("{generation_id}\n").as_bytes())?;
        #[cfg(windows)]
        if current_path.exists() {
            fs::remove_file(&current_path)
                .context("failed to replace Agent identity current pointer")?;
        }
        fs::rename(&temporary_path, &current_path)
            .context("failed to switch Agent identity generation")?;
        sync_directory(&self.root)
    }

    fn current_pointer_matches(&self, generation_id: Uuid) -> bool {
        let current_path = self.root.join("current");
        let Ok(current) = read_limited(&current_path, 128) else {
            return false;
        };
        std::str::from_utf8(&current)
            .ok()
            .and_then(|value| Uuid::parse_str(value.trim()).ok())
            == Some(generation_id)
    }
}

fn validate_issued_identity(
    pending: &PendingIdentity,
    certificate_pem: &str,
    ca_certificate_pem: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<ValidatedCertificate> {
    let leaf_pem = parse_single_certificate_pem(certificate_pem, "Agent identity certificate")?;
    let ca_pem = parse_single_certificate_pem(ca_certificate_pem, "Agent CA certificate")?;
    let leaf = parse_exact_certificate_der(&leaf_pem.contents, "Agent identity certificate")?;
    let ca = parse_exact_certificate_der(&ca_pem.contents, "Agent CA certificate")?;
    validate_root_ca_certificate(&ca, now, "Agent CA certificate")?;
    let ca_not_before = DateTime::<Utc>::from_timestamp(ca.validity().not_before.timestamp(), 0)
        .ok_or_else(|| anyhow!("Agent CA not-before is outside supported range"))?;
    let ca_not_after = DateTime::<Utc>::from_timestamp(ca.validity().not_after.timestamp(), 0)
        .ok_or_else(|| anyhow!("Agent CA not-after is outside supported range"))?;
    ensure!(
        now >= ca_not_before && now < ca_not_after,
        "Agent CA certificate is not currently valid"
    );
    ensure!(
        leaf.issuer() == ca.subject(),
        "Agent identity certificate issuer does not match the Agent CA"
    );
    leaf.verify_signature(Some(ca.public_key()))
        .map_err(|error| anyhow!("Agent identity certificate signature is invalid: {error}"))?;

    let key_pair = KeyPair::from_pem(pending.private_key_pem.as_str())
        .context("pending Agent identity key is invalid")?;
    ensure!(
        leaf.public_key().raw == key_pair.public_key_der().as_slice(),
        "Agent identity certificate does not match the generated private key"
    );
    let subject_alt_name = leaf
        .subject_alternative_name()
        .map_err(|error| anyhow!("Agent identity SAN is invalid: {error}"))?
        .ok_or_else(|| anyhow!("Agent identity certificate is missing its SPIFFE URI SAN"))?;
    let expected_uri = format!("spiffe://streamserver/agent/{}", pending.node_id);
    ensure!(
        subject_alt_name.value.general_names.len() == 1
            && matches!(
                subject_alt_name.value.general_names.first(),
                Some(GeneralName::URI(uri)) if *uri == expected_uri
            ),
        "Agent identity certificate must contain exactly the expected SPIFFE URI SAN"
    );
    let extended_key_usage = leaf
        .extended_key_usage()
        .map_err(|error| anyhow!("Agent identity EKU is invalid: {error}"))?
        .ok_or_else(|| anyhow!("Agent identity certificate is missing clientAuth EKU"))?;
    ensure!(
        extended_key_usage.value.client_auth
            && !extended_key_usage.value.any
            && !extended_key_usage.value.server_auth
            && !extended_key_usage.value.code_signing
            && !extended_key_usage.value.email_protection
            && !extended_key_usage.value.time_stamping
            && !extended_key_usage.value.ocsp_signing
            && extended_key_usage.value.other.is_empty(),
        "Agent identity certificate must be clientAuth-only"
    );
    let key_usage = leaf
        .key_usage()
        .map_err(|error| anyhow!("Agent identity key usage is invalid: {error}"))?
        .ok_or_else(|| anyhow!("Agent identity certificate is missing digitalSignature usage"))?;
    ensure!(
        key_usage.value.flags == 1 && key_usage.value.digital_signature(),
        "Agent identity certificate key usage must be digitalSignature-only"
    );
    ensure!(
        !leaf
            .basic_constraints()
            .map_err(|error| anyhow!("Agent identity constraints are invalid: {error}"))?
            .is_some_and(|constraints| constraints.value.ca),
        "Agent identity certificate must not be a CA"
    );

    let not_before = DateTime::<Utc>::from_timestamp(leaf.validity().not_before.timestamp(), 0)
        .ok_or_else(|| anyhow!("Agent identity not-before is outside supported range"))?;
    let not_after = DateTime::<Utc>::from_timestamp(leaf.validity().not_after.timestamp(), 0)
        .ok_or_else(|| anyhow!("Agent identity not-after is outside supported range"))?;
    ensure!(
        now >= not_before && now < not_after,
        "Agent identity certificate is not currently valid"
    );
    ensure!(
        not_after > not_before && not_after - not_before <= Duration::days(90),
        "Agent identity certificate validity exceeds policy"
    );
    Ok(ValidatedCertificate {
        fingerprint_sha256: sha256_hex(&leaf_pem.contents),
        serial_number: hex_lower(leaf.raw_serial()),
        not_before,
        not_after,
    })
}

fn validate_complete_enrollment_response(
    pending: &PendingIdentity,
    response: &AgentEnrollResponse,
    now: DateTime<Utc>,
) -> anyhow::Result<ValidatedEnrollmentBundle> {
    ensure!(
        response.node_id == pending.node_id,
        "Core enrollment response node identity does not match the request"
    );
    ensure!(
        response.ca_certificate_pem == response.agent_client_issuer_ca_pem,
        "Core enrollment response contains conflicting Agent issuer CA fields"
    );
    let control = validate_issued_identity(
        pending,
        &response.certificate_pem,
        &response.agent_client_issuer_ca_pem,
        now,
    )?;
    validate_certificate_response_metadata(
        &control,
        &response.fingerprint_sha256,
        &response.serial_number,
        response.not_before,
        response.not_after,
        "control client",
    )?;

    let issuer_pem = parse_single_certificate_pem(
        &response.agent_client_issuer_ca_pem,
        "Agent client issuer CA",
    )?;
    let issuer = parse_exact_certificate_der(&issuer_pem.contents, "Agent client issuer CA")?;
    validate_root_ca_certificate(&issuer, now, "Agent client issuer CA")?;
    let issuer_not_before =
        DateTime::<Utc>::from_timestamp(issuer.validity().not_before.timestamp(), 0)
            .ok_or_else(|| anyhow!("Agent issuer CA not-before is outside supported range"))?;
    let issuer_not_after =
        DateTime::<Utc>::from_timestamp(issuer.validity().not_after.timestamp(), 0)
            .ok_or_else(|| anyhow!("Agent issuer CA expiry is outside supported range"))?;
    ensure!(
        control.not_before >= issuer_not_before && control.not_after <= issuer_not_after,
        "Agent control certificate validity must be contained by its issuer CA"
    );
    let server_ca_pem = parse_single_certificate_pem(
        &response.control_plane_server_ca_pem,
        "control-plane server CA",
    )?;
    let server_ca =
        parse_exact_certificate_der(&server_ca_pem.contents, "control-plane server CA")?;
    validate_root_ca_certificate(&server_ca, now, "control-plane server CA")?;
    let management_client_ca_pem =
        parse_single_certificate_pem(&response.management_client_ca_pem, "management client CA")?;
    let management_client_ca =
        parse_exact_certificate_der(&management_client_ca_pem.contents, "management client CA")?;
    validate_root_ca_certificate(&management_client_ca, now, "management client CA")?;
    ensure!(
        issuer_pem.contents != server_ca_pem.contents
            && issuer_pem.contents != management_client_ca_pem.contents
            && server_ca_pem.contents != management_client_ca_pem.contents,
        "Agent issuer, control-plane server, and management client CAs must be independent"
    );
    ensure!(
        issuer.public_key().raw != server_ca.public_key().raw
            && issuer.public_key().raw != management_client_ca.public_key().raw
            && server_ca.public_key().raw != management_client_ca.public_key().raw,
        "Agent issuer, control-plane server, and management client CA SPKIs must be independent"
    );

    let management_pem = parse_single_certificate_pem(
        &response.management_certificate_pem,
        "Agent management certificate",
    )?;
    let management =
        parse_exact_certificate_der(&management_pem.contents, "Agent management certificate")?;
    ensure!(
        management.issuer() == issuer.subject(),
        "Agent management certificate issuer does not match the Agent issuer CA"
    );
    management
        .verify_signature(Some(issuer.public_key()))
        .map_err(|error| anyhow!("Agent management certificate signature is invalid: {error}"))?;
    let management_key = KeyPair::from_pem(pending.management_private_key_pem.as_str())
        .context("pending Agent management key is invalid")?;
    let control_key = KeyPair::from_pem(pending.private_key_pem.as_str())
        .context("pending Agent control key is invalid")?;
    ensure!(
        control_key.public_key_der() != management_key.public_key_der(),
        "Agent control and management public keys must be distinct"
    );
    ensure!(
        management.public_key().raw == management_key.public_key_der().as_slice(),
        "Agent management certificate does not match the generated management key"
    );
    let expected_uri = format!("spiffe://streamserver/agent-management/{}", pending.node_id);
    let management_dns_name = format!(
        "agent-{}.agent.streamserver.internal",
        pending.node_id.simple()
    );
    let san = management
        .subject_alternative_name()
        .map_err(|error| anyhow!("Agent management SAN is invalid: {error}"))?
        .ok_or_else(|| anyhow!("Agent management certificate is missing URI/DNS SANs"))?;
    let uri_count = san
        .value
        .general_names
        .iter()
        .filter(|name| matches!(name, GeneralName::URI(uri) if *uri == expected_uri))
        .count();
    let dns_count = san
        .value
        .general_names
        .iter()
        .filter(|name| matches!(name, GeneralName::DNSName(dns) if *dns == management_dns_name))
        .count();
    ensure!(
        san.value.general_names.len() == 2 && uri_count == 1 && dns_count == 1,
        "Agent management certificate must contain exactly the expected URI and DNS SANs"
    );
    let eku = management
        .extended_key_usage()
        .map_err(|error| anyhow!("Agent management EKU is invalid: {error}"))?
        .ok_or_else(|| anyhow!("Agent management certificate is missing serverAuth EKU"))?;
    ensure!(
        eku.value.server_auth
            && !eku.value.any
            && !eku.value.client_auth
            && !eku.value.code_signing
            && !eku.value.email_protection
            && !eku.value.time_stamping
            && !eku.value.ocsp_signing
            && eku.value.other.is_empty(),
        "Agent management certificate must be serverAuth-only"
    );
    let key_usage = management
        .key_usage()
        .map_err(|error| anyhow!("Agent management key usage is invalid: {error}"))?
        .ok_or_else(|| anyhow!("Agent management certificate is missing digitalSignature"))?;
    ensure!(
        key_usage.value.flags == 1 && key_usage.value.digital_signature(),
        "Agent management certificate key usage must be digitalSignature-only"
    );
    ensure!(
        !management
            .basic_constraints()
            .map_err(|error| anyhow!("Agent management constraints are invalid: {error}"))?
            .is_some_and(|constraints| constraints.value.ca),
        "Agent management certificate must not be a CA"
    );
    let management_not_before =
        DateTime::<Utc>::from_timestamp(management.validity().not_before.timestamp(), 0)
            .ok_or_else(|| anyhow!("Agent management not-before is outside supported range"))?;
    let management_not_after =
        DateTime::<Utc>::from_timestamp(management.validity().not_after.timestamp(), 0)
            .ok_or_else(|| anyhow!("Agent management expiry is outside supported range"))?;
    ensure!(
        now >= management_not_before && now < management_not_after,
        "Agent management certificate is not currently valid"
    );
    ensure!(
        management_not_after > management_not_before
            && management_not_after - management_not_before <= Duration::days(90),
        "Agent management certificate validity exceeds policy"
    );
    ensure!(
        management_not_before >= issuer_not_before && management_not_after <= issuer_not_after,
        "Agent management certificate validity must be contained by its issuer CA"
    );
    let management_validated = ValidatedCertificate {
        fingerprint_sha256: sha256_hex(&management_pem.contents),
        serial_number: hex_lower(management.raw_serial()),
        not_before: management_not_before,
        not_after: management_not_after,
    };
    validate_certificate_response_metadata(
        &management_validated,
        &response.management_fingerprint_sha256,
        &response.management_serial_number,
        response.management_not_before,
        response.management_not_after,
        "management server",
    )?;
    ensure!(
        control.fingerprint_sha256 != management_validated.fingerprint_sha256
            && control.serial_number != management_validated.serial_number,
        "Agent control and management certificates must have distinct fingerprints and serials"
    );

    let capability_pem = parse_single_pem(
        &response.capability_jwt_public_key_pem,
        "PUBLIC KEY",
        "capability JWT public key",
    )?;
    let (remainder, capability_spki) =
        x509_parser::x509::SubjectPublicKeyInfo::from_der(&capability_pem.contents)
            .map_err(|error| anyhow!("capability JWT public key SPKI is invalid: {error}"))?;
    ensure!(
        remainder.is_empty()
            && capability_spki.algorithm.algorithm == x509_parser::oid_registry::OID_SIG_ED25519
            && capability_spki.algorithm.parameters.is_none()
            && capability_spki.subject_public_key.unused_bits == 0
            && capability_spki.subject_public_key.data.len() == 32,
        "capability JWT public key must be an Ed25519 SubjectPublicKeyInfo"
    );
    let capability_jwt_kid = sha256_hex(&capability_pem.contents);
    ensure!(
        response.capability_jwt_kid == capability_jwt_kid
            && response.capability_jwt_kid.len() == 64
            && response
                .capability_jwt_kid
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "capability JWT kid must equal lowercase SHA-256 of the Ed25519 SPKI"
    );
    ensure!(
        capability_pem.contents != control_key.public_key_der()
            && capability_pem.contents != management_key.public_key_der(),
        "capability JWT public key must not reuse an Agent TLS key"
    );

    Ok(ValidatedEnrollmentBundle {
        control,
        management: management_validated,
        management_dns_name,
        agent_client_issuer_ca_sha256: sha256_hex(&issuer_pem.contents),
        control_plane_server_ca_sha256: sha256_hex(&server_ca_pem.contents),
        management_client_ca_sha256: sha256_hex(&management_client_ca_pem.contents),
        capability_jwt_kid,
    })
}

fn validate_certificate_response_metadata(
    certificate: &ValidatedCertificate,
    fingerprint: &str,
    serial: &str,
    not_before: DateTime<Utc>,
    not_after: DateTime<Utc>,
    description: &str,
) -> anyhow::Result<()> {
    ensure!(
        fingerprint == certificate.fingerprint_sha256,
        "Core enrollment {description} fingerprint does not match the certificate"
    );
    ensure!(
        serial == certificate.serial_number,
        "Core enrollment {description} serial does not match the certificate"
    );
    ensure!(
        not_before == certificate.not_before && not_after == certificate.not_after,
        "Core enrollment {description} validity does not match the certificate"
    );
    Ok(())
}

fn validate_root_ca_certificate(
    ca: &x509_parser::certificate::X509Certificate<'_>,
    now: DateTime<Utc>,
    description: &str,
) -> anyhow::Result<()> {
    ensure!(
        ca.basic_constraints()
            .map_err(|error| anyhow!("{description} constraints are invalid: {error}"))?
            .is_some_and(|constraints| constraints.value.ca),
        "{description} is not a CA"
    );
    let key_usage = ca
        .key_usage()
        .map_err(|error| anyhow!("{description} key usage is invalid: {error}"))?
        .ok_or_else(|| anyhow!("{description} is missing keyCertSign"))?;
    ensure!(
        key_usage.value.flags == 1 << 5,
        "{description} key usage must be keyCertSign-only"
    );
    ensure!(
        ca.extended_key_usage()
            .map_err(|error| anyhow!("{description} extended key usage is invalid: {error}"))?
            .is_none(),
        "{description} must not contain extended key usage"
    );
    ensure!(
        ca.issuer() == ca.subject(),
        "{description} must be self-issued"
    );
    ca.verify_signature(None)
        .map_err(|error| anyhow!("{description} self-signature is invalid: {error}"))?;
    let not_before = DateTime::<Utc>::from_timestamp(ca.validity().not_before.timestamp(), 0)
        .ok_or_else(|| anyhow!("{description} not-before is outside supported range"))?;
    let not_after = DateTime::<Utc>::from_timestamp(ca.validity().not_after.timestamp(), 0)
        .ok_or_else(|| anyhow!("{description} expiry is outside supported range"))?;
    ensure!(
        now >= not_before && now < not_after,
        "{description} is not currently valid"
    );
    Ok(())
}

fn parse_single_pem(pem: &str, label: &str, description: &str) -> anyhow::Result<Pem> {
    let trimmed = pem.trim_matches(char::is_whitespace);
    let begin_marker = format!("-----BEGIN {label}-----");
    let end_marker = format!("-----END {label}-----");
    ensure!(
        trimmed.starts_with(&begin_marker)
            && trimmed.ends_with(&end_marker)
            && !trimmed[begin_marker.len()..trimmed.len() - end_marker.len()]
                .contains("-----BEGIN ")
            && !trimmed[begin_marker.len()..trimmed.len() - end_marker.len()].contains("-----END "),
        "{description} must be one strictly framed {label} PEM block"
    );
    let (remainder, parsed) = parse_x509_pem(pem.as_bytes())
        .map_err(|error| anyhow!("{description} PEM is invalid: {error}"))?;
    ensure!(
        parsed.label == label && remainder.iter().all(u8::is_ascii_whitespace),
        "{description} must contain exactly one {label} PEM block"
    );
    Ok(parsed)
}

fn parse_single_certificate_pem(pem: &str, description: &str) -> anyhow::Result<Pem> {
    parse_single_pem(pem, "CERTIFICATE", description)
}

fn parse_exact_certificate_der<'a>(
    der: &'a [u8],
    description: &str,
) -> anyhow::Result<x509_parser::certificate::X509Certificate<'a>> {
    let (remainder, certificate) = parse_x509_certificate(der)
        .map_err(|error| anyhow!("{description} DER is invalid: {error}"))?;
    ensure!(
        remainder.is_empty(),
        "{description} DER contains trailing data"
    );
    Ok(certificate)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_lower(&digest)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn fail_at(actual: InstallFailpoint, expected: InstallFailpoint) -> anyhow::Result<()> {
    if actual == expected {
        bail!("injected Agent identity installation failure");
    }
    Ok(())
}

fn fail_rotation_install_at(
    actual: RotationInstallFailpoint,
    expected: RotationInstallFailpoint,
) -> anyhow::Result<()> {
    if actual == expected {
        bail!("injected certificate rotation installation failure");
    }
    Ok(())
}

fn fail_rotation_activation_at(
    actual: RotationActivationFailpoint,
    expected: RotationActivationFailpoint,
) -> anyhow::Result<()> {
    if actual == expected {
        bail!("injected certificate rotation activation failure");
    }
    Ok(())
}

fn fail_pending_cleanup_at(
    actual: PendingCleanupFailpoint,
    expected: PendingCleanupFailpoint,
) -> anyhow::Result<()> {
    if actual == expected {
        bail!("injected pending Agent identity cleanup failure");
    }
    Ok(())
}

fn path_exists_no_follow(path: &Path) -> anyhow::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn ensure_secure_directory(path: &Path) -> anyhow::Result<()> {
    assert_no_symlink_ancestors(path)?;
    if !path_exists_no_follow(path)? {
        create_secure_directory(path)?;
    }
    assert_secure_directory(path)
}

fn assert_no_symlink_ancestors(path: &Path) -> anyhow::Result<()> {
    use std::path::Component;

    ensure!(
        path.is_absolute()
            && path
                .components()
                .all(|component| !matches!(component, Component::CurDir | Component::ParentDir)),
        "Agent identity directory must be an absolute normalized path"
    );
    assert_trusted_directory_ancestors(path)?;
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "Agent identity path contains a symbolic-link ancestor: {}",
                ancestor.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", ancestor.display()));
            }
        }
    }
    Ok(())
}

fn assert_trusted_directory_ancestors(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let effective_uid = unsafe { libc::geteuid() };
        for ancestor in path.parent().into_iter().flat_map(Path::ancestors) {
            let metadata = match fs::symlink_metadata(ancestor) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to inspect trusted ancestor {}", ancestor.display())
                    });
                }
            };
            ensure!(
                !metadata.file_type().is_symlink() && metadata.is_dir(),
                "Agent identity ancestor must be a real directory: {}",
                ancestor.display()
            );
            let owner = metadata.uid();
            ensure!(
                owner == 0 || owner == effective_uid,
                "Agent identity ancestor must be owned by root or the service user: {}",
                ancestor.display()
            );
            let mode = metadata.permissions().mode();
            let root_owned_sticky_directory = owner == 0 && mode & 0o1000 != 0;
            ensure!(
                mode & 0o022 == 0 || root_owned_sticky_directory,
                "Agent identity ancestor must not be group/world writable unless it is a root-owned sticky directory: {}",
                ancestor.display()
            );
        }
    }
    Ok(())
}

fn create_secure_directory(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(path)
            .with_context(|| format!("failed to create secure directory {}", path.display()))?;
    }
    #[cfg(not(unix))]
    fs::create_dir(path)
        .with_context(|| format!("failed to create secure directory {}", path.display()))?;
    assert_secure_directory(path)
}

fn assert_secure_directory(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect directory {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_dir(),
        "Agent identity path must be a real directory: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "Agent identity directory must be owned by the current service user: {}",
            path.display()
        );
        ensure!(
            metadata.permissions().mode() & 0o777 == 0o700,
            "Agent identity directory permissions must be 0700: {}",
            path.display()
        );
    }
    Ok(())
}

fn write_new_secure_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create identity file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure identity file {}", path.display()))?;
    }
    file.write_all(contents)
        .with_context(|| format!("failed to write identity file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync identity file {}", path.display()))?;
    assert_private_identity_file_metadata(path, &file.metadata()?)?;
    Ok(())
}

fn assert_regular_file_no_symlink(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect identity file {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "Agent identity path must be a regular file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= MAX_IDENTITY_FILE_BYTES,
        "Agent identity file exceeds the size limit: {}",
        path.display()
    );
    assert_private_identity_file_metadata(path, &metadata)?;
    Ok(())
}

fn assert_private_identity_file_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "Agent identity file must be owned by the current service user: {}",
            path.display()
        );
        ensure!(
            metadata.permissions().mode() & 0o777 == 0o600,
            "Agent identity file permissions must be 0600: {}",
            path.display()
        );
        ensure!(
            metadata.nlink() == 1,
            "Agent identity file must not have multiple hard links: {}",
            path.display()
        );
    }
    Ok(())
}

fn read_limited(path: &Path, limit: u64) -> anyhow::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open identity file {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect identity file {}", path.display()))?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit,
        "Agent identity file is invalid or exceeds its size limit: {}",
        path.display()
    );
    assert_private_identity_file_metadata(path, &metadata)?;
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut contents)
        .with_context(|| format!("failed to read identity file {}", path.display()))?;
    ensure!(
        contents.len() as u64 <= limit,
        "Agent identity file exceeds its size limit: {}",
        path.display()
    );
    Ok(contents)
}

fn read_regular_file_limited(path: &Path, limit: u64) -> anyhow::Result<Vec<u8>> {
    assert_no_symlink_ancestors(path)?;
    let before = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect trusted file {}", path.display()))?;
    ensure!(
        !before.file_type().is_symlink() && before.is_file() && before.len() <= limit,
        "trusted file is invalid or exceeds its size limit: {}",
        path.display()
    );
    assert_trusted_public_file_metadata(path, &before)?;

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open trusted file {}", path.display()))?;
    let after = file
        .metadata()
        .with_context(|| format!("failed to inspect trusted file {}", path.display()))?;
    ensure!(
        after.is_file() && after.len() <= limit,
        "trusted file is invalid or exceeds its size limit: {}",
        path.display()
    );
    assert_trusted_public_file_metadata(path, &after)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.dev() == after.dev() && before.ino() == after.ino(),
            "trusted file changed while opening: {}",
            path.display()
        );
    }
    let mut contents = Vec::with_capacity(after.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut contents)
        .with_context(|| format!("failed to read trusted file {}", path.display()))?;
    ensure!(
        contents.len() as u64 <= limit,
        "trusted file exceeds its size limit: {}",
        path.display()
    );
    Ok(contents)
}

fn assert_trusted_public_file_metadata(path: &Path, metadata: &fs::Metadata) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let owner = metadata.uid();
        ensure!(
            owner == 0 || owner == unsafe { libc::geteuid() },
            "trusted file must be owned by root or the current service user: {}",
            path.display()
        );
        ensure!(
            metadata.permissions().mode() & 0o022 == 0,
            "trusted file must not be group/world writable: {}",
            path.display()
        );
    }
    Ok(())
}

fn ensure_directory_contains_exactly(path: &Path, expected: &[&str]) -> anyhow::Result<()> {
    let mut actual = fs::read_dir(path)
        .with_context(|| format!("failed to inspect identity directory {}", path.display()))?
        .map(|entry| {
            entry
                .context("failed to inspect identity directory entry")?
                .file_name()
                .into_string()
                .map_err(|_| anyhow!("identity directory contains a non-UTF-8 entry"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    actual.sort();
    let mut expected = expected
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    expected.sort();
    ensure!(
        actual == expected,
        "identity directory contains unexpected entries: {}",
        path.display()
    );
    Ok(())
}

fn remove_secure_identity_tree(path: &Path) -> anyhow::Result<()> {
    let mut removed_entries = 0;
    remove_secure_identity_tree_with_failpoint(
        path,
        PendingCleanupFailpoint::None,
        &mut removed_entries,
    )
}

fn remove_secure_identity_tree_with_failpoint(
    path: &Path,
    failpoint: PendingCleanupFailpoint,
    removed_entries: &mut usize,
) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect stale identity path {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "refusing to remove symlink from Agent identity root: {}",
        path.display()
    );
    if metadata.is_file() {
        assert_private_identity_file_metadata(path, &metadata)?;
        fs::remove_file(path)
            .with_context(|| format!("failed to remove stale identity file {}", path.display()))?;
        *removed_entries += 1;
        if failpoint == PendingCleanupFailpoint::AfterFirstRetiredEntry && *removed_entries == 1 {
            bail!("injected pending Agent identity cleanup failure");
        }
        return Ok(());
    }
    ensure!(
        metadata.is_dir(),
        "unsupported entry in Agent identity root: {}",
        path.display()
    );
    assert_secure_directory(path)?;
    for entry in fs::read_dir(path).with_context(|| {
        format!(
            "failed to inspect stale identity directory {}",
            path.display()
        )
    })? {
        remove_secure_identity_tree_with_failpoint(&entry?.path(), failpoint, removed_entries)?;
    }
    fs::remove_dir(path).with_context(|| {
        format!(
            "failed to remove stale identity directory {}",
            path.display()
        )
    })?;
    *removed_entries += 1;
    if failpoint == PendingCleanupFailpoint::AfterFirstRetiredEntry && *removed_entries == 1 {
        bail!("injected pending Agent identity cleanup failure");
    }
    Ok(())
}

fn sync_directory(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)
            .with_context(|| format!("failed to open directory {} for sync", path.display()))?
            .sync_all()
            .with_context(|| format!("failed to sync directory {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, sync::Arc};

    use chrono::{Duration, TimeZone, Utc};
    use media_rpc::control_plane::{ActivateCertificateRotation, CertificateRotationBundle};
    use rcgen::{
        BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
        DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, PKCS_ED25519,
        PublicKeyData, SanType, SerialNumber,
    };
    use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConnection};
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::{
        AgentIdentityLoadError, AgentIdentityStore, AuthenticatedRotationAction,
        EnrollmentPreparation, InstallFailpoint, parse_enroll_args, rotation_due,
    };

    fn tempdir() -> std::io::Result<TempDir> {
        let directory = tempfile::tempdir()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        }
        Ok(directory)
    }

    #[cfg(unix)]
    fn assert_complete_generation_permissions(identity: &super::LoadedIdentity) {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&identity.generation_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for name in [
            "agent-client-issuer-ca.pem",
            "capability-jwt-public-key.pem",
            "control-client-cert.pem",
            "control-client-key.pem",
            "control-plane-server-ca.pem",
            "management-client-ca.pem",
            "management-server-cert.pem",
            "management-server-key.pem",
            "metadata.json",
        ] {
            assert_eq!(
                fs::metadata(identity.generation_dir.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    fn issue_identity(
        node_id: Uuid,
        subject_key: &KeyPair,
        not_before: time::OffsetDateTime,
        not_after: time::OffsetDateTime,
    ) -> (String, String) {
        let ca_key = KeyPair::generate().expect("generate CA key");
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "StreamServer test Agent CA");
        let ca = ca_params.self_signed(&ca_key).expect("issue CA");

        let mut leaf = CertificateParams::default();
        leaf.not_before = not_before;
        leaf.not_after = not_after;
        leaf.is_ca = IsCa::NoCa;
        leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        leaf.serial_number = Some(SerialNumber::from_slice(&[0x12; 16]));
        leaf.subject_alt_names = vec![SanType::URI(
            format!("spiffe://streamserver/agent/{node_id}")
                .try_into()
                .expect("valid URI SAN"),
        )];
        leaf.distinguished_name = DistinguishedName::new();
        let cert = leaf
            .signed_by(subject_key, &ca, &ca_key)
            .expect("issue Agent leaf");
        (cert.pem(), ca.pem())
    }

    #[derive(Clone, Copy)]
    enum HostileCertificateProfile {
        ExpiredCa,
        NonSelfSignedCa,
        MismatchedIssuer,
        ExtraEku,
        CertificateSigningKeyUsage,
    }

    fn issue_hostile_identity(
        node_id: Uuid,
        subject_key: &KeyPair,
        now: time::OffsetDateTime,
        profile: HostileCertificateProfile,
    ) -> (String, String) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        ca_params.not_before = now - time::Duration::days(1);
        ca_params.not_after = now + time::Duration::days(365);
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "StreamServer hostile Agent CA");
        if matches!(profile, HostileCertificateProfile::ExpiredCa) {
            ca_params.not_before = now - time::Duration::days(365);
            ca_params.not_after = now - time::Duration::days(1);
        }

        let (signing_ca, returned_ca) = match profile {
            HostileCertificateProfile::NonSelfSignedCa => {
                let root_key = KeyPair::generate().unwrap();
                let mut root_params = CertificateParams::default();
                root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
                root_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
                let root = root_params.self_signed(&root_key).unwrap();
                let signing_ca = ca_params
                    .clone()
                    .signed_by(&ca_key, &root, &root_key)
                    .unwrap();
                let returned_ca = ca_params.signed_by(&ca_key, &root, &root_key).unwrap();
                (signing_ca, returned_ca)
            }
            HostileCertificateProfile::MismatchedIssuer => {
                let signing_ca = ca_params.clone().self_signed(&ca_key).unwrap();
                let mut returned_params = ca_params;
                returned_params.distinguished_name = DistinguishedName::new();
                returned_params
                    .distinguished_name
                    .push(DnType::CommonName, "Different CA with the same key");
                let returned_ca = returned_params.self_signed(&ca_key).unwrap();
                (signing_ca, returned_ca)
            }
            _ => {
                let signing_ca = ca_params.clone().self_signed(&ca_key).unwrap();
                let returned_ca = ca_params.self_signed(&ca_key).unwrap();
                (signing_ca, returned_ca)
            }
        };

        let mut leaf = CertificateParams::default();
        leaf.not_before = now - time::Duration::minutes(1);
        leaf.not_after = now + time::Duration::days(90) - time::Duration::minutes(1);
        leaf.is_ca = IsCa::NoCa;
        leaf.key_usages = if matches!(
            profile,
            HostileCertificateProfile::CertificateSigningKeyUsage
        ) {
            vec![KeyUsagePurpose::KeyCertSign]
        } else {
            vec![KeyUsagePurpose::DigitalSignature]
        };
        leaf.extended_key_usages = if matches!(profile, HostileCertificateProfile::ExtraEku) {
            vec![
                ExtendedKeyUsagePurpose::ClientAuth,
                ExtendedKeyUsagePurpose::CodeSigning,
            ]
        } else {
            vec![ExtendedKeyUsagePurpose::ClientAuth]
        };
        leaf.subject_alt_names = vec![SanType::URI(
            format!("spiffe://streamserver/agent/{node_id}")
                .try_into()
                .unwrap(),
        )];
        leaf.serial_number = Some(SerialNumber::from_slice(&[0x34; 16]));
        let certificate = leaf.signed_by(subject_key, &signing_ca, &ca_key).unwrap();
        (certificate.pem(), returned_ca.pem())
    }

    fn certificate_response_metadata(
        certificate_pem: &str,
    ) -> (String, String, chrono::DateTime<Utc>, chrono::DateTime<Utc>) {
        let (_, pem) = x509_parser::pem::parse_x509_pem(certificate_pem.as_bytes()).unwrap();
        let (remainder, certificate) = x509_parser::parse_x509_certificate(&pem.contents).unwrap();
        assert!(remainder.is_empty());
        (
            super::sha256_hex(&pem.contents),
            super::hex_lower(certificate.raw_serial()),
            chrono::DateTime::from_timestamp(certificate.validity().not_before.timestamp(), 0)
                .unwrap(),
            chrono::DateTime::from_timestamp(certificate.validity().not_after.timestamp(), 0)
                .unwrap(),
        )
    }

    #[derive(Clone, Copy)]
    enum ManagementCertificateProfile {
        Valid,
        WrongDns,
        ExtraDns,
        ClientAuth,
    }

    fn complete_enrollment_response(
        pending: &super::PendingIdentity,
        now: chrono::DateTime<Utc>,
    ) -> super::AgentEnrollResponse {
        complete_enrollment_response_with_profile(
            pending,
            now,
            &[0x21; 16],
            ManagementCertificateProfile::Valid,
        )
    }

    fn complete_enrollment_response_with_control_serial(
        pending: &super::PendingIdentity,
        now: chrono::DateTime<Utc>,
        control_serial: &[u8],
    ) -> super::AgentEnrollResponse {
        complete_enrollment_response_with_profile(
            pending,
            now,
            control_serial,
            ManagementCertificateProfile::Valid,
        )
    }

    fn complete_enrollment_response_with_profile(
        pending: &super::PendingIdentity,
        now: chrono::DateTime<Utc>,
        control_serial: &[u8],
        management_profile: ManagementCertificateProfile,
    ) -> super::AgentEnrollResponse {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "StreamServer test Agent issuer CA");
        let agent_ca = ca_params.self_signed(&ca_key).unwrap();

        let control_key = KeyPair::from_pem(pending.private_key_pem.as_str()).unwrap();
        let leaf_not_before =
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap();
        let leaf_not_after = time::OffsetDateTime::from_unix_timestamp(
            (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
        )
        .unwrap();
        let mut control_params = CertificateParams::default();
        control_params.not_before = leaf_not_before;
        control_params.not_after = leaf_not_after;
        control_params.is_ca = IsCa::NoCa;
        control_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        control_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        control_params.serial_number = Some(SerialNumber::from_slice(control_serial));
        control_params.subject_alt_names = vec![SanType::URI(
            format!("spiffe://streamserver/agent/{}", pending.node_id)
                .try_into()
                .unwrap(),
        )];
        let control_certificate = control_params
            .signed_by(&control_key, &agent_ca, &ca_key)
            .unwrap()
            .pem();

        let management_key =
            KeyPair::from_pem(pending.management_private_key_pem.as_str()).unwrap();
        let management_dns_name = format!(
            "agent-{}.agent.streamserver.internal",
            pending.node_id.simple()
        );
        let mut management_params = CertificateParams::default();
        management_params.not_before = leaf_not_before;
        management_params.not_after = leaf_not_after;
        management_params.is_ca = IsCa::NoCa;
        management_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        management_params.extended_key_usages =
            if matches!(management_profile, ManagementCertificateProfile::ClientAuth) {
                vec![ExtendedKeyUsagePurpose::ClientAuth]
            } else {
                vec![ExtendedKeyUsagePurpose::ServerAuth]
            };
        management_params.serial_number = Some(SerialNumber::from_slice(&[0x22; 16]));
        let dns_san = if matches!(management_profile, ManagementCertificateProfile::WrongDns) {
            "wrong.agent.streamserver.internal".to_string()
        } else {
            management_dns_name.clone()
        };
        management_params.subject_alt_names = vec![
            SanType::URI(
                format!("spiffe://streamserver/agent-management/{}", pending.node_id)
                    .try_into()
                    .unwrap(),
            ),
            SanType::DnsName(dns_san.try_into().unwrap()),
        ];
        if matches!(management_profile, ManagementCertificateProfile::ExtraDns) {
            management_params.subject_alt_names.push(SanType::DnsName(
                "extra.agent.streamserver.internal".try_into().unwrap(),
            ));
        }
        let management_certificate = management_params
            .signed_by(&management_key, &agent_ca, &ca_key)
            .unwrap()
            .pem();

        let server_ca_key = KeyPair::generate().unwrap();
        let mut server_ca_params = CertificateParams::default();
        server_ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        server_ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        server_ca_params.distinguished_name = DistinguishedName::new();
        server_ca_params
            .distinguished_name
            .push(DnType::CommonName, "StreamServer control-plane server CA");
        let server_ca_pem = server_ca_params.self_signed(&server_ca_key).unwrap().pem();

        let management_client_ca_key = KeyPair::generate().unwrap();
        let mut management_client_ca_params = CertificateParams::default();
        management_client_ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        management_client_ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        management_client_ca_params.distinguished_name = DistinguishedName::new();
        management_client_ca_params
            .distinguished_name
            .push(DnType::CommonName, "StreamServer management client CA");
        let management_client_ca_pem = management_client_ca_params
            .self_signed(&management_client_ca_key)
            .unwrap()
            .pem();

        let capability_key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let capability_public_key_pem = capability_key.public_key_pem();
        let capability_kid = super::sha256_hex(&capability_key.public_key_der());
        let (fingerprint, serial, not_before, not_after) =
            certificate_response_metadata(&control_certificate);
        let (
            management_fingerprint,
            management_serial,
            management_not_before,
            management_not_after,
        ) = certificate_response_metadata(&management_certificate);
        let agent_ca_pem = agent_ca.pem();
        super::AgentEnrollResponse {
            node_id: pending.node_id,
            certificate_pem: control_certificate,
            ca_certificate_pem: agent_ca_pem.clone(),
            fingerprint_sha256: fingerprint,
            serial_number: serial,
            not_before,
            not_after,
            agent_client_issuer_ca_pem: agent_ca_pem,
            control_plane_server_ca_pem: server_ca_pem,
            management_client_ca_pem,
            management_certificate_pem: management_certificate,
            management_fingerprint_sha256: management_fingerprint,
            management_serial_number: management_serial,
            management_not_before,
            management_not_after,
            capability_jwt_public_key_pem: capability_public_key_pem,
            capability_jwt_kid: capability_kid,
        }
    }

    struct RotationAuthority {
        agent_ca: rcgen::Certificate,
        agent_ca_key: KeyPair,
        control_plane_server_ca_pem: String,
        management_client_ca_pem: String,
        capability_jwt_public_key_pem: String,
        capability_jwt_kid: String,
    }

    fn rotation_authority() -> RotationAuthority {
        let agent_ca_key = KeyPair::generate().unwrap();
        let mut agent_ca_params = CertificateParams::default();
        agent_ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        agent_ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        agent_ca_params.distinguished_name = DistinguishedName::new();
        agent_ca_params.distinguished_name.push(
            DnType::CommonName,
            "StreamServer rotation test Agent issuer CA",
        );
        let agent_ca = agent_ca_params.self_signed(&agent_ca_key).unwrap();

        let server_ca_key = KeyPair::generate().unwrap();
        let mut server_ca_params = CertificateParams::default();
        server_ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        server_ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        server_ca_params.distinguished_name = DistinguishedName::new();
        server_ca_params
            .distinguished_name
            .push(DnType::CommonName, "StreamServer rotation test server CA");
        let control_plane_server_ca_pem =
            server_ca_params.self_signed(&server_ca_key).unwrap().pem();

        let management_ca_key = KeyPair::generate().unwrap();
        let mut management_ca_params = CertificateParams::default();
        management_ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        management_ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        management_ca_params.distinguished_name = DistinguishedName::new();
        management_ca_params.distinguished_name.push(
            DnType::CommonName,
            "StreamServer rotation test management CA",
        );
        let management_client_ca_pem = management_ca_params
            .self_signed(&management_ca_key)
            .unwrap()
            .pem();

        let capability_key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        RotationAuthority {
            agent_ca,
            agent_ca_key,
            control_plane_server_ca_pem,
            management_client_ca_pem,
            capability_jwt_public_key_pem: capability_key.public_key_pem(),
            capability_jwt_kid: super::sha256_hex(&capability_key.public_key_der()),
        }
    }

    fn response_with_rotation_authority(
        pending: &super::PendingIdentity,
        now: chrono::DateTime<Utc>,
        control_serial: &[u8],
        management_serial: &[u8],
        authority: &RotationAuthority,
    ) -> super::AgentEnrollResponse {
        let not_before =
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap();
        let not_after = time::OffsetDateTime::from_unix_timestamp(
            (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
        )
        .unwrap();
        let control_key = KeyPair::from_pem(pending.private_key_pem.as_str()).unwrap();
        let mut control_params = CertificateParams::default();
        control_params.not_before = not_before;
        control_params.not_after = not_after;
        control_params.is_ca = IsCa::NoCa;
        control_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        control_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        control_params.serial_number = Some(SerialNumber::from_slice(control_serial));
        control_params.subject_alt_names = vec![SanType::URI(
            format!("spiffe://streamserver/agent/{}", pending.node_id)
                .try_into()
                .unwrap(),
        )];
        let control_certificate = control_params
            .signed_by(&control_key, &authority.agent_ca, &authority.agent_ca_key)
            .unwrap()
            .pem();

        let management_key =
            KeyPair::from_pem(pending.management_private_key_pem.as_str()).unwrap();
        let mut management_params = CertificateParams::default();
        management_params.not_before = not_before;
        management_params.not_after = not_after;
        management_params.is_ca = IsCa::NoCa;
        management_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        management_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        management_params.serial_number = Some(SerialNumber::from_slice(management_serial));
        management_params.subject_alt_names = vec![
            SanType::URI(
                format!("spiffe://streamserver/agent-management/{}", pending.node_id)
                    .try_into()
                    .unwrap(),
            ),
            SanType::DnsName(
                format!(
                    "agent-{}.agent.streamserver.internal",
                    pending.node_id.simple()
                )
                .try_into()
                .unwrap(),
            ),
        ];
        let management_certificate = management_params
            .signed_by(
                &management_key,
                &authority.agent_ca,
                &authority.agent_ca_key,
            )
            .unwrap()
            .pem();
        let (fingerprint, serial_number, not_before, not_after) =
            certificate_response_metadata(&control_certificate);
        let (
            management_fingerprint_sha256,
            management_serial_number,
            management_not_before,
            management_not_after,
        ) = certificate_response_metadata(&management_certificate);
        let agent_ca_pem = authority.agent_ca.pem();
        super::AgentEnrollResponse {
            node_id: pending.node_id,
            certificate_pem: control_certificate,
            ca_certificate_pem: agent_ca_pem.clone(),
            fingerprint_sha256: fingerprint,
            serial_number,
            not_before,
            not_after,
            agent_client_issuer_ca_pem: agent_ca_pem,
            control_plane_server_ca_pem: authority.control_plane_server_ca_pem.clone(),
            management_client_ca_pem: authority.management_client_ca_pem.clone(),
            management_certificate_pem: management_certificate,
            management_fingerprint_sha256,
            management_serial_number,
            management_not_before,
            management_not_after,
            capability_jwt_public_key_pem: authority.capability_jwt_public_key_pem.clone(),
            capability_jwt_kid: authority.capability_jwt_kid.clone(),
        }
    }

    fn rotation_bundle(
        rotation_id: Uuid,
        expires_at: chrono::DateTime<Utc>,
        response: super::AgentEnrollResponse,
    ) -> CertificateRotationBundle {
        CertificateRotationBundle {
            rotation_id: rotation_id.to_string(),
            expires_at_ms: expires_at.timestamp_millis(),
            control_certificate_pem: response.certificate_pem,
            control_fingerprint_sha256: response.fingerprint_sha256,
            control_serial_number: response.serial_number,
            control_not_before_ms: response.not_before.timestamp_millis(),
            control_not_after_ms: response.not_after.timestamp_millis(),
            management_certificate_pem: response.management_certificate_pem,
            management_fingerprint_sha256: response.management_fingerprint_sha256,
            management_serial_number: response.management_serial_number,
            management_not_before_ms: response.management_not_before.timestamp_millis(),
            management_not_after_ms: response.management_not_after.timestamp_millis(),
            agent_client_issuer_ca_pem: response.agent_client_issuer_ca_pem,
            control_plane_server_ca_pem: response.control_plane_server_ca_pem,
            management_client_ca_pem: response.management_client_ca_pem,
            capability_jwt_public_key_pem: response.capability_jwt_public_key_pem,
            capability_jwt_kid: response.capability_jwt_kid,
        }
    }

    struct PreparedRotation {
        _root: TempDir,
        store: AgentIdentityStore,
        source_generation_id: Uuid,
        rotation_id: Uuid,
        now: chrono::DateTime<Utc>,
        bundle: CertificateRotationBundle,
    }

    fn prepared_rotation() -> PreparedRotation {
        let root = tempdir().unwrap();
        let store = AgentIdentityStore::new(root.path().join("identity"));
        let node_id = Uuid::now_v7();
        let enrolled_at = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let authority = rotation_authority();
        let lock = store.acquire_enrollment_lock().unwrap();
        let EnrollmentPreparation::Pending(enrollment) = store
            .prepare_enrollment(&lock, node_id, enrolled_at)
            .unwrap()
        else {
            panic!("unexpected recovered identity")
        };
        let response = response_with_rotation_authority(
            &enrollment,
            enrolled_at,
            &[0x41; 16],
            &[0x42; 16],
            &authority,
        );
        let installed = store
            .commit_enrollment_response(&lock, &enrollment, &response, enrolled_at)
            .unwrap();
        let source_generation_id = installed.metadata.generation_id;
        drop(lock);
        let now = installed.not_after() - Duration::days(30);
        let AuthenticatedRotationAction::SendRequest(request) = store
            .on_authenticated_session(source_generation_id, now)
            .unwrap()
        else {
            panic!("rotation request not prepared")
        };
        let pending = store.load_pending_rotation().unwrap();
        let response = response_with_rotation_authority(
            &pending.identity,
            now,
            &[0x51; 16],
            &[0x52; 16],
            &authority,
        );
        PreparedRotation {
            _root: root,
            store,
            source_generation_id,
            rotation_id: request.rotation_id(),
            now,
            bundle: rotation_bundle(request.rotation_id(), now + Duration::minutes(5), response),
        }
    }

    fn management_client_authority() -> (rcgen::Certificate, KeyPair) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "StreamServer test management client CA");
        let certificate = params.self_signed(&key).unwrap();
        (certificate, key)
    }

    fn management_client_identity(
        authority: &(rcgen::Certificate, KeyPair),
        core_id: Uuid,
        now: chrono::DateTime<Utc>,
    ) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.not_before =
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap();
        params.not_after =
            time::OffsetDateTime::from_unix_timestamp((now + Duration::days(30)).timestamp())
                .unwrap();
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.subject_alt_names = vec![SanType::URI(
            format!("spiffe://streamserver/core/{core_id}")
                .try_into()
                .unwrap(),
        )];
        let certificate = params.signed_by(&key, &authority.0, &authority.1).unwrap();
        (certificate.pem(), key.serialize_pem())
    }

    fn management_tls_client(
        server_ca_pem: &str,
        client_certificate_pem: &str,
        client_private_key_pem: &str,
    ) -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        for certificate in CertificateDer::pem_slice_iter(server_ca_pem.as_bytes()) {
            roots.add(certificate.unwrap()).unwrap();
        }
        let certificates = CertificateDer::pem_slice_iter(client_certificate_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let mut private_keys = PrivateKeyDer::pem_slice_iter(client_private_key_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_client_auth_cert(certificates, private_keys.remove(0))
                .unwrap(),
        )
    }

    fn complete_tls_handshake(
        server_config: Arc<rustls::ServerConfig>,
        client_config: Arc<ClientConfig>,
        server_name: &str,
    ) -> anyhow::Result<()> {
        let server_name = ServerName::try_from(server_name.to_string()).unwrap();
        let mut server = ServerConnection::new(server_config)?;
        let mut client = ClientConnection::new(client_config, server_name)?;
        for _ in 0..32 {
            if client.wants_write() {
                let mut bytes = Vec::new();
                client.write_tls(&mut bytes)?;
                server.read_tls(&mut Cursor::new(bytes))?;
                server.process_new_packets()?;
            }
            if server.wants_write() {
                let mut bytes = Vec::new();
                server.write_tls(&mut bytes)?;
                client.read_tls(&mut Cursor::new(bytes))?;
                client.process_new_packets()?;
            }
            if !server.is_handshaking() && !client.is_handshaking() {
                return Ok(());
            }
        }
        anyhow::bail!("TLS handshake did not complete")
    }

    #[test]
    fn csr_has_no_caller_selected_subject_alt_name() {
        let pending = AgentIdentityStore::prepare(Uuid::now_v7()).expect("prepare identity");
        assert!(pending.csr_pem().contains("BEGIN CERTIFICATE REQUEST"));
        assert!(!pending.csr_pem().contains("spiffe://"));
        assert!(!format!("{pending:?}").contains("PRIVATE KEY"));
    }

    #[test]
    fn missing_identity_is_typed_and_load_has_no_filesystem_side_effects() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("identity");
        let store = AgentIdentityStore::new(&root);
        let error = store.load_current(Utc::now()).unwrap_err();
        assert!(matches!(error, AgentIdentityLoadError::NotEnrolled));
        assert!(!root.exists());
    }

    #[test]
    fn pending_enrollment_is_persisted_and_reused() {
        let parent = tempdir().unwrap();
        let identity_root = parent.path().join("identity");
        let store = AgentIdentityStore::new(&identity_root);
        let node_id = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();

        let first = {
            let _lock = store.acquire_enrollment_lock().unwrap();
            match store.prepare_enrollment(&_lock, node_id, now).unwrap() {
                EnrollmentPreparation::Pending(pending) => pending,
                EnrollmentPreparation::Recovered(_) => panic!("unexpected recovered identity"),
            }
        };
        let second = {
            let _lock = store.acquire_enrollment_lock().unwrap();
            match store
                .prepare_enrollment(&_lock, node_id, now + Duration::minutes(1))
                .unwrap()
            {
                EnrollmentPreparation::Pending(pending) => pending,
                EnrollmentPreparation::Recovered(_) => panic!("unexpected recovered identity"),
            }
        };

        assert_eq!(first.generation_id, second.generation_id);
        assert_eq!(first.csr_pem(), second.csr_pem());
        assert_eq!(
            first.private_key_pem_for_test(),
            second.private_key_pem_for_test()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let pending_dir = identity_root.join("pending-enrollment");
            assert_eq!(
                fs::metadata(&pending_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            for file in [
                "control-client-key.pem",
                "control-client.csr.pem",
                "management-server-key.pem",
                "management-server.csr.pem",
                "metadata.json",
            ] {
                assert_eq!(
                    fs::metadata(pending_dir.join(file))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn pending_enrollment_persists_distinct_control_and_management_keypairs() {
        let parent = tempdir().unwrap();
        let identity_root = parent.path().join("identity");
        let store = AgentIdentityStore::new(&identity_root);
        let node_id = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let lock = store.acquire_enrollment_lock().unwrap();
        let EnrollmentPreparation::Pending(pending) =
            store.prepare_enrollment(&lock, node_id, now).unwrap()
        else {
            panic!("unexpected recovered identity")
        };

        assert_ne!(
            pending.control_public_key_der_for_test(),
            pending.management_public_key_der_for_test()
        );
        assert!(
            pending
                .management_csr_pem()
                .contains("BEGIN CERTIFICATE REQUEST")
        );
        let mut entries = fs::read_dir(identity_root.join("pending-enrollment"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                "control-client-key.pem",
                "control-client.csr.pem",
                "management-server-key.pem",
                "management-server.csr.pem",
                "metadata.json",
            ]
        );
    }

    #[test]
    fn pending_enrollment_is_node_bound_and_not_overwritten() {
        let parent = tempdir().unwrap();
        let store = AgentIdentityStore::new(parent.path().join("identity"));
        let first_node = Uuid::now_v7();
        let second_node = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        {
            let _lock = store.acquire_enrollment_lock().unwrap();
            assert!(matches!(
                store.prepare_enrollment(&_lock, first_node, now).unwrap(),
                EnrollmentPreparation::Pending(_)
            ));
        }
        {
            let _lock = store.acquire_enrollment_lock().unwrap();
            assert!(store.prepare_enrollment(&_lock, second_node, now).is_err());
            let pending = store.prepare_enrollment(&_lock, first_node, now).unwrap();
            assert!(matches!(pending, EnrollmentPreparation::Pending(_)));
        }
    }

    #[test]
    fn complete_orphan_generation_is_recovered_before_another_post() {
        let parent = tempdir().unwrap();
        let identity_root = parent.path().join("identity");
        let store = AgentIdentityStore::new(&identity_root);
        let node_id = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let pending = {
            let _lock = store.acquire_enrollment_lock().unwrap();
            match store.prepare_enrollment(&_lock, node_id, now).unwrap() {
                EnrollmentPreparation::Pending(pending) => pending,
                EnrollmentPreparation::Recovered(_) => panic!("unexpected recovered identity"),
            }
        };
        let key = KeyPair::from_pem(pending.private_key_pem_for_test()).unwrap();
        let (certificate, ca) = issue_identity(
            node_id,
            &key,
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap(),
            time::OffsetDateTime::from_unix_timestamp(
                (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
            )
            .unwrap(),
        );
        store.install(&pending, &certificate, &ca, now).unwrap();
        fs::remove_file(identity_root.join("current")).unwrap();

        let recovered = {
            let _lock = store.acquire_enrollment_lock().unwrap();
            store.prepare_enrollment(&_lock, node_id, now).unwrap()
        };
        let EnrollmentPreparation::Recovered(recovered) = recovered else {
            panic!("complete generation was not recovered")
        };
        assert_eq!(recovered.node_id(), node_id);
        assert!(identity_root.join("current").is_file());
        assert!(!identity_root.join("pending-enrollment").exists());
    }

    #[test]
    fn stale_staging_directories_are_cleaned_under_the_identity_lock() {
        let parent = tempdir().unwrap();
        let identity_root = parent.path().join("identity");
        let store = AgentIdentityStore::new(&identity_root);
        {
            let _lock = store.acquire_enrollment_lock().unwrap();
        }
        let stale_pending = identity_root.join(format!(".pending-staging-{}", Uuid::now_v7()));
        super::create_secure_directory(&stale_pending).unwrap();
        super::write_new_secure_file(
            &stale_pending.join("identity-key.pem"),
            b"stale-pending-secret",
        )
        .unwrap();
        let stale_generation = identity_root.join(format!(".staging-{}", Uuid::now_v7()));
        super::create_secure_directory(&stale_generation).unwrap();
        super::write_new_secure_file(
            &stale_generation.join("identity-key.pem"),
            b"stale-generation-secret",
        )
        .unwrap();

        let _lock = store.acquire_enrollment_lock().unwrap();
        let _ = store
            .prepare_enrollment(
                &_lock,
                Uuid::now_v7(),
                Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap(),
            )
            .unwrap();
        assert!(!stale_pending.exists());
        assert!(!stale_generation.exists());
    }

    #[test]
    fn pending_cleanup_failpoints_leave_a_recoverable_current_identity() {
        for failpoint in [
            super::PendingCleanupFailpoint::AfterRetiredRename,
            super::PendingCleanupFailpoint::AfterFirstRetiredEntry,
            super::PendingCleanupFailpoint::AfterRetiredDirectoryRemoval,
        ] {
            let parent = tempdir().unwrap();
            let identity_root = parent.path().join("identity");
            let store = AgentIdentityStore::new(&identity_root);
            let node_id = Uuid::now_v7();
            let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
            let lock = store.acquire_enrollment_lock().unwrap();
            let EnrollmentPreparation::Pending(pending) =
                store.prepare_enrollment(&lock, node_id, now).unwrap()
            else {
                panic!("unexpected recovered identity")
            };
            let key = KeyPair::from_pem(pending.private_key_pem_for_test()).unwrap();
            let (certificate, ca) = issue_identity(
                node_id,
                &key,
                time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                    .unwrap(),
                time::OffsetDateTime::from_unix_timestamp(
                    (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
                )
                .unwrap(),
            );
            store.install(&pending, &certificate, &ca, now).unwrap();

            assert!(
                store
                    .clear_pending_identity_with_failpoint(&pending, failpoint)
                    .is_err()
            );
            drop(lock);

            let lock = store.acquire_enrollment_lock().unwrap();
            let EnrollmentPreparation::Recovered(recovered) =
                store.prepare_enrollment(&lock, node_id, now).unwrap()
            else {
                panic!("current identity was not recovered after cleanup interruption")
            };
            assert_eq!(recovered.node_id(), node_id);
            assert!(!identity_root.join("pending-enrollment").exists());
            assert!(fs::read_dir(&identity_root).unwrap().all(|entry| {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                !name.starts_with(".retired-pending-") && !name.starts_with(".current-")
            }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn enrollment_store_rejects_a_non_sticky_world_writable_ancestor() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempdir().unwrap();
        let unsafe_parent = parent.path().join("unsafe-parent");
        fs::create_dir(&unsafe_parent).unwrap();
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777)).unwrap();
        let store = AgentIdentityStore::new(unsafe_parent.join("identity"));

        assert!(store.acquire_enrollment_lock().is_err());
        assert!(!unsafe_parent.join("identity").exists());
    }

    #[cfg(unix)]
    #[test]
    fn enrollment_lock_rejects_a_replaced_identity_root_inode() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let identity_root = parent.path().join("identity");
        let displaced_root = parent.path().join("identity-displaced");
        let store = AgentIdentityStore::new(&identity_root);
        let lock = store.acquire_enrollment_lock().unwrap();
        fs::rename(&identity_root, &displaced_root).unwrap();
        fs::create_dir(&identity_root).unwrap();
        fs::set_permissions(&identity_root, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            store
                .prepare_enrollment(&lock, Uuid::now_v7(), Utc::now())
                .is_err()
        );
        assert!(!identity_root.join("pending-enrollment").exists());
    }

    #[test]
    fn response_metadata_is_validated_before_current_is_switched() {
        #[derive(Clone, Copy)]
        enum Mismatch {
            Node,
            Serial,
            Fingerprint,
            NotBefore,
            NotAfter,
        }

        for mismatch in [
            Mismatch::Node,
            Mismatch::Serial,
            Mismatch::Fingerprint,
            Mismatch::NotBefore,
            Mismatch::NotAfter,
        ] {
            let parent = tempdir().unwrap();
            let identity_root = parent.path().join("identity");
            let store = AgentIdentityStore::new(&identity_root);
            let node_id = Uuid::now_v7();
            let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
            let lock = store.acquire_enrollment_lock().unwrap();
            let EnrollmentPreparation::Pending(pending) =
                store.prepare_enrollment(&lock, node_id, now).unwrap()
            else {
                panic!("unexpected recovered identity")
            };
            let mut response = complete_enrollment_response(&pending, now);
            match mismatch {
                Mismatch::Node => response.node_id = Uuid::now_v7(),
                Mismatch::Serial => response.serial_number = "00".to_string(),
                Mismatch::Fingerprint => response.fingerprint_sha256 = "00".to_string(),
                Mismatch::NotBefore => response.not_before += Duration::seconds(1),
                Mismatch::NotAfter => response.not_after += Duration::seconds(1),
            }

            assert!(
                store
                    .commit_enrollment_response(&lock, &pending, &response, now)
                    .is_err()
            );
            assert!(!identity_root.join("current").exists());
            assert!(identity_root.join("pending-enrollment").is_dir());
        }
    }

    #[test]
    fn valid_response_switches_current_and_removes_pending_material() {
        let parent = tempdir().unwrap();
        let identity_root = parent.path().join("identity");
        let store = AgentIdentityStore::new(&identity_root);
        let node_id = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let lock = store.acquire_enrollment_lock().unwrap();
        let EnrollmentPreparation::Pending(pending) =
            store.prepare_enrollment(&lock, node_id, now).unwrap()
        else {
            panic!("unexpected recovered identity")
        };
        let response = complete_enrollment_response(&pending, now);

        let installed = store
            .commit_enrollment_response(&lock, &pending, &response, now)
            .unwrap();
        assert_eq!(installed.node_id(), node_id);
        assert!(identity_root.join("current").is_file());
        assert!(!identity_root.join("pending-enrollment").exists());
    }

    #[test]
    fn complete_enrollment_installs_two_identities_and_independent_trust_material() {
        let parent = tempdir().unwrap();
        let identity_root = parent.path().join("identity");
        let store = AgentIdentityStore::new(&identity_root);
        let node_id = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let lock = store.acquire_enrollment_lock().unwrap();
        let EnrollmentPreparation::Pending(pending) =
            store.prepare_enrollment(&lock, node_id, now).unwrap()
        else {
            panic!("unexpected recovered identity")
        };
        let response = complete_enrollment_response(&pending, now);

        let installed = store
            .commit_enrollment_response(&lock, &pending, &response, now)
            .unwrap();
        installed.ensure_production_complete().unwrap();
        assert_ne!(
            installed.agent_client_issuer_ca_pem().unwrap(),
            installed.control_plane_server_ca_pem().unwrap()
        );
        assert_ne!(
            installed.management_client_ca_pem().unwrap(),
            installed.agent_client_issuer_ca_pem().unwrap()
        );
        assert_ne!(
            installed.management_client_ca_pem().unwrap(),
            installed.control_plane_server_ca_pem().unwrap()
        );
        assert_eq!(
            installed.management_dns_name().unwrap(),
            format!("agent-{}.agent.streamserver.internal", node_id.simple())
        );
        assert_eq!(installed.capability_jwt_kid().unwrap().len(), 64);
        assert!(
            installed
                .capability_jwt_public_key_pem()
                .unwrap()
                .contains("BEGIN PUBLIC KEY")
        );
        assert!(
            installed
                .management_certificate_pem()
                .unwrap()
                .contains("BEGIN CERTIFICATE")
        );
        assert!(
            installed
                .management_private_key_pem()
                .unwrap()
                .contains("BEGIN PRIVATE KEY")
        );

        #[cfg(unix)]
        assert_complete_generation_permissions(&installed);

        let mut entries = fs::read_dir(&installed.generation_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                "agent-client-issuer-ca.pem",
                "capability-jwt-public-key.pem",
                "control-client-cert.pem",
                "control-client-key.pem",
                "control-plane-server-ca.pem",
                "management-client-ca.pem",
                "management-server-cert.pem",
                "management-server-key.pem",
                "metadata.json",
            ]
        );
    }

    #[test]
    fn loaded_identity_wires_the_dedicated_management_client_ca_into_the_listener() {
        let parent = tempdir().unwrap();
        let identity_root = parent.path().join("identity");
        let store = AgentIdentityStore::new(&identity_root);
        let node_id = Uuid::now_v7();
        let now = Utc::now();
        let lock = store.acquire_enrollment_lock().unwrap();
        let EnrollmentPreparation::Pending(pending) =
            store.prepare_enrollment(&lock, node_id, now).unwrap()
        else {
            panic!("unexpected recovered identity")
        };
        let dedicated_client_ca = management_client_authority();
        let untrusted_client_ca = management_client_authority();
        let mut response = complete_enrollment_response(&pending, now);
        response.management_client_ca_pem = dedicated_client_ca.0.pem();
        let installed = store
            .commit_enrollment_response(&lock, &pending, &response, now)
            .unwrap();

        let server_config = crate::management::build_management_tls_config(
            installed.management_certificate_pem().unwrap().as_bytes(),
            installed.management_private_key_pem().unwrap().as_bytes(),
            installed.management_client_ca_pem().unwrap().as_bytes(),
        )
        .unwrap();
        let core_id = Uuid::now_v7();
        let (valid_certificate, valid_key) =
            management_client_identity(&dedicated_client_ca, core_id, now);
        let valid_client = management_tls_client(
            installed.agent_client_issuer_ca_pem().unwrap(),
            &valid_certificate,
            &valid_key,
        );
        assert!(
            complete_tls_handshake(
                server_config.clone(),
                valid_client,
                installed.management_dns_name().unwrap(),
            )
            .is_ok()
        );

        let (untrusted_certificate, untrusted_key) =
            management_client_identity(&untrusted_client_ca, core_id, now);
        let untrusted_client = management_tls_client(
            installed.agent_client_issuer_ca_pem().unwrap(),
            &untrusted_certificate,
            &untrusted_key,
        );
        assert!(
            complete_tls_handshake(
                server_config,
                untrusted_client,
                installed.management_dns_name().unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn control_serial_metadata_uses_the_canonical_der_integer_bytes() {
        let parent = tempdir().unwrap();
        let store = AgentIdentityStore::new(parent.path().join("identity"));
        let node_id = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let lock = store.acquire_enrollment_lock().unwrap();
        let EnrollmentPreparation::Pending(pending) =
            store.prepare_enrollment(&lock, node_id, now).unwrap()
        else {
            panic!("unexpected recovered identity")
        };
        let response = complete_enrollment_response_with_control_serial(
            &pending,
            now,
            &[0x00, 0x7f, 0x42, 0x11],
        );

        assert_eq!(response.serial_number, "7f4211");
        store
            .commit_enrollment_response(&lock, &pending, &response, now)
            .unwrap();
    }

    #[test]
    fn complete_enrollment_rejects_hostile_management_trust_and_capability_profiles() {
        #[derive(Clone, Copy)]
        enum Mutation {
            Management(ManagementCertificateProfile),
            SharedTlsCa,
            ManagementClientCaReusesIssuer,
            ManagementClientCaReusesServerCa,
            NonEd25519Capability,
            WrongCapabilityKid,
            ManagementMetadataMismatch,
        }

        for mutation in [
            Mutation::Management(ManagementCertificateProfile::WrongDns),
            Mutation::Management(ManagementCertificateProfile::ExtraDns),
            Mutation::Management(ManagementCertificateProfile::ClientAuth),
            Mutation::SharedTlsCa,
            Mutation::ManagementClientCaReusesIssuer,
            Mutation::ManagementClientCaReusesServerCa,
            Mutation::NonEd25519Capability,
            Mutation::WrongCapabilityKid,
            Mutation::ManagementMetadataMismatch,
        ] {
            let parent = tempdir().unwrap();
            let identity_root = parent.path().join("identity");
            let store = AgentIdentityStore::new(&identity_root);
            let node_id = Uuid::now_v7();
            let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
            let lock = store.acquire_enrollment_lock().unwrap();
            let EnrollmentPreparation::Pending(pending) =
                store.prepare_enrollment(&lock, node_id, now).unwrap()
            else {
                panic!("unexpected recovered identity")
            };
            let mut response = match mutation {
                Mutation::Management(profile) => {
                    complete_enrollment_response_with_profile(&pending, now, &[0x21; 16], profile)
                }
                _ => complete_enrollment_response(&pending, now),
            };
            match mutation {
                Mutation::Management(_) => {}
                Mutation::SharedTlsCa => {
                    response.control_plane_server_ca_pem =
                        response.agent_client_issuer_ca_pem.clone();
                }
                Mutation::ManagementClientCaReusesIssuer => {
                    response.management_client_ca_pem = response.agent_client_issuer_ca_pem.clone();
                }
                Mutation::ManagementClientCaReusesServerCa => {
                    response.management_client_ca_pem =
                        response.control_plane_server_ca_pem.clone();
                }
                Mutation::NonEd25519Capability => {
                    let key = KeyPair::generate().unwrap();
                    response.capability_jwt_public_key_pem = key.public_key_pem();
                    response.capability_jwt_kid = super::sha256_hex(&key.public_key_der());
                }
                Mutation::WrongCapabilityKid => {
                    response.capability_jwt_kid = "00".repeat(32);
                }
                Mutation::ManagementMetadataMismatch => {
                    response.management_serial_number = "01".to_string();
                }
            }

            assert!(
                store
                    .commit_enrollment_response(&lock, &pending, &response, now)
                    .is_err()
            );
            assert!(!identity_root.join("current").exists());
            assert!(identity_root.join("pending-enrollment").is_dir());
        }
    }

    #[test]
    fn complete_enrollment_failpoints_never_publish_a_partial_generation() {
        for failpoint in [
            InstallFailpoint::AfterPrivateKey,
            InstallFailpoint::AfterCertificate,
            InstallFailpoint::BeforeCurrentPointer,
        ] {
            let parent = tempdir().unwrap();
            let identity_root = parent.path().join("identity");
            let store = AgentIdentityStore::new(&identity_root);
            let node_id = Uuid::now_v7();
            let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
            let lock = store.acquire_enrollment_lock().unwrap();
            let EnrollmentPreparation::Pending(pending) =
                store.prepare_enrollment(&lock, node_id, now).unwrap()
            else {
                panic!("unexpected recovered identity")
            };
            let response = complete_enrollment_response(&pending, now);
            let validated =
                super::validate_complete_enrollment_response(&pending, &response, now).unwrap();

            assert!(
                store
                    .install_complete_enrollment_with_failpoint(
                        &pending, &response, &validated, now, failpoint,
                    )
                    .is_err()
            );
            assert!(!identity_root.join("current").exists());
            assert!(identity_root.join("pending-enrollment").is_dir());
            assert!(
                !identity_root
                    .join("generations")
                    .join(pending.generation_id.to_string())
                    .exists()
            );
            assert!(fs::read_dir(&identity_root).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".staging-")
            }));
        }
    }

    #[test]
    fn complete_orphan_generation_is_recovered_without_another_enrollment_request() {
        let parent = tempdir().unwrap();
        let identity_root = parent.path().join("identity");
        let store = AgentIdentityStore::new(&identity_root);
        let node_id = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let lock = store.acquire_enrollment_lock().unwrap();
        let EnrollmentPreparation::Pending(pending) =
            store.prepare_enrollment(&lock, node_id, now).unwrap()
        else {
            panic!("unexpected recovered identity")
        };
        let response = complete_enrollment_response(&pending, now);
        let validated =
            super::validate_complete_enrollment_response(&pending, &response, now).unwrap();
        store
            .install_complete_enrollment(&pending, &response, &validated, now)
            .unwrap();
        fs::remove_file(identity_root.join("current")).unwrap();
        drop(lock);

        let lock = store.acquire_enrollment_lock().unwrap();
        let EnrollmentPreparation::Recovered(recovered) =
            store.prepare_enrollment(&lock, node_id, now).unwrap()
        else {
            panic!("complete orphan generation was not recovered")
        };
        recovered.ensure_production_complete().unwrap();
        assert!(identity_root.join("current").is_file());
        assert!(!identity_root.join("pending-enrollment").exists());
    }

    #[test]
    fn installs_and_loads_an_atomic_generation() {
        let root = tempdir().expect("tempdir");
        let identity_root = root.path().join("identity");
        let store = AgentIdentityStore::new(&identity_root);
        let node_id = Uuid::now_v7();
        let pending = AgentIdentityStore::prepare(node_id).expect("prepare identity");
        let key = KeyPair::from_pem(pending.private_key_pem_for_test()).expect("parse key");
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let (certificate, ca) = issue_identity(
            node_id,
            &key,
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap(),
            time::OffsetDateTime::from_unix_timestamp(
                (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
            )
            .unwrap(),
        );

        let installed = store
            .install(&pending, &certificate, &ca, now)
            .expect("install identity");
        let loaded = store.load_current(now).expect("load identity");
        assert_eq!(loaded.node_id(), node_id);
        assert_eq!(loaded.fingerprint_sha256(), installed.fingerprint_sha256());
        assert!(!format!("{loaded:?}").contains("PRIVATE KEY"));
        assert!(identity_root.join("current").is_file());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&identity_root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(loaded.private_key_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn failed_install_does_not_replace_current_generation() {
        let root = tempdir().expect("tempdir");
        let store = AgentIdentityStore::new(root.path().join("identity"));
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();

        let first_id = Uuid::now_v7();
        let first = AgentIdentityStore::prepare(first_id).unwrap();
        let first_key = KeyPair::from_pem(first.private_key_pem_for_test()).unwrap();
        let (first_cert, first_ca) = issue_identity(
            first_id,
            &first_key,
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap(),
            time::OffsetDateTime::from_unix_timestamp(
                (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
            )
            .unwrap(),
        );
        let original = store.install(&first, &first_cert, &first_ca, now).unwrap();

        let second = AgentIdentityStore::prepare(first_id).unwrap();
        let second_key = KeyPair::from_pem(second.private_key_pem_for_test()).unwrap();
        let (second_cert, second_ca) = issue_identity(
            first_id,
            &second_key,
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap(),
            time::OffsetDateTime::from_unix_timestamp(
                (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
            )
            .unwrap(),
        );
        assert!(
            store
                .install_with_failpoint(
                    &second,
                    &second_cert,
                    &second_ca,
                    now,
                    InstallFailpoint::BeforeCurrentPointer,
                )
                .is_err()
        );
        let loaded = store.load_current(now).unwrap();
        assert_eq!(loaded.fingerprint_sha256(), original.fingerprint_sha256());
    }

    #[test]
    fn first_install_failpoints_leave_no_published_or_partial_identity() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        for failpoint in [
            InstallFailpoint::AfterPrivateKey,
            InstallFailpoint::AfterCertificate,
            InstallFailpoint::BeforeCurrentPointer,
        ] {
            let parent = tempdir().unwrap();
            let identity_root = parent.path().join("identity");
            let store = AgentIdentityStore::new(&identity_root);
            let node_id = Uuid::now_v7();
            let pending = AgentIdentityStore::prepare(node_id).unwrap();
            let key = KeyPair::from_pem(pending.private_key_pem_for_test()).unwrap();
            let (certificate, ca) = issue_identity(
                node_id,
                &key,
                time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                    .unwrap(),
                time::OffsetDateTime::from_unix_timestamp(
                    (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
                )
                .unwrap(),
            );
            assert!(
                store
                    .install_with_failpoint(&pending, &certificate, &ca, now, failpoint)
                    .is_err()
            );
            assert!(matches!(
                store.load_current(now),
                Err(AgentIdentityLoadError::NotEnrolled)
            ));
            assert!(
                !identity_root
                    .join("generations")
                    .join(pending.generation_id.to_string())
                    .exists()
            );
            assert!(fs::read_dir(&identity_root).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".staging-")
            }));
        }
    }

    #[test]
    fn rejects_wrong_key_and_wrong_spiffe_identity() {
        let root = tempdir().unwrap();
        let identity_root = root.path().join("identity");
        let store = AgentIdentityStore::new(&identity_root);
        let node_id = Uuid::now_v7();
        let pending = AgentIdentityStore::prepare(node_id).unwrap();
        let unrelated_key = KeyPair::generate().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let (wrong_key_cert, ca) = issue_identity(
            node_id,
            &unrelated_key,
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap(),
            time::OffsetDateTime::from_unix_timestamp(
                (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
            )
            .unwrap(),
        );
        assert!(store.install(&pending, &wrong_key_cert, &ca, now).is_err());

        let pending_key = KeyPair::from_pem(pending.private_key_pem_for_test()).unwrap();
        let (wrong_node_cert, ca) = issue_identity(
            Uuid::now_v7(),
            &pending_key,
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap(),
            time::OffsetDateTime::from_unix_timestamp(
                (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
            )
            .unwrap(),
        );
        assert!(store.install(&pending, &wrong_node_cert, &ca, now).is_err());
        assert!(!identity_root.join("current").exists());
    }

    #[test]
    fn rejects_hostile_ca_eku_key_usage_and_issuer_profiles() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        for profile in [
            HostileCertificateProfile::ExpiredCa,
            HostileCertificateProfile::NonSelfSignedCa,
            HostileCertificateProfile::MismatchedIssuer,
            HostileCertificateProfile::ExtraEku,
            HostileCertificateProfile::CertificateSigningKeyUsage,
        ] {
            let parent = tempdir().unwrap();
            let identity_root = parent.path().join("identity");
            let store = AgentIdentityStore::new(&identity_root);
            let node_id = Uuid::now_v7();
            let pending = AgentIdentityStore::prepare(node_id).unwrap();
            let key = KeyPair::from_pem(pending.private_key_pem_for_test()).unwrap();
            let (certificate, ca) = issue_hostile_identity(
                node_id,
                &key,
                time::OffsetDateTime::from_unix_timestamp(now.timestamp()).unwrap(),
                profile,
            );
            assert!(store.install(&pending, &certificate, &ca, now).is_err());
            assert!(!identity_root.join("current").exists());
        }
    }

    #[test]
    fn exact_der_parser_rejects_trailing_data() {
        let pending = AgentIdentityStore::prepare(Uuid::now_v7()).unwrap();
        let key = KeyPair::from_pem(pending.private_key_pem_for_test()).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let (certificate, _) = issue_identity(
            pending.node_id(),
            &key,
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap(),
            time::OffsetDateTime::from_unix_timestamp(
                (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
            )
            .unwrap(),
        );
        let (_, pem) = x509_parser::pem::parse_x509_pem(certificate.as_bytes()).unwrap();
        let mut der = pem.contents;
        der.push(0);
        assert!(super::parse_exact_certificate_der(&der, "test certificate").is_err());
    }

    #[test]
    fn pem_parser_rejects_prefix_junk_and_mismatched_end_labels() {
        let pending = AgentIdentityStore::prepare(Uuid::now_v7()).unwrap();
        let key = KeyPair::from_pem(pending.private_key_pem_for_test()).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let (certificate, _) = issue_identity(
            pending.node_id(),
            &key,
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap(),
            time::OffsetDateTime::from_unix_timestamp(
                (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
            )
            .unwrap(),
        );
        assert!(
            super::parse_single_certificate_pem(
                &format!("unexpected-prefix\n{certificate}"),
                "test certificate"
            )
            .is_err()
        );
        let mismatched_end =
            certificate.replace("-----END CERTIFICATE-----", "-----END PUBLIC KEY-----");
        assert!(super::parse_single_certificate_pem(&mismatched_end, "test certificate").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn identity_store_rejects_a_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir(outside.path().join("identity")).unwrap();
        symlink(outside.path(), parent.path().join("redirect")).unwrap();
        let store = AgentIdentityStore::new(parent.path().join("redirect/identity"));
        let node_id = Uuid::now_v7();
        let pending = AgentIdentityStore::prepare(node_id).unwrap();
        let key = KeyPair::from_pem(pending.private_key_pem_for_test()).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let (certificate, ca) = issue_identity(
            node_id,
            &key,
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap(),
            time::OffsetDateTime::from_unix_timestamp(
                (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
            )
            .unwrap(),
        );

        assert!(store.install(&pending, &certificate, &ca, now).is_err());
        assert!(!outside.path().join("identity/current").exists());
    }

    #[cfg(unix)]
    #[test]
    fn load_rejects_a_private_key_with_broadened_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let store = AgentIdentityStore::new(root.path().join("identity"));
        let node_id = Uuid::now_v7();
        let pending = AgentIdentityStore::prepare(node_id).unwrap();
        let key = KeyPair::from_pem(pending.private_key_pem_for_test()).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let (certificate, ca) = issue_identity(
            node_id,
            &key,
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap(),
            time::OffsetDateTime::from_unix_timestamp(
                (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
            )
            .unwrap(),
        );
        let installed = store.install(&pending, &certificate, &ca, now).unwrap();
        fs::set_permissions(
            installed.private_key_path(),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        assert!(store.load_current(now).is_err());
    }

    #[test]
    fn load_rejects_unexpected_generation_entries() {
        let parent = tempdir().unwrap();
        let identity_root = parent.path().join("identity");
        let store = AgentIdentityStore::new(&identity_root);
        let node_id = Uuid::now_v7();
        let pending = AgentIdentityStore::prepare(node_id).unwrap();
        let key = KeyPair::from_pem(pending.private_key_pem_for_test()).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let (certificate, ca) = issue_identity(
            node_id,
            &key,
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap(),
            time::OffsetDateTime::from_unix_timestamp(
                (now + Duration::days(90) - Duration::minutes(1)).timestamp(),
            )
            .unwrap(),
        );
        let installed = store.install(&pending, &certificate, &ca, now).unwrap();
        super::write_new_secure_file(&installed.generation_dir.join("unexpected"), b"surprise")
            .unwrap();

        assert!(store.load_current(now).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn explicit_server_ca_rejects_a_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let ca_path = outside.path().join("core-ca.pem");
        fs::write(&ca_path, b"untrusted-through-symlink").unwrap();
        symlink(outside.path(), parent.path().join("redirect")).unwrap();

        assert!(
            super::read_regular_file_limited(
                &parent.path().join("redirect/core-ca.pem"),
                super::MAX_IDENTITY_FILE_BYTES,
            )
            .is_err()
        );
    }

    #[test]
    fn rotation_threshold_is_inclusive_at_thirty_days() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        assert!(!rotation_due(
            now + Duration::days(30) + Duration::seconds(1),
            now
        ));
        assert!(rotation_due(now + Duration::days(30), now));
        assert!(rotation_due(now + Duration::days(1), now));
    }

    #[test]
    fn authenticated_session_persists_one_rotation_request_at_the_thirty_day_boundary() {
        let parent = tempdir().unwrap();
        let identity_root = parent.path().join("identity");
        let store = AgentIdentityStore::new(&identity_root);
        let node_id = Uuid::now_v7();
        let enrolled_at = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let lock = store.acquire_enrollment_lock().unwrap();
        let EnrollmentPreparation::Pending(pending) = store
            .prepare_enrollment(&lock, node_id, enrolled_at)
            .unwrap()
        else {
            panic!("unexpected recovered identity")
        };
        let response = complete_enrollment_response(&pending, enrolled_at);
        let installed = store
            .commit_enrollment_response(&lock, &pending, &response, enrolled_at)
            .unwrap();
        let source_generation_id = installed.metadata.generation_id;
        let old_control_key = KeyPair::from_pem(installed.private_key_pem.as_str())
            .unwrap()
            .public_key_der();
        let old_management_key = KeyPair::from_pem(
            installed
                .management_private_key_pem
                .as_deref()
                .expect("complete management key"),
        )
        .unwrap()
        .public_key_der();
        drop(lock);

        let rotation_time = installed.not_after() - Duration::days(30);
        let first = store
            .on_authenticated_session(source_generation_id, rotation_time)
            .unwrap();
        let second = store
            .on_authenticated_session(source_generation_id, rotation_time + Duration::seconds(1))
            .unwrap();
        let super::AuthenticatedRotationAction::SendRequest(first) = first else {
            panic!("rotation request was not generated at the inclusive boundary")
        };
        let super::AuthenticatedRotationAction::SendRequest(second) = second else {
            panic!("persisted rotation request was not replayed")
        };
        assert_eq!(first, second);

        let control = CertificateSigningRequestParams::from_pem(first.control_csr_pem()).unwrap();
        let management =
            CertificateSigningRequestParams::from_pem(first.management_csr_pem()).unwrap();
        assert_ne!(
            control.public_key.der_bytes(),
            management.public_key.der_bytes()
        );
        assert_ne!(control.public_key.der_bytes(), old_control_key.as_slice());
        assert_ne!(
            control.public_key.der_bytes(),
            old_management_key.as_slice()
        );
        assert_ne!(
            management.public_key.der_bytes(),
            old_control_key.as_slice()
        );
        assert_ne!(
            management.public_key.der_bytes(),
            old_management_key.as_slice()
        );

        let entries = fs::read_dir(identity_root.join("pending-rotation"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            entries,
            std::collections::BTreeSet::from([
                "control-client-key.pem".to_string(),
                "control-client.csr.pem".to_string(),
                "management-server-key.pem".to_string(),
                "management-server.csr.pem".to_string(),
                "metadata.json".to_string(),
            ])
        );
    }

    #[test]
    fn rotation_bundle_is_strict_and_switches_only_after_complete_validation() {
        let fixture = prepared_rotation();

        let mut wrong_rotation = fixture.bundle.clone();
        wrong_rotation.rotation_id = Uuid::now_v7().to_string();
        assert!(
            fixture
                .store
                .commit_rotation_bundle(fixture.source_generation_id, &wrong_rotation, fixture.now,)
                .is_err()
        );
        assert_eq!(
            fixture
                .store
                .load_current(fixture.now)
                .unwrap()
                .metadata
                .generation_id,
            fixture.source_generation_id
        );

        let mut expired = fixture.bundle.clone();
        expired.expires_at_ms = (fixture.now - Duration::seconds(1)).timestamp_millis();
        assert!(
            fixture
                .store
                .commit_rotation_bundle(fixture.source_generation_id, &expired, fixture.now)
                .is_err()
        );
        let mut excessive_deadline = fixture.bundle.clone();
        excessive_deadline.expires_at_ms =
            (fixture.now + Duration::minutes(5) + Duration::milliseconds(1)).timestamp_millis();
        assert!(
            fixture
                .store
                .commit_rotation_bundle(
                    fixture.source_generation_id,
                    &excessive_deadline,
                    fixture.now,
                )
                .is_err()
        );

        let mut changed_trust = fixture.bundle.clone();
        changed_trust.control_plane_server_ca_pem = changed_trust.management_client_ca_pem.clone();
        assert!(
            fixture
                .store
                .commit_rotation_bundle(fixture.source_generation_id, &changed_trust, fixture.now,)
                .is_err()
        );
        let mut wrong_key = fixture.bundle.clone();
        wrong_key.control_certificate_pem = wrong_key.management_certificate_pem.clone();
        assert!(
            fixture
                .store
                .commit_rotation_bundle(fixture.source_generation_id, &wrong_key, fixture.now,)
                .is_err()
        );

        assert_eq!(
            fixture
                .store
                .commit_rotation_bundle(fixture.source_generation_id, &fixture.bundle, fixture.now,)
                .unwrap(),
            super::RotationCommitOutcome::RestartRequired
        );
        let installed = fixture.store.load_current(fixture.now).unwrap();
        assert_eq!(installed.metadata.generation_id, fixture.rotation_id);
        installed.ensure_production_complete().unwrap();
        assert!(
            fixture
                .store
                .root
                .join("generations")
                .join(fixture.source_generation_id.to_string())
                .is_dir()
        );
        assert!(
            fixture
                .store
                .commit_rotation_bundle(fixture.source_generation_id, &fixture.bundle, fixture.now,)
                .is_err(),
            "a committed rotation bundle must not be replayed"
        );
    }

    #[test]
    fn rotation_commit_crash_points_recover_to_one_safe_generation() {
        for (failpoint, expected_generation) in [
            (
                super::RotationInstallFailpoint::AfterGenerationPublished,
                "source",
            ),
            (
                super::RotationInstallFailpoint::AfterSwitchAuthorized,
                "target",
            ),
            (
                super::RotationInstallFailpoint::AfterCurrentPointer,
                "target",
            ),
        ] {
            let fixture = prepared_rotation();
            assert!(
                fixture
                    .store
                    .commit_rotation_bundle_with_failpoint(
                        fixture.source_generation_id,
                        &fixture.bundle,
                        fixture.now,
                        failpoint,
                    )
                    .is_err()
            );

            let recovered = fixture
                .store
                .load_current_for_startup(fixture.now + Duration::seconds(1))
                .unwrap();
            let expected = if expected_generation == "source" {
                fixture.source_generation_id
            } else {
                fixture.rotation_id
            };
            assert_eq!(recovered.metadata.generation_id, expected);
            assert_eq!(
                fixture
                    .store
                    .load_current(fixture.now + Duration::seconds(1))
                    .unwrap()
                    .metadata
                    .generation_id,
                expected
            );
            if expected_generation == "source" {
                assert!(
                    !fixture
                        .store
                        .root
                        .join("generations")
                        .join(fixture.rotation_id.to_string())
                        .exists()
                );
                let AuthenticatedRotationAction::SendRequest(request) = fixture
                    .store
                    .on_authenticated_session(
                        fixture.source_generation_id,
                        fixture.now + Duration::seconds(2),
                    )
                    .unwrap()
                else {
                    panic!("the persisted rotation request was not recoverable")
                };
                assert_eq!(request.rotation_id(), fixture.rotation_id);
            }
        }
    }

    #[test]
    fn reconnect_after_partial_commit_never_keeps_running_with_stale_in_memory_keys() {
        for failpoint in [
            super::RotationInstallFailpoint::AfterGenerationPublished,
            super::RotationInstallFailpoint::AfterSwitchAuthorized,
            super::RotationInstallFailpoint::AfterCurrentPointer,
        ] {
            let fixture = prepared_rotation();
            assert!(
                fixture
                    .store
                    .commit_rotation_bundle_with_failpoint(
                        fixture.source_generation_id,
                        &fixture.bundle,
                        fixture.now,
                        failpoint,
                    )
                    .is_err()
            );
            let action = fixture
                .store
                .on_authenticated_session(
                    fixture.source_generation_id,
                    fixture.now + Duration::seconds(1),
                )
                .unwrap();
            match failpoint {
                super::RotationInstallFailpoint::AfterGenerationPublished => {
                    let AuthenticatedRotationAction::SendRequest(request) = action else {
                        panic!("orphan generation must replay the persisted request")
                    };
                    assert_eq!(request.rotation_id(), fixture.rotation_id);
                    assert!(
                        !fixture
                            .store
                            .root
                            .join("generations")
                            .join(fixture.rotation_id.to_string())
                            .exists(),
                        "orphan generation must be removed before the bundle is retried"
                    );
                }
                super::RotationInstallFailpoint::AfterSwitchAuthorized
                | super::RotationInstallFailpoint::AfterCurrentPointer => {
                    assert_eq!(action, AuthenticatedRotationAction::RestartRequired);
                }
                super::RotationInstallFailpoint::None => unreachable!(),
            }
        }
    }

    #[test]
    fn expired_unconsumed_rotation_rolls_back_but_ambiguous_rollback_fails_closed() {
        let fixture = prepared_rotation();
        fixture
            .store
            .commit_rotation_bundle(fixture.source_generation_id, &fixture.bundle, fixture.now)
            .unwrap();
        let after_expiry = super::timestamp_millis(fixture.bundle.expires_at_ms, "test expiry")
            .unwrap()
            + Duration::milliseconds(1);
        let recovered = fixture
            .store
            .load_current_for_startup(after_expiry)
            .unwrap();
        assert_eq!(
            recovered.metadata.generation_id,
            fixture.source_generation_id
        );
        assert!(!fixture.store.root.join("pending-rotation").exists());
        assert!(
            !fixture
                .store
                .root
                .join("generations")
                .join(fixture.rotation_id.to_string())
                .exists()
        );

        let ambiguous = prepared_rotation();
        ambiguous
            .store
            .commit_rotation_bundle(
                ambiguous.source_generation_id,
                &ambiguous.bundle,
                ambiguous.now,
            )
            .unwrap();
        super::remove_secure_identity_tree(
            &ambiguous
                .store
                .root
                .join("generations")
                .join(ambiguous.source_generation_id.to_string()),
        )
        .unwrap();
        let after_expiry = super::timestamp_millis(ambiguous.bundle.expires_at_ms, "test expiry")
            .unwrap()
            + Duration::milliseconds(1);
        assert!(
            ambiguous
                .store
                .load_current_for_startup(after_expiry)
                .is_err()
        );
        assert_eq!(
            ambiguous
                .store
                .load_current(ambiguous.now)
                .unwrap()
                .metadata
                .generation_id,
            ambiguous.rotation_id,
            "fail-closed recovery must not switch to a missing previous generation"
        );
    }

    #[test]
    fn activation_requires_the_new_session_and_replays_the_persisted_ack() {
        let fixture = prepared_rotation();
        fixture
            .store
            .commit_rotation_bundle(fixture.source_generation_id, &fixture.bundle, fixture.now)
            .unwrap();
        let activation_time = fixture.now + Duration::seconds(1);
        let command = ActivateCertificateRotation {
            rotation_id: fixture.rotation_id.to_string(),
            previous_identity_expires_at_ms: (activation_time + Duration::minutes(5))
                .timestamp_millis(),
        };
        assert!(
            fixture
                .store
                .activate_rotation(fixture.rotation_id, &command, activation_time)
                .is_err(),
            "activation before a successful new-certificate session must fail"
        );

        let loaded = fixture
            .store
            .load_current_for_startup(activation_time)
            .unwrap();
        assert_eq!(loaded.metadata.generation_id, fixture.rotation_id);
        assert_eq!(
            fixture
                .store
                .on_authenticated_session(fixture.rotation_id, activation_time)
                .unwrap(),
            AuthenticatedRotationAction::None
        );

        let mut wrong = command.clone();
        wrong.rotation_id = Uuid::now_v7().to_string();
        assert!(
            fixture
                .store
                .activate_rotation(fixture.rotation_id, &wrong, activation_time)
                .is_err()
        );
        let mut expired = command.clone();
        expired.previous_identity_expires_at_ms =
            (activation_time - Duration::milliseconds(1)).timestamp_millis();
        assert!(
            fixture
                .store
                .activate_rotation(fixture.rotation_id, &expired, activation_time)
                .is_err()
        );

        let first = fixture
            .store
            .activate_rotation(fixture.rotation_id, &command, activation_time)
            .unwrap();
        assert_eq!(first.rotation_id(), fixture.rotation_id);
        assert_eq!(first.activated_at_ms(), activation_time.timestamp_millis());
        assert!(!first.control_fingerprint_sha256().is_empty());
        assert!(!first.management_fingerprint_sha256().is_empty());
        assert!(!fixture.store.root.join("pending-rotation").exists());
        let audit_path = fixture
            .store
            .root
            .join(format!("rotation-audit-{}.json", fixture.rotation_id));
        let audit = fs::read_to_string(&audit_path).unwrap();
        assert!(!audit.contains("PRIVATE KEY"));
        assert!(!audit.contains("CERTIFICATE"));
        assert!(!audit.contains("csr_pem"));

        let replayed = fixture
            .store
            .activate_rotation(
                fixture.rotation_id,
                &command,
                activation_time + Duration::seconds(30),
            )
            .unwrap();
        assert_eq!(replayed, first, "lost ACK must be replayed exactly");
    }

    #[test]
    fn a_consumed_session_survives_bundle_expiry() {
        let consumed = prepared_rotation();
        consumed
            .store
            .commit_rotation_bundle(
                consumed.source_generation_id,
                &consumed.bundle,
                consumed.now,
            )
            .unwrap();
        let before_expiry = super::timestamp_millis(consumed.bundle.expires_at_ms, "test expiry")
            .unwrap()
            - Duration::milliseconds(1);
        consumed
            .store
            .load_current_for_startup(before_expiry)
            .unwrap();
        assert_eq!(
            consumed
                .store
                .on_authenticated_session(consumed.rotation_id, before_expiry)
                .unwrap(),
            AuthenticatedRotationAction::None
        );
        let after_expiry = before_expiry + Duration::seconds(1);
        assert_eq!(
            consumed
                .store
                .load_current_for_startup(after_expiry)
                .unwrap()
                .generation_id(),
            consumed.rotation_id
        );
    }

    #[test]
    fn core_accepted_new_generation_is_consumed_even_if_response_crosses_bundle_deadline() {
        let fixture = prepared_rotation();
        fixture
            .store
            .commit_rotation_bundle(fixture.source_generation_id, &fixture.bundle, fixture.now)
            .unwrap();
        let bundle_expires_at =
            super::timestamp_millis(fixture.bundle.expires_at_ms, "test expiry").unwrap();
        fixture
            .store
            .load_current_for_startup(bundle_expires_at - Duration::milliseconds(1))
            .unwrap();

        assert_eq!(
            fixture
                .store
                .on_authenticated_session(
                    fixture.rotation_id,
                    bundle_expires_at + Duration::milliseconds(1),
                )
                .unwrap(),
            AuthenticatedRotationAction::None,
            "a successful authenticated StreamConnect proves Core consumed the new generation"
        );
        assert_eq!(
            fixture
                .store
                .load_current(bundle_expires_at + Duration::milliseconds(1))
                .unwrap()
                .generation_id(),
            fixture.rotation_id
        );
        let pending = fixture.store.load_pending_rotation().unwrap();
        assert_eq!(pending.metadata.stage, super::RotationStage::Consumed);
        assert!(pending.metadata.new_session_consumed_at.is_some());
    }

    #[test]
    fn activation_crash_after_audit_replays_ack_and_finishes_secret_cleanup() {
        let fixture = prepared_rotation();
        fixture
            .store
            .commit_rotation_bundle(fixture.source_generation_id, &fixture.bundle, fixture.now)
            .unwrap();
        let activation_time = fixture.now + Duration::seconds(1);
        fixture
            .store
            .load_current_for_startup(activation_time)
            .unwrap();
        fixture
            .store
            .on_authenticated_session(fixture.rotation_id, activation_time)
            .unwrap();
        let command = ActivateCertificateRotation {
            rotation_id: fixture.rotation_id.to_string(),
            previous_identity_expires_at_ms: (activation_time + Duration::minutes(5))
                .timestamp_millis(),
        };

        assert!(
            fixture
                .store
                .activate_rotation_with_failpoint(
                    fixture.rotation_id,
                    &command,
                    activation_time,
                    super::RotationActivationFailpoint::AfterAudit,
                )
                .is_err()
        );
        assert!(fixture.store.root.join("pending-rotation").is_dir());
        assert!(
            fixture
                .store
                .root
                .join(format!("rotation-audit-{}.json", fixture.rotation_id))
                .is_file()
        );

        let replayed = fixture
            .store
            .activate_rotation(
                fixture.rotation_id,
                &command,
                activation_time + Duration::seconds(1),
            )
            .unwrap();
        assert_eq!(
            replayed.activated_at_ms(),
            activation_time.timestamp_millis()
        );
        assert!(!fixture.store.root.join("pending-rotation").exists());
    }

    #[test]
    fn healthy_session_uses_the_earliest_leaf_expiry_and_keeps_one_request_id() {
        let fixture = prepared_rotation();
        let mut loaded = fixture
            .store
            .load_current(fixture.now - Duration::days(1))
            .unwrap();
        loaded.metadata.not_after = fixture.now + Duration::days(31);
        loaded.metadata.management_not_after = Some(fixture.now + Duration::days(30));
        assert_eq!(
            loaded.rotation_not_after().unwrap(),
            fixture.now + Duration::days(30)
        );

        let first = fixture
            .store
            .on_authenticated_session(fixture.source_generation_id, fixture.now)
            .unwrap();
        let second = fixture
            .store
            .on_authenticated_session(
                fixture.source_generation_id,
                fixture.now + Duration::seconds(30),
            )
            .unwrap();
        let AuthenticatedRotationAction::SendRequest(first) = first else {
            panic!("persisted request was not replayed")
        };
        let AuthenticatedRotationAction::SendRequest(second) = second else {
            panic!("persisted request was not replayed")
        };
        assert_eq!(first, second);
    }

    #[test]
    fn requested_rotation_expires_locally_and_reset_is_exact_and_idempotent() {
        let fixture = prepared_rotation();
        assert!(
            fixture
                .store
                .reset_requested_rotation(&fixture.rotation_id.simple().to_string())
                .is_err()
        );
        let action = fixture
            .store
            .on_authenticated_session(
                fixture.source_generation_id,
                fixture.now + Duration::minutes(5),
            )
            .unwrap();
        let AuthenticatedRotationAction::SendRequest(replacement) = action else {
            panic!("expired request did not converge to a replacement")
        };
        assert_ne!(replacement.rotation_id(), fixture.rotation_id);

        fixture
            .store
            .reset_requested_rotation(&fixture.rotation_id.to_string())
            .unwrap();
        assert_eq!(
            fixture
                .store
                .load_pending_rotation()
                .unwrap()
                .metadata
                .rotation_id,
            replacement.rotation_id(),
            "an idempotent reset for the retired ID must not clear its replacement"
        );
        fixture
            .store
            .reset_requested_rotation(&replacement.rotation_id().to_string())
            .unwrap();
        assert!(!fixture.store.root.join("pending-rotation").exists());
        fixture
            .store
            .reset_requested_rotation(&replacement.rotation_id().to_string())
            .unwrap();
    }

    #[test]
    fn rotation_reset_refuses_an_authorized_or_consumed_generation() {
        let fixture = prepared_rotation();
        fixture
            .store
            .commit_rotation_bundle(fixture.source_generation_id, &fixture.bundle, fixture.now)
            .unwrap();
        assert!(
            fixture
                .store
                .reset_requested_rotation(&fixture.rotation_id.to_string())
                .is_err()
        );
        assert_eq!(
            fixture
                .store
                .load_current(fixture.now)
                .unwrap()
                .generation_id(),
            fixture.rotation_id
        );
    }

    #[test]
    fn authenticated_session_finishes_a_reset_that_crashed_after_the_audit() {
        let fixture = prepared_rotation();
        fixture
            .store
            .write_rotation_reset_audit(&super::RotationResetAudit {
                version: super::ROTATION_RESET_AUDIT_VERSION,
                rotation_id: fixture.rotation_id,
                node_id: fixture.store.load_current(fixture.now).unwrap().node_id(),
                source_generation_id: fixture.source_generation_id,
                reset_at: fixture.now,
            })
            .unwrap();

        let action = fixture
            .store
            .on_authenticated_session(
                fixture.source_generation_id,
                fixture.now + Duration::seconds(1),
            )
            .unwrap();
        let AuthenticatedRotationAction::SendRequest(replacement) = action else {
            panic!("crashed reset did not converge to a replacement request")
        };
        assert_ne!(replacement.rotation_id(), fixture.rotation_id);
    }

    #[test]
    fn activated_ack_replays_and_old_generation_is_retired_after_overlap() {
        let fixture = prepared_rotation();
        fixture
            .store
            .commit_rotation_bundle(fixture.source_generation_id, &fixture.bundle, fixture.now)
            .unwrap();
        let connected_at = fixture.now + Duration::seconds(1);
        fixture
            .store
            .load_current_for_startup(connected_at)
            .unwrap();
        fixture
            .store
            .on_authenticated_session(fixture.rotation_id, connected_at)
            .unwrap();
        let overlap_ends = connected_at + Duration::minutes(4);
        let command = ActivateCertificateRotation {
            rotation_id: fixture.rotation_id.to_string(),
            previous_identity_expires_at_ms: overlap_ends.timestamp_millis(),
        };
        let activated = fixture
            .store
            .activate_rotation(fixture.rotation_id, &command, connected_at)
            .unwrap();
        let restarted_store = AgentIdentityStore::new(fixture.store.root.clone());
        assert_eq!(
            restarted_store
                .load_current_for_startup(connected_at + Duration::seconds(1))
                .unwrap()
                .generation_id(),
            fixture.rotation_id
        );
        assert_eq!(
            restarted_store
                .replayable_activation_ack(fixture.rotation_id)
                .unwrap(),
            Some(activated),
            "a restarted Agent must replay the exact durable activation acknowledgement"
        );

        let previous = fixture
            .store
            .root
            .join("generations")
            .join(fixture.source_generation_id.to_string());
        assert!(previous.is_dir());
        restarted_store
            .on_authenticated_session(fixture.rotation_id, overlap_ends)
            .unwrap();
        assert!(!previous.exists());
        assert_eq!(
            fixture
                .store
                .load_current(overlap_ends)
                .unwrap()
                .generation_id(),
            fixture.rotation_id
        );
    }

    #[test]
    fn startup_finishes_crashed_previous_generation_retirement_without_touching_current() {
        let fixture = prepared_rotation();
        fixture
            .store
            .commit_rotation_bundle(fixture.source_generation_id, &fixture.bundle, fixture.now)
            .unwrap();
        let connected_at = fixture.now + Duration::seconds(1);
        fixture
            .store
            .load_current_for_startup(connected_at)
            .unwrap();
        fixture
            .store
            .on_authenticated_session(fixture.rotation_id, connected_at)
            .unwrap();
        let overlap_ends = connected_at + Duration::minutes(4);
        fixture
            .store
            .activate_rotation(
                fixture.rotation_id,
                &ActivateCertificateRotation {
                    rotation_id: fixture.rotation_id.to_string(),
                    previous_identity_expires_at_ms: overlap_ends.timestamp_millis(),
                },
                connected_at,
            )
            .unwrap();

        assert!(
            fixture
                .store
                .retire_previous_generation_with_failpoint(
                    fixture.rotation_id,
                    overlap_ends,
                    super::GenerationRetirementFailpoint::AfterRename,
                )
                .is_err()
        );
        assert!(
            fixture
                .store
                .root
                .join(format!(
                    ".retired-generation-{}",
                    fixture.source_generation_id
                ))
                .is_dir()
        );

        let recovered = fixture
            .store
            .load_current_for_startup(overlap_ends + Duration::seconds(1))
            .unwrap();
        assert_eq!(recovered.generation_id(), fixture.rotation_id);
        assert!(
            fixture
                .store
                .root
                .join("generations")
                .join(fixture.rotation_id.to_string())
                .is_dir()
        );
        assert!(
            !fixture
                .store
                .root
                .join(format!(
                    ".retired-generation-{}",
                    fixture.source_generation_id
                ))
                .exists()
        );
    }

    #[test]
    fn startup_removes_rotation_generation_orphaned_after_rollback_intent_lost_pending_state() {
        let fixture = prepared_rotation();
        fixture
            .store
            .commit_rotation_bundle(fixture.source_generation_id, &fixture.bundle, fixture.now)
            .unwrap();
        let pending = fixture.store.load_pending_rotation().unwrap();
        let target = fixture
            .store
            .load_generation(fixture.rotation_id, fixture.now)
            .unwrap();
        fixture
            .store
            .write_current_pointer(fixture.source_generation_id)
            .unwrap();
        fixture
            .store
            .write_rotation_audit(&super::RotationAudit {
                version: super::ROTATION_AUDIT_VERSION,
                status: super::RotationAuditStatus::RolledBack,
                rotation_id: fixture.rotation_id,
                node_id: pending.metadata.node_id,
                source_generation_id: fixture.source_generation_id,
                target_generation_id: fixture.rotation_id,
                created_at: pending.metadata.created_at,
                bundle_expires_at: pending.metadata.bundle_expires_at.unwrap(),
                new_session_consumed_at: None,
                activated_at: None,
                previous_identity_expires_at: None,
                control_fingerprint_sha256: target.metadata.fingerprint_sha256.clone(),
                management_fingerprint_sha256: target
                    .metadata
                    .management_fingerprint_sha256
                    .clone()
                    .unwrap(),
            })
            .unwrap();
        fixture.store.clear_pending_rotation(&pending).unwrap();
        let orphan = fixture
            .store
            .root
            .join("generations")
            .join(fixture.rotation_id.to_string());
        assert!(
            orphan.is_dir(),
            "test must reproduce the rollback crash gap"
        );

        let restarted = AgentIdentityStore::new(fixture.store.root.clone());
        assert_eq!(
            restarted
                .load_current_for_startup(fixture.now + Duration::minutes(6))
                .unwrap()
                .generation_id(),
            fixture.source_generation_id
        );
        assert!(
            !orphan.exists(),
            "startup retained an orphaned rotated private-key generation"
        );
    }

    #[test]
    fn startup_removes_reset_generation_orphaned_after_reset_intent_lost_pending_state() {
        let fixture = prepared_rotation();
        assert!(
            fixture
                .store
                .commit_rotation_bundle_with_failpoint(
                    fixture.source_generation_id,
                    &fixture.bundle,
                    fixture.now,
                    super::RotationInstallFailpoint::AfterGenerationPublished,
                )
                .is_err()
        );
        let pending = fixture.store.load_pending_rotation().unwrap();
        fixture
            .store
            .write_rotation_reset_audit(&super::RotationResetAudit {
                version: super::ROTATION_RESET_AUDIT_VERSION,
                rotation_id: fixture.rotation_id,
                node_id: pending.metadata.node_id,
                source_generation_id: fixture.source_generation_id,
                reset_at: fixture.now,
            })
            .unwrap();
        fixture.store.clear_pending_rotation(&pending).unwrap();
        let orphan = fixture
            .store
            .root
            .join("generations")
            .join(fixture.rotation_id.to_string());
        assert!(orphan.is_dir(), "test must reproduce the reset crash gap");

        let restarted = AgentIdentityStore::new(fixture.store.root.clone());
        assert_eq!(
            restarted
                .load_current_for_startup(fixture.now + Duration::seconds(1))
                .unwrap()
                .generation_id(),
            fixture.source_generation_id
        );
        assert!(
            !orphan.exists(),
            "startup retained an orphaned reset private-key generation"
        );
    }

    #[test]
    fn readonly_identity_check_reuses_full_validation_without_writes() {
        let fixture = prepared_rotation();
        let before = snapshot_tree(&fixture.store.root);
        let checked = fixture
            .store
            .check_current_read_only(
                fixture.store.load_current(fixture.now).unwrap().node_id(),
                fixture.now,
            )
            .unwrap();
        assert_eq!(checked.generation_id(), fixture.source_generation_id);
        assert_eq!(snapshot_tree(&fixture.store.root), before);
    }

    fn snapshot_tree(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        fn visit(
            root: &std::path::Path,
            path: &std::path::Path,
            output: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
        ) {
            let mut entries = fs::read_dir(path)
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    visit(root, &path, output);
                } else {
                    output.push((
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        fs::read(path).unwrap(),
                    ));
                }
            }
        }
        let mut output = Vec::new();
        visit(root, root, &mut output);
        output
    }

    #[test]
    fn root_ca_policy_rejects_extra_usage_and_reused_spki() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let ca_key = KeyPair::generate().unwrap();
        let mut extra_usage = CertificateParams::default();
        extra_usage.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        extra_usage.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let certificate = extra_usage.self_signed(&ca_key).unwrap();
        let (_, parsed) = x509_parser::parse_x509_certificate(certificate.der()).unwrap();
        assert!(super::validate_root_ca_certificate(&parsed, now, "test CA").is_err());

        let mut root_with_eku = CertificateParams::default();
        root_with_eku.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        root_with_eku.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        root_with_eku.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = root_with_eku.self_signed(&ca_key).unwrap();
        let (_, parsed) = x509_parser::parse_x509_certificate(certificate.der()).unwrap();
        assert!(super::validate_root_ca_certificate(&parsed, now, "test CA").is_err());

        let root = tempdir().unwrap();
        let store = AgentIdentityStore::new(root.path().join("identity"));
        let node_id = Uuid::now_v7();
        let lock = store.acquire_enrollment_lock().unwrap();
        let EnrollmentPreparation::Pending(pending) =
            store.prepare_enrollment(&lock, node_id, now).unwrap()
        else {
            panic!("unexpected recovered identity")
        };
        let mut authority = rotation_authority();
        let mut reused = CertificateParams::default();
        reused.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        reused.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        reused.distinguished_name = DistinguishedName::new();
        reused
            .distinguished_name
            .push(DnType::CommonName, "different CA certificate, reused SPKI");
        authority.control_plane_server_ca_pem =
            reused.self_signed(&authority.agent_ca_key).unwrap().pem();
        let response =
            response_with_rotation_authority(&pending, now, &[0x61; 16], &[0x62; 16], &authority);
        assert!(super::validate_complete_enrollment_response(&pending, &response, now).is_err());
    }

    #[test]
    fn leaf_certificate_validity_is_capped_at_exactly_ninety_days() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let pending = AgentIdentityStore::prepare(Uuid::now_v7()).unwrap();
        let key = KeyPair::from_pem(pending.private_key_pem_for_test()).unwrap();
        let not_before =
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap();
        let exact_not_after = not_before + time::Duration::days(90);
        let (exact, exact_ca) =
            issue_identity(pending.node_id(), &key, not_before, exact_not_after);
        super::validate_issued_identity(&pending, &exact, &exact_ca, now).unwrap();

        let (too_long, too_long_ca) = issue_identity(
            pending.node_id(),
            &key,
            not_before,
            exact_not_after + time::Duration::seconds(1),
        );
        assert!(super::validate_issued_identity(&pending, &too_long, &too_long_ca, now).is_err());
    }

    #[test]
    fn identity_check_cli_requires_canonical_node_and_absolute_identity_path() {
        let node_id = Uuid::now_v7();
        let parsed = super::parse_identity_check_args([
            "--node-id".to_string(),
            node_id.to_string(),
            "--identity-dir".to_string(),
            "/var/lib/streamserver/agent/identity".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed.node_id, node_id);
        assert!(parsed.identity_dir.is_absolute());
        assert!(
            super::parse_identity_check_args([
                "--node-id".to_string(),
                node_id.to_string().to_uppercase(),
                "--identity-dir".to_string(),
                "/var/lib/streamserver/agent/identity".to_string(),
            ])
            .is_err()
        );
        assert!(
            super::parse_identity_check_args([
                "--node-id".to_string(),
                node_id.to_string(),
                "--identity-dir".to_string(),
                "relative/identity".to_string(),
            ])
            .is_err()
        );
    }

    #[test]
    fn enrollment_cli_requires_https_and_token_stdin() {
        let node_id = Uuid::now_v7();
        let parsed = parse_enroll_args([
            "--node-id".to_string(),
            node_id.to_string(),
            "--core-url".to_string(),
            "https://core.example.test:8443".to_string(),
            "--server-ca".to_string(),
            "/etc/streamserver/core-http-ca.pem".to_string(),
            "--token-stdin".to_string(),
            "--identity-dir".to_string(),
            "/var/lib/streamserver/agent/identity".to_string(),
        ])
        .expect("parse enrollment CLI");
        assert_eq!(parsed.node_id, node_id);
        assert_eq!(parsed.core_url.scheme(), "https");

        assert!(
            parse_enroll_args([
                "--node-id".to_string(),
                node_id.to_string(),
                "--core-url".to_string(),
                "http://core.example.test:8080".to_string(),
                "--token-stdin".to_string(),
                "--identity-dir".to_string(),
                "/tmp/identity".to_string(),
            ])
            .is_err()
        );
        assert!(
            parse_enroll_args([
                "--node-id".to_string(),
                node_id.to_string(),
                "--core-url".to_string(),
                "https://core.example.test".to_string(),
                "--identity-dir".to_string(),
                "/tmp/identity".to_string(),
            ])
            .is_err()
        );
        assert!(
            parse_enroll_args([
                "--node-id".to_string(),
                node_id.to_string(),
                "--core-url".to_string(),
                "https://core.example.test".to_string(),
                "--token".to_string(),
                "secret-must-not-be-an-argument".to_string(),
                "--identity-dir".to_string(),
                "/tmp/identity".to_string(),
            ])
            .is_err()
        );

        let secret = "must-never-appear-in-an-error";
        let error = parse_enroll_args([format!("--token={secret}")]).unwrap_err();
        assert!(!error.to_string().contains(secret));
        let error = parse_enroll_args([format!("--token-stdin={secret}")]).unwrap_err();
        assert!(!error.to_string().contains(secret));
    }

    #[test]
    fn enrollment_stdin_accepts_only_versioned_fixed_shape_tokens() {
        let valid = format!("ssae1.{}.{}", "A".repeat(96), "b".repeat(43));
        assert_eq!(valid.len(), 146);
        assert!(super::is_valid_enrollment_token_wire(valid.as_bytes()));
        assert!(!super::is_valid_enrollment_token_wire(
            "A".repeat(43).as_bytes()
        ));
        assert!(!super::is_valid_enrollment_token_wire(
            format!("{valid}A").as_bytes()
        ));
        assert!(!super::is_valid_enrollment_token_wire(
            valid.replacen('.', "_", 1).as_bytes()
        ));
        assert!(!super::is_valid_enrollment_token_wire(
            valid.replacen('b', "=", 1).as_bytes()
        ));
    }

    #[test]
    fn enrollment_authorization_header_is_sensitive() {
        let token = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
        let header = super::enrollment_authorization_header(token).unwrap();
        assert!(header.is_sensitive());
        assert!(!format!("{header:?}").contains(token));
    }

    #[tokio::test]
    async fn enrollment_client_does_not_follow_redirects() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
            time::{Duration as TokioDuration, timeout},
        };

        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let redirect = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect_addr = redirect.local_addr().unwrap();
        let redirect_task = tokio::spawn(async move {
            let (mut stream, _) = redirect.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target_addr}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });

        let client = super::enrollment_client_builder(false, true)
            .build()
            .unwrap();
        let response = client
            .post(format!("http://{redirect_addr}/api/v1/agent-enroll"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        redirect_task.await.unwrap();
        assert!(
            timeout(TokioDuration::from_millis(150), target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    #[ignore = "requires outbound TLS for the explicit-root trust gate"]
    async fn explicit_ca_mode_excludes_public_webpki_roots() {
        let public_roots_client = super::enrollment_client_builder(true, true)
            .build()
            .unwrap();
        let public_response = public_roots_client
            .get("https://example.com/")
            .send()
            .await
            .expect("VM public WebPKI control request");
        assert!(public_response.status().is_success());

        let explicit_roots_only = super::enrollment_client_builder(true, false)
            .build()
            .unwrap();
        assert!(
            explicit_roots_only
                .get("https://example.com/")
                .send()
                .await
                .is_err(),
            "explicit CA mode unexpectedly retained public WebPKI roots"
        );
    }

    #[tokio::test]
    async fn enrollment_response_is_bounded_with_or_without_content_length() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        async fn serve_once(response: &'static [u8]) -> std::net::SocketAddr {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request).await.unwrap();
                stream.write_all(response).await.unwrap();
            });
            address
        }

        let client = super::enrollment_client_builder(false, true)
            .build()
            .unwrap();
        let declared = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\n123456789",
        )
        .await;
        let response = client
            .get(format!("http://{declared}/"))
            .send()
            .await
            .unwrap();
        assert!(super::read_bounded_response(response, 8).await.is_err());

        let chunked = serve_once(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\n12345\r\n5\r\n67890\r\n0\r\n\r\n",
        )
        .await;
        let response = client
            .get(format!("http://{chunked}/"))
            .send()
            .await
            .unwrap();
        assert!(super::read_bounded_response(response, 8).await.is_err());
    }
}
