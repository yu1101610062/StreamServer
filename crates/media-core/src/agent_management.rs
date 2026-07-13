use std::{
    collections::BTreeSet,
    fmt,
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use axum::body::Bytes;
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use reqwest::{Client, StatusCode, Url, redirect::Policy};
use rustls::{
    CertificateError, DigitallySignedStruct, Error as RustlsError, RootCertStore, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio_stream::StreamExt;
use uuid::Uuid;
use zeroize::Zeroizing;

const CAPABILITY_ISSUER: &str = "streamserver-core";
const CAPABILITY_SUBJECT: &str = "core-agent-write";
const CAPABILITY_TOKEN_TYPE: &str = "agent-cap+jwt";
const MIN_CAPABILITY_TTL_SECONDS: u64 = 10;
const MAX_CAPABILITY_TTL_SECONDS: u64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentManagementOperation {
    Upload,
    Delete,
}

impl AgentManagementOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Delete => "delete",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct AgentCapabilityClaims {
    iss: String,
    sub: String,
    aud: String,
    op: String,
    path: String,
    max_bytes: u64,
    jti: String,
    iat: i64,
    nbf: i64,
    exp: i64,
}

#[derive(Debug, Error)]
pub(crate) enum AgentManagementError {
    #[error("Agent management capability configuration is invalid")]
    InvalidCapabilityConfiguration,
    #[error("Agent management capability request is invalid")]
    InvalidCapabilityRequest,
    #[error("Agent management capability signing failed")]
    CapabilitySigning,
    #[error("authenticated Agent management target is invalid")]
    InvalidTarget,
    #[error("authenticated Agent management target is unavailable")]
    TargetUnavailable,
    #[error("authenticated Agent control session was fenced")]
    SessionFenced,
    #[error("Agent management TLS configuration is invalid")]
    InvalidTlsConfiguration,
    #[error("Agent management request concurrency is exhausted")]
    Busy,
    #[error("Agent management request is invalid")]
    InvalidRequest,
    #[error("Agent management audit failed")]
    Audit,
    #[error("Agent management HTTP client failed")]
    HttpClient,
    #[error("Agent management transport failed")]
    Transport(#[source] reqwest::Error),
    #[error("Agent management response exceeded the configured limit")]
    ResponseTooLarge,
    #[error("Agent management response body failed")]
    ResponseBody(#[source] reqwest::Error),
    #[error("Agent management endpoint is not ready")]
    NotReady,
}

impl AgentManagementError {
    pub(crate) fn safe_code(&self) -> &'static str {
        match self {
            Self::InvalidCapabilityConfiguration => "invalid_capability_configuration",
            Self::InvalidCapabilityRequest => "invalid_capability_request",
            Self::CapabilitySigning => "capability_signing",
            Self::InvalidTarget => "invalid_target",
            Self::TargetUnavailable => "target_unavailable",
            Self::SessionFenced => "session_fenced",
            Self::InvalidTlsConfiguration => "invalid_tls_configuration",
            Self::Busy => "busy",
            Self::InvalidRequest => "invalid_request",
            Self::Audit => "audit",
            Self::HttpClient => "http_client",
            Self::Transport(_) => "transport",
            Self::ResponseTooLarge => "response_too_large",
            Self::ResponseBody(_) => "response_body",
            Self::NotReady => "not_ready",
        }
    }
}

pub(crate) struct AgentCapabilitySigner {
    key: EncodingKey,
    kid: String,
    ttl_seconds: i64,
}

impl fmt::Debug for AgentCapabilitySigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentCapabilitySigner")
            .field("private_key", &"[REDACTED]")
            .field("kid", &self.kid)
            .field("ttl_seconds", &self.ttl_seconds)
            .finish()
    }
}

impl AgentCapabilitySigner {
    pub(crate) fn new(
        private_key_pem: &str,
        kid: &str,
        ttl_seconds: u64,
    ) -> Result<Self, AgentManagementError> {
        if private_key_pem.trim().is_empty()
            || kid.len() != 64
            || !kid
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || !(MIN_CAPABILITY_TTL_SECONDS..=MAX_CAPABILITY_TTL_SECONDS).contains(&ttl_seconds)
        {
            return Err(AgentManagementError::InvalidCapabilityConfiguration);
        }
        let key = EncodingKey::from_ed_pem(private_key_pem.as_bytes())
            .map_err(|_| AgentManagementError::InvalidCapabilityConfiguration)?;
        Ok(Self {
            key,
            kid: kid.to_string(),
            ttl_seconds: i64::try_from(ttl_seconds)
                .map_err(|_| AgentManagementError::InvalidCapabilityConfiguration)?,
        })
    }

    pub(crate) fn sign(
        &self,
        operation: AgentManagementOperation,
        node_id: Uuid,
        path: &str,
        max_bytes: u64,
        now: DateTime<Utc>,
    ) -> Result<SignedAgentCapability, AgentManagementError> {
        if node_id.is_nil()
            || path != path.trim()
            || !capability_path_is_valid(operation, node_id, path)
            || max_bytes == 0
        {
            return Err(AgentManagementError::InvalidCapabilityRequest);
        }
        let issued_at = now.timestamp();
        let claims = AgentCapabilityClaims {
            iss: CAPABILITY_ISSUER.to_string(),
            sub: CAPABILITY_SUBJECT.to_string(),
            aud: format!("agent:{node_id}"),
            op: operation.as_str().to_string(),
            path: path.to_string(),
            max_bytes,
            jti: Uuid::now_v7().to_string(),
            iat: issued_at,
            nbf: issued_at,
            exp: issued_at.saturating_add(self.ttl_seconds),
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(CAPABILITY_TOKEN_TYPE.to_string());
        header.kid = Some(self.kid.clone());
        let token = encode(&header, &claims, &self.key)
            .map_err(|_| AgentManagementError::CapabilitySigning)?;
        Ok(SignedAgentCapability {
            token: Zeroizing::new(token),
            jti: Uuid::parse_str(&claims.jti)
                .map_err(|_| AgentManagementError::CapabilitySigning)?,
            expires_at: DateTime::from_timestamp(claims.exp, 0)
                .ok_or(AgentManagementError::CapabilitySigning)?,
        })
    }
}

fn capability_path_is_valid(
    operation: AgentManagementOperation,
    node_id: Uuid,
    path: &str,
) -> bool {
    let prefix = format!("uploads/{node_id}/");
    match operation {
        AgentManagementOperation::Upload => path == prefix,
        AgentManagementOperation::Delete => {
            let Some(suffix) = path.strip_prefix(&prefix) else {
                return false;
            };
            !suffix.is_empty()
                && suffix.split('/').all(|segment| {
                    !segment.is_empty()
                        && segment != "."
                        && segment != ".."
                        && segment.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                        })
                })
        }
    }
}

