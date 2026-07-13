use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::{DateTime, TimeDelta, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error as RustlsError,
    RootCertStore, SignatureScheme,
    client::danger::HandshakeSignatureValid,
    server::{
        WebPkiClientVerifier,
        danger::{ClientCertVerified, ClientCertVerifier},
    },
};
use rustls_pki_types::{CertificateDer, UnixTime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use x509_parser::{prelude::FromDer, time::ASN1Time};

pub(crate) const CAPABILITY_ISSUER: &str = "streamserver-core";
pub(crate) const CAPABILITY_SUBJECT: &str = "core-agent-write";
pub(crate) const CAPABILITY_TOKEN_TYPE: &str = "agent-cap+jwt";
const CAPABILITY_MAX_LIFETIME_SECONDS: i64 = 120;
const CAPABILITY_CLOCK_SKEW_SECONDS: i64 = 5;
const DELETE_JTI_MAX_ENTRIES: usize = 4096;
const DELETE_JTI_MAX_AUDIT_BYTES: u64 = 8 * 1024;
const DELETE_JTI_CLOCK_FILE: &str = ".clock-high-watermark.json";
const DELETE_JTI_CLOCK_TEMP_PREFIX: &str = ".clock-high-watermark-";
#[cfg(unix)]
const DELETE_JTI_LOCK_FILE: &str = ".ledger.lock";
const DELETE_JTI_MALFORMED_QUARANTINE_SECONDS: i64 =
    CAPABILITY_MAX_LIFETIME_SECONDS + (2 * CAPABILITY_CLOCK_SKEW_SECONDS);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentWriteCapabilityClaims {
    pub(crate) iss: String,
    pub(crate) sub: String,
    pub(crate) aud: String,
    pub(crate) op: String,
    pub(crate) path: String,
    pub(crate) max_bytes: u64,
    pub(crate) jti: String,
    pub(crate) iat: i64,
    pub(crate) nbf: i64,
    pub(crate) exp: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentWriteOperation {
    Upload,
    Delete,
}

impl AgentWriteOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedCapability {
    pub(crate) jti: Uuid,
    pub(crate) path: String,
    pub(crate) max_bytes: u64,
    pub(crate) expires_at: DateTime<Utc>,
}

impl VerifiedCapability {
    pub(crate) fn authorize_path(&self, expected_path: &str) -> Result<(), ManagementAuthError> {
        if !expected_path.trim().is_empty() && self.path == expected_path {
            Ok(())
        } else {
            Err(ManagementAuthError::ForbiddenCapability)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum ManagementAuthError {
    #[error("invalid management capability")]
    InvalidCapability,
    #[error("management capability is not authorized for this operation")]
    ForbiddenCapability,
}

pub(crate) struct CapabilityVerifier {
    key: DecodingKey,
    kid: String,
    node_id: Uuid,
}

impl std::fmt::Debug for CapabilityVerifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CapabilityVerifier")
            .field("kid", &self.kid)
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl CapabilityVerifier {
    pub(crate) fn from_ed25519_public_pem_with_derived_kid(
        public_key_pem: &str,
        node_id: Uuid,
    ) -> Result<(Self, String), ManagementAuthError> {
        let (remaining, pem) = x509_parser::pem::parse_x509_pem(public_key_pem.as_bytes())
            .map_err(|_| ManagementAuthError::InvalidCapability)?;
        if !remaining.iter().all(u8::is_ascii_whitespace) || pem.label != "PUBLIC KEY" {
            return Err(ManagementAuthError::InvalidCapability);
        }
        rcgen::SubjectPublicKeyInfo::from_der(&pem.contents)
            .map_err(|_| ManagementAuthError::InvalidCapability)?;
        let kid = format!("{:x}", Sha256::digest(&pem.contents));
        let verifier = Self::from_ed25519_public_pem(public_key_pem, &kid, node_id)?;
        Ok((verifier, kid))
    }

    pub(crate) fn from_ed25519_public_pem(
        public_key_pem: &str,
        kid: &str,
        node_id: Uuid,
    ) -> Result<Self, ManagementAuthError> {
        if node_id.is_nil() || kid.trim().is_empty() || public_key_pem.trim().is_empty() {
            return Err(ManagementAuthError::InvalidCapability);
        }
        let key = DecodingKey::from_ed_pem(public_key_pem.as_bytes())
            .map_err(|_| ManagementAuthError::InvalidCapability)?;
        Ok(Self {
            key,
            kid: kid.trim().to_string(),
            node_id,
        })
    }

    #[cfg(test)]
    pub(crate) fn verify(
        &self,
        token: &str,
        expected_operation: AgentWriteOperation,
        expected_path: &str,
        local_max_bytes: u64,
        now: DateTime<Utc>,
    ) -> Result<VerifiedCapability, ManagementAuthError> {
        let capability = self.verify_operation(token, expected_operation, local_max_bytes, now)?;
        capability.authorize_path(expected_path)?;
        Ok(capability)
    }

    pub(crate) fn verify_operation(
        &self,
        token: &str,
        expected_operation: AgentWriteOperation,
        local_max_bytes: u64,
        now: DateTime<Utc>,
    ) -> Result<VerifiedCapability, ManagementAuthError> {
        if token.trim().is_empty() || local_max_bytes == 0 {
            return Err(ManagementAuthError::InvalidCapability);
        }
        let header = decode_header(token).map_err(|_| ManagementAuthError::InvalidCapability)?;
        if header.alg != Algorithm::EdDSA
            || header.typ.as_deref() != Some(CAPABILITY_TOKEN_TYPE)
            || header.kid.as_deref() != Some(self.kid.as_str())
        {
            return Err(ManagementAuthError::InvalidCapability);
        }

        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.required_spec_claims.clear();
        validation.set_audience(&[format!("agent:{}", self.node_id)]);
        let claims = decode::<AgentWriteCapabilityClaims>(token, &self.key, &validation)
            .map_err(|_| ManagementAuthError::InvalidCapability)?
            .claims;
        let jti = Uuid::parse_str(claims.jti.trim())
            .map_err(|_| ManagementAuthError::InvalidCapability)?;
        let lifetime = claims.exp.saturating_sub(claims.iat);
        let now = now.timestamp();
        if jti.is_nil()
            || claims.jti != jti.to_string()
            || claims.iss != CAPABILITY_ISSUER
            || claims.sub != CAPABILITY_SUBJECT
            || claims.iat <= 0
            || claims.nbf != claims.iat
            || !(1..=CAPABILITY_MAX_LIFETIME_SECONDS).contains(&lifetime)
            || claims.iat > now + CAPABILITY_CLOCK_SKEW_SECONDS
            || claims.nbf > now + CAPABILITY_CLOCK_SKEW_SECONDS
            || claims.exp <= now - CAPABILITY_CLOCK_SKEW_SECONDS
        {
            return Err(ManagementAuthError::InvalidCapability);
        }
        if claims.aud != format!("agent:{}", self.node_id)
            || claims.op != expected_operation.as_str()
            || claims.max_bytes == 0
            || claims.max_bytes > local_max_bytes
        {
            return Err(ManagementAuthError::ForbiddenCapability);
        }
        let expires_at = DateTime::from_timestamp(claims.exp, 0)
            .ok_or(ManagementAuthError::InvalidCapability)?;
        Ok(VerifiedCapability {
            jti,
            path: claims.path,
            max_bytes: claims.max_bytes,
            expires_at,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum CoreClientCertificateError {
    #[error("Core client certificate is invalid")]
    Invalid,
    #[error("Core client certificate is not currently valid")]
    NotCurrentlyValid,
    #[error("Core client certificate identity is invalid")]
    InvalidIdentity,
    #[error("Core client certificate usage is invalid")]
    InvalidUsage,
}

pub(crate) fn parse_core_client_identity(
    leaf_der: &[u8],
    now: DateTime<Utc>,
) -> Result<Uuid, CoreClientCertificateError> {
    let (remaining, certificate) = x509_parser::certificate::X509Certificate::from_der(leaf_der)
        .map_err(|_| CoreClientCertificateError::Invalid)?;
    if !remaining.is_empty() {
        return Err(CoreClientCertificateError::Invalid);
    }
    let validation_time = ASN1Time::from_timestamp(now.timestamp())
        .map_err(|_| CoreClientCertificateError::Invalid)?;
    if !certificate.validity().is_valid_at(validation_time) {
        return Err(CoreClientCertificateError::NotCurrentlyValid);
    }
    let san = certificate
        .subject_alternative_name()
        .map_err(|_| CoreClientCertificateError::InvalidIdentity)?
        .ok_or(CoreClientCertificateError::InvalidIdentity)?;
    let [x509_parser::extensions::GeneralName::URI(identity)] = san.value.general_names.as_slice()
    else {
        return Err(CoreClientCertificateError::InvalidIdentity);
    };
    let core_id_text = identity
        .strip_prefix("spiffe://streamserver/core/")
        .ok_or(CoreClientCertificateError::InvalidIdentity)?;
    let core_id =
        Uuid::parse_str(core_id_text).map_err(|_| CoreClientCertificateError::InvalidIdentity)?;
    if core_id.is_nil() || *identity != format!("spiffe://streamserver/core/{core_id}") {
        return Err(CoreClientCertificateError::InvalidIdentity);
    }
    let basic_constraints = certificate
        .basic_constraints()
        .map_err(|_| CoreClientCertificateError::InvalidUsage)?;
    if basic_constraints.is_some_and(|extension| extension.value.ca) {
        return Err(CoreClientCertificateError::InvalidUsage);
    }
    let key_usage = certificate
        .key_usage()
        .map_err(|_| CoreClientCertificateError::InvalidUsage)?
        .ok_or(CoreClientCertificateError::InvalidUsage)?;
    if key_usage.value.flags != 1 || !key_usage.value.digital_signature() {
        return Err(CoreClientCertificateError::InvalidUsage);
    }
    let extended = certificate
        .extended_key_usage()
        .map_err(|_| CoreClientCertificateError::InvalidUsage)?
        .ok_or(CoreClientCertificateError::InvalidUsage)?;
    if !extended.value.client_auth
        || extended.value.any
        || extended.value.server_auth
        || extended.value.code_signing
        || extended.value.email_protection
        || extended.value.time_stamping
        || extended.value.ocsp_signing
        || !extended.value.other.is_empty()
    {
        return Err(CoreClientCertificateError::InvalidUsage);
    }
    Ok(core_id)
}

#[derive(Debug)]
pub(crate) struct CoreClientCertificateVerifier {
    inner: ArcClientCertVerifier,
}

type ArcClientCertVerifier = std::sync::Arc<dyn ClientCertVerifier>;

impl CoreClientCertificateVerifier {
    pub(crate) fn new(roots: RootCertStore) -> anyhow::Result<Self> {
        anyhow::ensure!(!roots.is_empty(), "Core client CA store must not be empty");
        let inner = WebPkiClientVerifier::builder(std::sync::Arc::new(roots))
            .build()
            .map_err(|error| anyhow::anyhow!("build Core client certificate verifier: {error}"))?;
        Ok(Self { inner })
    }
}

impl ClientCertVerifier for CoreClientCertificateVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, RustlsError> {
        if !intermediates.is_empty() {
            return Err(profile_verification_error());
        }
        let verified = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;
        let timestamp = i64::try_from(now.as_secs()).map_err(|_| profile_verification_error())?;
        let now = DateTime::from_timestamp(timestamp, 0).ok_or_else(profile_verification_error)?;
        parse_core_client_identity(end_entity.as_ref(), now)
            .map_err(|_| profile_verification_error())?;
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

fn profile_verification_error() -> RustlsError {
    RustlsError::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

#[derive(Debug, Clone)]
pub(crate) struct DeleteJtiStore {
    root: PathBuf,
    max_entries: usize,
    mutation: Arc<Mutex<()>>,
    #[cfg(all(test, unix))]
    test_capacity_pause: std::time::Duration,
    #[cfg(all(test, unix))]
    test_clock_persist_hook: Option<Arc<DeleteJtiClockPersistHook>>,
    #[cfg(all(test, unix))]
    test_clock_persist_files: Option<DeleteJtiClockPersistFileHook>,
}

#[cfg(all(test, unix))]
#[derive(Debug)]
struct DeleteJtiClockPersistHook {
    reached: std::sync::Barrier,
    release: std::sync::Barrier,
}

#[cfg(all(test, unix))]
#[derive(Debug, Clone)]
struct DeleteJtiClockPersistFileHook {
    ready: PathBuf,
    release: PathBuf,
}

struct DeleteJtiMutationGuard<'a> {
    _process_guard: std::sync::MutexGuard<'a, ()>,
    #[cfg(unix)]
    lock_file: File,
}

impl Drop for DeleteJtiMutationGuard<'_> {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self.lock_file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum DeleteJtiError {
    #[error("delete capability was already consumed")]
    Replay,
    #[error("delete capability expired before it could be consumed")]
    ExpiredCapability,
    #[error("delete capability ledger reached its bounded capacity")]
    Capacity,
    #[error("system clock moved backwards across the delete capability replay boundary")]
    ClockRollback,
    #[error("delete capability ledger failed")]
    Io(#[source] io::Error),
    #[error("delete capability audit encoding failed")]
    Encoding(#[source] serde_json::Error),
}

#[derive(Debug, Serialize, Deserialize)]
struct DeleteJtiAudit {
    jti: Uuid,
    node_id: Uuid,
    op: String,
    path_sha256: String,
    consumed_at: DateTime<Utc>,
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
    outcome: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct DeleteJtiClockHighWatermark {
    last_seen_at: DateTime<Utc>,
}

impl DeleteJtiStore {
    pub(crate) fn new(root: impl AsRef<Path>) -> Result<Self, DeleteJtiError> {
        Self::new_with_limit_at(root, DELETE_JTI_MAX_ENTRIES, Utc::now())
    }

    fn new_with_limit_at(
        root: impl AsRef<Path>,
        max_entries: usize,
        now: DateTime<Utc>,
    ) -> Result<Self, DeleteJtiError> {
        if max_entries == 0 {
            return Err(DeleteJtiError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "delete capability ledger capacity must be positive",
            )));
        }
        let root = root.as_ref().to_path_buf();
        create_secure_directory(&root).map_err(DeleteJtiError::Io)?;
        let store = Self {
            root,
            max_entries,
            mutation: Arc::new(Mutex::new(())),
            #[cfg(all(test, unix))]
            test_capacity_pause: std::time::Duration::ZERO,
            #[cfg(all(test, unix))]
            test_clock_persist_hook: None,
            #[cfg(all(test, unix))]
            test_clock_persist_files: None,
        };
        store.prune_expired(now)?;
        Ok(store)
    }

    pub(crate) fn consume(
        &self,
        jti: Uuid,
        node_id: Uuid,
        path: &str,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), DeleteJtiError> {
        if jti.is_nil() || node_id.is_nil() || path.trim().is_empty() {
            return Err(DeleteJtiError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid delete capability audit input",
            )));
        }
        let _mutation = self.lock_mutation()?;
        let effective_now = self.check_and_advance_clock_locked(now)?;
        let earliest_valid_expiry = effective_now
            .checked_sub_signed(TimeDelta::seconds(CAPABILITY_CLOCK_SKEW_SECONDS))
            .ok_or_else(|| {
                DeleteJtiError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "delete capability expiry is outside the supported range",
                ))
            })?;
        let latest_valid_expiry = effective_now
            .checked_add_signed(TimeDelta::seconds(
                CAPABILITY_MAX_LIFETIME_SECONDS + CAPABILITY_CLOCK_SKEW_SECONDS,
            ))
            .ok_or_else(|| {
                DeleteJtiError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "delete capability expiry is outside the supported range",
                ))
            })?;
        if expires_at <= earliest_valid_expiry {
            return Err(DeleteJtiError::ExpiredCapability);
        }
        if expires_at > latest_valid_expiry {
            return Err(DeleteJtiError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid delete capability audit input",
            )));
        }
        self.prune_expired_locked(effective_now)?;
        let audit_path = self.root.join(format!("{jti}.json"));
        match fs::symlink_metadata(&audit_path) {
            Ok(_) => return Err(DeleteJtiError::Replay),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(DeleteJtiError::Io(error)),
        }
        if self.entry_count_locked()? >= self.max_entries {
            return Err(DeleteJtiError::Capacity);
        }
        #[cfg(all(test, unix))]
        if !self.test_capacity_pause.is_zero() {
            std::thread::sleep(self.test_capacity_pause);
        }
        let path_sha256 = format!("{:x}", Sha256::digest(path.as_bytes()));
        let audit = serde_json::to_vec(&DeleteJtiAudit {
            jti,
            node_id,
            op: "delete".to_string(),
            path_sha256,
            consumed_at: effective_now,
            expires_at: Some(expires_at),
            outcome: "started".to_string(),
        })
        .map_err(DeleteJtiError::Encoding)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&audit_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(DeleteJtiError::Replay);
            }
            Err(error) => return Err(DeleteJtiError::Io(error)),
        };
        let persist_result = file
            .write_all(&audit)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all());
        if let Err(error) = persist_result {
            drop(file);
            let _ = fs::remove_file(&audit_path);
            let _ = sync_directory(&self.root);
            return Err(DeleteJtiError::Io(error));
        }
        File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .map_err(DeleteJtiError::Io)?;
        Ok(())
    }

    fn prune_expired(&self, now: DateTime<Utc>) -> Result<(), DeleteJtiError> {
        let _mutation = self.lock_mutation()?;
        let effective_now = self.check_and_advance_clock_locked(now)?;
        self.prune_expired_locked(effective_now)
    }

    fn prune_expired_locked(&self, now: DateTime<Utc>) -> Result<(), DeleteJtiError> {
        let cutoff = now
            .checked_sub_signed(TimeDelta::seconds(CAPABILITY_CLOCK_SKEW_SECONDS))
            .ok_or_else(|| {
                DeleteJtiError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "delete capability cleanup time is outside the supported range",
                ))
            })?;
        let mut removed = false;
        for entry in fs::read_dir(&self.root).map_err(DeleteJtiError::Io)? {
            let entry = entry.map_err(DeleteJtiError::Io)?;
            let path = entry.path();
            let file_name = path.file_name().and_then(|value| value.to_str());
            if file_name.is_some_and(|value| {
                value.starts_with(DELETE_JTI_CLOCK_TEMP_PREFIX) && value.ends_with(".tmp")
            }) {
                if fs::symlink_metadata(&path)
                    .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    fs::remove_file(&path).map_err(DeleteJtiError::Io)?;
                    removed = true;
                }
                continue;
            }
            let Some(file_name_jti) = canonical_audit_jti(&path) else {
                continue;
            };
            let metadata = fs::symlink_metadata(&path).map_err(DeleteJtiError::Io)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(DeleteJtiError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "delete capability audit path is not a regular file",
                )));
            }
            let modified_at = metadata
                .modified()
                .map(DateTime::<Utc>::from)
                .map_err(DeleteJtiError::Io)?;
            let audit = match read_regular_audit_file(&path) {
                Ok(bytes) => serde_json::from_slice::<DeleteJtiAudit>(&bytes).ok(),
                Err(error) if error.kind() == io::ErrorKind::InvalidData => None,
                Err(error) => return Err(DeleteJtiError::Io(error)),
            }
            .filter(|audit| delete_jti_audit_is_reasonable(audit, file_name_jti, modified_at, now));
            let should_remove = if let Some(audit) = audit {
                let replay_until = audit.expires_at.unwrap_or_else(|| {
                    audit
                        .consumed_at
                        .checked_add_signed(TimeDelta::seconds(
                            CAPABILITY_MAX_LIFETIME_SECONDS + CAPABILITY_CLOCK_SKEW_SECONDS,
                        ))
                        .unwrap_or(DateTime::<Utc>::MAX_UTC)
                });
                replay_until <= cutoff
            } else {
                malformed_tombstone_quarantine_elapsed(modified_at, now)
                    .map_err(DeleteJtiError::Io)?
            };
            if should_remove {
                fs::remove_file(&path).map_err(DeleteJtiError::Io)?;
                removed = true;
            }
        }
        if removed {
            sync_directory(&self.root).map_err(DeleteJtiError::Io)?;
        }
        Ok(())
    }

    fn entry_count_locked(&self) -> Result<usize, DeleteJtiError> {
        let mut count = 0usize;
        for entry in fs::read_dir(&self.root).map_err(DeleteJtiError::Io)? {
            let entry = entry.map_err(DeleteJtiError::Io)?;
            if canonical_audit_jti(&entry.path()).is_none() {
                continue;
            }
            count = count.saturating_add(1);
            if count >= self.max_entries {
                break;
            }
        }
        Ok(count)
    }

    fn check_and_advance_clock_locked(
        &self,
        now: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, DeleteJtiError> {
        let path = self.root.join(DELETE_JTI_CLOCK_FILE);
        let previous = match fs::symlink_metadata(&path) {
            Ok(_) => {
                let bytes = read_regular_audit_file(&path).map_err(DeleteJtiError::Io)?;
                let watermark = serde_json::from_slice::<DeleteJtiClockHighWatermark>(&bytes)
                    .map_err(DeleteJtiError::Encoding)?;
                Some(watermark.last_seen_at)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(DeleteJtiError::Io(error)),
        };
        if let Some(previous) = previous {
            let rollback_limit = previous
                .checked_sub_signed(TimeDelta::seconds(CAPABILITY_CLOCK_SKEW_SECONDS))
                .unwrap_or(DateTime::<Utc>::MIN_UTC);
            if now < rollback_limit {
                return Err(DeleteJtiError::ClockRollback);
            }
            if now > previous {
                self.persist_clock_high_watermark_locked(now)?;
                Ok(now)
            } else {
                Ok(previous)
            }
        } else {
            self.persist_clock_high_watermark_locked(now)?;
            Ok(now)
        }
    }

    fn persist_clock_high_watermark_locked(
        &self,
        last_seen_at: DateTime<Utc>,
    ) -> Result<(), DeleteJtiError> {
        let contents = serde_json::to_vec(&DeleteJtiClockHighWatermark { last_seen_at })
            .map_err(DeleteJtiError::Encoding)?;
        let target = self.root.join(DELETE_JTI_CLOCK_FILE);
        let temporary = self.root.join(format!(
            "{DELETE_JTI_CLOCK_TEMP_PREFIX}{}.tmp",
            Uuid::now_v7()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(DeleteJtiError::Io)?;
        let write_result = file
            .write_all(&contents)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all());
        if let Err(error) = write_result {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(DeleteJtiError::Io(error));
        }
        drop(file);
        #[cfg(all(test, unix))]
        if let Some(hook) = &self.test_clock_persist_hook {
            hook.reached.wait();
            hook.release.wait();
        }
        #[cfg(all(test, unix))]
        if let Some(hook) = &self.test_clock_persist_files {
            let mut ready = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&hook.ready)
                .map_err(DeleteJtiError::Io)?;
            ready.write_all(b"ready\n").map_err(DeleteJtiError::Io)?;
            ready.sync_all().map_err(DeleteJtiError::Io)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !hook.release.exists() {
                if std::time::Instant::now() >= deadline {
                    return Err(DeleteJtiError::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "delete capability clock persistence test hook timed out",
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        #[cfg(not(windows))]
        let replace_result = fs::rename(&temporary, &target);
        #[cfg(windows)]
        let replace_result = (|| {
            match fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            fs::rename(&temporary, &target)
        })();
        if let Err(error) = replace_result {
            let _ = fs::remove_file(&temporary);
            return Err(DeleteJtiError::Io(error));
        }
        sync_directory(&self.root).map_err(DeleteJtiError::Io)
    }

    fn lock_mutation(&self) -> Result<DeleteJtiMutationGuard<'_>, DeleteJtiError> {
        let process_guard = self.mutation.lock().map_err(|_| {
            DeleteJtiError::Io(io::Error::other(
                "delete capability ledger mutation lock was poisoned",
            ))
        })?;
        #[cfg(unix)]
        let lock_file = acquire_delete_jti_process_lock(&self.root)?;
        Ok(DeleteJtiMutationGuard {
            _process_guard: process_guard,
            #[cfg(unix)]
            lock_file,
        })
    }
}

#[cfg(unix)]
fn acquire_delete_jti_process_lock(root: &Path) -> Result<File, DeleteJtiError> {
    use std::{os::fd::AsRawFd, os::unix::fs::MetadataExt, os::unix::fs::OpenOptionsExt};

    let root_before = fs::symlink_metadata(root).map_err(DeleteJtiError::Io)?;
    validate_delete_jti_root_metadata(&root_before).map_err(DeleteJtiError::Io)?;
    let lock_path = root.join(DELETE_JTI_LOCK_FILE);
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let lock_file = options.open(&lock_path).map_err(DeleteJtiError::Io)?;
    validate_delete_jti_lock_metadata(&lock_file.metadata().map_err(DeleteJtiError::Io)?)
        .map_err(DeleteJtiError::Io)?;
    loop {
        let result = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
        if result == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(DeleteJtiError::Io(error));
        }
    }

    let root_after = fs::symlink_metadata(root).map_err(DeleteJtiError::Io)?;
    validate_delete_jti_root_metadata(&root_after).map_err(DeleteJtiError::Io)?;
    if root_before.dev() != root_after.dev() || root_before.ino() != root_after.ino() {
        return Err(DeleteJtiError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "delete capability ledger directory changed while acquiring its lock",
        )));
    }
    let open_lock = lock_file.metadata().map_err(DeleteJtiError::Io)?;
    validate_delete_jti_lock_metadata(&open_lock).map_err(DeleteJtiError::Io)?;
    let named_lock = fs::symlink_metadata(&lock_path).map_err(DeleteJtiError::Io)?;
    validate_delete_jti_lock_metadata(&named_lock).map_err(DeleteJtiError::Io)?;
    if open_lock.dev() != named_lock.dev() || open_lock.ino() != named_lock.ino() {
        return Err(DeleteJtiError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "delete capability ledger lock path changed while locked",
        )));
    }
    Ok(lock_file)
}

