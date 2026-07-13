//! Core-owned Agent certificate authority and enrollment service.

use std::{
    collections::HashMap,
    fmt, fs,
    net::IpAddr,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration as StdDuration, Instant},
};

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use rand::{RngCore, rngs::OsRng};
use rcgen::{
    Certificate, CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;
use x509_parser::{pem::parse_x509_pem, prelude::FromDer, time::ASN1Time};
use zeroize::{Zeroize, Zeroizing};

use crate::repository::{
    AgentCertificateRotationBundle, AgentCertificateRotationIssue, AgentCertificateRotationRequest,
    AgentEnrollmentBundle, AgentEnrollmentPreflightOutcome, AgentEnrollmentRequest,
    ConsumeAgentEnrollmentOutcome, CreateAgentEnrollmentOutcome, NewAgentEnrollment,
    NewIssuedAgentCertificate, RepoError, StageAgentCertificateRotationOutcome, TaskRepository,
};

const AGENT_CERTIFICATE_VALIDITY: Duration = Duration::days(90);
const AGENT_CERTIFICATE_CLOCK_SKEW: Duration = Duration::minutes(5);
const AGENT_ENROLLMENT_TOKEN_TTL: Duration = Duration::minutes(10);
const AGENT_ENROLLMENT_TOKEN_PREFIX: &str = "ssae1";
const AGENT_ENROLLMENT_TOKEN_PAYLOAD_LEN: usize = 16 + 16 + 8 + 32;
const AGENT_ENROLLMENT_ADMISSION_KDF_SALT: &[u8] =
    b"streamserver.agent-enrollment.admission.kdf-extract.v1";
const AGENT_ENROLLMENT_ADMISSION_KDF_INFO: &[u8] =
    b"streamserver.agent-enrollment.admission.hmac-key.v1\0\x01";
const AGENT_ENROLLMENT_ADMISSION_MAC_DOMAIN: &[u8] =
    b"streamserver.agent-enrollment.admission.token.v1\0";
const AGENT_ENROLLMENT_HTTP_MAX_CONCURRENCY: usize = 4;
const AGENT_ENROLLMENT_HTTP_GLOBAL_BURST: u32 = 64;
const AGENT_ENROLLMENT_HTTP_GLOBAL_REFILL: StdDuration = StdDuration::from_millis(100);
const AGENT_ENROLLMENT_HTTP_IP_BURST: u32 = 8;
const AGENT_ENROLLMENT_HTTP_IP_REFILL: StdDuration = StdDuration::from_secs(2);
const AGENT_ENROLLMENT_HTTP_MAX_IP_BUCKETS: usize = 4_096;
const AGENT_ENROLLMENT_HTTP_BUCKET_IDLE_TTL: StdDuration = StdDuration::from_secs(15 * 60);

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentEnrollmentAdmissionError {
    Busy,
    RateLimited { retry_after: StdDuration },
}

pub(crate) struct AgentEnrollmentHttpPermit {
    _permit: OwnedSemaphorePermit,
}

impl fmt::Debug for AgentEnrollmentHttpPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentEnrollmentHttpPermit")
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct AgentEnrollmentIpBucket {
    tokens: f64,
    last_refill: Instant,
    last_seen: Instant,
}

#[derive(Debug)]
struct AgentEnrollmentGlobalBucket {
    tokens: f64,
    last_refill: Instant,
}

impl AgentEnrollmentGlobalBucket {
    fn new(now: Instant) -> Self {
        Self {
            tokens: f64::from(AGENT_ENROLLMENT_HTTP_GLOBAL_BURST),
            last_refill: now,
        }
    }

    fn retry_after(&mut self, now: Instant) -> Option<StdDuration> {
        let elapsed = now.saturating_duration_since(self.last_refill);
        if !elapsed.is_zero() {
            let replenished =
                elapsed.as_secs_f64() / AGENT_ENROLLMENT_HTTP_GLOBAL_REFILL.as_secs_f64();
            self.tokens =
                (self.tokens + replenished).min(f64::from(AGENT_ENROLLMENT_HTTP_GLOBAL_BURST));
            self.last_refill = now;
        }
        if self.tokens >= 1.0 {
            return None;
        }
        let retry_fraction = (1.0 - self.tokens).clamp(0.0, 1.0);
        Some(
            AGENT_ENROLLMENT_HTTP_GLOBAL_REFILL
                .mul_f64(retry_fraction)
                .max(StdDuration::from_millis(1)),
        )
    }

    fn consume(&mut self) {
        debug_assert!(self.tokens >= 1.0);
        self.tokens -= 1.0;
    }
}

#[derive(Debug)]
struct AgentEnrollmentIpLimiter {
    global: AgentEnrollmentGlobalBucket,
    buckets: HashMap<IpAddr, AgentEnrollmentIpBucket>,
    burst: u32,
    refill_interval: StdDuration,
    max_buckets: usize,
    last_maintenance: Instant,
    #[cfg(test)]
    peer_checks: usize,
    #[cfg(test)]
    maintenance_scans: usize,
    #[cfg(test)]
    maintenance_entries: usize,
}

impl AgentEnrollmentIpLimiter {
    fn admit(
        &mut self,
        peer_ip: IpAddr,
        now: Instant,
    ) -> Result<(), AgentEnrollmentAdmissionError> {
        if let Some(retry_after) = self.global.retry_after(now) {
            return Err(AgentEnrollmentAdmissionError::RateLimited { retry_after });
        }
        #[cfg(test)]
        {
            self.peer_checks = self.peer_checks.saturating_add(1);
        }
        let peer_is_new = !self.buckets.contains_key(&peer_ip);
        if peer_is_new {
            self.maintain_for_new_peer(now);
        }
        let burst = self.burst;
        let bucket = self
            .buckets
            .entry(peer_ip)
            .or_insert_with(|| AgentEnrollmentIpBucket {
                tokens: f64::from(burst),
                last_refill: now,
                last_seen: now,
            });
        let elapsed = now.saturating_duration_since(bucket.last_refill);
        if !elapsed.is_zero() {
            let replenished = elapsed.as_secs_f64() / self.refill_interval.as_secs_f64();
            bucket.tokens = (bucket.tokens + replenished).min(f64::from(self.burst));
            bucket.last_refill = now;
        }
        bucket.last_seen = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            self.global.consume();
            return Ok(());
        }
        let retry_fraction = (1.0 - bucket.tokens).clamp(0.0, 1.0);
        let retry_after = self
            .refill_interval
            .mul_f64(retry_fraction)
            .max(StdDuration::from_millis(1));
        Err(AgentEnrollmentAdmissionError::RateLimited { retry_after })
    }

    fn maintain_for_new_peer(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_maintenance)
            >= AGENT_ENROLLMENT_HTTP_BUCKET_IDLE_TTL
        {
            #[cfg(test)]
            let mut visited = 0_usize;
            self.buckets.retain(|_, bucket| {
                #[cfg(test)]
                {
                    visited = visited.saturating_add(1);
                }
                now.saturating_duration_since(bucket.last_seen)
                    < AGENT_ENROLLMENT_HTTP_BUCKET_IDLE_TTL
            });
            self.last_maintenance = now;
            #[cfg(test)]
            {
                self.maintenance_scans = self.maintenance_scans.saturating_add(1);
                self.maintenance_entries = self.maintenance_entries.saturating_add(visited);
            }
        }
        if self.buckets.len() < self.max_buckets {
            return;
        }
        #[cfg(test)]
        let visited = self.buckets.len();
        if let Some(oldest) = self
            .buckets
            .iter()
            .min_by_key(|(_, bucket)| bucket.last_seen)
            .map(|(peer, _)| *peer)
        {
            self.buckets.remove(&oldest);
        }
        #[cfg(test)]
        {
            self.maintenance_scans = self.maintenance_scans.saturating_add(1);
            self.maintenance_entries = self.maintenance_entries.saturating_add(visited);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AgentEnrollmentHttpAdmission {
    concurrency: Arc<Semaphore>,
    peers: Arc<Mutex<AgentEnrollmentIpLimiter>>,
}

impl AgentEnrollmentHttpAdmission {
    fn new() -> Self {
        Self::with_limits(
            AGENT_ENROLLMENT_HTTP_MAX_CONCURRENCY,
            AGENT_ENROLLMENT_HTTP_IP_BURST,
            AGENT_ENROLLMENT_HTTP_IP_REFILL,
            AGENT_ENROLLMENT_HTTP_MAX_IP_BUCKETS,
        )
    }

    fn with_limits(
        max_concurrency: usize,
        burst: u32,
        refill_interval: StdDuration,
        max_buckets: usize,
    ) -> Self {
        assert!(max_concurrency > 0 && max_concurrency <= 4);
        assert!(burst > 0);
        assert!(!refill_interval.is_zero());
        assert!(max_buckets > 0);
        let now = Instant::now();
        Self {
            concurrency: Arc::new(Semaphore::new(max_concurrency)),
            peers: Arc::new(Mutex::new(AgentEnrollmentIpLimiter {
                global: AgentEnrollmentGlobalBucket::new(now),
                buckets: HashMap::with_capacity(max_buckets.min(256)),
                burst,
                refill_interval,
                max_buckets,
                last_maintenance: now,
                #[cfg(test)]
                peer_checks: 0,
                #[cfg(test)]
                maintenance_scans: 0,
                #[cfg(test)]
                maintenance_entries: 0,
            })),
        }
    }

    #[cfg(test)]
    fn with_limits_for_test(
        max_concurrency: usize,
        burst: u32,
        refill_interval: StdDuration,
        max_buckets: usize,
    ) -> Self {
        Self::with_limits(max_concurrency, burst, refill_interval, max_buckets)
    }

    pub(crate) fn try_admit(
        &self,
        peer_ip: IpAddr,
    ) -> Result<AgentEnrollmentHttpPermit, AgentEnrollmentAdmissionError> {
        self.try_admit_at(peer_ip, Instant::now())
    }

    fn try_admit_at(
        &self,
        peer_ip: IpAddr,
        now: Instant,
    ) -> Result<AgentEnrollmentHttpPermit, AgentEnrollmentAdmissionError> {
        let permit = self
            .concurrency
            .clone()
            .try_acquire_owned()
            .map_err(|_| AgentEnrollmentAdmissionError::Busy)?;
        let mut peers = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        peers.admit(peer_ip, now)?;
        Ok(AgentEnrollmentHttpPermit { _permit: permit })
    }
}

#[derive(Clone)]
pub(crate) struct AgentEnrollmentTokenCodec {
    key: Arc<Zeroizing<[u8; 32]>>,
}

impl fmt::Debug for AgentEnrollmentTokenCodec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentEnrollmentTokenCodec")
            .field("key", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct VerifiedAgentEnrollmentToken {
    pub(crate) enrollment_id: Uuid,
    pub(crate) node_id: Uuid,
    pub(crate) expires_at: DateTime<Utc>,
    pub(crate) token_hash: [u8; 32],
}

impl fmt::Debug for VerifiedAgentEnrollmentToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedAgentEnrollmentToken")
            .field("enrollment_id", &self.enrollment_id)
            .field("node_id", &self.node_id)
            .field("expires_at", &self.expires_at)
            .field("token_hash", &"[REDACTED]")
            .finish()
    }
}

impl AgentEnrollmentTokenCodec {
    fn from_ca_signing_key(signing_key: &KeyPair) -> Self {
        let mut private_key_der = signing_key.serialize_der();
        let mut extract = HmacSha256::new_from_slice(AGENT_ENROLLMENT_ADMISSION_KDF_SALT)
            .expect("HMAC accepts a fixed-size derivation salt");
        extract.update(&private_key_der);
        let mut pseudorandom_key = extract.finalize().into_bytes();
        private_key_der.zeroize();

        let mut expand = HmacSha256::new_from_slice(&pseudorandom_key)
            .expect("HMAC accepts a SHA-256 pseudorandom key");
        expand.update(AGENT_ENROLLMENT_ADMISSION_KDF_INFO);
        let mut derived = expand.finalize().into_bytes();
        pseudorandom_key.zeroize();
        let mut key = [0_u8; 32];
        key.copy_from_slice(&derived);
        derived.zeroize();
        let protected_key = Zeroizing::new(key);
        key.zeroize();
        Self {
            key: Arc::new(protected_key),
        }
    }

    pub(crate) fn issue(
        &self,
        enrollment_id: Uuid,
        node_id: Uuid,
        expires_at: DateTime<Utc>,
    ) -> Result<Zeroizing<String>, AgentIdentityServiceError> {
        if enrollment_id.is_nil() || node_id.is_nil() {
            return Err(AgentIdentityServiceError::InvalidEnrollment);
        }
        let mut payload = [0_u8; AGENT_ENROLLMENT_TOKEN_PAYLOAD_LEN];
        payload[..16].copy_from_slice(enrollment_id.as_bytes());
        payload[16..32].copy_from_slice(node_id.as_bytes());
        payload[32..40].copy_from_slice(&expires_at.timestamp().to_be_bytes());
        OsRng.fill_bytes(&mut payload[40..]);

        let payload_b64 = Zeroizing::new(URL_SAFE_NO_PAD.encode(payload));
        let mut mac = HmacSha256::new_from_slice(self.key.as_ref().as_ref())
            .expect("HMAC accepts the derived admission key");
        mac.update(AGENT_ENROLLMENT_ADMISSION_MAC_DOMAIN);
        mac.update(&payload);
        let mut tag = mac.finalize().into_bytes();
        let tag_b64 = Zeroizing::new(URL_SAFE_NO_PAD.encode(&tag[..]));
        tag.zeroize();
        payload.zeroize();
        Ok(Zeroizing::new(format!(
            "{AGENT_ENROLLMENT_TOKEN_PREFIX}.{}.{}",
            payload_b64.as_str(),
            tag_b64.as_str()
        )))
    }

    pub(crate) fn verify(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<VerifiedAgentEnrollmentToken, AgentIdentityServiceError> {
        if !self.has_strict_wire_shape(token) {
            return Err(AgentIdentityServiceError::InvalidEnrollment);
        }
        let mut segments = token.split('.');
        let prefix = segments.next();
        let payload_text = segments.next();
        let tag_text = segments.next();
        if prefix != Some(AGENT_ENROLLMENT_TOKEN_PREFIX)
            || segments.next().is_some()
            || payload_text.is_none()
            || tag_text.is_none()
        {
            return Err(AgentIdentityServiceError::InvalidEnrollment);
        }
        let payload_text = payload_text.expect("checked above");
        let tag_text = tag_text.expect("checked above");
        let mut payload = URL_SAFE_NO_PAD
            .decode(payload_text)
            .map_err(|_| AgentIdentityServiceError::InvalidEnrollment)?;
        let mut tag = URL_SAFE_NO_PAD
            .decode(tag_text)
            .map_err(|_| AgentIdentityServiceError::InvalidEnrollment)?;
        let canonical_payload = Zeroizing::new(URL_SAFE_NO_PAD.encode(&payload));
        let canonical_tag = Zeroizing::new(URL_SAFE_NO_PAD.encode(&tag));
        let canonical = payload.len() == AGENT_ENROLLMENT_TOKEN_PAYLOAD_LEN
            && tag.len() == 32
            && canonical_payload.as_str() == payload_text
            && canonical_tag.as_str() == tag_text;
        if !canonical {
            payload.zeroize();
            tag.zeroize();
            return Err(AgentIdentityServiceError::InvalidEnrollment);
        }

        let mut mac = HmacSha256::new_from_slice(self.key.as_ref().as_ref())
            .expect("HMAC accepts the derived admission key");
        mac.update(AGENT_ENROLLMENT_ADMISSION_MAC_DOMAIN);
        mac.update(&payload);
        if mac.verify_slice(&tag).is_err() {
            payload.zeroize();
            tag.zeroize();
            return Err(AgentIdentityServiceError::InvalidEnrollment);
        }
        tag.zeroize();

        let parsed = (|| {
            let enrollment_id = Uuid::from_slice(&payload[..16])
                .map_err(|_| AgentIdentityServiceError::InvalidEnrollment)?;
            let node_id = Uuid::from_slice(&payload[16..32])
                .map_err(|_| AgentIdentityServiceError::InvalidEnrollment)?;
            let mut expiry_bytes = Zeroizing::new([0_u8; 8]);
            expiry_bytes.copy_from_slice(&payload[32..40]);
            let expires_at = DateTime::from_timestamp(i64::from_be_bytes(*expiry_bytes), 0)
                .ok_or(AgentIdentityServiceError::InvalidEnrollment)?;
            Ok::<_, AgentIdentityServiceError>((enrollment_id, node_id, expires_at))
        })();
        payload.zeroize();
        let (enrollment_id, node_id, expires_at) = parsed?;
        if enrollment_id.is_nil() || node_id.is_nil() || expires_at <= now {
            return Err(AgentIdentityServiceError::InvalidEnrollment);
        }
        Ok(VerifiedAgentEnrollmentToken {
            enrollment_id,
            node_id,
            expires_at,
            token_hash: sha256_array(token.as_bytes()),
        })
    }

    fn has_strict_wire_shape(&self, token: &str) -> bool {
        let bytes = token.as_bytes();
        bytes.len() == 146
            && bytes.starts_with(b"ssae1.")
            && bytes[102] == b'.'
            && bytes[6..102]
                .iter()
                .chain(bytes[103..].iter())
                .all(|value| value.is_ascii_alphanumeric() || matches!(*value, b'-' | b'_'))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthenticatedAgentPeer {
    pub(crate) node_id: Uuid,
    pub(crate) fingerprint_sha256: [u8; 32],
    pub(crate) not_before: DateTime<Utc>,
    pub(crate) not_after: DateTime<Utc>,
    pub(crate) peer_ip: IpAddr,
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
pub(crate) enum AgentPeerCertificateError {
    #[error("Agent peer certificate is invalid")]
    Invalid,
    #[error("Agent peer certificate is not currently valid")]
    NotCurrentlyValid,
    #[error("Agent peer certificate identity is invalid")]
    InvalidIdentity,
    #[error("Agent peer certificate usage is invalid")]
    InvalidUsage,
}

pub(crate) fn parse_authenticated_agent_peer(
    leaf_der: &[u8],
    peer_ip: IpAddr,
    now: DateTime<Utc>,
) -> Result<AuthenticatedAgentPeer, AgentPeerCertificateError> {
    let (remaining, certificate) = x509_parser::certificate::X509Certificate::from_der(leaf_der)
        .map_err(|_| AgentPeerCertificateError::Invalid)?;
    if !remaining.is_empty() {
        return Err(AgentPeerCertificateError::Invalid);
    }
    let validation_time = ASN1Time::from_timestamp(now.timestamp())
        .map_err(|_| AgentPeerCertificateError::Invalid)?;
    if !certificate.validity().is_valid_at(validation_time) {
        return Err(AgentPeerCertificateError::NotCurrentlyValid);
    }

    let san = certificate
        .subject_alternative_name()
        .map_err(|_| AgentPeerCertificateError::InvalidIdentity)?
        .ok_or(AgentPeerCertificateError::InvalidIdentity)?;
    let [x509_parser::extensions::GeneralName::URI(identity)] = san.value.general_names.as_slice()
    else {
        return Err(AgentPeerCertificateError::InvalidIdentity);
    };
    let node_id_text = identity
        .strip_prefix("spiffe://streamserver/agent/")
        .ok_or(AgentPeerCertificateError::InvalidIdentity)?;
    let node_id =
        Uuid::parse_str(node_id_text).map_err(|_| AgentPeerCertificateError::InvalidIdentity)?;
    if node_id.is_nil() || *identity != format!("spiffe://streamserver/agent/{node_id}") {
        return Err(AgentPeerCertificateError::InvalidIdentity);
    }

    let basic_constraints = certificate
        .basic_constraints()
        .map_err(|_| AgentPeerCertificateError::InvalidUsage)?;
    if basic_constraints.is_some_and(|extension| extension.value.ca) {
        return Err(AgentPeerCertificateError::InvalidUsage);
    }
    let key_usage = certificate
        .key_usage()
        .map_err(|_| AgentPeerCertificateError::InvalidUsage)?
        .ok_or(AgentPeerCertificateError::InvalidUsage)?;
    if key_usage.value.flags != 1 || !key_usage.value.digital_signature() {
        return Err(AgentPeerCertificateError::InvalidUsage);
    }
    let extended = certificate
        .extended_key_usage()
        .map_err(|_| AgentPeerCertificateError::InvalidUsage)?
        .ok_or(AgentPeerCertificateError::InvalidUsage)?;
    if !extended.value.client_auth
        || extended.value.any
        || extended.value.server_auth
        || extended.value.code_signing
        || extended.value.email_protection
        || extended.value.time_stamping
        || extended.value.ocsp_signing
        || !extended.value.other.is_empty()
    {
        return Err(AgentPeerCertificateError::InvalidUsage);
    }

    let not_before = DateTime::from_timestamp(certificate.validity().not_before.timestamp(), 0)
        .ok_or(AgentPeerCertificateError::Invalid)?;
    let not_after = DateTime::from_timestamp(certificate.validity().not_after.timestamp(), 0)
        .ok_or(AgentPeerCertificateError::Invalid)?;
    Ok(AuthenticatedAgentPeer {
        node_id,
        fingerprint_sha256: agent_certificate_fingerprint_sha256(leaf_der),
        not_before,
        not_after,
        peer_ip,
    })
}

pub(crate) fn agent_certificate_fingerprint_sha256(leaf_der: &[u8]) -> [u8; 32] {
    sha256_array(leaf_der)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentIdentityServiceError {
    #[error("agent identity is already active")]
    IdentityAlreadyActive,
    #[error("agent identity is revoked")]
    IdentityRevoked,
    #[error("invalid or expired agent enrollment")]
    InvalidEnrollment,
    #[error("agent CSR is invalid")]
    InvalidCsr,
    #[error("agent certificate signing failed")]
    CertificateSigning,
    #[error("agent certificate rotation is not authorized")]
    InvalidRotation,
    #[error("agent certificate rotation bundle expired before activation")]
    RotationExpired,
    #[error(transparent)]
    Repository(#[from] RepoError),
}

pub(crate) struct CreatedAgentEnrollment {
    pub(crate) enrollment_id: Uuid,
    pub(crate) node_id: Uuid,
    pub(crate) token: Zeroizing<String>,
    pub(crate) expires_at: DateTime<Utc>,
}

impl fmt::Debug for CreatedAgentEnrollment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CreatedAgentEnrollment")
            .field("enrollment_id", &self.enrollment_id)
            .field("node_id", &self.node_id)
            .field("token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

pub(crate) struct CompletedAgentEnrollment {
    pub(crate) node_id: Uuid,
    pub(crate) certificate_pem: String,
    pub(crate) ca_certificate_pem: String,
    pub(crate) agent_client_issuer_ca_pem: String,
    pub(crate) control_plane_server_ca_pem: String,
    pub(crate) management_client_ca_pem: String,
    pub(crate) fingerprint_sha256: String,
    pub(crate) serial_number: String,
    pub(crate) not_before: DateTime<Utc>,
    pub(crate) not_after: DateTime<Utc>,
    pub(crate) management_certificate_pem: String,
    pub(crate) management_fingerprint_sha256: String,
    pub(crate) management_serial_number: String,
    pub(crate) management_not_before: DateTime<Utc>,
    pub(crate) management_not_after: DateTime<Utc>,
    pub(crate) capability_jwt_public_key_pem: String,
    pub(crate) capability_jwt_kid: String,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CompletedAgentCertificateRotation {
    pub(crate) rotation_id: Uuid,
    pub(crate) expires_at: DateTime<Utc>,
    pub(crate) control_certificate_pem: String,
    pub(crate) control_fingerprint_sha256: String,
    pub(crate) control_serial_number: String,
    pub(crate) control_not_before: DateTime<Utc>,
    pub(crate) control_not_after: DateTime<Utc>,
    pub(crate) management_certificate_pem: String,
    pub(crate) management_fingerprint_sha256: String,
    pub(crate) management_serial_number: String,
    pub(crate) management_not_before: DateTime<Utc>,
    pub(crate) management_not_after: DateTime<Utc>,
    pub(crate) agent_client_issuer_ca_pem: String,
    pub(crate) control_plane_server_ca_pem: String,
    pub(crate) management_client_ca_pem: String,
    pub(crate) capability_jwt_public_key_pem: String,
    pub(crate) capability_jwt_kid: String,
}

impl fmt::Debug for CompletedAgentCertificateRotation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompletedAgentCertificateRotation")
            .field("rotation_id", &self.rotation_id)
            .field("expires_at", &self.expires_at)
            .field("control_certificate_pem", &"[REDACTED]")
            .field(
                "control_fingerprint_sha256",
                &self.control_fingerprint_sha256,
            )
            .field("control_serial_number", &self.control_serial_number)
            .field("control_not_before", &self.control_not_before)
            .field("control_not_after", &self.control_not_after)
            .field("management_certificate_pem", &"[REDACTED]")
            .field(
                "management_fingerprint_sha256",
                &self.management_fingerprint_sha256,
            )
            .field("management_serial_number", &self.management_serial_number)
            .field("management_not_before", &self.management_not_before)
            .field("management_not_after", &self.management_not_after)
            .field("agent_client_issuer_ca_pem", &"[REDACTED]")
            .field("control_plane_server_ca_pem", &"[REDACTED]")
            .field("management_client_ca_pem", &"[REDACTED]")
            .field("capability_jwt_public_key_pem", &"[REDACTED]")
            .field("capability_jwt_kid", &self.capability_jwt_kid)
            .finish()
    }
}

impl fmt::Debug for CompletedAgentEnrollment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompletedAgentEnrollment")
            .field("node_id", &self.node_id)
            .field("certificate_pem", &"[REDACTED]")
            .field("ca_certificate_pem", &"[REDACTED]")
            .field("agent_client_issuer_ca_pem", &"[REDACTED]")
            .field("control_plane_server_ca_pem", &"[REDACTED]")
            .field("management_client_ca_pem", &"[REDACTED]")
            .field("fingerprint_sha256", &self.fingerprint_sha256)
            .field("serial_number", &self.serial_number)
            .field("not_before", &self.not_before)
            .field("not_after", &self.not_after)
            .field("management_certificate_pem", &"[REDACTED]")
            .field(
                "management_fingerprint_sha256",
                &self.management_fingerprint_sha256,
            )
            .field("management_serial_number", &self.management_serial_number)
            .field("management_not_before", &self.management_not_before)
            .field("management_not_after", &self.management_not_after)
            .field("capability_jwt_public_key_pem", &"[REDACTED]")
            .field("capability_jwt_kid", &self.capability_jwt_kid)
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct AgentEnrollmentPublicConfig {
    pub(crate) control_plane_server_ca_pem: String,
    pub(crate) management_client_ca_pem: String,
    pub(crate) capability_jwt_public_key_pem: String,
    pub(crate) capability_jwt_kid: String,
}

impl fmt::Debug for AgentEnrollmentPublicConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentEnrollmentPublicConfig")
            .field("control_plane_server_ca_pem", &"[REDACTED]")
            .field("management_client_ca_pem", &"[REDACTED]")
            .field("capability_jwt_public_key_pem", &"[REDACTED]")
            .field("capability_jwt_kid", &self.capability_jwt_kid)
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct AgentIdentityService {
    repository: Arc<TaskRepository>,
    authority: Arc<AgentCertificateAuthority>,
    public_config: Arc<AgentEnrollmentPublicConfig>,
    enrollment_token_codec: AgentEnrollmentTokenCodec,
    http_admission: AgentEnrollmentHttpAdmission,
}

impl fmt::Debug for AgentIdentityService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentIdentityService")
            .field("authority", &self.authority)
            .field("public_config", &self.public_config)
            .finish_non_exhaustive()
    }
}

impl AgentIdentityService {
    pub(crate) fn new(
        repository: Arc<TaskRepository>,
        authority: AgentCertificateAuthority,
        public_config: AgentEnrollmentPublicConfig,
    ) -> Self {
        let enrollment_token_codec = authority.enrollment_token_codec();
        Self {
            repository,
            authority: Arc::new(authority),
            public_config: Arc::new(public_config),
            enrollment_token_codec,
            http_admission: AgentEnrollmentHttpAdmission::new(),
        }
    }

    pub(crate) fn verify_enrollment_token(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<VerifiedAgentEnrollmentToken, AgentIdentityServiceError> {
        self.enrollment_token_codec.verify(token, now)
    }

    pub(crate) fn try_admit_http(
        &self,
        peer_ip: IpAddr,
    ) -> Result<AgentEnrollmentHttpPermit, AgentEnrollmentAdmissionError> {
        self.http_admission.try_admit(peer_ip)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create_enrollment(
        &self,
        node_id: Uuid,
        actor: &str,
        remote_ip: Option<IpAddr>,
        user_agent: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<CreatedAgentEnrollment, AgentIdentityServiceError> {
        let enrollment_id = Uuid::now_v7();
        let expires_at = now + AGENT_ENROLLMENT_TOKEN_TTL;
        let mut token = self
            .enrollment_token_codec
            .issue(enrollment_id, node_id, expires_at)?;
        let outcome = match self
            .repository
            .create_agent_enrollment(NewAgentEnrollment {
                id: enrollment_id,
                node_id,
                token_hash: sha256_array(token.as_bytes()),
                created_by: actor.to_string(),
                created_at: now,
                expires_at,
                remote_ip,
                user_agent,
            })
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                token.zeroize();
                return Err(error.into());
            }
        };
        match outcome {
            CreateAgentEnrollmentOutcome::Created { expires_at } => Ok(CreatedAgentEnrollment {
                enrollment_id,
                node_id,
                token,
                expires_at,
            }),
            CreateAgentEnrollmentOutcome::IdentityAlreadyActive => {
                token.zeroize();
                Err(AgentIdentityServiceError::IdentityAlreadyActive)
            }
            CreateAgentEnrollmentOutcome::IdentityRevoked => {
                token.zeroize();
                Err(AgentIdentityServiceError::IdentityRevoked)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn enroll_verified(
        &self,
        verified: &VerifiedAgentEnrollmentToken,
        node_id: Uuid,
        csr_pem: &str,
        management_csr_pem: &str,
        remote_ip: Option<IpAddr>,
        user_agent: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<CompletedAgentEnrollment, AgentIdentityServiceError> {
        if verified.node_id != node_id || verified.expires_at <= now {
            return Err(AgentIdentityServiceError::InvalidEnrollment);
        }
        if self
            .repository
            .preflight_agent_enrollment(verified.enrollment_id, &verified.token_hash, node_id)
            .await?
            != AgentEnrollmentPreflightOutcome::Admissible
        {
            return Err(AgentIdentityServiceError::InvalidEnrollment);
        }
        let control_public_key_sha256 = csr_public_key_sha256(csr_pem)?;
        let management_public_key_sha256 = csr_public_key_sha256(management_csr_pem)?;
        if control_public_key_sha256 == management_public_key_sha256 {
            return Err(AgentIdentityServiceError::InvalidCsr);
        }
        let authority = self.authority.clone();
        let public_config = self.public_config.clone();
        let outcome = self
            .repository
            .consume_agent_enrollment(
                &verified.token_hash,
                AgentEnrollmentRequest {
                    node_id,
                    control_csr_public_key_sha256: control_public_key_sha256,
                    management_csr_public_key_sha256: management_public_key_sha256,
                    attempted_at: now,
                    remote_ip,
                    user_agent,
                },
                |decision_at| {
                    let control = authority
                        .issue_agent_certificate(node_id, csr_pem, decision_at)
                        .map_err(map_certificate_issue_error)?;
                    let management = authority
                        .issue_agent_management_certificate(
                            node_id,
                            management_csr_pem,
                            decision_at,
                        )
                        .map_err(map_certificate_issue_error)?;
                    Ok::<AgentEnrollmentBundle, AgentIdentityServiceError>(AgentEnrollmentBundle {
                        node_id,
                        control_certificate: repository_certificate(control),
                        management_certificate: repository_certificate(management),
                        agent_client_issuer_ca_pem: authority.certificate_pem.clone(),
                        control_plane_server_ca_pem: public_config
                            .control_plane_server_ca_pem
                            .clone(),
                        management_client_ca_pem: public_config.management_client_ca_pem.clone(),
                        capability_jwt_public_key_pem: public_config
                            .capability_jwt_public_key_pem
                            .clone(),
                        capability_jwt_kid: public_config.capability_jwt_kid.clone(),
                    })
                },
            )
            .await?;
        let bundle = match outcome {
            ConsumeAgentEnrollmentOutcome::Issued(bundle)
            | ConsumeAgentEnrollmentOutcome::Recovered(bundle) => bundle,
            ConsumeAgentEnrollmentOutcome::Invalid => {
                return Err(AgentIdentityServiceError::InvalidEnrollment);
            }
        };
        Ok(completed_agent_enrollment(bundle))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn rotate_agent_certificates(
        &self,
        rotation_id: Uuid,
        node_id: Uuid,
        session_id: Uuid,
        control_csr_pem: &str,
        management_csr_pem: &str,
        remote_ip: IpAddr,
        now: DateTime<Utc>,
    ) -> Result<CompletedAgentCertificateRotation, AgentIdentityServiceError> {
        if rotation_id.is_nil() || node_id.is_nil() || session_id.is_nil() {
            return Err(AgentIdentityServiceError::InvalidRotation);
        }
        let control_public_key_sha256 = csr_public_key_sha256(control_csr_pem)?;
        let management_public_key_sha256 = csr_public_key_sha256(management_csr_pem)?;
        if control_public_key_sha256 == management_public_key_sha256 {
            return Err(AgentIdentityServiceError::InvalidCsr);
        }
        let authority = self.authority.clone();
        let public_config = self.public_config.clone();
        let outcome = self
            .repository
            .stage_agent_certificate_rotation(
                AgentCertificateRotationRequest {
                    rotation_id,
                    node_id,
                    session_id,
                    control_csr_public_key_sha256: control_public_key_sha256,
                    management_csr_public_key_sha256: management_public_key_sha256,
                    requested_at: now,
                    remote_ip,
                },
                |decision_at| {
                    let control = authority
                        .issue_agent_certificate(node_id, control_csr_pem, decision_at)
                        .map_err(map_certificate_issue_error)?;
                    let management = authority
                        .issue_agent_management_certificate(
                            node_id,
                            management_csr_pem,
                            decision_at,
                        )
                        .map_err(map_certificate_issue_error)?;
                    Ok::<AgentCertificateRotationIssue, AgentIdentityServiceError>(
                        AgentCertificateRotationIssue {
                            control_certificate: repository_certificate(control),
                            management_certificate: repository_certificate(management),
                        },
                    )
                },
            )
            .await?;
        let bundle = match outcome {
            StageAgentCertificateRotationOutcome::Issued(bundle)
            | StageAgentCertificateRotationOutcome::Recovered(bundle) => bundle,
            StageAgentCertificateRotationOutcome::Expired => {
                return Err(AgentIdentityServiceError::RotationExpired);
            }
            StageAgentCertificateRotationOutcome::Rejected => {
                return Err(AgentIdentityServiceError::InvalidRotation);
            }
        };
        Ok(completed_agent_certificate_rotation(
            bundle,
            &authority.certificate_pem,
            &public_config,
        ))
    }
}

fn csr_public_key_sha256(csr_pem: &str) -> Result<[u8; 32], AgentIdentityServiceError> {
    CertificateSigningRequestParams::from_pem(csr_pem)
        .map_err(|_| AgentIdentityServiceError::InvalidCsr)?;
    let (remaining, pem) =
        parse_x509_pem(csr_pem.as_bytes()).map_err(|_| AgentIdentityServiceError::InvalidCsr)?;
    if !remaining.iter().all(u8::is_ascii_whitespace) {
        return Err(AgentIdentityServiceError::InvalidCsr);
    }
    let (der_remaining, csr) =
        x509_parser::certification_request::X509CertificationRequest::from_der(&pem.contents)
            .map_err(|_| AgentIdentityServiceError::InvalidCsr)?;
    if !der_remaining.is_empty() {
        return Err(AgentIdentityServiceError::InvalidCsr);
    }
    Ok(sha256_array(csr.certification_request_info.subject_pki.raw))
}

fn map_certificate_issue_error(error: AgentCertificateIssueError) -> AgentIdentityServiceError {
    match error {
        AgentCertificateIssueError::InvalidCsr => AgentIdentityServiceError::InvalidCsr,
        AgentCertificateIssueError::Signing => AgentIdentityServiceError::CertificateSigning,
    }
}

fn repository_certificate(issued: IssuedAgentCertificate) -> NewIssuedAgentCertificate {
    NewIssuedAgentCertificate {
        id: Uuid::now_v7(),
        serial_number: issued.serial_number,
        fingerprint_sha256: issued.fingerprint_sha256,
        public_key_sha256: issued.public_key_sha256,
        certificate_pem: issued.certificate_pem,
        not_before: issued.not_before,
        not_after: issued.not_after,
    }
}

fn completed_agent_enrollment(bundle: AgentEnrollmentBundle) -> CompletedAgentEnrollment {
    let AgentEnrollmentBundle {
        node_id,
        control_certificate,
        management_certificate,
        agent_client_issuer_ca_pem,
        control_plane_server_ca_pem,
        management_client_ca_pem,
        capability_jwt_public_key_pem,
        capability_jwt_kid,
    } = bundle;
    CompletedAgentEnrollment {
        node_id,
        certificate_pem: control_certificate.certificate_pem,
        ca_certificate_pem: agent_client_issuer_ca_pem.clone(),
        agent_client_issuer_ca_pem,
        control_plane_server_ca_pem,
        management_client_ca_pem,
        fingerprint_sha256: hex_lower(&control_certificate.fingerprint_sha256),
        serial_number: control_certificate.serial_number,
        not_before: control_certificate.not_before,
        not_after: control_certificate.not_after,
        management_certificate_pem: management_certificate.certificate_pem,
        management_fingerprint_sha256: hex_lower(&management_certificate.fingerprint_sha256),
        management_serial_number: management_certificate.serial_number,
        management_not_before: management_certificate.not_before,
        management_not_after: management_certificate.not_after,
        capability_jwt_public_key_pem,
        capability_jwt_kid,
    }
}

fn completed_agent_certificate_rotation(
    bundle: AgentCertificateRotationBundle,
    agent_client_issuer_ca_pem: &str,
    public_config: &AgentEnrollmentPublicConfig,
) -> CompletedAgentCertificateRotation {
    CompletedAgentCertificateRotation {
        rotation_id: bundle.rotation_id,
        expires_at: bundle.authorized_until,
        control_certificate_pem: bundle.control_certificate.certificate_pem,
        control_fingerprint_sha256: hex_lower(&bundle.control_certificate.fingerprint_sha256),
        control_serial_number: bundle.control_certificate.serial_number,
        control_not_before: bundle.control_certificate.not_before,
        control_not_after: bundle.control_certificate.not_after,
        management_certificate_pem: bundle.management_certificate.certificate_pem,
        management_fingerprint_sha256: hex_lower(&bundle.management_certificate.fingerprint_sha256),
        management_serial_number: bundle.management_certificate.serial_number,
        management_not_before: bundle.management_certificate.not_before,
        management_not_after: bundle.management_certificate.not_after,
        agent_client_issuer_ca_pem: agent_client_issuer_ca_pem.to_string(),
        control_plane_server_ca_pem: public_config.control_plane_server_ca_pem.clone(),
        management_client_ca_pem: public_config.management_client_ca_pem.clone(),
        capability_jwt_public_key_pem: public_config.capability_jwt_public_key_pem.clone(),
        capability_jwt_kid: public_config.capability_jwt_kid.clone(),
    }
}

pub(crate) struct IssuedAgentCertificate {
    pub(crate) certificate_pem: String,
    pub(crate) serial_number: String,
    pub(crate) fingerprint_sha256: [u8; 32],
    pub(crate) public_key_sha256: [u8; 32],
    pub(crate) not_before: DateTime<Utc>,
    pub(crate) not_after: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
pub(crate) enum AgentCertificateIssueError {
    #[error("Agent CSR is invalid")]
    InvalidCsr,
    #[error("Agent certificate signing failed")]
    Signing,
}

pub(crate) struct AgentCertificateAuthority {
    signing_certificate: Certificate,
    signing_key: KeyPair,
    enrollment_token_codec: AgentEnrollmentTokenCodec,
    certificate_pem: String,
    fingerprint_sha256: [u8; 32],
    not_before: DateTime<Utc>,
    not_after: DateTime<Utc>,
    certificate_path: Option<String>,
}

impl fmt::Debug for AgentCertificateAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentCertificateAuthority")
            .field("fingerprint_sha256", &hex_lower(&self.fingerprint_sha256))
            .field("not_before", &self.not_before)
            .field("not_after", &self.not_after)
            .field("certificate_path", &self.certificate_path)
            .field("private_key", &"[REDACTED]")
            .finish()
    }
}

impl Drop for AgentCertificateAuthority {
    fn drop(&mut self) {
        self.signing_key.zeroize();
    }
}

impl AgentCertificateAuthority {
    pub(crate) fn certificate_pem(&self) -> &str {
        &self.certificate_pem
    }

    pub(crate) fn enrollment_token_codec(&self) -> AgentEnrollmentTokenCodec {
        self.enrollment_token_codec.clone()
    }

    pub(crate) fn from_paths(
        certificate_path: &Path,
        private_key_path: &Path,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Self> {
        let certificate_pem = fs::read_to_string(certificate_path).with_context(|| {
            format!(
                "failed to read Agent CA certificate {}",
                certificate_path.display()
            )
        })?;
        let mut private_key_pem = fs::read_to_string(private_key_path).with_context(|| {
            format!(
                "failed to read Agent CA private key {}",
                private_key_path.display()
            )
        })?;
        let result = Self::from_pem(
            certificate_pem,
            &private_key_pem,
            now,
            Some(certificate_path.display().to_string()),
        );
        private_key_pem.zeroize();
        result
    }

    pub(crate) fn ensure_present_in_client_ca_bundle(
        &self,
        client_ca_bundle_path: &Path,
    ) -> anyhow::Result<()> {
        let bundle = fs::read(client_ca_bundle_path).with_context(|| {
            format!(
                "failed to read gRPC client CA bundle {}",
                client_ca_bundle_path.display()
            )
        })?;
        let mut found = false;
        for block in x509_parser::pem::Pem::iter_from_buffer(&bundle) {
            let block = block.map_err(|_| {
                anyhow::anyhow!(
                    "gRPC client CA bundle {} contains invalid PEM",
                    client_ca_bundle_path.display()
                )
            })?;
            if block.label == "CERTIFICATE"
                && sha256_array(&block.contents) == self.fingerprint_sha256
            {
                found = true;
            }
        }
        anyhow::ensure!(
            found,
            "Agent signing CA certificate is not present in gRPC client CA bundle {}",
            client_ca_bundle_path.display()
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn from_pem_for_test(
        certificate_pem: String,
        mut private_key_pem: String,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Self> {
        let result = Self::from_pem(certificate_pem, &private_key_pem, now, None);
        private_key_pem.zeroize();
        result
    }

    fn from_pem(
        certificate_pem: String,
        private_key_pem: &str,
        now: DateTime<Utc>,
        certificate_path: Option<String>,
    ) -> anyhow::Result<Self> {
        let (remaining, pem) = parse_x509_pem(certificate_pem.as_bytes())
            .map_err(|_| anyhow::anyhow!("Agent CA certificate is not valid PEM"))?;
        anyhow::ensure!(
            remaining.iter().all(u8::is_ascii_whitespace),
            "Agent CA certificate file must contain exactly one PEM certificate"
        );
        anyhow::ensure!(
            pem.label == "CERTIFICATE",
            "Agent CA PEM is not a certificate"
        );
        let (der_remaining, parsed) =
            x509_parser::certificate::X509Certificate::from_der(&pem.contents)
                .map_err(|_| anyhow::anyhow!("Agent CA certificate is not valid X.509"))?;
        anyhow::ensure!(
            der_remaining.is_empty(),
            "Agent CA certificate contains trailing DER data"
        );
        let basic_constraints = parsed
            .basic_constraints()
            .map_err(|_| anyhow::anyhow!("Agent CA BasicConstraints is malformed"))?
            .ok_or_else(|| anyhow::anyhow!("Agent CA requires BasicConstraints CA:TRUE"))?;
        anyhow::ensure!(
            basic_constraints.value.ca,
            "Agent CA requires BasicConstraints CA:TRUE"
        );
        let key_usage = parsed
            .key_usage()
            .map_err(|_| anyhow::anyhow!("Agent CA key usage is malformed"))?
            .ok_or_else(|| anyhow::anyhow!("Agent CA requires keyCertSign key usage"))?;
        anyhow::ensure!(
            key_usage.value.flags == (1 << 5) && key_usage.value.key_cert_sign(),
            "Agent CA requires keyCertSign-only key usage"
        );
        anyhow::ensure!(
            parsed
                .extended_key_usage()
                .map_err(|_| anyhow::anyhow!("Agent CA EKU is malformed"))?
                .is_none(),
            "Agent CA must not contain EKU"
        );
        let validation_time = ASN1Time::from_timestamp(now.timestamp())
            .map_err(|_| anyhow::anyhow!("Agent CA validation time is invalid"))?;
        anyhow::ensure!(
            parsed.validity().is_valid_at(validation_time),
            "Agent CA certificate is not currently valid"
        );
        anyhow::ensure!(
            parsed.validity().not_before.timestamp()
                <= (now - AGENT_CERTIFICATE_CLOCK_SKEW).timestamp()
                && parsed.validity().not_after.timestamp()
                    >= (now + AGENT_CERTIFICATE_VALIDITY).timestamp(),
            "Agent CA must cover the full 90-day Agent leaf window including clock skew"
        );
        anyhow::ensure!(
            parsed.subject() == parsed.issuer(),
            "Agent CA must be a single self-signed root certificate"
        );
        parsed
            .verify_signature(None)
            .map_err(|_| anyhow::anyhow!("Agent CA self-signature is invalid"))?;

        let mut signing_key = KeyPair::from_pem(private_key_pem)
            .map_err(|_| anyhow::anyhow!("Agent CA private key is invalid"))?;
        let signing_certificate = (|| {
            anyhow::ensure!(
                parsed.public_key().raw == signing_key.public_key_der(),
                "Agent CA certificate and private key do not match"
            );
            let params = CertificateParams::from_ca_cert_pem(&certificate_pem)
                .map_err(|_| anyhow::anyhow!("Agent CA certificate parameters are invalid"))?;
            params
                .self_signed(&signing_key)
                .map_err(|_| anyhow::anyhow!("Agent CA signing identity is invalid"))
        })();
        let signing_certificate = match signing_certificate {
            Ok(certificate) => certificate,
            Err(error) => {
                signing_key.zeroize();
                return Err(error);
            }
        };
        let fingerprint_sha256 = sha256_array(&pem.contents);
        let not_before = DateTime::from_timestamp(parsed.validity().not_before.timestamp(), 0)
            .ok_or_else(|| anyhow::anyhow!("Agent CA not-before is out of range"))?;
        let not_after = DateTime::from_timestamp(parsed.validity().not_after.timestamp(), 0)
            .ok_or_else(|| anyhow::anyhow!("Agent CA not-after is out of range"))?;
        let enrollment_token_codec = AgentEnrollmentTokenCodec::from_ca_signing_key(&signing_key);

        Ok(Self {
            signing_certificate,
            signing_key,
            enrollment_token_codec,
            certificate_pem,
            fingerprint_sha256,
            not_before,
            not_after,
            certificate_path,
        })
    }

    pub(crate) fn issue_agent_certificate(
        &self,
        node_id: Uuid,
        csr_pem: &str,
        now: DateTime<Utc>,
    ) -> Result<IssuedAgentCertificate, AgentCertificateIssueError> {
        self.issue_certificate(
            node_id,
            csr_pem,
            now,
            AgentCertificateProfile::Control,
            None,
        )
    }

    pub(crate) fn issue_agent_management_certificate(
        &self,
        node_id: Uuid,
        csr_pem: &str,
        now: DateTime<Utc>,
    ) -> Result<IssuedAgentCertificate, AgentCertificateIssueError> {
        self.issue_certificate(
            node_id,
            csr_pem,
            now,
            AgentCertificateProfile::Management,
            None,
        )
    }

    #[cfg(test)]
    fn issue_agent_certificate_with_serial_for_test(
        &self,
        node_id: Uuid,
        csr_pem: &str,
        now: DateTime<Utc>,
        serial: &[u8],
    ) -> Result<IssuedAgentCertificate, AgentCertificateIssueError> {
        self.issue_certificate(
            node_id,
            csr_pem,
            now,
            AgentCertificateProfile::Control,
            Some(serial),
        )
    }

    fn issue_certificate(
        &self,
        node_id: Uuid,
        csr_pem: &str,
        now: DateTime<Utc>,
        profile: AgentCertificateProfile,
        forced_serial: Option<&[u8]>,
    ) -> Result<IssuedAgentCertificate, AgentCertificateIssueError> {
        if self.not_before.timestamp() > (now - AGENT_CERTIFICATE_CLOCK_SKEW).timestamp()
            || self.not_after.timestamp() < (now + AGENT_CERTIFICATE_VALIDITY).timestamp()
        {
            return Err(AgentCertificateIssueError::Signing);
        }
        let mut csr = CertificateSigningRequestParams::from_pem(csr_pem)
            .map_err(|_| AgentCertificateIssueError::InvalidCsr)?;

        // Never inherit identity or authorization extensions from the caller.  The
        // CSR contributes only its verified public key.
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        match profile {
            AgentCertificateProfile::Control => {
                params
                    .distinguished_name
                    .push(DnType::CommonName, format!("StreamServer Agent {node_id}"));
                params.subject_alt_names = vec![SanType::URI(
                    format!("spiffe://streamserver/agent/{node_id}")
                        .try_into()
                        .map_err(|_| AgentCertificateIssueError::Signing)?,
                )];
                params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
            }
            AgentCertificateProfile::Management => {
                params.distinguished_name.push(
                    DnType::CommonName,
                    format!("StreamServer Agent Management {node_id}"),
                );
                params.subject_alt_names = vec![
                    SanType::URI(
                        format!("spiffe://streamserver/agent-management/{node_id}")
                            .try_into()
                            .map_err(|_| AgentCertificateIssueError::Signing)?,
                    ),
                    SanType::DnsName(
                        format!("agent-{}.agent.streamserver.internal", node_id.simple())
                            .try_into()
                            .map_err(|_| AgentCertificateIssueError::Signing)?,
                    ),
                ];
                params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            }
        }
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.name_constraints = None;
        params.crl_distribution_points.clear();
        params.custom_extensions.clear();
        params.use_authority_key_identifier_extension = true;
        let not_before = now - AGENT_CERTIFICATE_CLOCK_SKEW;
        params.not_before =
            chrono_to_offset(not_before).map_err(|_| AgentCertificateIssueError::Signing)?;
        params.not_after = chrono_to_offset(not_before + AGENT_CERTIFICATE_VALIDITY)
            .map_err(|_| AgentCertificateIssueError::Signing)?;
        let mut random_serial = [0_u8; 16];
        let serial_bytes = if let Some(serial) = forced_serial {
            if serial.is_empty() || serial.iter().all(|value| *value == 0) {
                return Err(AgentCertificateIssueError::Signing);
            }
            serial
        } else {
            OsRng.fill_bytes(&mut random_serial);
            random_serial[0] &= 0x7f;
            if random_serial.iter().all(|value| *value == 0) {
                random_serial[15] = 1;
            }
            &random_serial
        };
        params.serial_number = Some(SerialNumber::from_slice(serial_bytes));
        csr.params = params;

        let certificate = csr
            .signed_by(&self.signing_certificate, &self.signing_key)
            .map_err(|_| AgentCertificateIssueError::Signing)?;
        let certificate_der = certificate.der().to_vec();
        let certificate_pem = certificate.pem();
        let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(&certificate_der)
            .map_err(|_| AgentCertificateIssueError::Signing)?;
        match profile {
            AgentCertificateProfile::Control => verify_issued_agent_certificate(&parsed, node_id),
            AgentCertificateProfile::Management => {
                verify_issued_agent_management_certificate(&parsed, node_id)
            }
        }
        .map_err(|_| AgentCertificateIssueError::Signing)?;

        let not_before = DateTime::from_timestamp(parsed.validity().not_before.timestamp(), 0)
            .ok_or(AgentCertificateIssueError::Signing)?;
        let not_after = DateTime::from_timestamp(parsed.validity().not_after.timestamp(), 0)
            .ok_or(AgentCertificateIssueError::Signing)?;

        Ok(IssuedAgentCertificate {
            certificate_pem,
            // X.509 INTEGER encoding canonicalizes redundant leading zeroes.
            // Persist and return the certificate's actual serial, never the
            // pre-encoding random buffer.
            serial_number: hex_lower(parsed.raw_serial()),
            fingerprint_sha256: sha256_array(&certificate_der),
            public_key_sha256: sha256_array(parsed.public_key().raw),
            not_before,
            not_after,
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum AgentCertificateProfile {
    Control,
    Management,
}

fn chrono_to_offset(value: DateTime<Utc>) -> anyhow::Result<time::OffsetDateTime> {
    time::OffsetDateTime::from_unix_timestamp(value.timestamp())
        .map_err(|_| anyhow::anyhow!("certificate validity time is out of range"))
}

fn verify_issued_agent_certificate(
    certificate: &x509_parser::certificate::X509Certificate<'_>,
    node_id: Uuid,
) -> anyhow::Result<()> {
    let san = certificate
        .subject_alternative_name()
        .map_err(|_| anyhow::anyhow!("issued Agent certificate SAN is malformed"))?
        .ok_or_else(|| anyhow::anyhow!("issued Agent certificate is missing SAN"))?;
    let expected = format!("spiffe://streamserver/agent/{node_id}");
    anyhow::ensure!(
        san.value.general_names.len() == 1
            && matches!(
                san.value.general_names.first(),
                Some(x509_parser::extensions::GeneralName::URI(value)) if *value == expected
            ),
        "issued Agent certificate has an unexpected identity"
    );
    let basic_constraints = certificate
        .basic_constraints()
        .map_err(|_| anyhow::anyhow!("issued Agent certificate BasicConstraints is malformed"))?;
    anyhow::ensure!(
        basic_constraints.is_none_or(|extension| !extension.value.ca),
        "issued Agent certificate must not be a CA"
    );
    let key_usage = certificate
        .key_usage()
        .map_err(|_| anyhow::anyhow!("issued Agent certificate key usage is malformed"))?
        .ok_or_else(|| anyhow::anyhow!("issued Agent certificate is missing key usage"))?;
    anyhow::ensure!(
        key_usage.value.flags == 1 && key_usage.value.digital_signature(),
        "issued Agent certificate has an unexpected key usage"
    );
    let extended = certificate
        .extended_key_usage()
        .map_err(|_| anyhow::anyhow!("issued Agent certificate EKU is malformed"))?
        .ok_or_else(|| anyhow::anyhow!("issued Agent certificate is missing EKU"))?;
    anyhow::ensure!(
        extended.value.client_auth
            && !extended.value.server_auth
            && !extended.value.any
            && !extended.value.code_signing
            && !extended.value.email_protection
            && !extended.value.time_stamping
            && !extended.value.ocsp_signing
            && extended.value.other.is_empty(),
        "issued Agent certificate has an unexpected extended key usage"
    );
    Ok(())
}

fn verify_issued_agent_management_certificate(
    certificate: &x509_parser::certificate::X509Certificate<'_>,
    node_id: Uuid,
) -> anyhow::Result<()> {
    let san = certificate
        .subject_alternative_name()
        .map_err(|_| anyhow::anyhow!("issued Agent management certificate SAN is malformed"))?
        .ok_or_else(|| anyhow::anyhow!("issued Agent management certificate is missing SAN"))?;
    let expected_uri = format!("spiffe://streamserver/agent-management/{node_id}");
    let expected_dns = format!("agent-{}.agent.streamserver.internal", node_id.simple());
    anyhow::ensure!(
        san.value.general_names.as_slice()
            == [
                x509_parser::extensions::GeneralName::URI(expected_uri.as_str()),
                x509_parser::extensions::GeneralName::DNSName(expected_dns.as_str()),
            ],
        "issued Agent management certificate has an unexpected identity"
    );
    let basic_constraints = certificate
        .basic_constraints()?
        .map(|extension| extension.value.ca);
    anyhow::ensure!(
        basic_constraints.is_none_or(|is_ca| !is_ca),
        "issued Agent management certificate must not be a CA"
    );
    let key_usage = certificate.key_usage()?.ok_or_else(|| {
        anyhow::anyhow!("issued Agent management certificate is missing key usage")
    })?;
    anyhow::ensure!(
        key_usage.value.flags == 1 && key_usage.value.digital_signature(),
        "issued Agent management certificate has an unexpected key usage"
    );
    let extended = certificate
        .extended_key_usage()?
        .ok_or_else(|| anyhow::anyhow!("issued Agent management certificate is missing EKU"))?;
    anyhow::ensure!(
        extended.value.server_auth
            && !extended.value.client_auth
            && !extended.value.any
            && !extended.value.code_signing
            && !extended.value.email_protection
            && !extended.value.time_stamping
            && !extended.value.ocsp_signing
            && extended.value.other.is_empty(),
        "issued Agent management certificate has an unexpected extended key usage"
    );
    Ok(())
}

fn sha256_array(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

fn hex_lower(value: &[u8]) -> String {
    use fmt::Write as _;

    let mut output = String::with_capacity(value.len() * 2);
    for byte in value {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Extension,
        body::{Body, to_bytes},
        extract::ConnectInfo,
        http::{Request, StatusCode, header},
    };
    use chrono::{Duration, TimeZone};
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair,
        KeyUsagePurpose, SanType,
    };
    use serde_json::{Value, json};
    use sqlx::{Row, postgres::PgPoolOptions};
    use tower::ServiceExt;
    use x509_parser::{extensions::GeneralName, prelude::FromDer};

    use crate::{
        AppState,
        auth::{ApiRole, AuthConfig},
        build_app,
        config::{AuthMode, CoreSettings},
        control_plane::ControlPlaneService,
        test_database::{acquire_test_database_slot, config_from_env, finish_setup},
    };

    use super::*;

    const TEST_AUTH_PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIMAlSI3/XdPzRT72Rw08g6NnTnJ2eaq1JoJoW5Vlbm/T\n-----END PRIVATE KEY-----";
    const TEST_AUTH_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAA5Q5gilpT0f2fcLhC7l30Wou7Ng/gESlFWWx8z6TGJw=\n-----END PUBLIC KEY-----";

    struct CapturedTraceWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedTraceWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .write_all(buffer)?;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct TestDatabase {
        _slot: tokio::sync::OwnedSemaphorePermit,
        admin_pool: sqlx::PgPool,
        pool: sqlx::PgPool,
        database_name: String,
    }

    impl TestDatabase {
        async fn maybe_new() -> anyhow::Result<Option<Self>> {
            let config = config_from_env()?;
            let result = async {
                let slot = acquire_test_database_slot().await?;
                let admin_pool = PgPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(std::time::Duration::from_secs(1))
                    .connect(&config.admin_url)
                    .await?;
                let database_name = format!("streamserver_test_{}", Uuid::now_v7().simple());
                sqlx::query(&format!("create database {database_name}"))
                    .execute(&admin_pool)
                    .await?;
                let mut database_url = reqwest::Url::parse(&config.admin_url)?;
                database_url.set_path(&format!("/{database_name}"));
                database_url.set_query(None);
                let pool = PgPoolOptions::new()
                    .max_connections(10)
                    .connect(database_url.as_str())
                    .await?;
                sqlx::migrate!("../../migrations").run(&pool).await?;
                Ok(Self {
                    _slot: slot,
                    admin_pool,
                    pool,
                    database_name,
                })
            }
            .await;
            finish_setup(config.required, result)
        }

        async fn cleanup(self) -> anyhow::Result<()> {
            self.pool.close().await;
            sqlx::query(
                r#"
                select pg_terminate_backend(pid)
                  from pg_stat_activity
                 where datname = $1 and pid <> pg_backend_pid()
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

    fn test_ca(now: DateTime<Utc>) -> AgentCertificateAuthority {
        let key = KeyPair::generate().expect("generate CA key");
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.not_before =
            time::OffsetDateTime::from_unix_timestamp((now - Duration::days(1)).timestamp())
                .unwrap();
        params.not_after =
            time::OffsetDateTime::from_unix_timestamp((now + Duration::days(365)).timestamp())
                .unwrap();
        let cert = params.self_signed(&key).expect("self-sign CA");
        AgentCertificateAuthority::from_pem_for_test(cert.pem(), key.serialize_pem(), now)
            .expect("load test CA")
    }

    fn test_ca_pem(now: DateTime<Utc>) -> (String, String) {
        let key = KeyPair::generate().expect("generate CA key");
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.not_before =
            time::OffsetDateTime::from_unix_timestamp((now - Duration::days(1)).timestamp())
                .unwrap();
        params.not_after =
            time::OffsetDateTime::from_unix_timestamp((now + Duration::days(365)).timestamp())
                .unwrap();
        let cert = params.self_signed(&key).expect("self-sign CA");
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn enrollment_admission_token_survives_restart_and_rejects_tampering() -> anyhow::Result<()> {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 12, 0, 0).unwrap();
        let (certificate_pem, private_key_pem) = test_ca_pem(now);
        let first = AgentCertificateAuthority::from_pem_for_test(
            certificate_pem.clone(),
            private_key_pem.clone(),
            now,
        )?;
        let enrollment_id = Uuid::now_v7();
        let node_id = Uuid::now_v7();
        let token = first.enrollment_token_codec().issue(
            enrollment_id,
            node_id,
            now + AGENT_ENROLLMENT_TOKEN_TTL,
        )?;
        assert!(first.enrollment_token_codec().has_strict_wire_shape(&token));
        assert!(
            !first
                .enrollment_token_codec()
                .has_strict_wire_shape(&format!("{}A", token.as_str()))
        );
        assert_eq!(token.len(), 146);
        let codec_debug = format!("{:?}", first.enrollment_token_codec());
        assert!(codec_debug.contains("[REDACTED]"));
        assert!(!codec_debug.contains(token.as_str()));

        let restarted =
            AgentCertificateAuthority::from_pem_for_test(certificate_pem, private_key_pem, now)?;
        let verified = restarted
            .enrollment_token_codec()
            .verify(token.as_str(), now)?;
        assert_eq!(verified.enrollment_id, enrollment_id);
        assert_eq!(verified.node_id, node_id);

        let mut tampered = token.to_string().into_bytes();
        let last = tampered.last_mut().expect("token byte");
        *last = if *last == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered)?;
        assert!(
            restarted
                .enrollment_token_codec()
                .verify(&tampered, now)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn enrollment_http_admission_is_fail_fast_and_rate_limited() {
        let admission = AgentEnrollmentHttpAdmission::with_limits_for_test(
            2,
            2,
            std::time::Duration::from_secs(60),
            2,
        );
        let now = std::time::Instant::now();
        let first = admission
            .try_admit_at("192.0.2.1".parse().unwrap(), now)
            .expect("first request");
        let second = admission
            .try_admit_at("192.0.2.2".parse().unwrap(), now)
            .expect("second request");
        assert!(matches!(
            admission.try_admit_at("192.0.2.3".parse().unwrap(), now),
            Err(AgentEnrollmentAdmissionError::Busy)
        ));
        drop(first);
        drop(second);

        let ip = "198.51.100.9".parse().unwrap();
        drop(admission.try_admit_at(ip, now).expect("burst request one"));
        drop(admission.try_admit_at(ip, now).expect("burst request two"));
        assert!(matches!(
            admission.try_admit_at(ip, now),
            Err(AgentEnrollmentAdmissionError::RateLimited { .. })
        ));
        drop(
            admission
                .try_admit_at(ip, now + std::time::Duration::from_secs(60))
                .expect("one token refilled"),
        );

        // The per-peer map is bounded. Adding more peers evicts an old bucket,
        // so attacker-controlled source cardinality cannot grow memory forever.
        for address in ["203.0.113.1", "203.0.113.2", "203.0.113.3"] {
            drop(
                admission
                    .try_admit_at(address.parse().unwrap(), now)
                    .expect("new peer admitted"),
            );
        }
    }

    #[test]
    fn enrollment_global_bucket_bounds_ten_thousand_ipv6_peers_and_recovers() {
        let admission = AgentEnrollmentHttpAdmission::with_limits_for_test(
            4,
            AGENT_ENROLLMENT_HTTP_IP_BURST,
            AGENT_ENROLLMENT_HTTP_IP_REFILL,
            AGENT_ENROLLMENT_HTTP_MAX_IP_BUCKETS,
        );
        let now = std::time::Instant::now();
        let started = std::time::Instant::now();
        let mut admitted = 0_usize;
        let mut globally_limited = 0_usize;
        let ipv6_base = u128::from(std::net::Ipv6Addr::new(
            0x2001, 0x0db8, 0x0001, 0, 0, 0, 0, 0,
        ));

        for offset in 0_u128..10_000 {
            let peer = std::net::IpAddr::V6(std::net::Ipv6Addr::from(ipv6_base + offset));
            match admission.try_admit_at(peer, now) {
                Ok(permit) => {
                    admitted += 1;
                    drop(permit);
                }
                Err(AgentEnrollmentAdmissionError::RateLimited { .. }) => {
                    globally_limited += 1;
                }
                Err(AgentEnrollmentAdmissionError::Busy) => {
                    panic!("sequential requests must not exhaust concurrency")
                }
            }
        }

        assert_eq!(admitted, AGENT_ENROLLMENT_HTTP_GLOBAL_BURST as usize);
        assert_eq!(globally_limited, 10_000 - admitted);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "10k high-cardinality admission probes exceeded the fixed work budget"
        );
        let (peer_checks, maintenance_scans, maintenance_entries, peer_buckets) = {
            let limiter = admission
                .peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                limiter.peer_checks,
                limiter.maintenance_scans,
                limiter.maintenance_entries,
                limiter.buckets.len(),
            )
        };
        assert_eq!(peer_checks, AGENT_ENROLLMENT_HTTP_GLOBAL_BURST as usize);
        assert_eq!(peer_buckets, AGENT_ENROLLMENT_HTTP_GLOBAL_BURST as usize);
        assert_eq!(maintenance_scans, 0);
        assert_eq!(maintenance_entries, 0);

        let recovered_peer = std::net::IpAddr::V6(std::net::Ipv6Addr::from(ipv6_base + 20_000));
        drop(
            admission
                .try_admit_at(recovered_peer, now + AGENT_ENROLLMENT_HTTP_GLOBAL_REFILL)
                .expect("one global admission token must refill"),
        );
        let limiter = admission
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            limiter.peer_checks,
            AGENT_ENROLLMENT_HTTP_GLOBAL_BURST as usize + 1
        );
        assert_eq!(limiter.buckets.len(), peer_buckets + 1);
        assert_eq!(limiter.maintenance_scans, 0);
        assert_eq!(limiter.maintenance_entries, 0);
    }

    #[test]
    fn enrollment_peer_cleanup_scans_once_per_idle_ttl_period() {
        let admission = AgentEnrollmentHttpAdmission::with_limits_for_test(
            4,
            AGENT_ENROLLMENT_HTTP_IP_BURST,
            AGENT_ENROLLMENT_HTTP_IP_REFILL,
            AGENT_ENROLLMENT_HTTP_MAX_IP_BUCKETS,
        );
        let now = std::time::Instant::now();
        drop(
            admission
                .try_admit_at("2001:db8:2::1".parse().unwrap(), now)
                .expect("initial peer"),
        );
        drop(
            admission
                .try_admit_at(
                    "2001:db8:2::2".parse().unwrap(),
                    now + AGENT_ENROLLMENT_HTTP_BUCKET_IDLE_TTL,
                )
                .expect("first peer after maintenance interval"),
        );
        drop(
            admission
                .try_admit_at(
                    "2001:db8:2::3".parse().unwrap(),
                    now + AGENT_ENROLLMENT_HTTP_BUCKET_IDLE_TTL,
                )
                .expect("same-period peer"),
        );

        let limiter = admission
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(limiter.maintenance_scans, 1);
        assert_eq!(limiter.maintenance_entries, 1);
        assert_eq!(limiter.buckets.len(), 2);
    }

    #[test]
    fn per_peer_rejections_do_not_consume_the_global_bucket() {
        let admission = AgentEnrollmentHttpAdmission::with_limits_for_test(
            4,
            2,
            std::time::Duration::from_secs(60),
            128,
        );
        let now = std::time::Instant::now();
        let noisy_peer = "192.0.2.200".parse().unwrap();
        for attempt in 0..100 {
            let outcome = admission.try_admit_at(noisy_peer, now);
            if attempt < 2 {
                drop(outcome.expect("per-peer burst request"));
            } else {
                assert!(matches!(
                    outcome,
                    Err(AgentEnrollmentAdmissionError::RateLimited { .. })
                ));
            }
        }

        for offset in 0..(AGENT_ENROLLMENT_HTTP_GLOBAL_BURST - 2) {
            let peer = std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                198,
                51,
                (offset / 250) as u8,
                (offset % 250 + 1) as u8,
            ));
            drop(
                admission
                    .try_admit_at(peer, now)
                    .expect("per-peer denials must not drain global capacity"),
            );
        }
        assert!(matches!(
            admission.try_admit_at("203.0.113.250".parse().unwrap(), now),
            Err(AgentEnrollmentAdmissionError::RateLimited { .. })
        ));
    }

    #[tokio::test]
    async fn forged_enrollment_token_is_rejected_before_body_or_database() -> anyhow::Result<()> {
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgresql://postgres@127.0.0.1:1/postgres")?;
        let state = test_app_state(pool, test_ca(Utc::now()));
        let app = build_app(state);
        let forged_legacy_shape = URL_SAFE_NO_PAD.encode([7_u8; 32]);
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/v1/agent-enroll")
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::AUTHORIZATION,
                format!("Bearer {forged_legacy_shape}"),
            )
            // Deliberately invalid JSON: admission must reject the
            // forged MAC before the body extractor is polled.
            .body(Body::from("{"))?;
        request.extensions_mut().insert(ConnectInfo(
            "192.0.2.44:43100".parse::<std::net::SocketAddr>()?,
        ));
        let response = app.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn enrollment_admission_fails_closed_without_socket_peer() -> anyhow::Result<()> {
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgresql://postgres@127.0.0.1:1/postgres")?;
        let app = build_app(test_app_state(pool, test_ca(Utc::now())));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/agent-enroll")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer forged")
                    .body(Body::from("{}"))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        Ok(())
    }

    #[tokio::test]
    async fn enrollment_http_enforces_aggregate_and_per_csr_limits_before_database()
    -> anyhow::Result<()> {
        let now = Utc::now();
        let authority = test_ca(now);
        let codec = authority.enrollment_token_codec();
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgresql://postgres@127.0.0.1:1/postgres")?;
        let app = build_app(test_app_state(pool, authority));
        let node_id = Uuid::now_v7();
        let token = codec.issue(Uuid::now_v7(), node_id, now + AGENT_ENROLLMENT_TOKEN_TTL)?;
        let request = |csr: String, management_csr: String| -> anyhow::Result<Request<Body>> {
            let mut request = Request::builder()
                .method("POST")
                .uri("/api/v1/agent-enroll")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {}", token.as_str()))
                .body(Body::from(
                    json!({
                        "node_id": node_id,
                        "csr_pem": csr,
                        "management_csr_pem": management_csr,
                    })
                    .to_string(),
                ))?;
            request.extensions_mut().insert(ConnectInfo(
                "192.0.2.72:43100".parse::<std::net::SocketAddr>()?,
            ));
            Ok(request)
        };

        let aggregate = app
            .clone()
            .oneshot(request("a".repeat(20 * 1024), "b".repeat(20 * 1024))?)
            .await?;
        assert_eq!(aggregate.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let per_csr = app
            .oneshot(request("a".repeat(16 * 1024 + 1), "b".to_string())?)
            .await?;
        assert_eq!(per_csr.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[tokio::test]
    async fn enrollment_http_rate_limit_returns_429_and_retry_after() -> anyhow::Result<()> {
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgresql://postgres@127.0.0.1:1/postgres")?;
        let app = build_app(test_app_state(pool, test_ca(Utc::now()))).layer(Extension(
            ConnectInfo("192.0.2.91:43100".parse::<std::net::SocketAddr>()?),
        ));
        for attempt in 0..=AGENT_ENROLLMENT_HTTP_IP_BURST {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/agent-enroll")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::AUTHORIZATION, "Bearer forged")
                        .body(Body::from("{"))?,
                )
                .await?;
            if attempt < AGENT_ENROLLMENT_HTTP_IP_BURST {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            } else {
                assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
                assert!(response.headers().contains_key(header::RETRY_AFTER));
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn enrollment_admission_strips_authorization_but_preserves_user_agent()
    -> anyhow::Result<()> {
        async fn downstream(
            Extension(_verified): Extension<VerifiedAgentEnrollmentToken>,
            headers: axum::http::HeaderMap,
        ) -> StatusCode {
            assert!(!headers.contains_key(header::AUTHORIZATION));
            assert_eq!(
                headers.get(header::USER_AGENT),
                Some(&header::HeaderValue::from_static("enrollment-test-agent"))
            );
            StatusCode::OK
        }

        let now = Utc::now();
        let authority = test_ca(now);
        let codec = authority.enrollment_token_codec();
        let node_id = Uuid::now_v7();
        let token = codec.issue(Uuid::now_v7(), node_id, now + AGENT_ENROLLMENT_TOKEN_TTL)?;
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgresql://postgres@127.0.0.1:1/postgres")?;
        let state = test_app_state(pool, authority);
        let app = axum::Router::new()
            .route(
                "/probe",
                axum::routing::post(downstream).layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    crate::admit_agent_enrollment,
                )),
            )
            .with_state(state)
            .layer(Extension(ConnectInfo(
                "192.0.2.101:43100".parse::<std::net::SocketAddr>()?,
            )));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .header(header::AUTHORIZATION, format!("Bearer {}", token.as_str()))
                    .header(header::USER_AGENT, "enrollment-test-agent")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn enrollment_socket_service_supplies_real_peer_address() -> anyhow::Result<()> {
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgresql://postgres@127.0.0.1:1/postgres")?;
        let app = build_app(test_app_state(pool, test_ca(Utc::now())));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
        });
        let response = reqwest::Client::new()
            .post(format!("http://{address}/api/v1/agent-enroll"))
            .header(reqwest::header::AUTHORIZATION, "Bearer forged")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body("{")
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        server.abort();
        let _ = server.await;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_trace_records_path_status_and_latency_without_request_secrets()
    -> anyhow::Result<()> {
        const CHILD_ENV: &str = "STREAMSERVER_HTTP_TRACE_CONTRACT_CHILD";
        const TEST_NAME: &str = "agent_identity::tests::http_trace_records_path_status_and_latency_without_request_secrets";
        if std::env::var_os(CHILD_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
            // tracing callsite interest is process-global. Run the output
            // capture in a one-test child so unrelated parallel tests cannot
            // register the same tower-http callsites against a null subscriber.
            let output = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD_ENV, "1")
                .output()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::ensure!(
                output.status.success() && stdout.contains("1 passed"),
                "isolated HTTP trace contract failed (status={}): stdout={stdout}; stderr={stderr}",
                output.status
            );
            return Ok(());
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer = {
            let captured = Arc::clone(&captured);
            move || CapturedTraceWriter(Arc::clone(&captured))
        };
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(writer)
            .finish();
        // Hold a thread-local subscriber for the whole current-thread runtime.
        // A future-scoped subscriber is not inherited by the per-request tasks
        // used by a real Axum server and made this assertion timing-dependent.
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgresql://postgres@127.0.0.1:1/postgres")?;
        let app = build_app(test_app_state(pool, test_ca(Utc::now())));
        let enrollment_query_secret = format!("ssae1.{}.{}", "Q".repeat(96), "s".repeat(43));
        let ordinary_query_secret = "ordinary-query-secret-7e6123";
        let authorization_secret = "authorization-secret-3b971d";

        let mut enrollment_request = Request::builder()
            .method("POST")
            .uri(format!(
                "/api/v1/agent-enroll?token={enrollment_query_secret}"
            ))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {authorization_secret}"),
            )
            .body(Body::empty())?;
        enrollment_request.extensions_mut().insert(ConnectInfo(
            "192.0.2.150:43100".parse::<std::net::SocketAddr>()?,
        ));
        let enrollment_response = app.clone().oneshot(enrollment_request).await?;
        assert_eq!(enrollment_response.status(), StatusCode::UNAUTHORIZED);

        let ordinary_response = tokio::spawn(async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!(
                            "/health/live?api_key={ordinary_query_secret}&mode=sensitive"
                        ))
                        .header(
                            header::AUTHORIZATION,
                            format!("Bearer {authorization_secret}"),
                        )
                        .body(Body::empty())?,
                )
                .await?;
            Ok::<_, anyhow::Error>(response)
        })
        .await??;
        assert_eq!(ordinary_response.status(), StatusCode::OK);

        let captured = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let captured = std::str::from_utf8(&captured)?;
        assert!(!captured.contains(&enrollment_query_secret));
        assert!(!captured.contains(ordinary_query_secret));
        assert!(!captured.contains(authorization_secret));
        assert!(!captured.contains("token="));
        assert!(!captured.contains("api_key="));
        assert!(captured.contains("path=/api/v1/agent-enroll"));
        assert!(captured.contains("path=/health/live"));
        assert!(captured.contains("status=401"));
        assert!(captured.contains("status=200"));
        assert!(captured.contains("latency="));
        Ok(())
    }

    #[tokio::test]
    async fn hmac_valid_unknown_enrollment_is_rejected_before_csr_and_without_audit()
    -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let now = Utc::now();
        let repository = Arc::new(TaskRepository::new(database.pool.clone()));
        let authority = test_ca(now);
        let codec = authority.enrollment_token_codec();
        let service = AgentIdentityService::new(
            repository,
            authority,
            AgentEnrollmentPublicConfig {
                control_plane_server_ca_pem: test_ca(now).certificate_pem.clone(),
                management_client_ca_pem: test_ca(now).certificate_pem.clone(),
                capability_jwt_public_key_pem: "capability-public-key-pem".to_string(),
                capability_jwt_kid: "capability-kid".to_string(),
            },
        );
        let token = codec.issue(
            Uuid::now_v7(),
            Uuid::now_v7(),
            now + AGENT_ENROLLMENT_TOKEN_TTL,
        )?;
        let verified = service.verify_enrollment_token(&token, now)?;
        let error = service
            .enroll_verified(
                &verified,
                verified.node_id,
                "not-a-csr",
                "also-not-a-csr",
                Some("192.0.2.9".parse()?),
                Some("test".to_string()),
                now,
            )
            .await
            .expect_err("unknown enrollment must fail");
        assert!(matches!(
            error,
            AgentIdentityServiceError::InvalidEnrollment
        ));
        let rejected_audits: i64 = sqlx::query_scalar(
            "select count(*) from security_audit_events where event_type = 'agent_enrollment_rejected'",
        )
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(rejected_audits, 0);
        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn enrollment_http_timeout_cancels_and_rolls_back_blocked_transaction()
    -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let now = Utc::now();
        let state = test_app_state(database.pool.clone(), test_ca(now));
        let service = state
            .agent_identity
            .clone()
            .expect("Agent identity service");
        let node_id = Uuid::now_v7();
        let enrollment = service
            .create_enrollment(node_id, "admin", None, None, now)
            .await?;
        let body = json!({
            "node_id": node_id,
            "csr_pem": csr_with_sans(Vec::new()),
            "management_csr_pem": csr_with_sans(Vec::new()),
        })
        .to_string();
        let app = build_app(state).layer(Extension(ConnectInfo(
            "192.0.2.81:43000".parse::<std::net::SocketAddr>()?,
        )));
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/api/v1/agent-enroll")
                .header(header::CONTENT_TYPE, "application/json")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", enrollment.token.as_str()),
                )
                .body(Body::from(body.clone()))
        };

        let audit_before: i64 = sqlx::query_scalar("select count(*) from security_audit_events")
            .fetch_one(&database.pool)
            .await?;
        let mut identity_lock = database.pool.begin().await?;
        sqlx::query("select 1 from agent_identities where node_id = $1 for update")
            .bind(node_id)
            .fetch_one(&mut *identity_lock)
            .await?;
        let started = std::time::Instant::now();
        let timed_out = app.clone().oneshot(request()?).await?;
        assert_eq!(timed_out.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(started.elapsed() >= std::time::Duration::from_secs(9));

        let consumed: bool = sqlx::query_scalar(
            "select consumed_at is not null from agent_enrollment_tokens where id = $1",
        )
        .bind(enrollment.enrollment_id)
        .fetch_one(&database.pool)
        .await?;
        assert!(!consumed);
        let audit_after_timeout: i64 =
            sqlx::query_scalar("select count(*) from security_audit_events")
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(audit_after_timeout, audit_before);
        identity_lock.rollback().await?;

        let retry = app.oneshot(request()?).await?;
        assert_eq!(retry.status(), StatusCode::OK);
        database.cleanup().await?;
        Ok(())
    }

    fn csr_with_sans(sans: Vec<SanType>) -> String {
        let key = KeyPair::generate().expect("generate Agent key");
        let mut params = CertificateParams::default();
        params.subject_alt_names = sans;
        params
            .serialize_request(&key)
            .expect("serialize CSR")
            .pem()
            .expect("encode CSR PEM")
    }

    fn valid_peer_certificate_params(now: DateTime<Utc>, node_id: Uuid) -> CertificateParams {
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![SanType::URI(
            format!("spiffe://streamserver/agent/{node_id}")
                .try_into()
                .expect("valid SPIFFE URI"),
        )];
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.not_before =
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .expect("valid not-before");
        params.not_after =
            time::OffsetDateTime::from_unix_timestamp((now + Duration::days(1)).timestamp())
                .expect("valid not-after");
        params
    }

    fn signed_peer_der(
        authority: &AgentCertificateAuthority,
        params: CertificateParams,
    ) -> Vec<u8> {
        let key = KeyPair::generate().expect("generate peer key");
        params
            .signed_by(&key, &authority.signing_certificate, &authority.signing_key)
            .expect("sign peer certificate")
            .der()
            .to_vec()
    }

    #[derive(Debug, Clone, Copy)]
    enum ExpectedPeerCertificateError {
        Invalid,
        NotCurrentlyValid,
        InvalidIdentity,
        InvalidUsage,
    }

    fn assert_peer_certificate_error(
        case_name: &str,
        der: &[u8],
        now: DateTime<Utc>,
        expected: ExpectedPeerCertificateError,
    ) {
        let actual =
            parse_authenticated_agent_peer(der, "192.0.2.25".parse().expect("valid peer IP"), now)
                .expect_err(case_name);
        let matches_expected = matches!(
            (expected, actual),
            (
                ExpectedPeerCertificateError::Invalid,
                AgentPeerCertificateError::Invalid
            ) | (
                ExpectedPeerCertificateError::NotCurrentlyValid,
                AgentPeerCertificateError::NotCurrentlyValid
            ) | (
                ExpectedPeerCertificateError::InvalidIdentity,
                AgentPeerCertificateError::InvalidIdentity
            ) | (
                ExpectedPeerCertificateError::InvalidUsage,
                AgentPeerCertificateError::InvalidUsage
            )
        );
        assert!(
            matches_expected,
            "{case_name}: expected {expected:?}, got {actual:?}"
        );
    }

    fn test_app_state(pool: sqlx::PgPool, authority: AgentCertificateAuthority) -> AppState {
        let repository = Arc::new(TaskRepository::new(pool));
        let server_ca = test_ca(Utc::now());
        AppState {
            control_plane: ControlPlaneService::new(repository.clone()),
            agent_management: None,
            agent_identity: Some(AgentIdentityService::new(
                repository.clone(),
                authority,
                AgentEnrollmentPublicConfig {
                    control_plane_server_ca_pem: server_ca.certificate_pem.clone(),
                    management_client_ca_pem: test_ca(Utc::now()).certificate_pem.clone(),
                    capability_jwt_public_key_pem: "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAA5Q5gilpT0f2fcLhC7l30Wou7Ng/gESlFWWx8z6TGJw=\n-----END PUBLIC KEY-----\n".to_string(),
                    capability_jwt_kid:
                        "a1d595e6a09dd5bf7e22bf6e24e95a805e20b43a8f46179cfcf951dbe55cfb70"
                            .to_string(),
                },
            )),
            repository,
            started_at: Utc::now(),
            environment: "test".to_string(),
            auth: AuthConfig::from_settings(&CoreSettings::default())
                .expect("disabled auth config"),
            hook_shared_secret: String::new(),
            hook_source_allowlist: Vec::new(),
            zlm_auto_close_on_no_reader_enabled: false,
            storage_allowlist: vec![std::env::temp_dir().to_string_lossy().to_string()],
        }
    }

    async fn response_json(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        serde_json::from_slice(&bytes).expect("response JSON")
    }

    #[test]
    fn issued_certificate_ignores_hostile_csr_sans_and_has_exact_agent_identity() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let node_id = Uuid::now_v7();
        let hostile_csr = csr_with_sans(vec![
            SanType::URI("spiffe://attacker/admin".try_into().unwrap()),
            SanType::DnsName("attacker.invalid".try_into().unwrap()),
        ]);

        let authority = test_ca(now);
        let issued = authority
            .issue_agent_certificate(node_id, &hostile_csr, now)
            .expect("issue Agent certificate");

        let (_, certificate_pem) = parse_x509_pem(issued.certificate_pem.as_bytes())
            .expect("parse issued certificate PEM");
        let (_, certificate) =
            x509_parser::certificate::X509Certificate::from_der(&certificate_pem.contents)
                .expect("parse issued certificate");
        let (_, ca_pem) = parse_x509_pem(authority.certificate_pem.as_bytes())
            .expect("parse Agent CA certificate PEM");
        let (_, ca_certificate) =
            x509_parser::certificate::X509Certificate::from_der(&ca_pem.contents)
                .expect("parse Agent CA certificate");
        certificate
            .verify_signature(Some(ca_certificate.public_key()))
            .expect("issued certificate signature must verify under Agent CA");
        let (_, csr_pem) = parse_x509_pem(hostile_csr.as_bytes()).expect("parse CSR PEM");
        let (_, csr) = x509_parser::certification_request::X509CertificationRequest::from_der(
            &csr_pem.contents,
        )
        .expect("parse CSR");
        assert_eq!(
            certificate.public_key().raw,
            csr.certification_request_info.subject_pki.raw,
            "issued certificate must preserve the CSR public key"
        );
        let san = certificate
            .subject_alternative_name()
            .expect("read SAN")
            .expect("issued certificate must contain SAN");
        assert_eq!(san.value.general_names.len(), 1);
        assert!(matches!(
            &san.value.general_names[0],
            GeneralName::URI(value)
                if *value == format!("spiffe://streamserver/agent/{node_id}")
        ));
        assert_eq!(issued.not_before, now - Duration::minutes(5));
        assert_eq!(
            issued.not_after - issued.not_before,
            Duration::days(90),
            "Agent leaf lifetime must never exceed the 90-day policy"
        );
        assert_eq!(
            issued.not_after,
            now + Duration::days(90) - Duration::minutes(5)
        );
        assert_eq!(issued.fingerprint_sha256.len(), 32);
        assert_eq!(issued.public_key_sha256.len(), 32);
        assert!(!issued.certificate_pem.is_empty());
        assert!(!issued.serial_number.is_empty());
    }

    #[test]
    fn issued_management_certificate_has_exact_server_identity_and_usage() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let node_id = Uuid::now_v7();
        let authority = test_ca(now);
        let issued = authority
            .issue_agent_management_certificate(
                node_id,
                &csr_with_sans(vec![SanType::DnsName(
                    "attacker.invalid".try_into().unwrap(),
                )]),
                now,
            )
            .expect("issue Agent management certificate");
        let (_, pem) = parse_x509_pem(issued.certificate_pem.as_bytes()).unwrap();
        let (_, certificate) =
            x509_parser::certificate::X509Certificate::from_der(&pem.contents).unwrap();

        verify_issued_agent_management_certificate(&certificate, node_id)
            .expect("management profile");
        let san = certificate.subject_alternative_name().unwrap().unwrap();
        assert!(matches!(
            san.value.general_names.as_slice(),
            [GeneralName::URI(uri), GeneralName::DNSName(dns)]
                if *uri == format!("spiffe://streamserver/agent-management/{node_id}")
                    && *dns == format!("agent-{}.agent.streamserver.internal", node_id.simple())
        ));
    }

    #[test]
    fn issued_serial_uses_canonical_x509_raw_serial_after_leading_zero_normalization() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let authority = test_ca(now);
        let issued = authority
            .issue_agent_certificate_with_serial_for_test(
                Uuid::now_v7(),
                &csr_with_sans(Vec::new()),
                now,
                &[0, 0x12, 0x34, 0x56],
            )
            .expect("issue certificate with leading-zero source serial");
        let (_, pem) = parse_x509_pem(issued.certificate_pem.as_bytes()).unwrap();
        let (_, certificate) =
            x509_parser::certificate::X509Certificate::from_der(&pem.contents).unwrap();

        assert_eq!(issued.serial_number, hex_lower(certificate.raw_serial()));
        assert_eq!(issued.serial_number, "123456");
    }

    #[test]
    fn peer_parser_derives_node_fingerprint_and_peer_ip_from_valid_leaf() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let node_id = Uuid::now_v7();
        let authority = test_ca(now);
        let issued = authority
            .issue_agent_certificate(node_id, &csr_with_sans(Vec::new()), now)
            .expect("issue test Agent certificate");
        let (_, pem) = parse_x509_pem(issued.certificate_pem.as_bytes()).unwrap();
        let peer_ip: IpAddr = "192.0.2.25".parse().unwrap();

        let peer = parse_authenticated_agent_peer(&pem.contents, peer_ip, now)
            .expect("valid Agent leaf must parse");

        assert_eq!(peer.node_id, node_id);
        assert_eq!(peer.fingerprint_sha256, issued.fingerprint_sha256);
        assert_eq!(peer.not_before, now - Duration::minutes(5));
        assert_eq!(
            peer.not_after,
            now + Duration::days(90) - Duration::minutes(5)
        );
        assert_eq!(peer.peer_ip, peer_ip);
    }

    #[test]
    fn peer_parser_rejects_nil_spiffe_node_id() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let authority = test_ca(now);
        let issued = authority
            .issue_agent_certificate(Uuid::nil(), &csr_with_sans(Vec::new()), now)
            .expect("issue nil-identity certificate for negative parser test");
        let (_, pem) = parse_x509_pem(issued.certificate_pem.as_bytes()).unwrap();

        assert!(matches!(
            parse_authenticated_agent_peer(&pem.contents, "192.0.2.25".parse().unwrap(), now),
            Err(AgentPeerCertificateError::InvalidIdentity)
        ));
    }

    #[test]
    fn peer_parser_rejects_malformed_or_trailing_der() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let authority = test_ca(now);
        let valid_der = signed_peer_der(
            &authority,
            valid_peer_certificate_params(now, Uuid::now_v7()),
        );
        let mut trailing_der = valid_der;
        trailing_der.push(0);

        let cases = [
            (
                "malformed DER",
                vec![0x30, 0x03, 0x01],
                ExpectedPeerCertificateError::Invalid,
            ),
            (
                "trailing DER bytes",
                trailing_der,
                ExpectedPeerCertificateError::Invalid,
            ),
        ];
        for (case_name, der, expected) in cases {
            assert_peer_certificate_error(case_name, &der, now, expected);
        }
    }

    #[test]
    fn peer_parser_rejects_certificates_outside_their_validity_window() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let authority = test_ca(now);
        let node_id = Uuid::now_v7();
        let mut not_yet_valid = valid_peer_certificate_params(now, node_id);
        not_yet_valid.not_before =
            time::OffsetDateTime::from_unix_timestamp((now + Duration::minutes(1)).timestamp())
                .unwrap();
        not_yet_valid.not_after =
            time::OffsetDateTime::from_unix_timestamp((now + Duration::days(1)).timestamp())
                .unwrap();
        let mut expired = valid_peer_certificate_params(now, node_id);
        expired.not_before =
            time::OffsetDateTime::from_unix_timestamp((now - Duration::days(1)).timestamp())
                .unwrap();
        expired.not_after =
            time::OffsetDateTime::from_unix_timestamp((now - Duration::seconds(1)).timestamp())
                .unwrap();

        let cases = [
            ("not-yet-valid certificate", not_yet_valid),
            ("expired certificate", expired),
        ];
        for (case_name, params) in cases {
            let der = signed_peer_der(&authority, params);
            assert_peer_certificate_error(
                case_name,
                &der,
                now,
                ExpectedPeerCertificateError::NotCurrentlyValid,
            );
        }
    }

    #[test]
    fn peer_parser_rejects_missing_wrong_noncanonical_or_multiple_sans() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let authority = test_ca(now);
        let node_id = Uuid::now_v7();
        let valid_uri = format!("spiffe://streamserver/agent/{node_id}");

        let mut missing_san = valid_peer_certificate_params(now, node_id);
        missing_san.subject_alt_names.clear();
        let mut dns_only = valid_peer_certificate_params(now, node_id);
        dns_only.subject_alt_names = vec![SanType::DnsName(
            "agent.example".try_into().expect("valid DNS SAN"),
        )];
        let mut wrong_uri = valid_peer_certificate_params(now, node_id);
        wrong_uri.subject_alt_names = vec![SanType::URI(
            format!("spiffe://streamserver/admin/{node_id}")
                .try_into()
                .expect("valid wrong URI SAN"),
        )];
        let mut noncanonical_uri = valid_peer_certificate_params(now, node_id);
        noncanonical_uri.subject_alt_names = vec![SanType::URI(
            format!(
                "spiffe://streamserver/agent/{}",
                node_id.to_string().to_ascii_uppercase()
            )
            .try_into()
            .expect("valid noncanonical URI SAN"),
        )];
        let mut multiple_sans = valid_peer_certificate_params(now, node_id);
        multiple_sans.subject_alt_names = vec![
            SanType::URI(valid_uri.try_into().expect("valid Agent URI SAN")),
            SanType::DnsName("agent.example".try_into().expect("valid DNS SAN")),
        ];

        let cases = [
            ("missing SAN", missing_san),
            ("DNS-only SAN", dns_only),
            ("wrong URI SAN", wrong_uri),
            ("noncanonical UUID URI SAN", noncanonical_uri),
            ("multiple SANs", multiple_sans),
        ];
        for (case_name, params) in cases {
            let der = signed_peer_der(&authority, params);
            assert_peer_certificate_error(
                case_name,
                &der,
                now,
                ExpectedPeerCertificateError::InvalidIdentity,
            );
        }
    }

    #[test]
    fn peer_parser_rejects_ca_or_non_client_only_key_usages() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let authority = test_ca(now);
        let node_id = Uuid::now_v7();

        let mut ca = valid_peer_certificate_params(now, node_id);
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let mut missing_key_usage = valid_peer_certificate_params(now, node_id);
        missing_key_usage.key_usages.clear();
        let mut extra_key_usage = valid_peer_certificate_params(now, node_id);
        extra_key_usage.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        let mut missing_extended_key_usage = valid_peer_certificate_params(now, node_id);
        missing_extended_key_usage.extended_key_usages.clear();
        let mut server_auth_only = valid_peer_certificate_params(now, node_id);
        server_auth_only.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let mut client_and_server_auth = valid_peer_certificate_params(now, node_id);
        client_and_server_auth.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ];

        let cases = [
            ("CA certificate", ca),
            ("missing key usage", missing_key_usage),
            ("extra key usage", extra_key_usage),
            ("missing extended key usage", missing_extended_key_usage),
            ("serverAuth-only extended key usage", server_auth_only),
            (
                "clientAuth plus serverAuth extended key usages",
                client_and_server_auth,
            ),
        ];
        for (case_name, params) in cases {
            let der = signed_peer_der(&authority, params);
            assert_peer_certificate_error(
                case_name,
                &der,
                now,
                ExpectedPeerCertificateError::InvalidUsage,
            );
        }
    }

    #[test]
    fn sensitive_identity_values_are_redacted_from_debug() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let enrollment = CreatedAgentEnrollment {
            enrollment_id: Uuid::now_v7(),
            node_id: Uuid::now_v7(),
            token: Zeroizing::new("super-secret-enrollment-token".to_string()),
            expires_at: now + Duration::minutes(10),
        };
        let enrollment_debug = format!("{enrollment:?}");
        assert!(enrollment_debug.contains("[REDACTED]"));
        assert!(!enrollment_debug.contains("super-secret"));

        let authority = test_ca(now);
        let authority_debug = format!("{authority:?}");
        assert!(authority_debug.contains("[REDACTED]"));
        assert!(!authority_debug.contains("PRIVATE KEY"));
    }

    #[test]
    fn malformed_csr_is_rejected_without_echoing_its_contents() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let error =
            match test_ca(now).issue_agent_certificate(Uuid::now_v7(), "not-a-csr-secret", now) {
                Ok(_) => panic!("malformed CSR must fail"),
                Err(error) => error,
            };
        assert_eq!(error.to_string(), "Agent CSR is invalid");
        assert!(!error.to_string().contains("not-a-csr-secret"));
    }

    #[test]
    fn ca_loader_rejects_non_ca_and_mismatched_private_key() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.not_before = chrono_to_offset(now - Duration::days(1)).unwrap();
        params.not_after = chrono_to_offset(now + Duration::days(1)).unwrap();
        let non_ca = params.self_signed(&key).unwrap();
        let error =
            AgentCertificateAuthority::from_pem_for_test(non_ca.pem(), key.serialize_pem(), now)
                .expect_err("non-CA certificate must fail");
        assert!(error.to_string().contains("CA:TRUE"));

        let valid_authority = test_ca(now);
        let different_key = KeyPair::generate().unwrap();
        let error = AgentCertificateAuthority::from_pem_for_test(
            valid_authority.certificate_pem.clone(),
            different_key.serialize_pem(),
            now,
        )
        .expect_err("mismatched key must fail");
        assert!(error.to_string().contains("do not match"));
        assert!(!error.to_string().contains("PRIVATE KEY"));
    }

    #[test]
    fn ca_loader_rejects_intermediate_and_invalid_self_signature() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let root_key = KeyPair::generate().unwrap();
        let mut root_params = CertificateParams::default();
        root_params.distinguished_name = DistinguishedName::new();
        root_params
            .distinguished_name
            .push(DnType::CommonName, "Root");
        root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        root_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        root_params.not_before = chrono_to_offset(now - Duration::days(1)).unwrap();
        root_params.not_after = chrono_to_offset(now + Duration::days(365)).unwrap();
        let root = root_params.self_signed(&root_key).unwrap();

        let intermediate_key = KeyPair::generate().unwrap();
        let mut intermediate_params = CertificateParams::default();
        intermediate_params.distinguished_name = DistinguishedName::new();
        intermediate_params
            .distinguished_name
            .push(DnType::CommonName, "Intermediate");
        intermediate_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        intermediate_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        intermediate_params.not_before = chrono_to_offset(now - Duration::days(1)).unwrap();
        intermediate_params.not_after = chrono_to_offset(now + Duration::days(90)).unwrap();
        let intermediate = intermediate_params
            .signed_by(&intermediate_key, &root, &root_key)
            .unwrap();
        let error = AgentCertificateAuthority::from_pem_for_test(
            intermediate.pem(),
            intermediate_key.serialize_pem(),
            now,
        )
        .expect_err("intermediate CA must not be accepted as the Agent root");
        assert!(error.to_string().contains("self-signed root"));

        let self_signed = test_ca(now);
        let (_, pem) = parse_x509_pem(self_signed.certificate_pem.as_bytes()).unwrap();
        let mut corrupt_der = pem.contents;
        let last = corrupt_der.last_mut().unwrap();
        *last ^= 1;
        let corrupt_pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            STANDARD.encode(corrupt_der)
        );
        let error = AgentCertificateAuthority::from_pem_for_test(
            corrupt_pem,
            self_signed.signing_key.serialize_pem(),
            now,
        )
        .expect_err("invalid self-signature must fail");
        assert!(error.to_string().contains("self-signature"));
    }

    #[test]
    fn ca_loader_requires_the_full_leaf_validity_window() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();

        for (name, not_before, not_after) in [
            (
                "CA starts after leaf clock-skew window",
                now - Duration::minutes(1),
                now + Duration::days(365),
            ),
            (
                "CA expires before ninety-day leaf",
                now - Duration::days(1),
                now + Duration::days(89),
            ),
        ] {
            let key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::default();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
            params.not_before = chrono_to_offset(not_before).unwrap();
            params.not_after = chrono_to_offset(not_after).unwrap();
            let certificate = params.self_signed(&key).unwrap();
            let error = AgentCertificateAuthority::from_pem_for_test(
                certificate.pem(),
                key.serialize_pem(),
                now,
            )
            .expect_err(name);
            assert!(error.to_string().contains("full 90-day Agent leaf window"));
        }
    }

    #[test]
    fn ca_loader_rejects_authorization_usages_beyond_key_cert_sign() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();

        for (name, key_usages, extended_key_usages) in [
            (
                "extra CA key usage",
                vec![
                    KeyUsagePurpose::KeyCertSign,
                    KeyUsagePurpose::DigitalSignature,
                ],
                Vec::new(),
            ),
            (
                "CA extended key usage",
                vec![KeyUsagePurpose::KeyCertSign],
                vec![ExtendedKeyUsagePurpose::ClientAuth],
            ),
        ] {
            let key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::default();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params.key_usages = key_usages;
            params.extended_key_usages = extended_key_usages;
            params.not_before = chrono_to_offset(now - Duration::days(1)).unwrap();
            params.not_after = chrono_to_offset(now + Duration::days(365)).unwrap();
            let certificate = params.self_signed(&key).unwrap();

            let error = AgentCertificateAuthority::from_pem_for_test(
                certificate.pem(),
                key.serialize_pem(),
                now,
            )
            .expect_err(name);
            assert!(
                error.to_string().contains("keyCertSign-only")
                    || error.to_string().contains("must not contain EKU"),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn long_running_authority_refuses_leaf_that_would_outlive_its_root() {
        let loaded_at = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.not_before = chrono_to_offset(loaded_at - Duration::days(1)).unwrap();
        params.not_after = chrono_to_offset(loaded_at + Duration::days(100)).unwrap();
        let certificate = params.self_signed(&key).unwrap();
        let authority = AgentCertificateAuthority::from_pem_for_test(
            certificate.pem(),
            key.serialize_pem(),
            loaded_at,
        )
        .expect("CA initially covers the complete leaf window");

        let error = match authority.issue_agent_certificate(
            Uuid::now_v7(),
            &csr_with_sans(Vec::new()),
            loaded_at + Duration::days(11),
        ) {
            Ok(_) => panic!("later leaf would outlive CA"),
            Err(error) => error,
        };
        assert!(matches!(error, AgentCertificateIssueError::Signing));
    }

    #[test]
    fn ca_loader_rejects_trailing_der_data() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.not_before = chrono_to_offset(now - Duration::days(1)).unwrap();
        params.not_after = chrono_to_offset(now + Duration::days(1)).unwrap();
        let certificate = params.self_signed(&key).unwrap();
        let mut hostile_der = certificate.der().to_vec();
        hostile_der.push(0);
        let encoded = STANDARD.encode(hostile_der);
        let body = encoded
            .as_bytes()
            .chunks(64)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let hostile_pem =
            format!("-----BEGIN CERTIFICATE-----\n{body}\n-----END CERTIFICATE-----\n");

        let error =
            AgentCertificateAuthority::from_pem_for_test(hostile_pem, key.serialize_pem(), now)
                .expect_err("trailing DER data must fail closed");
        assert!(error.to_string().contains("trailing DER"));
    }

    #[test]
    fn issued_certificate_verifier_rejects_extra_key_and_extended_key_usages() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let authority = test_ca(now);
        let node_id = Uuid::now_v7();

        let mut extra_key_usage = valid_peer_certificate_params(now, node_id);
        extra_key_usage.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        let extra_key_usage_der = signed_peer_der(&authority, extra_key_usage);
        let (_, extra_key_usage_certificate) =
            x509_parser::certificate::X509Certificate::from_der(&extra_key_usage_der).unwrap();
        assert!(verify_issued_agent_certificate(&extra_key_usage_certificate, node_id).is_err());

        let mut extra_eku = valid_peer_certificate_params(now, node_id);
        extra_eku.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ];
        let extra_eku_der = signed_peer_der(&authority, extra_eku);
        let (_, extra_eku_certificate) =
            x509_parser::certificate::X509Certificate::from_der(&extra_eku_der).unwrap();
        assert!(verify_issued_agent_certificate(&extra_eku_certificate, node_id).is_err());
    }

    #[test]
    fn signing_ca_must_be_present_in_grpc_client_trust_bundle() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let authority = test_ca(now);
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("client-ca.pem");
        std::fs::write(&bundle, &authority.certificate_pem).unwrap();
        authority
            .ensure_present_in_client_ca_bundle(&bundle)
            .expect("exact signing CA is trusted");

        std::fs::write(&bundle, test_ca(now).certificate_pem.clone()).unwrap();
        let error = authority
            .ensure_present_in_client_ca_bundle(&bundle)
            .expect_err("different CA must fail");
        assert!(error.to_string().contains("not present"));
    }

    #[tokio::test]
    async fn leading_zero_serial_is_canonical_in_response_and_database() -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let token_hash = [91_u8; 32];
        repository
            .create_agent_enrollment(NewAgentEnrollment {
                id: Uuid::now_v7(),
                node_id,
                token_hash,
                created_by: "admin".to_string(),
                created_at: now,
                expires_at: now + Duration::minutes(10),
                remote_ip: None,
                user_agent: None,
            })
            .await?;

        let control_csr = csr_with_sans(Vec::new());
        let management_csr = csr_with_sans(Vec::new());
        let authority = test_ca(now);
        let control = authority.issue_agent_certificate_with_serial_for_test(
            node_id,
            &control_csr,
            now + Duration::seconds(1),
            &[0, 0x12, 0x34, 0x56],
        )?;
        let management = authority.issue_agent_management_certificate(
            node_id,
            &management_csr,
            now + Duration::seconds(1),
        )?;
        let request = AgentEnrollmentRequest {
            node_id,
            control_csr_public_key_sha256: csr_public_key_sha256(&control_csr)?,
            management_csr_public_key_sha256: csr_public_key_sha256(&management_csr)?,
            attempted_at: now + Duration::seconds(1),
            remote_ip: None,
            user_agent: None,
        };
        let bundle = AgentEnrollmentBundle {
            node_id,
            control_certificate: repository_certificate(control),
            management_certificate: repository_certificate(management),
            agent_client_issuer_ca_pem: authority.certificate_pem.clone(),
            control_plane_server_ca_pem: test_ca(now).certificate_pem.clone(),
            management_client_ca_pem: test_ca(now).certificate_pem.clone(),
            capability_jwt_public_key_pem: "capability-public-key-pem".to_string(),
            capability_jwt_kid: "capability-kid".to_string(),
        };
        let outcome = repository
            .consume_agent_enrollment(&token_hash, request, |_| {
                Ok::<_, AgentIdentityServiceError>(bundle)
            })
            .await?;
        let bundle = match outcome {
            ConsumeAgentEnrollmentOutcome::Issued(bundle) => bundle,
            other => anyhow::bail!("expected issued enrollment, got {other:?}"),
        };
        let completed = completed_agent_enrollment(bundle);
        let persisted: String =
            sqlx::query_scalar("select serial_number from agent_certificates where node_id = $1")
                .bind(node_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(completed.serial_number, "123456");
        assert_eq!(persisted, completed.serial_number);

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn identity_service_rotation_signs_both_profiles_and_recovers_same_bundle()
    -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let authority = test_ca(now);
        let agent_client_issuer_ca_pem = authority.certificate_pem.clone();
        let public_config = AgentEnrollmentPublicConfig {
            control_plane_server_ca_pem: test_ca(now).certificate_pem.clone(),
            management_client_ca_pem: test_ca(now).certificate_pem.clone(),
            capability_jwt_public_key_pem: "capability-public-key-pem".to_string(),
            capability_jwt_kid: "capability-kid".to_string(),
        };
        let repository = Arc::new(TaskRepository::new(database.pool.clone()));
        let service =
            AgentIdentityService::new(repository.clone(), authority, public_config.clone());
        let enrollment = service
            .create_enrollment(node_id, "admin", None, None, now)
            .await?;
        let control_csr = csr_with_sans(Vec::new());
        let management_csr = csr_with_sans(Vec::new());
        let verified =
            service.verify_enrollment_token(&enrollment.token, now + Duration::seconds(1))?;
        service
            .enroll_verified(
                &verified,
                node_id,
                &control_csr,
                &management_csr,
                None,
                None,
                now + Duration::seconds(1),
            )
            .await?;
        sqlx::query("update agent_certificates set not_after = $1 where node_id = $2")
            .bind(now + Duration::days(20))
            .bind(node_id)
            .execute(&database.pool)
            .await?;
        sqlx::query("update agent_management_certificates set not_after = $1 where node_id = $2")
            .bind(now + Duration::days(20))
            .bind(node_id)
            .execute(&database.pool)
            .await?;
        let certificate = sqlx::query(
            "select id, fingerprint_sha256 from agent_certificates where node_id = $1 and state = 'active'",
        )
        .bind(node_id)
        .fetch_one(&database.pool)
        .await?;
        let session_id = Uuid::now_v7();
        sqlx::query(
            r#"
            insert into agent_control_sessions (
              node_id, session_id, core_instance_id, certificate_id, peer_ip,
              connected_at, last_activity_at, lease_expires_at
            ) values ($1, $2, $3, $4, '192.0.2.40', $5, $5, $6)
            "#,
        )
        .bind(node_id)
        .bind(session_id)
        .bind(Uuid::now_v7())
        .bind(certificate.try_get::<Uuid, _>("id")?)
        .bind(now + Duration::seconds(2))
        .bind(now + Duration::seconds(32))
        .execute(&database.pool)
        .await?;

        let rotation_id = Uuid::now_v7();
        let new_control_csr = csr_with_sans(Vec::new());
        let new_management_csr = csr_with_sans(Vec::new());
        let rotated = service
            .rotate_agent_certificates(
                rotation_id,
                node_id,
                session_id,
                &new_control_csr,
                &new_management_csr,
                "192.0.2.40".parse().unwrap(),
                now + Duration::seconds(3),
            )
            .await?;
        let recovered = service
            .rotate_agent_certificates(
                rotation_id,
                node_id,
                session_id,
                &new_control_csr,
                &new_management_csr,
                "192.0.2.40".parse().unwrap(),
                now + Duration::seconds(3),
            )
            .await?;
        assert_eq!(rotated, recovered);
        assert_eq!(rotated.rotation_id, rotation_id);
        assert_eq!(
            rotated.agent_client_issuer_ca_pem,
            agent_client_issuer_ca_pem
        );
        assert_eq!(
            rotated.control_plane_server_ca_pem,
            public_config.control_plane_server_ca_pem
        );
        assert_eq!(
            rotated.management_client_ca_pem,
            public_config.management_client_ca_pem
        );
        assert_eq!(
            rotated.capability_jwt_public_key_pem,
            public_config.capability_jwt_public_key_pem
        );
        assert_eq!(rotated.capability_jwt_kid, public_config.capability_jwt_kid);

        let (_, control_pem) = parse_x509_pem(rotated.control_certificate_pem.as_bytes())?;
        let (_, control_certificate) =
            x509_parser::certificate::X509Certificate::from_der(&control_pem.contents)?;
        verify_issued_agent_certificate(&control_certificate, node_id)?;
        let (_, management_pem) = parse_x509_pem(rotated.management_certificate_pem.as_bytes())?;
        let (_, management_certificate) =
            x509_parser::certificate::X509Certificate::from_der(&management_pem.contents)?;
        verify_issued_agent_management_certificate(&management_certificate, node_id)?;

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn enrollment_http_flow_is_bearer_only_idempotent_and_hash_only() -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let now = Utc::now();
        let mut state = test_app_state(database.pool.clone(), test_ca(now));
        state
            .agent_identity
            .as_mut()
            .expect("Agent identity service")
            .http_admission = AgentEnrollmentHttpAdmission::with_limits_for_test(
            4,
            64,
            std::time::Duration::from_secs(1),
            16,
        );
        let identity = state
            .agent_identity
            .clone()
            .expect("Agent identity service");
        let app = build_app(state).layer(Extension(ConnectInfo(
            "192.0.2.60:43000".parse::<std::net::SocketAddr>()?,
        )));
        let node_id = Uuid::now_v7();

        let created = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/admin/agent-enrollments")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json!({ "node_id": node_id }).to_string()))?,
            )
            .await?;
        assert_eq!(created.status(), StatusCode::CREATED);
        assert_eq!(
            created.headers().get(header::CACHE_CONTROL),
            Some(&header::HeaderValue::from_static("no-store"))
        );
        assert_eq!(
            created.headers().get(header::PRAGMA),
            Some(&header::HeaderValue::from_static("no-cache"))
        );
        let created = response_json(created).await;
        let token = created["token"]
            .as_str()
            .expect("one-time token")
            .to_string();
        assert_eq!(created["node_id"], node_id.to_string());
        let verified = identity.verify_enrollment_token(&token, now)?;
        assert_eq!(verified.node_id, node_id);
        assert_eq!(
            verified.enrollment_id,
            Uuid::parse_str(created["enrollment_id"].as_str().unwrap())?
        );

        let persisted_hash: Vec<u8> =
            sqlx::query_scalar("select token_hash from agent_enrollment_tokens where node_id = $1")
                .bind(node_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(persisted_hash, sha256_array(token.as_bytes()));
        let audit_contains_token: bool = sqlx::query_scalar(
            "select exists (select 1 from security_audit_events where payload::text like '%' || $1 || '%')",
        )
        .bind(&token)
        .fetch_one(&database.pool)
        .await?;
        assert!(!audit_contains_token);

        let csr = csr_with_sans(vec![
            SanType::URI("spiffe://attacker/admin".try_into().unwrap()),
            SanType::DnsName("attacker.invalid".try_into().unwrap()),
        ]);
        let management_csr = csr_with_sans(vec![SanType::DnsName(
            "attacker.invalid".try_into().unwrap(),
        )]);
        let request_body = json!({
            "node_id": node_id,
            "csr_pem": csr,
            "management_csr_pem": management_csr,
        });
        let same_key = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/agent-enroll")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::from(
                        json!({
                            "node_id": node_id,
                            "csr_pem": request_body["csr_pem"],
                            "management_csr_pem": request_body["csr_pem"],
                        })
                        .to_string(),
                    ))?,
            )
            .await?;
        assert_eq!(same_key.status(), StatusCode::BAD_REQUEST);
        let query_only = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/agent-enroll?token={token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(request_body.to_string()))?,
            )
            .await?;
        assert_eq!(query_only.status(), StatusCode::UNAUTHORIZED);

        let malformed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/agent-enroll")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::from(
                        json!({
                            "node_id": node_id,
                            "csr_pem": "not-a-csr",
                            "management_csr_pem": request_body["management_csr_pem"],
                        })
                        .to_string(),
                    ))?,
            )
            .await?;
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        let still_unused: bool = sqlx::query_scalar(
            "select consumed_at is null from agent_enrollment_tokens where node_id = $1",
        )
        .bind(node_id)
        .fetch_one(&database.pool)
        .await?;
        assert!(still_unused);

        let mut concurrent = Vec::new();
        for _ in 0..20 {
            let app = app.clone();
            let token = token.clone();
            let body = request_body.to_string();
            concurrent.push(tokio::spawn(async move {
                for _ in 0..100 {
                    let response = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .method("POST")
                                .uri("/api/v1/agent-enroll")
                                .header(header::CONTENT_TYPE, "application/json")
                                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                                .body(Body::from(body.clone()))
                                .expect("enrollment request"),
                        )
                        .await?;
                    if response.status() != StatusCode::TOO_MANY_REQUESTS {
                        return Ok::<_, std::convert::Infallible>(response);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                panic!("enrollment admission did not reopen after completed requests")
            }));
        }
        let mut completed_bytes = None;
        for response in concurrent {
            let response = response.await??;
            let status = response.status();
            let bytes = to_bytes(response.into_body(), usize::MAX).await?;
            assert_eq!(
                status,
                StatusCode::OK,
                "unexpected enrollment response: {}",
                String::from_utf8_lossy(&bytes)
            );
            if let Some(expected) = &completed_bytes {
                assert_eq!(
                    &bytes, expected,
                    "concurrent bundles must be byte-equivalent"
                );
            } else {
                completed_bytes = Some(bytes);
            }
        }
        let completed_bytes = completed_bytes.expect("at least one enrollment response");
        let completed: Value = serde_json::from_slice(&completed_bytes)?;
        assert_eq!(completed["node_id"], node_id.to_string());
        assert!(!completed["certificate_pem"].as_str().unwrap().is_empty());
        assert!(!completed["ca_certificate_pem"].as_str().unwrap().is_empty());
        assert_eq!(
            completed["agent_client_issuer_ca_pem"],
            completed["ca_certificate_pem"]
        );
        for field in [
            "control_plane_server_ca_pem",
            "management_client_ca_pem",
            "management_certificate_pem",
            "management_fingerprint_sha256",
            "management_serial_number",
            "capability_jwt_public_key_pem",
            "capability_jwt_kid",
        ] {
            assert!(
                !completed[field].as_str().unwrap_or_default().is_empty(),
                "missing enrollment response field {field}"
            );
        }

        let replay = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/agent-enroll")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::from(request_body.to_string()))?,
            )
            .await?;
        assert_eq!(replay.status(), StatusCode::OK);
        let replay_bytes = to_bytes(replay.into_body(), usize::MAX).await?;
        assert_eq!(
            replay_bytes, completed_bytes,
            "same request retry must recover a byte-equivalent bundle"
        );

        for changed in [
            json!({
                "node_id": node_id,
                "csr_pem": csr_with_sans(Vec::new()),
                "management_csr_pem": request_body["management_csr_pem"],
            }),
            json!({
                "node_id": node_id,
                "csr_pem": request_body["csr_pem"],
                "management_csr_pem": csr_with_sans(Vec::new()),
            }),
            json!({
                "node_id": Uuid::now_v7(),
                "csr_pem": request_body["csr_pem"],
                "management_csr_pem": request_body["management_csr_pem"],
            }),
        ] {
            let rejected = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/agent-enroll")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::from(changed.to_string()))?,
                )
                .await?;
            assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        }

        let certificate_count: i64 =
            sqlx::query_scalar("select count(*) from agent_certificates where node_id = $1")
                .bind(node_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(certificate_count, 1);
        let management_certificate_count: i64 = sqlx::query_scalar(
            "select count(*) from agent_management_certificates where node_id = $1",
        )
        .bind(node_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(management_certificate_count, 1);
        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn enrollment_admin_http_requires_current_non_bootstrap_user_token() -> anyhow::Result<()>
    {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(database.pool.clone()));
        repository
            .create_bootstrap_admin("security-admin", "test-password-hash", true)
            .await?;
        let user = repository
            .find_auth_user_by_username("security-admin")
            .await?
            .expect("bootstrap administrator");
        let key_dir = tempfile::tempdir()?;
        let private_key_path = key_dir.path().join("jwt-private.pem");
        let public_key_path = key_dir.path().join("jwt-public.pem");
        std::fs::write(&private_key_path, TEST_AUTH_PRIVATE_KEY)?;
        std::fs::write(&public_key_path, TEST_AUTH_PUBLIC_KEY)?;
        let auth = AuthConfig::from_settings(&CoreSettings {
            auth_mode: AuthMode::LocalPassword,
            auth_jwt_private_key_path: private_key_path.to_string_lossy().to_string(),
            auth_jwt_public_key_path: public_key_path.to_string_lossy().to_string(),
            ..CoreSettings::default()
        })?;
        let must_change_token = auth
            .issue_access_token(
                "security-admin",
                ApiRole::Admin,
                user.credential_version,
                true,
            )?
            .token;
        let mut state = test_app_state(database.pool.clone(), test_ca(Utc::now()));
        state.auth = auth.clone();
        let app = build_app(state);

        let request = |node_id: Uuid| {
            Request::builder()
                .method("POST")
                .uri("/api/v1/admin/agent-enrollments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "node_id": node_id }).to_string()))
        };
        let missing = app.clone().oneshot(request(Uuid::now_v7())?).await?;
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);

        sqlx::query(
            "insert into machine_api_allowlist (id, cidr, description, created_at, updated_at) values ($1, '192.0.2.51/32'::cidr, 'test machine', clock_timestamp(), clock_timestamp())",
        )
        .bind(Uuid::now_v7())
        .execute(&database.pool)
        .await?;
        let mut machine_request = request(Uuid::now_v7())?;
        machine_request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(
                "192.0.2.51:4321".parse::<std::net::SocketAddr>()?,
            ));
        let machine = app.clone().oneshot(machine_request).await?;
        assert_eq!(machine.status(), StatusCode::FORBIDDEN);

        let must_change = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/admin/agent-enrollments")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {must_change_token}"))
                    .body(Body::from(json!({ "node_id": Uuid::now_v7() }).to_string()))?,
            )
            .await?;
        assert_eq!(must_change.status(), StatusCode::FORBIDDEN);

        repository
            .reset_user_password(
                "security-admin",
                "changed-password-hash",
                false,
                "test",
                "test_password_changed",
                None,
                None,
            )
            .await?;
        let current_user = repository
            .find_auth_user_by_username("security-admin")
            .await?
            .expect("current administrator");
        let admin_token = auth
            .issue_access_token(
                "security-admin",
                ApiRole::Admin,
                current_user.credential_version,
                false,
            )?
            .token;
        let admin_node = Uuid::now_v7();
        let created = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/admin/agent-enrollments")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                    .body(Body::from(json!({ "node_id": admin_node }).to_string()))?,
            )
            .await?;
        assert_eq!(created.status(), StatusCode::CREATED);
        let enrollment_count: i64 =
            sqlx::query_scalar("select count(*) from agent_enrollment_tokens")
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(enrollment_count, 1);
        let created_by: String =
            sqlx::query_scalar("select created_by from agent_enrollment_tokens where node_id = $1")
                .bind(admin_node)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(created_by, "security-admin");

        database.cleanup().await?;
        Ok(())
    }

    #[test]
    fn security_write_remains_the_admin_enrollment_permission() {
        let source = include_str!("main.rs");
        let handler = source
            .split("async fn create_agent_enrollment(")
            .nth(1)
            .and_then(|source| source.split("async fn enroll_agent(").next())
            .expect("admin enrollment handler source");
        assert!(
            handler
                .contains("authorize_api_request(&state, &headers, ApiPermission::SecurityWrite)")
        );
    }
}