pub(crate) struct SignedAgentCapability {
    token: Zeroizing<String>,
    pub(crate) jti: Uuid,
    pub(crate) expires_at: DateTime<Utc>,
}

impl SignedAgentCapability {
    pub(crate) fn expose_for_authorization(&self) -> &str {
        self.token.as_str()
    }
}

impl fmt::Debug for SignedAgentCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignedAgentCapability")
            .field("token", &"[REDACTED]")
            .field("jti", &self.jti)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthenticatedAgentManagementTarget {
    node_id: Uuid,
    session_id: Uuid,
    peer_ip: IpAddr,
    management_port: u16,
    management_upload_max_bytes: u64,
    server_name: String,
    upload_url: Url,
    certificate_pins: AgentManagementCertificatePins,
}

impl AuthenticatedAgentManagementTarget {
    pub(crate) fn new(
        node_id: Uuid,
        session_id: Uuid,
        peer_ip: IpAddr,
        management_port: u16,
        management_upload_max_bytes: u64,
        certificate_pins: AgentManagementCertificatePins,
    ) -> Result<Self, AgentManagementError> {
        if node_id.is_nil()
            || session_id.is_nil()
            || management_port == 0
            || management_upload_max_bytes == 0
        {
            return Err(AgentManagementError::InvalidTarget);
        }
        let server_name = format!("agent-{}.agent.streamserver.internal", node_id.simple());
        let upload_url = Url::parse(&format!(
            "https://{server_name}:{management_port}/internal/uploads/media"
        ))
        .map_err(|_| AgentManagementError::InvalidTarget)?;
        Ok(Self {
            node_id,
            session_id,
            peer_ip,
            management_port,
            management_upload_max_bytes,
            server_name,
            upload_url,
            certificate_pins,
        })
    }

    pub(crate) fn node_id(&self) -> Uuid {
        self.node_id
    }

    #[cfg(test)]
    pub(crate) fn peer_ip(&self) -> IpAddr {
        self.peer_ip
    }

    pub(crate) fn session_id(&self) -> Uuid {
        self.session_id
    }

    #[cfg(test)]
    pub(crate) fn management_port(&self) -> u16 {
        self.management_port
    }

    #[cfg(test)]
    pub(crate) fn management_upload_max_bytes(&self) -> u64 {
        self.management_upload_max_bytes
    }

    pub(crate) fn server_name(&self) -> &str {
        &self.server_name
    }

    pub(crate) fn resolved_address(&self) -> SocketAddr {
        SocketAddr::new(self.peer_ip, self.management_port)
    }

    fn certificate_pins(&self) -> &AgentManagementCertificatePins {
        &self.certificate_pins
    }

    pub(crate) fn upload_url(&self) -> Url {
        self.upload_url.clone()
    }

    fn readiness_url(&self) -> Url {
        let mut url = self.upload_url.clone();
        url.set_path("/internal/health/ready");
        url
    }

    pub(crate) fn delete_url(&self, path: &str) -> Result<Url, AgentManagementError> {
        if !capability_path_is_valid(AgentManagementOperation::Delete, self.node_id, path) {
            return Err(AgentManagementError::InvalidCapabilityRequest);
        }
        let mut url = self.upload_url.clone();
        url.set_path(&format!("/internal/uploads/media/{path}"));
        Ok(url)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AgentManagementCertificatePins {
    fingerprints: BTreeSet<String>,
}

impl AgentManagementCertificatePins {
    pub(crate) fn new(
        current_fingerprint_sha256: &str,
        rotating_fingerprint_sha256: Option<&str>,
    ) -> Result<Self, AgentManagementError> {
        let mut fingerprints = BTreeSet::new();
        for fingerprint in
            std::iter::once(current_fingerprint_sha256).chain(rotating_fingerprint_sha256)
        {
            if fingerprint.len() != 64
                || !fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err(AgentManagementError::InvalidTlsConfiguration);
            }
            fingerprints.insert(fingerprint.to_string());
        }
        if fingerprints.is_empty() || fingerprints.len() > 2 {
            return Err(AgentManagementError::InvalidTlsConfiguration);
        }
        Ok(Self { fingerprints })
    }

    fn contains_der(&self, certificate: &CertificateDer<'_>) -> bool {
        let fingerprint = format!("{:x}", sha2::Sha256::digest(certificate.as_ref()));
        self.fingerprints.contains(&fingerprint)
    }
}

impl fmt::Debug for AgentManagementCertificatePins {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentManagementCertificatePins")
            .field("fingerprints", &self.fingerprints)
            .finish()
    }
}

#[derive(Debug)]
struct PinnedAgentServerVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    pins: AgentManagementCertificatePins,
}

impl PinnedAgentServerVerifier {
    fn new(
        roots: RootCertStore,
        pins: AgentManagementCertificatePins,
    ) -> Result<Self, AgentManagementError> {
        if roots.is_empty() {
            return Err(AgentManagementError::InvalidTlsConfiguration);
        }
        let inner = WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|_| AgentManagementError::InvalidTlsConfiguration)?;
        Ok(Self { inner, pins })
    }
}

impl ServerCertVerifier for PinnedAgentServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        if !intermediates.is_empty() {
            return Err(pinned_certificate_error());
        }
        let verified = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        if !self.pins.contains_der(end_entity) {
            return Err(pinned_certificate_error());
        }
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn pinned_certificate_error() -> RustlsError {
    RustlsError::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

pub(crate) type AgentManagementFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(crate) trait AgentManagementTargetProvider: Send + Sync {
    fn target(
        &self,
        node_id: Uuid,
    ) -> AgentManagementFuture<'_, Result<AuthenticatedAgentManagementTarget, AgentManagementError>>;

    fn begin_request_fence<'a>(
        &'a self,
        target: &'a AuthenticatedAgentManagementTarget,
    ) -> AgentManagementFuture<'a, Result<Box<dyn AgentManagementSessionFence>, AgentManagementError>>;
}

pub(crate) trait AgentManagementSessionFence: Send {
    fn release(self: Box<Self>)
    -> AgentManagementFuture<'static, Result<(), AgentManagementError>>;
}

pub(crate) trait AgentManagementReadinessProbe: Send + Sync {
    fn probe<'a>(
        &'a self,
        target: &'a AuthenticatedAgentManagementTarget,
    ) -> AgentManagementFuture<'a, Result<(), AgentManagementError>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentManagementAuditOutcome {
    Issued,
    Success,
    Failure,
}