#[cfg(unix)]
fn validate_delete_jti_root_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "delete capability ledger directory is not secure",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_delete_jti_lock_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "delete capability ledger lock is not a private regular file",
        ));
    }
    Ok(())
}

fn delete_jti_audit_is_reasonable(
    audit: &DeleteJtiAudit,
    file_name_jti: Uuid,
    modified_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
) -> bool {
    let Some(latest_consumed_at) =
        modified_at.checked_add_signed(TimeDelta::seconds(CAPABILITY_CLOCK_SKEW_SECONDS))
    else {
        return false;
    };
    let Some(latest_observed_at) =
        observed_at.checked_add_signed(TimeDelta::seconds(CAPABILITY_CLOCK_SKEW_SECONDS))
    else {
        return false;
    };
    if audit.jti != file_name_jti
        || audit.node_id.is_nil()
        || audit.op != "delete"
        || audit.outcome != "started"
        || !is_lowercase_sha256(&audit.path_sha256)
        || audit.consumed_at > latest_consumed_at
        || audit.consumed_at > latest_observed_at
    {
        return false;
    }
    let Some(expires_at) = audit.expires_at else {
        return true;
    };
    let Some(earliest_expiry) = audit
        .consumed_at
        .checked_sub_signed(TimeDelta::seconds(CAPABILITY_CLOCK_SKEW_SECONDS))
    else {
        return false;
    };
    let Some(latest_expiry) = audit.consumed_at.checked_add_signed(TimeDelta::seconds(
        CAPABILITY_MAX_LIFETIME_SECONDS + CAPABILITY_CLOCK_SKEW_SECONDS,
    )) else {
        return false;
    };
    expires_at > earliest_expiry && expires_at <= latest_expiry
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_audit_jti(path: &Path) -> Option<Uuid> {
    if path.extension().and_then(|value| value.to_str()) != Some("json") {
        return None;
    }
    let file_stem = path.file_stem()?.to_str()?;
    let jti = Uuid::parse_str(file_stem).ok()?;
    (jti.to_string() == file_stem).then_some(jti)
}

fn malformed_tombstone_quarantine_elapsed(
    modified_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> io::Result<bool> {
    let latest_reasonable_mtime = now
        .checked_add_signed(TimeDelta::seconds(CAPABILITY_CLOCK_SKEW_SECONDS))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "delete capability quarantine time is outside the supported range",
            )
        })?;
    if modified_at > latest_reasonable_mtime {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "delete capability audit modification time is unexpectedly in the future",
        ));
    }
    let quarantine_until = modified_at
        .checked_add_signed(TimeDelta::seconds(DELETE_JTI_MALFORMED_QUARANTINE_SECONDS))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "delete capability quarantine time is outside the supported range",
            )
        })?;
    Ok(quarantine_until <= now)
}

fn read_regular_audit_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "delete capability audit is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(DELETE_JTI_MAX_AUDIT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > DELETE_JTI_MAX_AUDIT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "delete capability audit exceeds the size limit",
        ));
    }
    Ok(bytes)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn create_secure_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)?;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "delete capability ledger directory is not secure",
            ));
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)?;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "delete capability ledger directory is not secure",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, thread};

    #[cfg(unix)]
    use std::process::{Child, Command, Stdio};

    use chrono::{Duration, Utc};
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose, SanType,
    };
    use rustls::{RootCertStore, server::danger::ClientCertVerifier};
    use rustls_pki_types::{CertificateDer, UnixTime};
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::*;

    #[cfg(unix)]
    const DELETE_JTI_SUBPROCESS_HELPER: &str =
        "management_auth::tests::delete_jti_subprocess_helper";

    #[cfg(unix)]
    fn helper_time(name: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&std::env::var(name).unwrap())
            .unwrap()
            .with_timezone(&Utc)
    }

    #[cfg(unix)]
    fn wait_for_test_file(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn subprocess_result(result: Result<(), DeleteJtiError>) -> &'static str {
        match result {
            Ok(()) => "ok",
            Err(DeleteJtiError::Replay) => "replay",
            Err(DeleteJtiError::ExpiredCapability) => "expired",
            Err(DeleteJtiError::Capacity) => "capacity",
            Err(DeleteJtiError::ClockRollback) => "clock-rollback",
            Err(DeleteJtiError::Io(_)) => "io",
            Err(DeleteJtiError::Encoding(_)) => "encoding",
        }
    }

    #[cfg(unix)]
    struct DeleteJtiSubprocessSpec<'a> {
        root: &'a Path,
        init_at: DateTime<Utc>,
        consume_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        max_entries: usize,
        jti: Uuid,
        node_id: Uuid,
        result: &'a Path,
    }

    #[cfg(unix)]
    fn subprocess_command(spec: DeleteJtiSubprocessSpec<'_>) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                DELETE_JTI_SUBPROCESS_HELPER,
                "--test-threads=1",
            ])
            .env("STREAMSERVER_DELETE_JTI_HELPER", "v1")
            .env("STREAMSERVER_DELETE_JTI_ROOT", spec.root)
            .env("STREAMSERVER_DELETE_JTI_INIT_AT", spec.init_at.to_rfc3339())
            .env(
                "STREAMSERVER_DELETE_JTI_CONSUME_AT",
                spec.consume_at.to_rfc3339(),
            )
            .env(
                "STREAMSERVER_DELETE_JTI_EXPIRES_AT",
                spec.expires_at.to_rfc3339(),
            )
            .env(
                "STREAMSERVER_DELETE_JTI_MAX_ENTRIES",
                spec.max_entries.to_string(),
            )
            .env("STREAMSERVER_DELETE_JTI_JTI", spec.jti.to_string())
            .env("STREAMSERVER_DELETE_JTI_NODE_ID", spec.node_id.to_string())
            .env("STREAMSERVER_DELETE_JTI_RESULT", spec.result)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[cfg(unix)]
    fn wait_for_success(child: &mut Child) {
        let status = child.wait().unwrap();
        assert!(status.success(), "delete JTI subprocess failed: {status}");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "invoked only as a subprocess helper"]
    fn delete_jti_subprocess_helper() {
        if std::env::var("STREAMSERVER_DELETE_JTI_HELPER").as_deref() != Ok("v1") {
            return;
        }
        if let Some(started) = std::env::var_os("STREAMSERVER_DELETE_JTI_STARTED") {
            std::fs::write(started, b"started\n").unwrap();
        }
        let root = PathBuf::from(std::env::var_os("STREAMSERVER_DELETE_JTI_ROOT").unwrap());
        let init_at = helper_time("STREAMSERVER_DELETE_JTI_INIT_AT");
        let consume_at = helper_time("STREAMSERVER_DELETE_JTI_CONSUME_AT");
        let expires_at = helper_time("STREAMSERVER_DELETE_JTI_EXPIRES_AT");
        let max_entries = std::env::var("STREAMSERVER_DELETE_JTI_MAX_ENTRIES")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let jti = std::env::var("STREAMSERVER_DELETE_JTI_JTI")
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        let node_id = std::env::var("STREAMSERVER_DELETE_JTI_NODE_ID")
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        let result_path =
            PathBuf::from(std::env::var_os("STREAMSERVER_DELETE_JTI_RESULT").unwrap());
        let mut store = DeleteJtiStore::new_with_limit_at(&root, max_entries, init_at).unwrap();
        if let Ok(milliseconds) = std::env::var("STREAMSERVER_DELETE_JTI_CAPACITY_PAUSE_MS") {
            store.test_capacity_pause =
                std::time::Duration::from_millis(milliseconds.parse().unwrap());
        }
        if let (Some(ready), Some(release)) = (
            std::env::var_os("STREAMSERVER_DELETE_JTI_CLOCK_READY"),
            std::env::var_os("STREAMSERVER_DELETE_JTI_CLOCK_RELEASE"),
        ) {
            store.test_clock_persist_files = Some(DeleteJtiClockPersistFileHook {
                ready: PathBuf::from(ready),
                release: PathBuf::from(release),
            });
        }
        if let (Some(ready), Some(go)) = (
            std::env::var_os("STREAMSERVER_DELETE_JTI_READY"),
            std::env::var_os("STREAMSERVER_DELETE_JTI_GO"),
        ) {
            std::fs::write(ready, b"ready\n").unwrap();
            wait_for_test_file(Path::new(&go));
        }
        let outcome = store.consume(
            jti,
            node_id,
            &format!("uploads/{jti}.mp4"),
            expires_at,
            consume_at,
        );
        std::fs::write(result_path, subprocess_result(outcome)).unwrap();
    }

    fn authority(now: chrono::DateTime<Utc>) -> (rcgen::Certificate, KeyPair) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.not_before =
            time::OffsetDateTime::from_unix_timestamp((now - Duration::days(1)).timestamp())
                .unwrap();
        params.not_after =
            time::OffsetDateTime::from_unix_timestamp((now + Duration::days(365)).timestamp())
                .unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert, key)
    }

    fn core_client_der(
        authority: &(rcgen::Certificate, KeyPair),
        now: chrono::DateTime<Utc>,
        sans: Vec<SanType>,
        extended_key_usages: Vec<ExtendedKeyUsagePurpose>,
    ) -> Vec<u8> {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.is_ca = IsCa::NoCa;
        params.subject_alt_names = sans;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = extended_key_usages;
        params.not_before =
            time::OffsetDateTime::from_unix_timestamp((now - Duration::minutes(1)).timestamp())
                .unwrap();
        params.not_after =
            time::OffsetDateTime::from_unix_timestamp((now + Duration::days(30)).timestamp())
                .unwrap();
        params
            .signed_by(&key, &authority.0, &authority.1)
            .unwrap()
            .der()
            .to_vec()
    }

    #[test]
    fn core_client_certificate_requires_exact_core_spiffe_and_client_only_profile() {
        let now = Utc::now();
        let ca = authority(now);
        let core_id = Uuid::now_v7();
        let valid = core_client_der(
            &ca,
            now,
            vec![SanType::URI(
                format!("spiffe://streamserver/core/{core_id}")
                    .try_into()
                    .unwrap(),
            )],
            vec![ExtendedKeyUsagePurpose::ClientAuth],
        );
        assert_eq!(parse_core_client_identity(&valid, now).unwrap(), core_id);

        let wrong_identity = core_client_der(
            &ca,
            now,
            vec![SanType::URI(
                format!("spiffe://streamserver/agent/{core_id}")
                    .try_into()
                    .unwrap(),
            )],
            vec![ExtendedKeyUsagePurpose::ClientAuth],
        );
        assert!(parse_core_client_identity(&wrong_identity, now).is_err());

        let mixed_usage = core_client_der(
            &ca,
            now,
            vec![SanType::URI(
                format!("spiffe://streamserver/core/{core_id}")
                    .try_into()
                    .unwrap(),
            )],
            vec![
                ExtendedKeyUsagePurpose::ClientAuth,
                ExtendedKeyUsagePurpose::ServerAuth,
            ],
        );
        assert!(parse_core_client_identity(&mixed_usage, now).is_err());
    }

    #[test]
    fn core_client_certificate_rejects_nil_multiple_sans_and_trailing_der() {
        let now = Utc::now();
        let ca = authority(now);
        let nil = core_client_der(
            &ca,
            now,
            vec![SanType::URI(
                "spiffe://streamserver/core/00000000-0000-0000-0000-000000000000"
                    .try_into()
                    .unwrap(),
            )],
            vec![ExtendedKeyUsagePurpose::ClientAuth],
        );
        assert!(parse_core_client_identity(&nil, now).is_err());

        let core_id = Uuid::now_v7();
        let multiple = core_client_der(
            &ca,
            now,
            vec![
                SanType::URI(
                    format!("spiffe://streamserver/core/{core_id}")
                        .try_into()
                        .unwrap(),
                ),
                SanType::DnsName("extra.example".try_into().unwrap()),
            ],
            vec![ExtendedKeyUsagePurpose::ClientAuth],
        );
        assert!(parse_core_client_identity(&multiple, now).is_err());

        let mut trailing = core_client_der(
            &ca,
            now,
            vec![SanType::URI(
                format!("spiffe://streamserver/core/{core_id}")
                    .try_into()
                    .unwrap(),
            )],
            vec![ExtendedKeyUsagePurpose::ClientAuth],
        );
        trailing.push(0);
        assert!(parse_core_client_identity(&trailing, now).is_err());
    }

    #[test]
    fn rustls_client_verifier_requires_trusted_ca_and_core_profile() {
        let now = Utc::now();
        let trusted = authority(now);
        let untrusted = authority(now);
        let core_id = Uuid::now_v7();
        let sans = || {
            vec![SanType::URI(
                format!("spiffe://streamserver/core/{core_id}")
                    .try_into()
                    .unwrap(),
            )]
        };
        let trusted_leaf = core_client_der(
            &trusted,
            now,
            sans(),
            vec![ExtendedKeyUsagePurpose::ClientAuth],
        );
        let untrusted_leaf = core_client_der(
            &untrusted,
            now,
            sans(),
            vec![ExtendedKeyUsagePurpose::ClientAuth],
        );
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(trusted.0.der().to_vec()))
            .unwrap();
        let verifier = CoreClientCertificateVerifier::new(roots).unwrap();
        let verification_time = UnixTime::since_unix_epoch(std::time::Duration::from_secs(
            u64::try_from(now.timestamp()).unwrap(),
        ));

        assert!(
            verifier
                .verify_client_cert(&CertificateDer::from(trusted_leaf), &[], verification_time,)
                .is_ok()
        );
        assert!(
            verifier
                .verify_client_cert(
                    &CertificateDer::from(untrusted_leaf),
                    &[],
                    verification_time,
                )
                .is_err()
        );
        assert!(
            verifier
                .verify_client_cert(
                    &CertificateDer::from(core_client_der(
                        &trusted,
                        now,
                        sans(),
                        vec![ExtendedKeyUsagePurpose::ClientAuth],
                    )),
                    &[CertificateDer::from(trusted.0.der().to_vec())],
                    verification_time,
                )
                .is_err(),
            "management client certificate must be signed directly by its dedicated root"
        );
    }

    fn signed_capability(
        key: &EncodingKey,
        kid: &str,
        claims: &AgentWriteCapabilityClaims,
    ) -> String {
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(CAPABILITY_TOKEN_TYPE.to_string());
        header.kid = Some(kid.to_string());
        encode(&header, claims, key).unwrap()
    }

    fn delete_jti_audit_count(root: &Path) -> usize {
        std::fs::read_dir(root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| canonical_audit_jti(&entry.path()).is_some())
            .count()
    }

    fn write_delete_jti_audit(root: &Path, audit: &DeleteJtiAudit) {
        let path = root.join(format!("{}.json", audit.jti));
        let mut contents = serde_json::to_vec(audit).unwrap();
        contents.push(b'\n');
        std::fs::write(path, contents).unwrap();
    }

    fn file_modified_at(path: &Path) -> DateTime<Utc> {
        DateTime::<Utc>::from(std::fs::symlink_metadata(path).unwrap().modified().unwrap())
    }

    #[test]
    fn capability_verifier_enforces_header_audience_scope_and_lifetime() {
        let key = KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let private_pem = key.serialize_pem();
        let public_pem = key.public_key_pem();
        let node_id = Uuid::now_v7();
        let kid = "test-kid";
        let verifier =
            CapabilityVerifier::from_ed25519_public_pem(&public_pem, kid, node_id).unwrap();
        let now = Utc::now();
        let claims = AgentWriteCapabilityClaims {
            iss: CAPABILITY_ISSUER.to_string(),
            sub: CAPABILITY_SUBJECT.to_string(),
            aud: format!("agent:{node_id}"),
            op: "delete".to_string(),
            path: format!("uploads/{node_id}/clip.mp4"),
            max_bytes: 4096,
            jti: Uuid::now_v7().to_string(),
            iat: now.timestamp(),
            nbf: now.timestamp(),
            exp: (now + Duration::seconds(60)).timestamp(),
        };
        let token = signed_capability(
            &EncodingKey::from_ed_pem(private_pem.as_bytes()).unwrap(),
            kid,
            &claims,
        );
        let verified = verifier
            .verify(&token, AgentWriteOperation::Delete, &claims.path, 4096, now)
            .unwrap();
        assert_eq!(verified.jti.to_string(), claims.jti);
        let operation_verified = verifier
            .verify_operation(&token, AgentWriteOperation::Delete, 4096, now)
            .unwrap();
        assert!(operation_verified.authorize_path(&claims.path).is_ok());
        assert!(
            operation_verified
                .authorize_path("uploads/other/file.mp4")
                .is_err()
        );

        let mut wrong_audience = claims.clone();
        wrong_audience.aud = format!("agent:{}", Uuid::now_v7());
        let token = signed_capability(
            &EncodingKey::from_ed_pem(private_pem.as_bytes()).unwrap(),
            kid,
            &wrong_audience,
        );
        assert!(
            verifier
                .verify(&token, AgentWriteOperation::Delete, &claims.path, 4096, now)
                .is_err()
        );

        let mut too_long = claims.clone();
        too_long.exp = (now + Duration::seconds(121)).timestamp();
        let token = signed_capability(
            &EncodingKey::from_ed_pem(private_pem.as_bytes()).unwrap(),
            kid,
            &too_long,
        );
        assert!(
            verifier
                .verify(&token, AgentWriteOperation::Delete, &claims.path, 4096, now)
                .is_err()
        );
    }

    #[test]
    fn capability_kid_is_derived_from_public_key_der() {
        let key = KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let expected = format!("{:x}", Sha256::digest(key.public_key_der()));
        let (_, kid) = CapabilityVerifier::from_ed25519_public_pem_with_derived_kid(
            &key.public_key_pem(),
            Uuid::now_v7(),
        )
        .unwrap();

        assert_eq!(kid, expected);
    }

    #[test]
    fn delete_jti_store_has_one_durable_winner_under_concurrency() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("delete-jti");
        let store = Arc::new(DeleteJtiStore::new(&root).unwrap());
        let jti = Uuid::now_v7();
        let node_id = Uuid::now_v7();
        let path = format!("uploads/{node_id}/clip.mp4");
        let now = Utc::now();
        let expires_at = now + Duration::seconds(60);

        let handles = (0..20)
            .map(|_| {
                let store = store.clone();
                let path = path.clone();
                thread::spawn(move || store.consume(jti, node_id, &path, expires_at, now))
            })
            .collect::<Vec<_>>();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, Err(DeleteJtiError::Replay)))
                .count(),
            19
        );

        let reopened = DeleteJtiStore::new(&root).unwrap();
        assert!(matches!(
            reopened.consume(jti, node_id, &path, expires_at, now),
            Err(DeleteJtiError::Replay)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn independent_delete_jti_stores_do_not_exceed_capacity_concurrently() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("delete-jti");
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let mut first = DeleteJtiStore::new_with_limit_at(&root, 1, now).unwrap();
        let mut second = DeleteJtiStore::new_with_limit_at(&root, 1, now).unwrap();
        first.test_capacity_pause = std::time::Duration::from_millis(250);
        second.test_capacity_pause = std::time::Duration::from_millis(250);

        let first = thread::spawn(move || {
            first.consume(
                Uuid::now_v7(),
                node_id,
                "uploads/first.mp4",
                now + Duration::seconds(60),
                now,
            )
        });
        let second = thread::spawn(move || {
            second.consume(
                Uuid::now_v7(),
                node_id,
                "uploads/second.mp4",
                now + Duration::seconds(60),
                now,
            )
        });
        let outcomes = [first.join().unwrap(), second.join().unwrap()];

        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, Err(DeleteJtiError::Capacity)))
                .count(),
            1
        );
        assert_eq!(delete_jti_audit_count(&root), 1);
    }

    #[cfg(unix)]
    #[test]
    fn independent_delete_jti_stores_cannot_regress_clock_high_watermark() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("delete-jti");
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        drop(DeleteJtiStore::new_with_limit_at(&root, 8, now).unwrap());
        let hook = Arc::new(DeleteJtiClockPersistHook {
            reached: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
        });
        let mut lower = DeleteJtiStore::new_with_limit_at(&root, 8, now).unwrap();
        lower.test_clock_persist_hook = Some(hook.clone());
        let higher = DeleteJtiStore::new_with_limit_at(&root, 8, now).unwrap();

        let lower = thread::spawn(move || {
            lower.consume(
                Uuid::now_v7(),
                node_id,
                "uploads/lower-clock.mp4",
                now + Duration::seconds(60),
                now + Duration::seconds(1),
            )
        });
        hook.reached.wait();
        let higher = thread::spawn(move || {
            higher.consume(
                Uuid::now_v7(),
                node_id,
                "uploads/higher-clock.mp4",
                now + Duration::seconds(60),
                now + Duration::seconds(4),
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(150));
        hook.release.wait();

        lower.join().unwrap().unwrap();
        higher.join().unwrap().unwrap();
        let watermark: DeleteJtiClockHighWatermark = serde_json::from_slice(
            &read_regular_audit_file(&root.join(DELETE_JTI_CLOCK_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(watermark.last_seen_at, now + Duration::seconds(4));
    }

    #[cfg(unix)]
    #[test]
    fn delete_jti_capacity_and_high_watermark_are_fenced_across_processes() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("delete-jti");
        let sync = temp.path().join("sync");
        std::fs::create_dir(&sync).unwrap();
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        drop(DeleteJtiStore::new_with_limit_at(&root, 4, now).unwrap());
        let go = sync.join("go");
        let mut children = Vec::new();
        let mut ready_paths = Vec::new();
        let mut result_paths = Vec::new();

        for index in 0..12 {
            let ready = sync.join(format!("ready-{index}"));
            let result = sync.join(format!("result-{index}"));
            let mut command = subprocess_command(DeleteJtiSubprocessSpec {
                root: &root,
                init_at: now,
                consume_at: now + Duration::seconds(index % 5),
                expires_at: now + Duration::seconds(120),
                max_entries: 4,
                jti: Uuid::now_v7(),
                node_id,
                result: &result,
            });
            command
                .env("STREAMSERVER_DELETE_JTI_READY", &ready)
                .env("STREAMSERVER_DELETE_JTI_GO", &go)
                .env("STREAMSERVER_DELETE_JTI_CAPACITY_PAUSE_MS", "750");
            children.push(command.spawn().unwrap());
            ready_paths.push(ready);
            result_paths.push(result);
        }
        for ready in &ready_paths {
            wait_for_test_file(ready);
        }
        std::fs::write(&go, b"go\n").unwrap();
        for child in &mut children {
            wait_for_success(child);
        }
        let outcomes = result_paths
            .iter()
            .map(|path| std::fs::read_to_string(path).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes
                .iter()
                .filter(|value| value.as_str() == "ok")
                .count(),
            4
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|value| value.as_str() == "capacity")
                .count(),
            8
        );
        assert_eq!(delete_jti_audit_count(&root), 4);
        let watermark: DeleteJtiClockHighWatermark = serde_json::from_slice(
            &read_regular_audit_file(&root.join(DELETE_JTI_CLOCK_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(watermark.last_seen_at, now + Duration::seconds(4));

        let lower_result = sync.join("lower-result");
        let mut lower = subprocess_command(DeleteJtiSubprocessSpec {
            root: &root,
            init_at: now + Duration::seconds(1),
            consume_at: now + Duration::seconds(1),
            expires_at: now + Duration::seconds(120),
            max_entries: 4,
            jti: Uuid::now_v7(),
            node_id,
            result: &lower_result,
        })
        .spawn()
        .unwrap();
        wait_for_success(&mut lower);
        assert_eq!(std::fs::read_to_string(lower_result).unwrap(), "capacity");
        let watermark: DeleteJtiClockHighWatermark = serde_json::from_slice(
            &read_regular_audit_file(&root.join(DELETE_JTI_CLOCK_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(watermark.last_seen_at, now + Duration::seconds(4));
    }

    #[cfg(unix)]
    #[test]
    fn delete_jti_clock_replacement_is_serialized_across_processes() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("delete-jti");
        let sync = temp.path().join("sync");
        std::fs::create_dir(&sync).unwrap();
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        drop(DeleteJtiStore::new_with_limit_at(&root, 8, now).unwrap());
        let low_result = sync.join("low-result");
        let low_ready = sync.join("low-clock-ready");
        let low_release = sync.join("low-clock-release");
        let mut low_command = subprocess_command(DeleteJtiSubprocessSpec {
            root: &root,
            init_at: now,
            consume_at: now + Duration::seconds(1),
            expires_at: now + Duration::seconds(60),
            max_entries: 8,
            jti: Uuid::now_v7(),
            node_id,
            result: &low_result,
        });
        low_command
            .env("STREAMSERVER_DELETE_JTI_CLOCK_READY", &low_ready)
            .env("STREAMSERVER_DELETE_JTI_CLOCK_RELEASE", &low_release);
        let mut low = low_command.spawn().unwrap();
        wait_for_test_file(&low_ready);

        let high_result = sync.join("high-result");
        let high_started = sync.join("high-started");
        let mut high_command = subprocess_command(DeleteJtiSubprocessSpec {
            root: &root,
            init_at: now,
            consume_at: now + Duration::seconds(4),
            expires_at: now + Duration::seconds(60),
            max_entries: 8,
            jti: Uuid::now_v7(),
            node_id,
            result: &high_result,
        });
        high_command.env("STREAMSERVER_DELETE_JTI_STARTED", &high_started);
        let mut high = high_command.spawn().unwrap();
        wait_for_test_file(&high_started);
        std::thread::sleep(std::time::Duration::from_millis(750));
        std::fs::write(&low_release, b"release\n").unwrap();

        wait_for_success(&mut low);
        wait_for_success(&mut high);
        assert_eq!(std::fs::read_to_string(low_result).unwrap(), "ok");
        assert_eq!(std::fs::read_to_string(high_result).unwrap(), "ok");
        let watermark: DeleteJtiClockHighWatermark = serde_json::from_slice(
            &read_regular_audit_file(&root.join(DELETE_JTI_CLOCK_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(watermark.last_seen_at, now + Duration::seconds(4));
    }

    #[test]
    fn delete_jti_store_prunes_only_entries_past_expiry_and_clock_skew() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("delete-jti");
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let store = DeleteJtiStore::new_with_limit_at(&root, 2, now).unwrap();
        let expired = Uuid::now_v7();
        let active = Uuid::now_v7();

        store
            .consume(
                expired,
                node_id,
                "uploads/expired.mp4",
                now + Duration::seconds(1),
                now,
            )
            .unwrap();
        store
            .consume(
                active,
                node_id,
                "uploads/active.mp4",
                now + Duration::seconds(60),
                now,
            )
            .unwrap();

        assert!(matches!(
            store.consume(
                Uuid::now_v7(),
                node_id,
                "uploads/full.mp4",
                now + Duration::seconds(60),
                now + Duration::seconds(5),
            ),
            Err(DeleteJtiError::Capacity)
        ));

        let replacement = Uuid::now_v7();
        store
            .consume(
                replacement,
                node_id,
                "uploads/replacement.mp4",
                now + Duration::seconds(60),
                now + Duration::seconds(6),
            )
            .unwrap();
        assert!(!root.join(format!("{expired}.json")).exists());
        assert!(root.join(format!("{active}.json")).exists());
        assert!(root.join(format!("{replacement}.json")).exists());

        let audit: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join(format!("{replacement}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            audit["expires_at"],
            serde_json::json!(now + Duration::seconds(60))
        );
    }

    #[test]
    fn delete_jti_store_prunes_expired_entries_on_restart_and_stays_bounded_concurrently() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("delete-jti");
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let initial = DeleteJtiStore::new_with_limit_at(&root, 32, now).unwrap();
        for index in 0..32 {
            initial
                .consume(
                    Uuid::now_v7(),
                    node_id,
                    &format!("uploads/expired-{index}.mp4"),
                    now + Duration::seconds(1),
                    now,
                )
                .unwrap();
        }
        drop(initial);

        let restarted = Arc::new(
            DeleteJtiStore::new_with_limit_at(&root, 32, now + Duration::seconds(6)).unwrap(),
        );
        assert_eq!(delete_jti_audit_count(&root), 0);

        let handles = (0..32)
            .map(|index| {
                let store = restarted.clone();
                thread::spawn(move || {
                    store.consume(
                        Uuid::now_v7(),
                        node_id,
                        &format!("uploads/active-{index}.mp4"),
                        now + Duration::seconds(60),
                        now + Duration::seconds(6),
                    )
                })
            })
            .collect::<Vec<_>>();
        assert!(
            handles
                .into_iter()
                .all(|handle| handle.join().unwrap().is_ok())
        );
        assert_eq!(delete_jti_audit_count(&root), 32);
        assert!(matches!(
            restarted.consume(
                Uuid::now_v7(),
                node_id,
                "uploads/overflow.mp4",
                now + Duration::seconds(60),
                now + Duration::seconds(6),
            ),
            Err(DeleteJtiError::Capacity)
        ));
    }

    #[test]
    fn delete_jti_store_fails_closed_after_clock_rollback_even_after_pruning() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("delete-jti");
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let store = DeleteJtiStore::new_with_limit_at(&root, 4, now).unwrap();
        let consumed = Uuid::now_v7();
        let expires_at = now + Duration::seconds(1);
        store
            .consume(consumed, node_id, "uploads/consumed.mp4", expires_at, now)
            .unwrap();
        store
            .consume(
                Uuid::now_v7(),
                node_id,
                "uploads/advance-clock.mp4",
                now + Duration::seconds(60),
                now + Duration::seconds(6),
            )
            .unwrap();
        assert!(!root.join(format!("{consumed}.json")).exists());

        assert!(matches!(
            store.consume(consumed, node_id, "uploads/consumed.mp4", expires_at, now,),
            Err(DeleteJtiError::ClockRollback)
        ));
        assert!(matches!(
            DeleteJtiStore::new_with_limit_at(&root, 4, now),
            Err(DeleteJtiError::ClockRollback)
        ));

        // A rollback inside the accepted skew still uses the durable high
        // watermark for expiry decisions, so the removed JTI cannot reopen.
        assert!(
            store
                .consume(
                    consumed,
                    node_id,
                    "uploads/consumed.mp4",
                    expires_at,
                    now + Duration::seconds(2),
                )
                .is_err()
        );
    }

    #[test]
    fn valid_tombstone_is_retained_until_exact_expiry_plus_clock_skew() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("delete-jti");
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let store = DeleteJtiStore::new_with_limit_at(&root, 1, now).unwrap();
        store
            .consume(
                Uuid::now_v7(),
                node_id,
                "uploads/consumed.mp4",
                now + Duration::seconds(1),
                now,
            )
            .unwrap();

        let replacement = Uuid::now_v7();
        assert!(matches!(
            store.consume(
                replacement,
                node_id,
                "uploads/before-boundary.mp4",
                now + Duration::seconds(60),
                now + Duration::seconds(6) - Duration::nanoseconds(1),
            ),
            Err(DeleteJtiError::Capacity)
        ));
        store
            .consume(
                replacement,
                node_id,
                "uploads/at-boundary.mp4",
                now + Duration::seconds(60),
                now + Duration::seconds(6),
            )
            .unwrap();
    }

    #[test]
    fn malformed_crash_tombstone_covers_latest_token_through_expiry_skew() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("delete-jti");
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let store = DeleteJtiStore::new_with_limit_at(&root, 1, now).unwrap();
        let malformed = Uuid::now_v7();
        let malformed_path = root.join(format!("{malformed}.json"));
        std::fs::write(&malformed_path, b"{\"truncated\":").unwrap();
        let quarantined_at = file_modified_at(&malformed_path);

        assert!(matches!(
            store.consume(
                Uuid::now_v7(),
                node_id,
                "uploads/at-129-seconds.mp4",
                quarantined_at + Duration::seconds(180),
                quarantined_at + Duration::seconds(129),
            ),
            Err(DeleteJtiError::Capacity)
        ));
        store
            .consume(
                Uuid::now_v7(),
                node_id,
                "uploads/at-130-seconds.mp4",
                quarantined_at + Duration::seconds(180),
                quarantined_at + Duration::seconds(130),
            )
            .unwrap();
        assert!(!root.join(format!("{malformed}.json")).exists());
    }

    #[test]
    fn legacy_tombstone_without_expiry_covers_latest_token_through_expiry_skew() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("delete-jti");
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let store = DeleteJtiStore::new_with_limit_at(&root, 1, now).unwrap();
        let legacy = Uuid::now_v7();
        let mut legacy_contents = serde_json::to_vec(&serde_json::json!({
            "jti": legacy,
            "node_id": node_id,
            "op": "delete",
            "path_sha256": "a".repeat(64),
            "consumed_at": now,
            "outcome": "started",
        }))
        .unwrap();
        legacy_contents.push(b'\n');
        std::fs::write(root.join(format!("{legacy}.json")), legacy_contents).unwrap();

        assert!(matches!(
            store.consume(
                Uuid::now_v7(),
                node_id,
                "uploads/legacy-at-129-seconds.mp4",
                now + Duration::seconds(180),
                now + Duration::seconds(129),
            ),
            Err(DeleteJtiError::Capacity)
        ));
        store
            .consume(
                Uuid::now_v7(),
                node_id,
                "uploads/legacy-at-130-seconds.mp4",
                now + Duration::seconds(180),
                now + Duration::seconds(130),
            )
            .unwrap();
        assert!(!root.join(format!("{legacy}.json")).exists());
    }

    #[test]
    fn unreasonable_future_expiry_is_quarantined_for_a_bounded_interval() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("delete-jti");
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let store = DeleteJtiStore::new_with_limit_at(&root, 1, now).unwrap();
        let unreasonable = Uuid::now_v7();
        write_delete_jti_audit(
            &root,
            &DeleteJtiAudit {
                jti: unreasonable,
                node_id,
                op: "delete".to_string(),
                path_sha256: "b".repeat(64),
                consumed_at: now + Duration::days(364),
                expires_at: Some(now + Duration::days(365)),
                outcome: "started".to_string(),
            },
        );
        let quarantined_at = file_modified_at(&root.join(format!("{unreasonable}.json")));

        assert!(matches!(
            store.consume(
                Uuid::now_v7(),
                node_id,
                "uploads/future-at-129-seconds.mp4",
                quarantined_at + Duration::seconds(180),
                quarantined_at + Duration::seconds(129),
            ),
            Err(DeleteJtiError::Capacity)
        ));
        store
            .consume(
                Uuid::now_v7(),
                node_id,
                "uploads/future-at-130-seconds.mp4",
                quarantined_at + Duration::seconds(180),
                quarantined_at + Duration::seconds(130),
            )
            .unwrap();
        assert!(!root.join(format!("{unreasonable}.json")).exists());
    }

    #[cfg(unix)]
    #[test]
    fn canonical_non_regular_entries_fail_closed_on_startup() {
        use std::os::unix::fs::symlink;

        let now = Utc::now();
        for kind in ["directory", "symlink"] {
            let temp = tempdir().unwrap();
            let root = temp.path().join("delete-jti");
            drop(DeleteJtiStore::new_with_limit_at(&root, 4, now).unwrap());
            let path = root.join(format!("{}.json", Uuid::now_v7()));
            if kind == "directory" {
                std::fs::create_dir(&path).unwrap();
            } else {
                symlink(root.join("missing-target"), &path).unwrap();
            }

            assert!(
                DeleteJtiStore::new_with_limit_at(&root, 4, now).is_err(),
                "canonical {kind} entry must fail closed"
            );
        }
    }
}