impl AgentManagementAuditOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Issued => "issued",
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentManagementAuditRecord {
    pub(crate) outcome: AgentManagementAuditOutcome,
    pub(crate) node_id: Uuid,
    pub(crate) session_id: Uuid,
    pub(crate) operation: AgentManagementOperation,
    pub(crate) jti: Uuid,
    pub(crate) path_sha256: String,
    pub(crate) max_bytes: u64,
    pub(crate) peer: SocketAddr,
    pub(crate) http_status: Option<u16>,
    pub(crate) error_code: Option<&'static str>,
    pub(crate) occurred_at: DateTime<Utc>,
}

pub(crate) trait AgentManagementAuditSink: Send + Sync {
    fn record(
        &self,
        record: AgentManagementAuditRecord,
    ) -> AgentManagementFuture<'_, Result<(), AgentManagementError>>;
}

#[derive(Debug, Default)]
pub(crate) struct TracingAgentManagementAuditSink;

impl AgentManagementAuditSink for TracingAgentManagementAuditSink {
    fn record(
        &self,
        record: AgentManagementAuditRecord,
    ) -> AgentManagementFuture<'_, Result<(), AgentManagementError>> {
        tracing::info!(
            target: "streamserver.agent_management.audit",
            outcome = record.outcome.as_str(),
            node_id = %record.node_id,
            session_id = %record.session_id,
            operation = record.operation.as_str(),
            jti = %record.jti,
            path_sha256 = %record.path_sha256,
            max_bytes = record.max_bytes,
            peer = %record.peer,
            http_status = record.http_status,
            error_code = record.error_code,
            occurred_at = %record.occurred_at,
            "Agent management write audit"
        );
        Box::pin(async { Ok(()) })
    }
}

pub(crate) struct AgentManagementTlsMaterial {
    client_certificates: Vec<CertificateDer<'static>>,
    client_private_key: PrivateKeyDer<'static>,
    agent_issuer_roots: RootCertStore,
}

impl fmt::Debug for AgentManagementTlsMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentManagementTlsMaterial")
            .field("client_certificate", &"[REDACTED]")
            .field("client_private_key", &"[REDACTED]")
            .field("agent_issuer_roots", &self.agent_issuer_roots.len())
            .finish()
    }
}

impl AgentManagementTlsMaterial {
    pub(crate) fn from_pem(
        client_certificate_pem: &str,
        client_private_key_pem: &str,
        agent_issuer_ca_pem: &str,
    ) -> Result<Self, AgentManagementError> {
        let client_certificates = CertificateDer::pem_slice_iter(client_certificate_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| AgentManagementError::InvalidTlsConfiguration)?;
        if client_certificates.len() != 1 {
            return Err(AgentManagementError::InvalidTlsConfiguration);
        }
        let mut client_private_keys =
            PrivateKeyDer::pem_slice_iter(client_private_key_pem.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| AgentManagementError::InvalidTlsConfiguration)?;
        if client_private_keys.len() != 1 {
            return Err(AgentManagementError::InvalidTlsConfiguration);
        }
        let agent_issuer_certificates =
            CertificateDer::pem_slice_iter(agent_issuer_ca_pem.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| AgentManagementError::InvalidTlsConfiguration)?;
        if agent_issuer_certificates.len() != 1 {
            return Err(AgentManagementError::InvalidTlsConfiguration);
        }
        let mut agent_issuer_roots = RootCertStore::empty();
        agent_issuer_roots
            .add(agent_issuer_certificates[0].clone())
            .map_err(|_| AgentManagementError::InvalidTlsConfiguration)?;
        Ok(Self {
            client_certificates,
            client_private_key: client_private_keys.remove(0),
            agent_issuer_roots,
        })
    }

    fn client_config(
        &self,
        pins: AgentManagementCertificatePins,
    ) -> Result<rustls::ClientConfig, AgentManagementError> {
        let verifier = Arc::new(PinnedAgentServerVerifier::new(
            self.agent_issuer_roots.clone(),
            pins,
        )?);
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(
                self.client_certificates.clone(),
                self.client_private_key.clone_key(),
            )
            .map_err(|_| AgentManagementError::InvalidTlsConfiguration)
    }
}

pub(crate) struct AgentUploadRequest {
    node_id: Uuid,
    max_bytes: u64,
    content_type: String,
    body: reqwest::Body,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentDeleteRequest {
    node_id: Uuid,
    path: String,
    max_bytes: u64,
}

impl AgentDeleteRequest {
    pub(crate) fn new(
        node_id: Uuid,
        path: &str,
        max_bytes: u64,
    ) -> Result<Self, AgentManagementError> {
        if node_id.is_nil()
            || max_bytes == 0
            || path != path.trim()
            || !capability_path_is_valid(AgentManagementOperation::Delete, node_id, path)
        {
            return Err(AgentManagementError::InvalidRequest);
        }
        Ok(Self {
            node_id,
            path: path.to_string(),
            max_bytes,
        })
    }

    pub(crate) fn node_id(&self) -> Uuid {
        self.node_id
    }
}

impl AgentUploadRequest {
    pub(crate) fn new(
        node_id: Uuid,
        max_bytes: u64,
        content_type: &str,
        body: reqwest::Body,
    ) -> Result<Self, AgentManagementError> {
        let content_type = content_type.trim();
        if node_id.is_nil()
            || max_bytes == 0
            || !content_type
                .to_ascii_lowercase()
                .starts_with("multipart/form-data;")
            || content_type.contains(['\r', '\n'])
        {
            return Err(AgentManagementError::InvalidRequest);
        }
        Ok(Self {
            node_id,
            max_bytes,
            content_type: content_type.to_string(),
            body,
        })
    }

    pub(crate) fn node_id(&self) -> Uuid {
        self.node_id
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AgentManagementResponse {
    status: StatusCode,
    body: Bytes,
}

impl AgentManagementResponse {
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn body(&self) -> &Bytes {
        &self.body
    }
}

struct AgentManagementAuditAttempt<'a> {
    target: &'a AuthenticatedAgentManagementTarget,
    operation: AgentManagementOperation,
    capability: &'a SignedAgentCapability,
    path: &'a str,
    max_bytes: u64,
}

impl AgentManagementAuditAttempt<'_> {
    fn record(
        &self,
        outcome: AgentManagementAuditOutcome,
        http_status: Option<u16>,
        error_code: Option<&'static str>,
    ) -> AgentManagementAuditRecord {
        AgentManagementAuditRecord {
            outcome,
            node_id: self.target.node_id,
            session_id: self.target.session_id,
            operation: self.operation,
            jti: self.capability.jti,
            path_sha256: format!("{:x}", sha2::Sha256::digest(self.path.as_bytes())),
            max_bytes: self.max_bytes,
            peer: self.target.resolved_address(),
            http_status,
            error_code,
            occurred_at: Utc::now(),
        }
    }
}

pub(crate) trait AgentManagementService: Send + Sync + fmt::Debug {
    fn upload(
        &self,
        request: AgentUploadRequest,
    ) -> AgentManagementFuture<'_, Result<AgentManagementResponse, AgentManagementError>>;

    fn delete(
        &self,
        request: AgentDeleteRequest,
    ) -> AgentManagementFuture<'_, Result<AgentManagementResponse, AgentManagementError>>;
}

pub(crate) struct RoutedAgentManagementService {
    client: Arc<AgentManagementClient>,
    targets: Arc<dyn AgentManagementTargetProvider>,
}

impl RoutedAgentManagementService {
    pub(crate) fn new(
        client: Arc<AgentManagementClient>,
        targets: Arc<dyn AgentManagementTargetProvider>,
    ) -> Self {
        Self { client, targets }
    }
}

impl fmt::Debug for RoutedAgentManagementService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RoutedAgentManagementService")
            .field("client", &self.client)
            .field("targets", &"[DYNAMIC]")
            .finish()
    }
}

impl AgentManagementService for RoutedAgentManagementService {
    fn upload(
        &self,
        request: AgentUploadRequest,
    ) -> AgentManagementFuture<'_, Result<AgentManagementResponse, AgentManagementError>> {
        Box::pin(async move {
            let target = self.targets.target(request.node_id()).await?;
            let fence = self.targets.begin_request_fence(&target).await?;
            let result = self.client.upload_to_target(&target, request).await;
            if let Err(error) = fence.release().await {
                tracing::warn!(
                    node_id = %target.node_id,
                    session_id = %target.session_id,
                    error = %error,
                    "Agent management upload completed while releasing its session fence failed"
                );
            }
            result
        })
    }

    fn delete(
        &self,
        request: AgentDeleteRequest,
    ) -> AgentManagementFuture<'_, Result<AgentManagementResponse, AgentManagementError>> {
        Box::pin(async move {
            let target = self.targets.target(request.node_id()).await?;
            let fence = self.targets.begin_request_fence(&target).await?;
            let result = self.client.delete_from_target(&target, request).await;
            if let Err(error) = fence.release().await {
                tracing::warn!(
                    node_id = %target.node_id,
                    session_id = %target.session_id,
                    error = %error,
                    "Agent management delete completed while releasing its session fence failed"
                );
            }
            result
        })
    }
}

pub(crate) struct AgentManagementClient {
    signer: AgentCapabilitySigner,
    tls: AgentManagementTlsMaterial,
    audit: Arc<dyn AgentManagementAuditSink>,
    admission: Arc<Semaphore>,
    timeout: Duration,
    max_response_bytes: usize,
}

impl fmt::Debug for AgentManagementClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentManagementClient")
            .field("signer", &self.signer)
            .field("tls", &self.tls)
            .field("audit", &"[DYNAMIC]")
            .field("max_concurrency", &4)
            .field("timeout", &self.timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

impl AgentManagementClient {
    const MAX_CONCURRENCY: usize = 4;
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
    const MAX_RESPONSE_BYTES: usize = 64 * 1024;

    pub(crate) fn new(
        signer: AgentCapabilitySigner,
        tls: AgentManagementTlsMaterial,
        audit: Arc<dyn AgentManagementAuditSink>,
    ) -> Result<Self, AgentManagementError> {
        Ok(Self {
            signer,
            tls,
            audit,
            admission: Arc::new(Semaphore::new(Self::MAX_CONCURRENCY)),
            timeout: Self::REQUEST_TIMEOUT,
            max_response_bytes: Self::MAX_RESPONSE_BYTES,
        })
    }

    pub(crate) async fn upload_to_target(
        &self,
        target: &AuthenticatedAgentManagementTarget,
        request: AgentUploadRequest,
    ) -> Result<AgentManagementResponse, AgentManagementError> {
        let _permit = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| AgentManagementError::Busy)?;
        if request.node_id != target.node_id
            || request.max_bytes > target.management_upload_max_bytes
        {
            return Err(AgentManagementError::InvalidRequest);
        }
        let path = format!("uploads/{}/", target.node_id);
        let capability = self.signer.sign(
            AgentManagementOperation::Upload,
            target.node_id,
            &path,
            request.max_bytes,
            Utc::now(),
        )?;
        let audit = AgentManagementAuditAttempt {
            target,
            operation: AgentManagementOperation::Upload,
            capability: &capability,
            path: &path,
            max_bytes: request.max_bytes,
        };
        self.audit
            .record(audit.record(AgentManagementAuditOutcome::Issued, None, None))
            .await
            .map_err(|_| AgentManagementError::Audit)?;
        let client = match self.client_for_target(target) {
            Ok(client) => client,
            Err(error) => {
                let error_code = error.safe_code();
                self.audit_failure(&audit, None, error_code).await?;
                return Err(error);
            }
        };
        let response = client
            .post(target.upload_url())
            .bearer_auth(capability.expose_for_authorization())
            .header(reqwest::header::CONTENT_TYPE, request.content_type)
            .header(reqwest::header::CONTENT_LENGTH, request.max_bytes)
            .body(request.body)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.audit_failure(&audit, None, "transport").await?;
                return Err(AgentManagementError::Transport(error));
            }
        };
        let status = response.status();
        let body = match read_bounded_response(response, self.max_response_bytes).await {
            Ok(body) => body,
            Err(error) => {
                let error_code = error.safe_code();
                self.audit_failure(&audit, Some(status.as_u16()), error_code)
                    .await?;
                return Err(error);
            }
        };
        let outcome = if status.is_success() {
            AgentManagementAuditOutcome::Success
        } else {
            AgentManagementAuditOutcome::Failure
        };
        self.audit
            .record(audit.record(
                outcome,
                Some(status.as_u16()),
                (!status.is_success()).then_some("http_status"),
            ))
            .await
            .map_err(|_| AgentManagementError::Audit)?;
        Ok(AgentManagementResponse { status, body })
    }

    pub(crate) async fn delete_from_target(
        &self,
        target: &AuthenticatedAgentManagementTarget,
        request: AgentDeleteRequest,
    ) -> Result<AgentManagementResponse, AgentManagementError> {
        let _permit = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| AgentManagementError::Busy)?;
        if request.node_id != target.node_id
            || request.max_bytes > target.management_upload_max_bytes
        {
            return Err(AgentManagementError::InvalidRequest);
        }
        let capability = self.signer.sign(
            AgentManagementOperation::Delete,
            target.node_id,
            &request.path,
            request.max_bytes,
            Utc::now(),
        )?;
        let audit = AgentManagementAuditAttempt {
            target,
            operation: AgentManagementOperation::Delete,
            capability: &capability,
            path: &request.path,
            max_bytes: request.max_bytes,
        };
        self.audit
            .record(audit.record(AgentManagementAuditOutcome::Issued, None, None))
            .await
            .map_err(|_| AgentManagementError::Audit)?;
        let client = match self.client_for_target(target) {
            Ok(client) => client,
            Err(error) => {
                let error_code = error.safe_code();
                self.audit_failure(&audit, None, error_code).await?;
                return Err(error);
            }
        };
        let response = client
            .delete(target.delete_url(&request.path)?)
            .bearer_auth(capability.expose_for_authorization())
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.audit_failure(&audit, None, "transport").await?;
                return Err(AgentManagementError::Transport(error));
            }
        };
        let status = response.status();
        let body = match read_bounded_response(response, self.max_response_bytes).await {
            Ok(body) => body,
            Err(error) => {
                let error_code = error.safe_code();
                self.audit_failure(&audit, Some(status.as_u16()), error_code)
                    .await?;
                return Err(error);
            }
        };
        let outcome = if status.is_success() {
            AgentManagementAuditOutcome::Success
        } else {
            AgentManagementAuditOutcome::Failure
        };
        self.audit
            .record(audit.record(
                outcome,
                Some(status.as_u16()),
                (!status.is_success()).then_some("http_status"),
            ))
            .await
            .map_err(|_| AgentManagementError::Audit)?;
        Ok(AgentManagementResponse { status, body })
    }

    async fn probe_target(
        &self,
        target: &AuthenticatedAgentManagementTarget,
    ) -> Result<(), AgentManagementError> {
        let _permit = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| AgentManagementError::Busy)?;
        let client = self.client_for_target(target)?;
        let response = client
            .get(target.readiness_url())
            .send()
            .await
            .map_err(AgentManagementError::Transport)?;
        let status = response.status();
        let _ = read_bounded_response(response, self.max_response_bytes).await?;
        if !status.is_success() {
            return Err(AgentManagementError::NotReady);
        }
        Ok(())
    }

    fn client_for_target(
        &self,
        target: &AuthenticatedAgentManagementTarget,
    ) -> Result<Client, AgentManagementError> {
        let tls = self.tls.client_config(target.certificate_pins().clone())?;
        Client::builder()
            .use_preconfigured_tls(tls)
            .https_only(true)
            .no_proxy()
            .redirect(Policy::none())
            .timeout(self.timeout)
            .connect_timeout(self.timeout)
            .pool_idle_timeout(self.timeout)
            .pool_max_idle_per_host(Self::MAX_CONCURRENCY)
            .resolve(target.server_name(), target.resolved_address())
            .build()
            .map_err(|_| AgentManagementError::HttpClient)
    }

    async fn audit_failure(
        &self,
        audit: &AgentManagementAuditAttempt<'_>,
        http_status: Option<u16>,
        error_code: &'static str,
    ) -> Result<(), AgentManagementError> {
        self.audit
            .record(audit.record(
                AgentManagementAuditOutcome::Failure,
                http_status,
                Some(error_code),
            ))
            .await
            .map_err(|_| AgentManagementError::Audit)
    }
}

impl AgentManagementReadinessProbe for AgentManagementClient {
    fn probe<'a>(
        &'a self,
        target: &'a AuthenticatedAgentManagementTarget,
    ) -> AgentManagementFuture<'a, Result<(), AgentManagementError>> {
        Box::pin(self.probe_target(target))
    }
}

async fn read_bounded_response(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Bytes, AgentManagementError> {
    let mut buffer = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(AgentManagementError::ResponseBody)?;
        if buffer.len().saturating_add(chunk.len()) > max_bytes {
            return Err(AgentManagementError::ResponseTooLarge);
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buffer))
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, TcpListener as StdTcpListener},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use axum::{
        Router,
        extract::Path as AxumPath,
        http::{HeaderMap, StatusCode, header},
        response::IntoResponse,
        routing::{delete, get, post},
    };
    use axum_server::{Handle, tls_rustls::RustlsConfig};
    use chrono::{TimeZone, Utc};
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose, PKCS_ED25519, SanType,
    };
    use rustls::{
        RootCertStore, ServerConfig, client::danger::ServerCertVerifier,
        server::WebPkiClientVerifier,
    };
    use rustls_pki_types::{
        CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime,
    };
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use super::*;

    #[test]
    fn eddsa_capability_matches_the_agent_contract_and_redacts_debug() {
        let key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let kid = format!("{:x}", Sha256::digest(key.public_key_der()));
        let signer = AgentCapabilitySigner::new(&key.serialize_pem(), &kid, 60).unwrap();
        let node_id = Uuid::now_v7();
        let path = format!("uploads/{node_id}/2026/07/12/clip.mp4");
        let now = Utc.timestamp_opt(1_800_000_000, 0).unwrap();

        let signed = signer
            .sign(AgentManagementOperation::Delete, node_id, &path, 4096, now)
            .unwrap();

        let header = decode_header(signed.expose_for_authorization()).unwrap();
        assert_eq!(header.alg, Algorithm::EdDSA);
        assert_eq!(header.typ.as_deref(), Some("agent-cap+jwt"));
        assert_eq!(header.kid.as_deref(), Some(kid.as_str()));
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.required_spec_claims.clear();
        validation.set_audience(&[format!("agent:{node_id}")]);
        let claims = decode::<AgentCapabilityClaims>(
            signed.expose_for_authorization(),
            &DecodingKey::from_ed_pem(key.public_key_pem().as_bytes()).unwrap(),
            &validation,
        )
        .unwrap()
        .claims;
        assert_eq!(claims.iss, "streamserver-core");
        assert_eq!(claims.sub, "core-agent-write");
        assert_eq!(claims.aud, format!("agent:{node_id}"));
        assert_eq!(claims.op, "delete");
        assert_eq!(claims.path, path);
        assert_eq!(claims.max_bytes, 4096);
        assert_eq!(claims.iat, now.timestamp());
        assert_eq!(claims.nbf, claims.iat);
        assert_eq!(claims.exp - claims.iat, 60);
        let jti = Uuid::parse_str(&claims.jti).unwrap();
        assert!(!jti.is_nil());
        assert_eq!(claims.jti, jti.to_string());
        assert!(!format!("{signer:?}").contains("PRIVATE KEY"));
        assert!(!format!("{signed:?}").contains(signed.expose_for_authorization()));
    }

    #[test]
    fn capability_scope_is_node_bound_canonical_and_operation_specific() {
        let key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let kid = format!("{:x}", Sha256::digest(key.public_key_der()));
        let signer = AgentCapabilitySigner::new(&key.serialize_pem(), &kid, 60).unwrap();
        let node_id = Uuid::now_v7();
        let other_node = Uuid::now_v7();
        let now = Utc.timestamp_opt(1_800_000_000, 0).unwrap();

        assert!(
            signer
                .sign(
                    AgentManagementOperation::Upload,
                    node_id,
                    &format!("uploads/{node_id}/"),
                    1024,
                    now,
                )
                .is_ok()
        );
        for invalid_upload in [
            format!("uploads/{node_id}"),
            format!("/uploads/{node_id}/"),
            format!("uploads/{node_id}/clip.mp4"),
            format!("uploads/{other_node}/"),
        ] {
            assert!(
                signer
                    .sign(
                        AgentManagementOperation::Upload,
                        node_id,
                        &invalid_upload,
                        1024,
                        now,
                    )
                    .is_err(),
                "accepted invalid upload scope {invalid_upload:?}"
            );
        }

        let valid_delete = format!("uploads/{node_id}/2026/07/12/clip.mp4");
        assert!(
            signer
                .sign(
                    AgentManagementOperation::Delete,
                    node_id,
                    &valid_delete,
                    1024,
                    now,
                )
                .is_ok()
        );
        for invalid_delete in [
            format!("/uploads/{node_id}/clip.mp4"),
            format!("uploads/{node_id}/../clip.mp4"),
            format!("uploads/{other_node}/clip.mp4"),
            format!("uploads/{node_id}/"),
        ] {
            assert!(
                signer
                    .sign(
                        AgentManagementOperation::Delete,
                        node_id,
                        &invalid_delete,
                        1024,
                        now,
                    )
                    .is_err(),
                "accepted invalid delete scope {invalid_delete:?}"
            );
        }
        assert!(
            signer
                .sign(
                    AgentManagementOperation::Delete,
                    node_id,
                    &valid_delete,
                    0,
                    now,
                )
                .is_err()
        );
        assert!(AgentCapabilitySigner::new(&key.serialize_pem(), &kid, 9).is_err());
    }

    #[test]
    fn authenticated_target_uses_certificate_dns_and_never_a_reported_url() {
        let node_id = Uuid::now_v7();
        let session_id = Uuid::now_v7();
        let peer_ip = IpAddr::V4(Ipv4Addr::new(10, 22, 3, 9));
        let pins = AgentManagementCertificatePins::new(&"11".repeat(32), None).unwrap();
        let target = AuthenticatedAgentManagementTarget::new(
            node_id,
            session_id,
            peer_ip,
            8443,
            10 * 1024 * 1024,
            pins.clone(),
        )
        .unwrap();

        assert_eq!(target.node_id(), node_id);
        assert_eq!(target.session_id(), session_id);
        assert_eq!(target.peer_ip(), peer_ip);
        assert_eq!(target.management_port(), 8443);
        assert_eq!(target.management_upload_max_bytes(), 10 * 1024 * 1024);
        assert_eq!(
            target.server_name(),
            format!("agent-{}.agent.streamserver.internal", node_id.simple())
        );
        assert_eq!(
            target.upload_url().as_str(),
            format!(
                "https://agent-{}.agent.streamserver.internal:8443/internal/uploads/media",
                node_id.simple()
            )
        );
        let delete_path = format!("uploads/{node_id}/2026/07/12/clip.mp4");
        assert_eq!(
            target.delete_url(&delete_path).unwrap().as_str(),
            format!(
                "https://agent-{}.agent.streamserver.internal:8443/internal/uploads/media/{delete_path}",
                node_id.simple()
            )
        );
        assert!(
            AuthenticatedAgentManagementTarget::new(
                node_id,
                session_id,
                peer_ip,
                0,
                1024,
                pins.clone(),
            )
            .is_err()
        );
        assert!(
            AuthenticatedAgentManagementTarget::new(
                Uuid::nil(),
                session_id,
                peer_ip,
                8443,
                1024,
                pins,
            )
            .is_err()
        );
        assert!(target.delete_url("https://attacker.example/file").is_err());
    }

    #[test]
    fn server_verifier_requires_webpki_dns_direct_chain_and_exact_leaf_pin() {
        let node_id = Uuid::now_v7();
        let server_name = format!("agent-{}.agent.streamserver.internal", node_id.simple());
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let make_leaf = || {
            let key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::default();
            params.distinguished_name = DistinguishedName::new();
            params.is_ca = IsCa::NoCa;
            params.subject_alt_names =
                vec![SanType::DnsName(server_name.clone().try_into().unwrap())];
            params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            params.signed_by(&key, &ca, &ca_key).unwrap()
        };
        let expected_leaf = make_leaf();
        let same_ca_same_dns_wrong_leaf = make_leaf();
        let expected_fingerprint = format!("{:x}", Sha256::digest(expected_leaf.der()));
        let pins = AgentManagementCertificatePins::new(&expected_fingerprint, None).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(ca.der().to_vec())).unwrap();
        let verifier = PinnedAgentServerVerifier::new(roots.clone(), pins).unwrap();
        let server_name = ServerName::try_from(server_name).unwrap();
        let now = UnixTime::now();

        assert!(
            verifier
                .verify_server_cert(
                    &CertificateDer::from(expected_leaf.der().to_vec()),
                    &[],
                    &server_name,
                    &[],
                    now,
                )
                .is_ok()
        );
        assert!(
            verifier
                .verify_server_cert(
                    &CertificateDer::from(same_ca_same_dns_wrong_leaf.der().to_vec()),
                    &[],
                    &server_name,
                    &[],
                    now,
                )
                .is_err()
        );
        let rotating_fingerprint =
            format!("{:x}", Sha256::digest(same_ca_same_dns_wrong_leaf.der()));
        let rotating_pins =
            AgentManagementCertificatePins::new(&expected_fingerprint, Some(&rotating_fingerprint))
                .unwrap();
        let rotating_verifier =
            PinnedAgentServerVerifier::new(roots.clone(), rotating_pins).unwrap();
        assert!(
            rotating_verifier
                .verify_server_cert(
                    &CertificateDer::from(expected_leaf.der().to_vec()),
                    &[],
                    &server_name,
                    &[],
                    now,
                )
                .is_ok()
        );
        assert!(
            rotating_verifier
                .verify_server_cert(
                    &CertificateDer::from(same_ca_same_dns_wrong_leaf.der().to_vec()),
                    &[],
                    &server_name,
                    &[],
                    now,
                )
                .is_ok()
        );
        assert!(
            verifier
                .verify_server_cert(
                    &CertificateDer::from(expected_leaf.der().to_vec()),
                    &[CertificateDer::from(ca.der().to_vec())],
                    &server_name,
                    &[],
                    now,
                )
                .is_err()
        );
        let wrong_pins = AgentManagementCertificatePins::new(&"00".repeat(32), None).unwrap();
        let wrong_verifier = PinnedAgentServerVerifier::new(roots, wrong_pins).unwrap();
        assert!(
            wrong_verifier
                .verify_server_cert(
                    &CertificateDer::from(expected_leaf.der().to_vec()),
                    &[],
                    &server_name,
                    &[],
                    now,
                )
                .is_err()
        );
    }

    #[derive(Default)]
    struct RecordingAuditSink {
        records: Mutex<Vec<AgentManagementAuditRecord>>,
    }

    impl AgentManagementAuditSink for RecordingAuditSink {
        fn record(
            &self,
            record: AgentManagementAuditRecord,
        ) -> AgentManagementFuture<'_, Result<(), AgentManagementError>> {
            self.records.lock().unwrap().push(record);
            Box::pin(async { Ok(()) })
        }
    }

    struct StaticTargetProvider {
        target: AuthenticatedAgentManagementTarget,
        request_fence_held: Arc<AtomicBool>,
    }

    struct TestSessionFence {
        held: Arc<AtomicBool>,
    }

    impl AgentManagementSessionFence for TestSessionFence {
        fn release(
            self: Box<Self>,
        ) -> AgentManagementFuture<'static, Result<(), AgentManagementError>> {
            Box::pin(async move {
                assert!(self.held.swap(false, Ordering::SeqCst));
                Ok(())
            })
        }
    }

    impl AgentManagementTargetProvider for StaticTargetProvider {
        fn target(
            &self,
            node_id: Uuid,
        ) -> AgentManagementFuture<
            '_,
            Result<AuthenticatedAgentManagementTarget, AgentManagementError>,
        > {
            Box::pin(async move {
                if self.target.node_id() != node_id {
                    return Err(AgentManagementError::TargetUnavailable);
                }
                Ok(self.target.clone())
            })
        }

        fn begin_request_fence<'a>(
            &'a self,
            target: &'a AuthenticatedAgentManagementTarget,
        ) -> AgentManagementFuture<
            'a,
            Result<Box<dyn AgentManagementSessionFence>, AgentManagementError>,
        > {
            Box::pin(async move {
                if target != &self.target || self.request_fence_held.swap(true, Ordering::SeqCst) {
                    return Err(AgentManagementError::SessionFenced);
                }
                Ok(Box::new(TestSessionFence {
                    held: self.request_fence_held.clone(),
                }) as Box<dyn AgentManagementSessionFence>)
            })
        }
    }

    #[tokio::test]
    async fn management_client_uses_mtls_pinned_dns_resolution_and_no_redirects() {
        let node_id = Uuid::now_v7();
        let session_id = Uuid::now_v7();
        let server_name = format!("agent-{}.agent.streamserver.internal", node_id.simple());

        let agent_ca_key = KeyPair::generate().unwrap();
        let mut agent_ca_params = CertificateParams::default();
        agent_ca_params.distinguished_name = DistinguishedName::new();
        agent_ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        agent_ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let agent_ca = agent_ca_params.self_signed(&agent_ca_key).unwrap();
        let server_key = KeyPair::generate().unwrap();
        let mut server_params = CertificateParams::default();
        server_params.distinguished_name = DistinguishedName::new();
        server_params.is_ca = IsCa::NoCa;
        server_params.subject_alt_names =
            vec![SanType::DnsName(server_name.clone().try_into().unwrap())];
        server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_certificate = server_params
            .signed_by(&server_key, &agent_ca, &agent_ca_key)
            .unwrap();

        let core_ca_key = KeyPair::generate().unwrap();
        let mut core_ca_params = CertificateParams::default();
        core_ca_params.distinguished_name = DistinguishedName::new();
        core_ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        core_ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let core_ca = core_ca_params.self_signed(&core_ca_key).unwrap();
        let core_client_key = KeyPair::generate().unwrap();
        let mut core_client_params = CertificateParams::default();
        core_client_params.distinguished_name = DistinguishedName::new();
        core_client_params.is_ca = IsCa::NoCa;
        core_client_params.subject_alt_names = vec![SanType::URI(
            format!("spiffe://streamserver/core/{}", Uuid::now_v7())
                .try_into()
                .unwrap(),
        )];
        core_client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        core_client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let core_client_certificate = core_client_params
            .signed_by(&core_client_key, &core_ca, &core_ca_key)
            .unwrap();

        let mut client_roots = RootCertStore::empty();
        client_roots
            .add(CertificateDer::from(core_ca.der().to_vec()))
            .unwrap();
        let client_verifier = WebPkiClientVerifier::builder(Arc::new(client_roots))
            .build()
            .unwrap();
        let server_config = ServerConfig::builder()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(
                vec![CertificateDer::from(server_certificate.der().to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
            )
            .unwrap();
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (authorization_tx, authorization_rx) = tokio::sync::oneshot::channel();
        let authorization_tx = Arc::new(Mutex::new(Some(authorization_tx)));
        let request_fence_held = Arc::new(AtomicBool::new(false));
        let upload_fence_held = request_fence_held.clone();
        let (delete_tx, delete_rx) = tokio::sync::oneshot::channel();
        let delete_tx = Arc::new(Mutex::new(Some(delete_tx)));
        let delete_fence_held = request_fence_held.clone();
        let app = Router::new()
            .route(
                "/internal/health/ready",
                get(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/internal/uploads/media",
                post(move |headers: HeaderMap| {
                    let authorization_tx = authorization_tx.clone();
                    let request_fence_held = upload_fence_held.clone();
                    async move {
                        if !request_fence_held.load(Ordering::SeqCst) {
                            return StatusCode::PRECONDITION_FAILED.into_response();
                        }
                        if let Some(sender) = authorization_tx.lock().unwrap().take() {
                            let _ = sender.send(
                                headers
                                    .get(header::AUTHORIZATION)
                                    .and_then(|value| value.to_str().ok())
                                    .unwrap_or_default()
                                    .to_string(),
                            );
                        }
                        (
                            StatusCode::TEMPORARY_REDIRECT,
                            [(header::LOCATION, "https://attacker.invalid/")],
                        )
                            .into_response()
                    }
                }),
            )
            .route(
                "/internal/uploads/media/{*path}",
                delete(
                    move |AxumPath(path): AxumPath<String>, headers: HeaderMap| {
                        let delete_tx = delete_tx.clone();
                        let request_fence_held = delete_fence_held.clone();
                        async move {
                            if !request_fence_held.load(Ordering::SeqCst) {
                                return StatusCode::PRECONDITION_FAILED.into_response();
                            }
                            if let Some(sender) = delete_tx.lock().unwrap().take() {
                                let _ = sender.send((
                                    path.clone(),
                                    headers
                                        .get(header::AUTHORIZATION)
                                        .and_then(|value| value.to_str().ok())
                                        .unwrap_or_default()
                                        .to_string(),
                                ));
                            }
                            if path.ends_with("large.bin") {
                                (
                                    StatusCode::OK,
                                    vec![b'x'; AgentManagementClient::MAX_RESPONSE_BYTES + 1],
                                )
                                    .into_response()
                            } else {
                                StatusCode::NO_CONTENT.into_response()
                            }
                        }
                    },
                ),
            );
        let handle = Handle::new();
        let server_handle = handle.clone();
        let mut server = tokio::spawn(async move {
            axum_server::from_tcp_rustls(
                listener,
                RustlsConfig::from_config(Arc::new(server_config)),
            )
            .unwrap()
            .handle(server_handle)
            .serve(app.into_make_service())
            .await
            .unwrap();
        });

        let fingerprint = format!("{:x}", Sha256::digest(server_certificate.der()));
        let target = AuthenticatedAgentManagementTarget::new(
            node_id,
            session_id,
            address.ip(),
            address.port(),
            1024,
            AgentManagementCertificatePins::new(&fingerprint, None).unwrap(),
        )
        .unwrap();
        let capability_key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let capability_kid = format!("{:x}", Sha256::digest(capability_key.public_key_der()));
        let audit = Arc::new(RecordingAuditSink::default());
        let client = Arc::new(
            AgentManagementClient::new(
                AgentCapabilitySigner::new(&capability_key.serialize_pem(), &capability_kid, 60)
                    .unwrap(),
                AgentManagementTlsMaterial::from_pem(
                    &core_client_certificate.pem(),
                    &core_client_key.serialize_pem(),
                    &agent_ca.pem(),
                )
                .unwrap(),
                audit.clone(),
            )
            .unwrap(),
        );
        assert_eq!(client.timeout, Duration::from_secs(30));
        assert_eq!(client.admission.available_permits(), 4);
        assert_eq!(client.max_response_bytes, 64 * 1024);
        let permits = (0..4)
            .map(|_| client.admission.clone().try_acquire_owned().unwrap())
            .collect::<Vec<_>>();
        assert!(client.admission.clone().try_acquire_owned().is_err());
        drop(permits);

        let provider: Arc<dyn AgentManagementTargetProvider> = Arc::new(StaticTargetProvider {
            target: target.clone(),
            request_fence_held: request_fence_held.clone(),
        });
        let provided_target = provider.target(node_id).await.unwrap();
        assert_eq!(provided_target.session_id(), session_id);
        let readiness: Arc<dyn AgentManagementReadinessProbe> = client.clone();
        tokio::time::timeout(Duration::from_secs(2), readiness.probe(&provided_target))
            .await
            .expect("Agent readiness probe did not finish within 2 seconds")
            .unwrap();
        assert!(audit.records.lock().unwrap().is_empty());
        let management: Arc<dyn AgentManagementService> =
            Arc::new(RoutedAgentManagementService::new(client.clone(), provider));

        let response = match tokio::time::timeout(
            Duration::from_secs(2),
            management.upload(
                AgentUploadRequest::new(
                    node_id,
                    1,
                    "multipart/form-data; boundary=streamserver",
                    reqwest::Body::from("x"),
                )
                .unwrap(),
            ),
        )
        .await
        {
            Ok(result) => result.unwrap(),
            Err(_) => {
                handle.graceful_shutdown(Some(Duration::from_secs(1)));
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut server).await;
                server.abort();
                panic!("management upload did not finish within 2 seconds");
            }
        };

        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert!(!request_fence_held.load(Ordering::SeqCst));
        let authorization = tokio::time::timeout(Duration::from_secs(2), authorization_rx)
            .await
            .expect("server did not capture authorization within 2 seconds")
            .unwrap();
        assert!(authorization.starts_with("Bearer "));
        assert_eq!(audit.records.lock().unwrap().len(), 2);
        assert_eq!(
            audit.records.lock().unwrap()[0].outcome,
            AgentManagementAuditOutcome::Issued
        );
        assert_eq!(
            audit.records.lock().unwrap()[1].outcome,
            AgentManagementAuditOutcome::Failure
        );
        assert!(!format!("{:?}", audit.records.lock().unwrap()).contains(&authorization[7..]));

        let delete_path = format!("uploads/{node_id}/2026/07/12/clip.mp4");
        let delete_response = tokio::time::timeout(
            Duration::from_secs(2),
            management.delete(AgentDeleteRequest::new(node_id, &delete_path, 512).unwrap()),
        )
        .await
        .expect("management delete did not finish within 2 seconds")
        .unwrap();
        assert_eq!(delete_response.status(), StatusCode::NO_CONTENT);
        assert!(!request_fence_held.load(Ordering::SeqCst));
        let (captured_delete_path, delete_authorization) =
            tokio::time::timeout(Duration::from_secs(2), delete_rx)
                .await
                .expect("server did not capture delete within 2 seconds")
                .unwrap();
        assert_eq!(captured_delete_path, delete_path);
        let delete_token = delete_authorization.strip_prefix("Bearer ").unwrap();
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.required_spec_claims.clear();
        validation.set_audience(&[format!("agent:{node_id}")]);
        let delete_claims = decode::<AgentCapabilityClaims>(
            delete_token,
            &DecodingKey::from_ed_pem(capability_key.public_key_pem().as_bytes()).unwrap(),
            &validation,
        )
        .unwrap()
        .claims;
        assert_eq!(delete_claims.op, "delete");
        assert_eq!(delete_claims.path, delete_path);
        assert_eq!(delete_claims.max_bytes, 512);
        {
            let records = audit.records.lock().unwrap();
            assert_eq!(records.len(), 4);
            assert_eq!(records[2].outcome, AgentManagementAuditOutcome::Issued);
            assert_eq!(records[2].operation, AgentManagementOperation::Delete);
            assert_eq!(records[3].outcome, AgentManagementAuditOutcome::Success);
            assert_eq!(records[3].operation, AgentManagementOperation::Delete);
            assert!(!format!("{records:?}").contains(delete_token));
        }

        let oversized_path = format!("uploads/{node_id}/large.bin");
        let oversized_error = tokio::time::timeout(
            Duration::from_secs(2),
            management.delete(AgentDeleteRequest::new(node_id, &oversized_path, 512).unwrap()),
        )
        .await
        .expect("bounded response test did not finish within 2 seconds")
        .unwrap_err();
        assert!(matches!(
            oversized_error,
            AgentManagementError::ResponseTooLarge
        ));
        {
            let records = audit.records.lock().unwrap();
            assert_eq!(records.len(), 6);
            assert_eq!(records[4].outcome, AgentManagementAuditOutcome::Issued);
            assert_eq!(records[5].outcome, AgentManagementAuditOutcome::Failure);
            assert_eq!(records[5].error_code, Some("response_too_large"));
        }

        drop(response);
        drop(delete_response);
        drop(client);
        handle.graceful_shutdown(Some(Duration::from_secs(1)));
        match tokio::time::timeout(Duration::from_secs(2), &mut server).await {
            Ok(result) => result.unwrap(),
            Err(_) => {
                server.abort();
                panic!("management server did not stop within 2 seconds");
            }
        }
    }
}
