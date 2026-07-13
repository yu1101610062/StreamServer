//! Durable Agent identity, enrollment, certificate, rotation, and session state.

use std::{fmt, net::IpAddr, str::FromStr};

use chrono::{DateTime, Utc};
use media_domain::{AgentRegistration, EventSource, TaskStatus};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use super::{RepoError, TaskRepository};

const MAX_ENROLLMENT_TTL: chrono::Duration = chrono::Duration::minutes(10);
const AGENT_CONTROL_SESSION_LEASE: chrono::Duration = chrono::Duration::seconds(30);
const AGENT_CERTIFICATE_ROTATION_WINDOW: chrono::Duration = chrono::Duration::days(30);
const AGENT_CERTIFICATE_ROTATION_DEADLINE: chrono::Duration = chrono::Duration::minutes(5);
const AGENT_PREVIOUS_IDENTITY_RETIREMENT_WINDOW: chrono::Duration = chrono::Duration::minutes(5);
const DISPATCH_RECLAIM_GRACE: chrono::Duration = chrono::Duration::seconds(10);
const RUNTIME_RECLAIM_GRACE: chrono::Duration = chrono::Duration::seconds(60);

#[derive(Clone)]
pub struct NewAgentEnrollment {
    pub id: Uuid,
    pub node_id: Uuid,
    pub token_hash: [u8; 32],
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub remote_ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

impl fmt::Debug for NewAgentEnrollment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewAgentEnrollment")
            .field("id", &self.id)
            .field("node_id", &self.node_id)
            .field("token_hash", &"[REDACTED]")
            .field("created_by", &self.created_by)
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .field("remote_ip", &self.remote_ip)
            .field("user_agent", &self.user_agent)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateAgentEnrollmentOutcome {
    Created { expires_at: DateTime<Utc> },
    IdentityAlreadyActive,
    IdentityRevoked,
}

#[derive(Clone, PartialEq, Eq)]
pub struct NewIssuedAgentCertificate {
    pub id: Uuid,
    pub serial_number: String,
    pub fingerprint_sha256: [u8; 32],
    pub public_key_sha256: [u8; 32],
    pub certificate_pem: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
}

impl fmt::Debug for NewIssuedAgentCertificate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewIssuedAgentCertificate")
            .field("id", &self.id)
            .field("serial_number", &self.serial_number)
            .field("fingerprint_sha256", &hex_lower(&self.fingerprint_sha256))
            .field("public_key_sha256", &hex_lower(&self.public_key_sha256))
            .field("certificate_pem", &"[REDACTED]")
            .field("not_before", &self.not_before)
            .field("not_after", &self.not_after)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct AgentEnrollmentRequest {
    pub node_id: Uuid,
    pub control_csr_public_key_sha256: [u8; 32],
    pub management_csr_public_key_sha256: [u8; 32],
    pub attempted_at: DateTime<Utc>,
    pub remote_ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AgentEnrollmentBundle {
    pub node_id: Uuid,
    pub control_certificate: NewIssuedAgentCertificate,
    pub management_certificate: NewIssuedAgentCertificate,
    pub agent_client_issuer_ca_pem: String,
    pub control_plane_server_ca_pem: String,
    pub management_client_ca_pem: String,
    pub capability_jwt_public_key_pem: String,
    pub capability_jwt_kid: String,
}

impl fmt::Debug for AgentEnrollmentBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentEnrollmentBundle")
            .field("node_id", &self.node_id)
            .field("control_certificate", &self.control_certificate)
            .field("management_certificate", &self.management_certificate)
            .field("agent_client_issuer_ca_pem", &"[REDACTED]")
            .field("control_plane_server_ca_pem", &"[REDACTED]")
            .field("management_client_ca_pem", &"[REDACTED]")
            .field("capability_jwt_public_key_pem", &"[REDACTED]")
            .field("capability_jwt_kid", &self.capability_jwt_kid)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsumeAgentEnrollmentOutcome {
    Issued(AgentEnrollmentBundle),
    Recovered(AgentEnrollmentBundle),
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEnrollmentPreflightOutcome {
    Admissible,
    Invalid,
}

#[derive(Debug, Clone)]
pub struct AgentCertificateRotationRequest {
    pub rotation_id: Uuid,
    pub node_id: Uuid,
    pub session_id: Uuid,
    pub control_csr_public_key_sha256: [u8; 32],
    pub management_csr_public_key_sha256: [u8; 32],
    pub requested_at: DateTime<Utc>,
    pub remote_ip: IpAddr,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AgentCertificateRotationIssue {
    pub control_certificate: NewIssuedAgentCertificate,
    pub management_certificate: NewIssuedAgentCertificate,
}

impl fmt::Debug for AgentCertificateRotationIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentCertificateRotationIssue")
            .field("control_certificate", &self.control_certificate)
            .field("management_certificate", &self.management_certificate)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AgentCertificateRotationBundle {
    pub rotation_id: Uuid,
    pub node_id: Uuid,
    pub control_certificate: NewIssuedAgentCertificate,
    pub management_certificate: NewIssuedAgentCertificate,
    pub authorized_until: DateTime<Utc>,
}

impl fmt::Debug for AgentCertificateRotationBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentCertificateRotationBundle")
            .field("rotation_id", &self.rotation_id)
            .field("node_id", &self.node_id)
            .field("control_certificate", &self.control_certificate)
            .field("management_certificate", &self.management_certificate)
            .field("authorized_until", &self.authorized_until)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageAgentCertificateRotationOutcome {
    Issued(AgentCertificateRotationBundle),
    Recovered(AgentCertificateRotationBundle),
    Expired,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCertificateAuthorizationFailure {
    Unknown,
    NodeMismatch,
    IdentityRevoked,
    NotYetValid,
    Expired,
    Revoked,
    Replaced,
    RotationNotAuthorized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSessionTakeoverReason {
    StaleTimeout,
    CleanDisconnect,
    CertificateRotation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSessionWriteOutcome {
    Applied,
    IgnoredStaleAttempt,
    FencedSession,
}

impl AgentSessionTakeoverReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::StaleTimeout => "stale_timeout",
            Self::CleanDisconnect => "clean_disconnect",
            Self::CertificateRotation => "certificate_rotation",
        }
    }
}

#[derive(Clone)]
pub struct AgentControlSessionClaim {
    pub registration: AgentRegistration,
    pub session_id: Uuid,
    pub core_instance_id: Uuid,
    pub certificate_fingerprint_sha256: [u8; 32],
    pub peer_ip: IpAddr,
    pub connected_at: DateTime<Utc>,
    pub lease_expires_at: DateTime<Utc>,
}

impl fmt::Debug for AgentControlSessionClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentControlSessionClaim")
            .field("node_id", &self.registration.node_id)
            .field("session_id", &self.session_id)
            .field("core_instance_id", &self.core_instance_id)
            .field(
                "certificate_fingerprint_sha256",
                &hex_lower(&self.certificate_fingerprint_sha256),
            )
            .field("peer_ip", &self.peer_ip)
            .field("connected_at", &self.connected_at)
            .field("lease_expires_at", &self.lease_expires_at)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentControlSessionClaimOutcome {
    Claimed {
        certificate_id: Uuid,
        replaced_session_id: Option<Uuid>,
        takeover_reason: Option<AgentSessionTakeoverReason>,
        rotation_context: Option<AgentCertificateRotationTakeoverContext>,
    },
    DuplicateHealthy {
        existing_session_id: Uuid,
    },
    UnauthorizedCertificate(AgentCertificateAuthorizationFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCertificateRotationTakeoverContext {
    pub rotation_id: Uuid,
    pub authorized_until: DateTime<Utc>,
    pub old_control_fingerprint_sha256: [u8; 32],
    pub new_control_fingerprint_sha256: [u8; 32],
    pub old_management_fingerprint_sha256: [u8; 32],
    pub new_management_fingerprint_sha256: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentManagementRotationActivationRequest {
    pub rotation_id: Uuid,
    pub node_id: Uuid,
    pub session_id: Uuid,
    pub control_fingerprint_sha256: [u8; 32],
    pub management_fingerprint_sha256: [u8; 32],
    pub activated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentManagementRotationActivationContext {
    pub rotation_id: Uuid,
    pub previous_identity_expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentManagementRotationActivationOutcome {
    Activated(AgentManagementRotationActivationContext),
    Recovered(AgentManagementRotationActivationContext),
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCertificateRotationAcknowledgement {
    pub rotation_id: Uuid,
    pub node_id: Uuid,
    pub session_id: Uuid,
    pub control_fingerprint_sha256: [u8; 32],
    pub management_fingerprint_sha256: [u8; 32],
    pub acknowledged_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteAgentCertificateRotationOutcome {
    Completed,
    Recovered,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentManagementCertificateFingerprints {
    pub current_fingerprint_sha256: [u8; 32],
    pub rotating_fingerprint_sha256: Option<[u8; 32]>,
}

fn validate_agent_control_session_claim(claim: &AgentControlSessionClaim) -> Result<(), RepoError> {
    if claim.registration.node_id.is_nil()
        || claim.session_id.is_nil()
        || claim.core_instance_id.is_nil()
    {
        return Err(RepoError::AgentIdentityInvariant(
            "Agent control-session identifiers must be non-nil UUIDs".to_string(),
        ));
    }
    if claim.lease_expires_at - claim.connected_at != AGENT_CONTROL_SESSION_LEASE {
        return Err(RepoError::AgentIdentityInvariant(
            "Agent control-session lease must be exactly thirty seconds".to_string(),
        ));
    }
    Ok(())
}

fn validate_new_agent_enrollment(record: &NewAgentEnrollment) -> Result<(), RepoError> {
    let ttl = record.expires_at - record.created_at;
    if record.id.is_nil() || record.node_id.is_nil() {
        return Err(RepoError::AgentIdentityInvariant(
            "enrollment and node identifiers must be non-nil".to_string(),
        ));
    }
    if record.created_by.trim().is_empty() {
        return Err(RepoError::AgentIdentityInvariant(
            "enrollment actor must not be empty".to_string(),
        ));
    }
    if ttl <= chrono::Duration::zero() || ttl > MAX_ENROLLMENT_TTL {
        return Err(RepoError::AgentIdentityInvariant(
            "enrollment TTL must be positive and no greater than ten minutes".to_string(),
        ));
    }
    Ok(())
}

impl TaskRepository {
    pub async fn record_agent_peer_rejection(
        &self,
        certificate_node_id: Option<Uuid>,
        claimed_node_id: Option<Uuid>,
        certificate_fingerprint_sha256: [u8; 32],
        peer_ip: IpAddr,
        reason: &'static str,
        _attempted_at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            insert into security_audit_events (
              id, event_type, actor, subject, remote_ip, user_agent, payload, created_at
            ) values ($1, $2, $3, $4, $5::inet, null, $6, clock_timestamp())
            "#,
        )
        .bind(Uuid::now_v7())
        .bind("agent_peer_identity_rejected")
        .bind("agent-control")
        .bind(
            certificate_node_id
                .or(claimed_node_id)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown-agent".to_string()),
        )
        .bind(peer_ip.to_string())
        .bind(json!({
            "certificate_node_id": certificate_node_id,
            "claimed_node_id": claimed_node_id,
            "fingerprint_sha256": hex_lower(&certificate_fingerprint_sha256),
            "peer_ip": peer_ip,
            "reason": reason,
        }))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn create_agent_enrollment(
        &self,
        record: NewAgentEnrollment,
    ) -> Result<CreateAgentEnrollmentOutcome, RepoError> {
        validate_new_agent_enrollment(&record)?;
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            r#"
            insert into agent_identities (node_id, status, created_at, updated_at)
            values ($1, 'pending_enrollment', clock_timestamp(), clock_timestamp())
            on conflict (node_id) do nothing
            "#,
        )
        .bind(record.node_id)
        .execute(&mut *tx)
        .await?;

        let status: String =
            sqlx::query_scalar("select status from agent_identities where node_id = $1 for update")
                .bind(record.node_id)
                .fetch_one(&mut *tx)
                .await?;
        let refusal = match status.as_str() {
            "pending_enrollment" => None,
            "active" => Some(CreateAgentEnrollmentOutcome::IdentityAlreadyActive),
            "revoked" => Some(CreateAgentEnrollmentOutcome::IdentityRevoked),
            other => {
                return Err(RepoError::AgentIdentityInvariant(format!(
                    "unsupported identity status {other}"
                )));
            }
        };
        if let Some(outcome) = refusal {
            let rejected_at = transaction_clock_timestamp(&mut tx).await?;
            insert_agent_identity_audit(
                &mut tx,
                "agent_enrollment_rejected",
                &record.created_by,
                record.node_id,
                record.remote_ip,
                record.user_agent.as_deref(),
                json!({ "reason": "identity_not_pending" }),
                rejected_at,
            )
            .await?;
            tx.commit().await?;
            return Ok(outcome);
        }

        let created_at = transaction_clock_timestamp(&mut tx).await?;
        let expires_at = created_at + (record.expires_at - record.created_at);

        sqlx::query(
            r#"
            update agent_enrollment_tokens
               set revoked_at = $1
             where node_id = $2
               and consumed_at is null
               and revoked_at is null
            "#,
        )
        .bind(created_at)
        .bind(record.node_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            insert into agent_enrollment_tokens (
              id, node_id, token_hash, created_by, created_at, expires_at,
              consumed_at, revoked_at, consumed_certificate_id
            ) values ($1, $2, $3, $4, $5, $6, null, null, null)
            "#,
        )
        .bind(record.id)
        .bind(record.node_id)
        .bind(record.token_hash.as_slice())
        .bind(&record.created_by)
        .bind(created_at)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;

        insert_agent_identity_audit(
            &mut tx,
            "agent_enrollment_created",
            &record.created_by,
            record.node_id,
            record.remote_ip,
            record.user_agent.as_deref(),
            json!({
                "enrollment_id": record.id,
                "expires_at": expires_at,
            }),
            created_at,
        )
        .await?;
        tx.commit().await?;
        Ok(CreateAgentEnrollmentOutcome::Created { expires_at })
    }

    pub async fn preflight_agent_enrollment(
        &self,
        enrollment_id: Uuid,
        token_hash: &[u8; 32],
        node_id: Uuid,
    ) -> Result<AgentEnrollmentPreflightOutcome, RepoError> {
        if enrollment_id.is_nil() || node_id.is_nil() {
            return Ok(AgentEnrollmentPreflightOutcome::Invalid);
        }
        let admissible: bool = sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from agent_enrollment_tokens
               where id = $1
                 and token_hash = $2
                 and node_id = $3
                 and revoked_at is null
                 and expires_at > clock_timestamp()
            )
            "#,
        )
        .bind(enrollment_id)
        .bind(token_hash.as_slice())
        .bind(node_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(if admissible {
            AgentEnrollmentPreflightOutcome::Admissible
        } else {
            AgentEnrollmentPreflightOutcome::Invalid
        })
    }

    pub async fn consume_agent_enrollment<F, E>(
        &self,
        token_hash: &[u8; 32],
        mut request: AgentEnrollmentRequest,
        issue_bundle: F,
    ) -> Result<ConsumeAgentEnrollmentOutcome, E>
    where
        F: FnOnce(DateTime<Utc>) -> Result<AgentEnrollmentBundle, E> + Send,
        E: From<RepoError>,
    {
        validate_agent_enrollment_request(&request).map_err(E::from)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(RepoError::from)
            .map_err(E::from)?;
        // Enrollment creation locks identity -> token. Consumers must take the
        // same locks in the same order so an administrator replacing a token
        // cannot deadlock with a consumer holding the old token row.
        let identity_status = sqlx::query_scalar::<_, String>(
            "select status from agent_identities where node_id = $1 for update",
        )
        .bind(request.node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(RepoError::from)
        .map_err(E::from)?;
        let token_row = sqlx::query(
            r#"
            select id, node_id, expires_at, consumed_at, revoked_at,
                   consumed_certificate_id, consumed_management_certificate_id,
                   control_csr_public_key_sha256, management_csr_public_key_sha256,
                   agent_client_issuer_ca_pem, control_plane_server_ca_pem,
                   management_client_ca_pem,
                   capability_jwt_public_key_pem, capability_jwt_kid
              from agent_enrollment_tokens
             where token_hash = $1
             for update
            "#,
        )
        .bind(token_hash.as_slice())
        .fetch_optional(&mut *tx)
        .await
        .map_err(RepoError::from)
        .map_err(E::from)?;

        let token_row = match token_row {
            None => None,
            Some(row) => {
                let token_node_id: Uuid = row
                    .try_get("node_id")
                    .map_err(RepoError::from)
                    .map_err(E::from)?;
                if token_node_id != request.node_id {
                    None
                } else {
                    Some(row)
                }
            }
        };
        let Some(token_row) = token_row else {
            tx.rollback()
                .await
                .map_err(RepoError::from)
                .map_err(E::from)?;
            return Ok(ConsumeAgentEnrollmentOutcome::Invalid);
        };
        request.attempted_at = transaction_clock_timestamp(&mut tx)
            .await
            .map_err(E::from)?;

        let enrollment_id: Uuid = token_row
            .try_get("id")
            .map_err(RepoError::from)
            .map_err(E::from)?;
        let expires_at: DateTime<Utc> = token_row
            .try_get("expires_at")
            .map_err(RepoError::from)
            .map_err(E::from)?;
        let consumed_at: Option<DateTime<Utc>> = token_row
            .try_get("consumed_at")
            .map_err(RepoError::from)
            .map_err(E::from)?;
        let revoked_at: Option<DateTime<Utc>> = token_row
            .try_get("revoked_at")
            .map_err(RepoError::from)
            .map_err(E::from)?;

        if consumed_at.is_some()
            && revoked_at.is_none()
            && expires_at > request.attempted_at
            && identity_status.as_deref() == Some("active")
        {
            let control_hash: Option<Vec<u8>> = token_row
                .try_get("control_csr_public_key_sha256")
                .map_err(RepoError::from)
                .map_err(E::from)?;
            let management_hash: Option<Vec<u8>> = token_row
                .try_get("management_csr_public_key_sha256")
                .map_err(RepoError::from)
                .map_err(E::from)?;
            let same_request = control_hash.as_deref()
                == Some(request.control_csr_public_key_sha256.as_slice())
                && management_hash.as_deref()
                    == Some(request.management_csr_public_key_sha256.as_slice());
            if same_request {
                let bundle = recover_agent_enrollment_bundle(&mut tx, &token_row, request.node_id)
                    .await
                    .map_err(E::from)?;
                tx.commit()
                    .await
                    .map_err(RepoError::from)
                    .map_err(E::from)?;
                return Ok(ConsumeAgentEnrollmentOutcome::Recovered(bundle));
            }
        }

        if consumed_at.is_some() || revoked_at.is_some() || expires_at <= request.attempted_at {
            tx.rollback()
                .await
                .map_err(RepoError::from)
                .map_err(E::from)?;
            return Ok(ConsumeAgentEnrollmentOutcome::Invalid);
        }

        if identity_status.as_deref() != Some("pending_enrollment") {
            tx.rollback()
                .await
                .map_err(RepoError::from)
                .map_err(E::from)?;
            return Ok(ConsumeAgentEnrollmentOutcome::Invalid);
        }

        // The issuer is invoked only after the identity and token rows are
        // locked and found eligible. Concurrent identical callers recover the
        // committed bundle without producing additional certificates.
        let mut bundle = issue_bundle(request.attempted_at)?;
        normalize_enrollment_bundle_timestamps(&mut bundle)?;
        validate_agent_enrollment_bundle(&request, &bundle).map_err(E::from)?;

        sqlx::query(
            r#"
            insert into agent_certificates (
              id, node_id, serial_number, fingerprint_sha256, public_key_sha256,
              certificate_pem, state, not_before, not_after, issued_at, activated_at,
              revoked_at, revocation_reason, issued_via
            ) values (
              $1, $2, $3, $4, $5, $6, 'active', $7, $8, $9, $9,
              null, null, 'enrollment'
            )
            "#,
        )
        .bind(bundle.control_certificate.id)
        .bind(request.node_id)
        .bind(&bundle.control_certificate.serial_number)
        .bind(bundle.control_certificate.fingerprint_sha256.as_slice())
        .bind(bundle.control_certificate.public_key_sha256.as_slice())
        .bind(&bundle.control_certificate.certificate_pem)
        .bind(bundle.control_certificate.not_before)
        .bind(bundle.control_certificate.not_after)
        .bind(request.attempted_at)
        .execute(&mut *tx)
        .await
        .map_err(RepoError::from)
        .map_err(E::from)?;

        sqlx::query(
            r#"
            insert into agent_management_certificates (
              id, node_id, serial_number, fingerprint_sha256, public_key_sha256,
              certificate_pem, state, not_before, not_after, issued_at, activated_at,
              revoked_at, revocation_reason, issued_via
            ) values (
              $1, $2, $3, $4, $5, $6, 'active', $7, $8, $9, $9,
              null, null, 'enrollment'
            )
            "#,
        )
        .bind(bundle.management_certificate.id)
        .bind(request.node_id)
        .bind(&bundle.management_certificate.serial_number)
        .bind(bundle.management_certificate.fingerprint_sha256.as_slice())
        .bind(bundle.management_certificate.public_key_sha256.as_slice())
        .bind(&bundle.management_certificate.certificate_pem)
        .bind(bundle.management_certificate.not_before)
        .bind(bundle.management_certificate.not_after)
        .bind(request.attempted_at)
        .execute(&mut *tx)
        .await
        .map_err(RepoError::from)
        .map_err(E::from)?;

        let consumed = sqlx::query(
            r#"
            update agent_enrollment_tokens
               set consumed_at = $1,
                   consumed_certificate_id = $2,
                   consumed_management_certificate_id = $3,
                   control_csr_public_key_sha256 = $4,
                   management_csr_public_key_sha256 = $5,
                   agent_client_issuer_ca_pem = $6,
                   control_plane_server_ca_pem = $7,
                   management_client_ca_pem = $8,
                   capability_jwt_public_key_pem = $9,
                   capability_jwt_kid = $10
             where id = $11
               and consumed_at is null
               and revoked_at is null
               and expires_at > $1
            "#,
        )
        .bind(request.attempted_at)
        .bind(bundle.control_certificate.id)
        .bind(bundle.management_certificate.id)
        .bind(request.control_csr_public_key_sha256.as_slice())
        .bind(request.management_csr_public_key_sha256.as_slice())
        .bind(&bundle.agent_client_issuer_ca_pem)
        .bind(&bundle.control_plane_server_ca_pem)
        .bind(&bundle.management_client_ca_pem)
        .bind(&bundle.capability_jwt_public_key_pem)
        .bind(&bundle.capability_jwt_kid)
        .bind(enrollment_id)
        .execute(&mut *tx)
        .await
        .map_err(RepoError::from)
        .map_err(E::from)?;
        if consumed.rows_affected() != 1 {
            return Err(E::from(RepoError::AgentIdentityInvariant(
                "enrollment changed while locked".to_string(),
            )));
        }

        let activated = sqlx::query(
            r#"
            update agent_identities
               set status = 'active', updated_at = $1
             where node_id = $2
               and status = 'pending_enrollment'
            "#,
        )
        .bind(request.attempted_at)
        .bind(request.node_id)
        .execute(&mut *tx)
        .await
        .map_err(RepoError::from)
        .map_err(E::from)?;
        if activated.rows_affected() != 1 {
            return Err(E::from(RepoError::AgentIdentityInvariant(
                "identity changed while locked".to_string(),
            )));
        }

        insert_agent_identity_audit(
            &mut tx,
            "agent_certificate_issued",
            "agent-enrollment",
            request.node_id,
            request.remote_ip,
            request.user_agent.as_deref(),
            json!({
                "enrollment_id": enrollment_id,
                "control_certificate_id": bundle.control_certificate.id,
                "control_serial_number": bundle.control_certificate.serial_number,
                "control_fingerprint_sha256": hex_lower(&bundle.control_certificate.fingerprint_sha256),
                "management_certificate_id": bundle.management_certificate.id,
                "management_serial_number": bundle.management_certificate.serial_number,
                "management_fingerprint_sha256": hex_lower(&bundle.management_certificate.fingerprint_sha256),
                "not_before": bundle.control_certificate.not_before,
                "not_after": bundle.control_certificate.not_after,
                "issued_via": "enrollment",
            }),
            request.attempted_at,
        )
        .await
        .map_err(E::from)?;
        tx.commit()
            .await
            .map_err(RepoError::from)
            .map_err(E::from)?;
        Ok(ConsumeAgentEnrollmentOutcome::Issued(bundle))
    }

    pub async fn stage_agent_certificate_rotation<F, E>(
        &self,
        mut request: AgentCertificateRotationRequest,
        issue_certificates: F,
    ) -> Result<StageAgentCertificateRotationOutcome, E>
    where
        F: FnOnce(DateTime<Utc>) -> Result<AgentCertificateRotationIssue, E> + Send,
        E: From<RepoError>,
    {
        validate_agent_certificate_rotation_request(&request).map_err(E::from)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(RepoError::from)
            .map_err(E::from)?;

        let identity_status = sqlx::query_scalar::<_, String>(
            "select status from agent_identities where node_id = $1 for update",
        )
        .bind(request.node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(RepoError::from)
        .map_err(E::from)?;
        request.requested_at = transaction_clock_timestamp(&mut tx)
            .await
            .map_err(E::from)?;
        if identity_status.as_deref() != Some("active") {
            reject_agent_certificate_rotation(&mut tx, &request, "identity_not_active")
                .await
                .map_err(E::from)?;
            tx.commit()
                .await
                .map_err(RepoError::from)
                .map_err(E::from)?;
            return Ok(StageAgentCertificateRotationOutcome::Rejected);
        }

        let current_control = sqlx::query(
            r#"
            select s.certificate_id, c.not_after
              from agent_control_sessions s
              join agent_certificates c
                on c.id = s.certificate_id and c.node_id = s.node_id
             where s.node_id = $1
               and s.session_id = $2
               and s.disconnected_at is null
               and s.lease_expires_at > $3
               and c.state = 'active'
               and c.not_before <= $3
               and c.not_after > $3
             for update of s, c
            "#,
        )
        .bind(request.node_id)
        .bind(request.session_id)
        .bind(request.requested_at)
        .fetch_optional(&mut *tx)
        .await
        .map_err(RepoError::from)
        .map_err(E::from)?;
        let Some(current_control) = current_control else {
            reject_agent_certificate_rotation(&mut tx, &request, "session_fenced")
                .await
                .map_err(E::from)?;
            tx.commit()
                .await
                .map_err(RepoError::from)
                .map_err(E::from)?;
            return Ok(StageAgentCertificateRotationOutcome::Rejected);
        };
        let old_control_id: Uuid = current_control
            .try_get("certificate_id")
            .map_err(RepoError::from)
            .map_err(E::from)?;
        let control_not_after: DateTime<Utc> = current_control
            .try_get("not_after")
            .map_err(RepoError::from)
            .map_err(E::from)?;

        let current_management = sqlx::query(
            r#"
            select id, not_after
              from agent_management_certificates
             where node_id = $1 and state = 'active'
             for update
            "#,
        )
        .bind(request.node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(RepoError::from)
        .map_err(E::from)?;
        let Some(current_management) = current_management else {
            reject_agent_certificate_rotation(&mut tx, &request, "management_identity_missing")
                .await
                .map_err(E::from)?;
            tx.commit()
                .await
                .map_err(RepoError::from)
                .map_err(E::from)?;
            return Ok(StageAgentCertificateRotationOutcome::Rejected);
        };
        let old_management_id: Uuid = current_management
            .try_get("id")
            .map_err(RepoError::from)
            .map_err(E::from)?;
        let management_not_after: DateTime<Utc> = current_management
            .try_get("not_after")
            .map_err(RepoError::from)
            .map_err(E::from)?;

        if let Some(existing) = sqlx::query(
            r#"
            select id, node_id, state, authorized_until,
                   control_csr_public_key_sha256, management_csr_public_key_sha256,
                   new_certificate_id, new_management_certificate_id
              from agent_certificate_rotations
             where id = $1
             for update
            "#,
        )
        .bind(request.rotation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(RepoError::from)
        .map_err(E::from)?
        {
            let same_request = existing
                .try_get::<Uuid, _>("node_id")
                .map_err(RepoError::from)
                .map_err(E::from)?
                == request.node_id
                && existing
                    .try_get::<Vec<u8>, _>("control_csr_public_key_sha256")
                    .map_err(RepoError::from)
                    .map_err(E::from)?
                    == request.control_csr_public_key_sha256
                && existing
                    .try_get::<Vec<u8>, _>("management_csr_public_key_sha256")
                    .map_err(RepoError::from)
                    .map_err(E::from)?
                    == request.management_csr_public_key_sha256;
            let state: String = existing
                .try_get("state")
                .map_err(RepoError::from)
                .map_err(E::from)?;
            let authorized_until: DateTime<Utc> = existing
                .try_get("authorized_until")
                .map_err(RepoError::from)
                .map_err(E::from)?;
            let exact_expired_request = same_request
                && matches!(state.as_str(), "pending" | "expired")
                && authorized_until <= request.requested_at;
            if same_request && state == "pending" && authorized_until > request.requested_at {
                let bundle = recover_agent_certificate_rotation_bundle(
                    &mut tx,
                    request.rotation_id,
                    request.node_id,
                    existing
                        .try_get("new_certificate_id")
                        .map_err(RepoError::from)
                        .map_err(E::from)?,
                    existing
                        .try_get("new_management_certificate_id")
                        .map_err(RepoError::from)
                        .map_err(E::from)?,
                    authorized_until,
                )
                .await
                .map_err(E::from)?;
                tx.commit()
                    .await
                    .map_err(RepoError::from)
                    .map_err(E::from)?;
                return Ok(StageAgentCertificateRotationOutcome::Recovered(bundle));
            }
            if state == "pending" && authorized_until <= request.requested_at {
                expire_pending_agent_certificate_rotation(
                    &mut tx,
                    request.rotation_id,
                    request.requested_at,
                )
                .await
                .map_err(E::from)?;
            }
            let reason = if !same_request {
                "request_mismatch"
            } else if authorized_until <= request.requested_at {
                "expired"
            } else {
                "replay"
            };
            reject_agent_certificate_rotation(&mut tx, &request, reason)
                .await
                .map_err(E::from)?;
            tx.commit()
                .await
                .map_err(RepoError::from)
                .map_err(E::from)?;
            return Ok(if exact_expired_request {
                StageAgentCertificateRotationOutcome::Expired
            } else {
                StageAgentCertificateRotationOutcome::Rejected
            });
        }

        if let Some(existing) = sqlx::query(
            r#"
            select id, state, authorized_until
              from agent_certificate_rotations
             where node_id = $1
               and state in ('pending', 'control_activated', 'management_activated')
             for update
            "#,
        )
        .bind(request.node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(RepoError::from)
        .map_err(E::from)?
        {
            let existing_id: Uuid = existing
                .try_get("id")
                .map_err(RepoError::from)
                .map_err(E::from)?;
            let state: String = existing
                .try_get("state")
                .map_err(RepoError::from)
                .map_err(E::from)?;
            let authorized_until: DateTime<Utc> = existing
                .try_get("authorized_until")
                .map_err(RepoError::from)
                .map_err(E::from)?;
            if state == "pending" && authorized_until <= request.requested_at {
                expire_pending_agent_certificate_rotation(
                    &mut tx,
                    existing_id,
                    request.requested_at,
                )
                .await
                .map_err(E::from)?;
            } else {
                reject_agent_certificate_rotation(&mut tx, &request, "rotation_already_pending")
                    .await
                    .map_err(E::from)?;
                tx.commit()
                    .await
                    .map_err(RepoError::from)
                    .map_err(E::from)?;
                return Ok(StageAgentCertificateRotationOutcome::Rejected);
            }
        }

        let due_at = request.requested_at + AGENT_CERTIFICATE_ROTATION_WINDOW;
        if control_not_after > due_at && management_not_after > due_at {
            reject_agent_certificate_rotation(&mut tx, &request, "not_due")
                .await
                .map_err(E::from)?;
            tx.commit()
                .await
                .map_err(RepoError::from)
                .map_err(E::from)?;
            return Ok(StageAgentCertificateRotationOutcome::Rejected);
        }

        let mut issued = issue_certificates(request.requested_at)?;
        normalize_agent_certificate_rotation_issue_timestamps(&mut issued).map_err(E::from)?;
        validate_agent_certificate_rotation_issue(&request, &issued).map_err(E::from)?;
        let authorized_until = DateTime::from_timestamp_micros(
            (request.requested_at + AGENT_CERTIFICATE_ROTATION_DEADLINE).timestamp_micros(),
        )
        .ok_or_else(|| {
            E::from(RepoError::AgentIdentityInvariant(
                "rotation deadline is outside PostgreSQL timestamp range".to_string(),
            ))
        })?;

        insert_pending_agent_certificate(
            &mut tx,
            request.node_id,
            &issued.control_certificate,
            request.requested_at,
            false,
        )
        .await
        .map_err(E::from)?;
        insert_pending_agent_certificate(
            &mut tx,
            request.node_id,
            &issued.management_certificate,
            request.requested_at,
            true,
        )
        .await
        .map_err(E::from)?;
        sqlx::query(
            r#"
            insert into agent_certificate_rotations (
              id, node_id, old_certificate_id, new_certificate_id,
              old_management_certificate_id, new_management_certificate_id,
              control_csr_public_key_sha256, management_csr_public_key_sha256,
              state, authorized_at, authorized_until
            ) values (
              $1, $2, $3, $4, $5, $6, $7, $8,
              'pending', $9, $10
            )
            "#,
        )
        .bind(request.rotation_id)
        .bind(request.node_id)
        .bind(old_control_id)
        .bind(issued.control_certificate.id)
        .bind(old_management_id)
        .bind(issued.management_certificate.id)
        .bind(request.control_csr_public_key_sha256.as_slice())
        .bind(request.management_csr_public_key_sha256.as_slice())
        .bind(request.requested_at)
        .bind(authorized_until)
        .execute(&mut *tx)
        .await
        .map_err(RepoError::from)
        .map_err(E::from)?;

        insert_agent_identity_audit(
            &mut tx,
            "agent_certificate_rotation_staged",
            "agent-control",
            request.node_id,
            Some(request.remote_ip),
            None,
            json!({
                "rotation_id": request.rotation_id,
                "session_id": request.session_id,
                "old_control_certificate_id": old_control_id,
                "new_control_certificate_id": issued.control_certificate.id,
                "old_management_certificate_id": old_management_id,
                "new_management_certificate_id": issued.management_certificate.id,
                "authorized_until": authorized_until,
            }),
            request.requested_at,
        )
        .await
        .map_err(E::from)?;
        let bundle = AgentCertificateRotationBundle {
            rotation_id: request.rotation_id,
            node_id: request.node_id,
            control_certificate: issued.control_certificate,
            management_certificate: issued.management_certificate,
            authorized_until,
        };
        tx.commit()
            .await
            .map_err(RepoError::from)
            .map_err(E::from)?;
        Ok(StageAgentCertificateRotationOutcome::Issued(bundle))
    }

    pub async fn claim_agent_control_session(
        &self,
        mut claim: AgentControlSessionClaim,
    ) -> Result<AgentControlSessionClaimOutcome, RepoError> {
        validate_agent_control_session_claim(&claim)?;
        let node_id = claim.registration.node_id;
        let mut tx = self.pool.begin().await?;
        // Every claim for a node serializes on its identity first. Certificate
        // and session locks always follow, preventing old-certificate login and
        // pending-rotation takeover from acquiring those rows in reverse order.
        let claim_identity_status = sqlx::query_scalar::<_, String>(
            "select status from agent_identities where node_id = $1 for update",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;
        let certificate = sqlx::query(
            r#"
            select c.id, c.node_id, c.state, c.not_before, c.not_after
              from agent_certificates c
             where c.fingerprint_sha256 = $1
             for update of c
            "#,
        )
        .bind(claim.certificate_fingerprint_sha256.as_slice())
        .fetch_optional(&mut *tx)
        .await?;

        let decision_at = transaction_clock_timestamp(&mut tx).await?;
        claim.connected_at = decision_at;
        claim.lease_expires_at = decision_at + AGENT_CONTROL_SESSION_LEASE;

        let Some(certificate) = certificate else {
            insert_agent_session_rejection_audit(
                &mut tx,
                &claim,
                AgentCertificateAuthorizationFailure::Unknown,
            )
            .await?;
            tx.commit().await?;
            return Ok(AgentControlSessionClaimOutcome::UnauthorizedCertificate(
                AgentCertificateAuthorizationFailure::Unknown,
            ));
        };
        let certificate_id: Uuid = certificate.try_get("id")?;
        let certificate_node_id: Uuid = certificate.try_get("node_id")?;
        let certificate_state: String = certificate.try_get("state")?;
        let certificate_not_before: DateTime<Utc> = certificate.try_get("not_before")?;
        let certificate_not_after: DateTime<Utc> = certificate.try_get("not_after")?;
        let existing = sqlx::query(
            r#"
            select s.session_id, s.certificate_id, s.peer_ip::text as peer_ip,
                   s.lease_expires_at, s.disconnected_at,
                   c.fingerprint_sha256 as old_fingerprint_sha256,
                   c.state as old_certificate_state
              from agent_control_sessions s
              join agent_certificates c on c.id = s.certificate_id
             where s.node_id = $1
             for update of s, c
            "#,
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;

        let decision_at = transaction_clock_timestamp(&mut tx).await?;
        claim.connected_at = decision_at;
        claim.lease_expires_at = decision_at + AGENT_CONTROL_SESSION_LEASE;
        let authorization_failure = if certificate_node_id != node_id {
            Some(AgentCertificateAuthorizationFailure::NodeMismatch)
        } else if claim_identity_status.as_deref() != Some("active") {
            Some(AgentCertificateAuthorizationFailure::IdentityRevoked)
        } else if claim.connected_at < certificate_not_before {
            Some(AgentCertificateAuthorizationFailure::NotYetValid)
        } else if claim.connected_at >= certificate_not_after {
            Some(AgentCertificateAuthorizationFailure::Expired)
        } else {
            match certificate_state.as_str() {
                "active" | "pending_rotation" => None,
                "revoked" => Some(AgentCertificateAuthorizationFailure::Revoked),
                "replaced" => Some(AgentCertificateAuthorizationFailure::Replaced),
                _ => Some(AgentCertificateAuthorizationFailure::Unknown),
            }
        };
        if let Some(failure) = authorization_failure {
            insert_agent_session_rejection_audit(&mut tx, &claim, failure).await?;
            tx.commit().await?;
            return Ok(AgentControlSessionClaimOutcome::UnauthorizedCertificate(
                failure,
            ));
        }

        let mut replaced_session_id = None;
        let mut old_certificate_id = None;
        let mut old_fingerprint = None;
        let mut old_peer_ip = None;
        let mut old_certificate_state = None;
        let mut takeover_reason = None;
        let mut rotation_id = None;
        let mut rotation_context = None;
        if let Some(existing) = &existing {
            let existing_session_id: Uuid = existing.try_get("session_id")?;
            let existing_certificate_id: Uuid = existing.try_get("certificate_id")?;
            let lease_expires_at: DateTime<Utc> = existing.try_get("lease_expires_at")?;
            let disconnected_at: Option<DateTime<Utc>> = existing.try_get("disconnected_at")?;
            let existing_fingerprint: Vec<u8> = existing.try_get("old_fingerprint_sha256")?;
            let existing_peer_ip: Option<String> = existing.try_get("peer_ip")?;
            let existing_certificate_state: String = existing.try_get("old_certificate_state")?;
            replaced_session_id = Some(existing_session_id);
            old_certificate_id = Some(existing_certificate_id);
            old_fingerprint = Some(existing_fingerprint);
            old_peer_ip = existing_peer_ip;
            old_certificate_state = Some(existing_certificate_state);
            let healthy = disconnected_at.is_none() && lease_expires_at > claim.connected_at;

            if certificate_state == "active" {
                if healthy {
                    insert_agent_duplicate_audit(
                        &mut tx,
                        &claim,
                        existing_session_id,
                        old_fingerprint.as_deref().unwrap_or_default(),
                        old_peer_ip.as_deref(),
                    )
                    .await?;
                    tx.commit().await?;
                    return Ok(AgentControlSessionClaimOutcome::DuplicateHealthy {
                        existing_session_id,
                    });
                }
                takeover_reason = Some(if disconnected_at.is_some() {
                    AgentSessionTakeoverReason::CleanDisconnect
                } else {
                    AgentSessionTakeoverReason::StaleTimeout
                });
            }
        }

        if certificate_state == "pending_rotation" {
            let Some(existing_certificate_id) = old_certificate_id else {
                insert_agent_session_rejection_audit(
                    &mut tx,
                    &claim,
                    AgentCertificateAuthorizationFailure::RotationNotAuthorized,
                )
                .await?;
                tx.commit().await?;
                return Ok(AgentControlSessionClaimOutcome::UnauthorizedCertificate(
                    AgentCertificateAuthorizationFailure::RotationNotAuthorized,
                ));
            };
            if old_certificate_state.as_deref() != Some("active") {
                insert_agent_session_rejection_audit(
                    &mut tx,
                    &claim,
                    AgentCertificateAuthorizationFailure::RotationNotAuthorized,
                )
                .await?;
                tx.commit().await?;
                return Ok(AgentControlSessionClaimOutcome::UnauthorizedCertificate(
                    AgentCertificateAuthorizationFailure::RotationNotAuthorized,
                ));
            }
            let rotation = sqlx::query(
                r#"
                select r.id, r.authorized_until,
                       old_control.fingerprint_sha256 as old_control_fingerprint_sha256,
                       new_control.fingerprint_sha256 as new_control_fingerprint_sha256,
                       old_management.fingerprint_sha256 as old_management_fingerprint_sha256,
                       new_management.fingerprint_sha256 as new_management_fingerprint_sha256
                  from agent_certificate_rotations r
                  join agent_certificates old_control
                    on old_control.id = r.old_certificate_id and old_control.node_id = r.node_id
                  join agent_certificates new_control
                    on new_control.id = r.new_certificate_id and new_control.node_id = r.node_id
                  join agent_management_certificates old_management
                    on old_management.id = r.old_management_certificate_id
                   and old_management.node_id = r.node_id
                  join agent_management_certificates new_management
                    on new_management.id = r.new_management_certificate_id
                   and new_management.node_id = r.node_id
                 where r.node_id = $1
                   and r.old_certificate_id = $2
                   and r.new_certificate_id = $3
                   and r.state = 'pending'
                   and r.consumed_at is null
                   and r.authorized_at <= $4
                   and r.authorized_until > $4
                   and old_management.state = 'active'
                   and new_management.state = 'pending_rotation'
                 for update of r, old_management, new_management
                "#,
            )
            .bind(node_id)
            .bind(existing_certificate_id)
            .bind(certificate_id)
            .bind(claim.connected_at)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(rotation) = rotation else {
                insert_agent_session_rejection_audit(
                    &mut tx,
                    &claim,
                    AgentCertificateAuthorizationFailure::RotationNotAuthorized,
                )
                .await?;
                tx.commit().await?;
                return Ok(AgentControlSessionClaimOutcome::UnauthorizedCertificate(
                    AgentCertificateAuthorizationFailure::RotationNotAuthorized,
                ));
            };
            let claimed_rotation_id: Uuid = rotation.try_get("id")?;
            rotation_id = Some(claimed_rotation_id);
            rotation_context = Some(AgentCertificateRotationTakeoverContext {
                rotation_id: claimed_rotation_id,
                authorized_until: rotation.try_get("authorized_until")?,
                old_control_fingerprint_sha256: vec_to_sha256(
                    rotation.try_get("old_control_fingerprint_sha256")?,
                )?,
                new_control_fingerprint_sha256: vec_to_sha256(
                    rotation.try_get("new_control_fingerprint_sha256")?,
                )?,
                old_management_fingerprint_sha256: vec_to_sha256(
                    rotation.try_get("old_management_fingerprint_sha256")?,
                )?,
                new_management_fingerprint_sha256: vec_to_sha256(
                    rotation.try_get("new_management_fingerprint_sha256")?,
                )?,
            });
            let consumed = sqlx::query(
                r#"
                update agent_certificate_rotations
                   set state = 'control_activated',
                       consumed_at = $1,
                       consumed_by_session_id = $2
                 where id = $3 and state = 'pending' and consumed_at is null
                "#,
            )
            .bind(claim.connected_at)
            .bind(claim.session_id)
            .bind(claimed_rotation_id)
            .execute(&mut *tx)
            .await?;
            if consumed.rows_affected() != 1 {
                return Err(RepoError::AgentIdentityInvariant(
                    "certificate rotation changed while locked".to_string(),
                ));
            }
            let replaced = sqlx::query(
                "update agent_certificates set state = 'replaced', revoked_at = $1, revocation_reason = 'rotation_takeover' where id = $2 and state = 'active'",
            )
            .bind(claim.connected_at)
            .bind(existing_certificate_id)
            .execute(&mut *tx)
            .await?;
            if replaced.rows_affected() != 1 {
                return Err(RepoError::AgentIdentityInvariant(
                    "previous agent certificate changed while locked".to_string(),
                ));
            }
            let activated = sqlx::query(
                "update agent_certificates set state = 'active', activated_at = $1 where id = $2 and state = 'pending_rotation'",
            )
            .bind(claim.connected_at)
            .bind(certificate_id)
            .execute(&mut *tx)
            .await?;
            if activated.rows_affected() != 1 {
                return Err(RepoError::AgentIdentityInvariant(
                    "rotation certificate changed while locked".to_string(),
                ));
            }
            takeover_reason = Some(AgentSessionTakeoverReason::CertificateRotation);
        }

        if certificate_state == "active"
            && matches!(
                takeover_reason,
                Some(
                    AgentSessionTakeoverReason::StaleTimeout
                        | AgentSessionTakeoverReason::CleanDisconnect
                )
            )
        {
            if let Some(incomplete) = sqlx::query(
                r#"
                select r.id, r.state, r.authorized_until,
                       old_control.fingerprint_sha256 as old_control_fingerprint_sha256,
                       new_control.fingerprint_sha256 as new_control_fingerprint_sha256,
                       old_management.fingerprint_sha256 as old_management_fingerprint_sha256,
                       new_management.fingerprint_sha256 as new_management_fingerprint_sha256,
                       r.consumed_by_session_id, r.management_activated_by_session_id
                  from agent_certificate_rotations r
                  join agent_certificates old_control
                    on old_control.id = r.old_certificate_id and old_control.node_id = r.node_id
                  join agent_certificates new_control
                    on new_control.id = r.new_certificate_id and new_control.node_id = r.node_id
                  join agent_management_certificates old_management
                    on old_management.id = r.old_management_certificate_id
                   and old_management.node_id = r.node_id
                  join agent_management_certificates new_management
                    on new_management.id = r.new_management_certificate_id
                   and new_management.node_id = r.node_id
                 where r.node_id = $1
                   and r.new_certificate_id = $2
                   and r.state in ('control_activated', 'management_activated')
                   and r.completed_at is null
                 for update of r
                "#,
            )
            .bind(node_id)
            .bind(certificate_id)
            .fetch_optional(&mut *tx)
            .await?
            {
                let incomplete_state: String = incomplete.try_get("state")?;
                let consumed_by_session_id: Option<Uuid> =
                    incomplete.try_get("consumed_by_session_id")?;
                let management_activated_by_session_id: Option<Uuid> =
                    incomplete.try_get("management_activated_by_session_id")?;
                let previous_session_matches = consumed_by_session_id == replaced_session_id
                    && (incomplete_state == "control_activated"
                        || management_activated_by_session_id == replaced_session_id);
                if previous_session_matches {
                    let rebound = sqlx::query(
                        r#"
                        update agent_certificate_rotations
                           set consumed_by_session_id = $1,
                               management_activated_by_session_id =
                                 case when state = 'management_activated' then $1
                                      else management_activated_by_session_id end
                         where id = $2
                           and consumed_by_session_id = $3
                           and state in ('control_activated', 'management_activated')
                        "#,
                    )
                    .bind(claim.session_id)
                    .bind(incomplete.try_get::<Uuid, _>("id")?)
                    .bind(replaced_session_id)
                    .execute(&mut *tx)
                    .await?;
                    if rebound.rows_affected() != 1 {
                        return Err(RepoError::AgentIdentityInvariant(
                            "incomplete rotation session changed while locked".to_string(),
                        ));
                    }
                    let rebound_rotation_id: Uuid = incomplete.try_get("id")?;
                    rotation_id = Some(rebound_rotation_id);
                    rotation_context = Some(AgentCertificateRotationTakeoverContext {
                        rotation_id: rebound_rotation_id,
                        authorized_until: incomplete.try_get("authorized_until")?,
                        old_control_fingerprint_sha256: vec_to_sha256(
                            incomplete.try_get("old_control_fingerprint_sha256")?,
                        )?,
                        new_control_fingerprint_sha256: vec_to_sha256(
                            incomplete.try_get("new_control_fingerprint_sha256")?,
                        )?,
                        old_management_fingerprint_sha256: vec_to_sha256(
                            incomplete.try_get("old_management_fingerprint_sha256")?,
                        )?,
                        new_management_fingerprint_sha256: vec_to_sha256(
                            incomplete.try_get("new_management_fingerprint_sha256")?,
                        )?,
                    });
                }
            }
        }

        upsert_agent_registration_in_transaction(&mut tx, &claim.registration, claim.connected_at)
            .await?;
        sqlx::query(
            r#"
            insert into agent_control_sessions (
              node_id, session_id, core_instance_id, certificate_id, peer_ip,
              connected_at, last_activity_at, lease_expires_at, disconnected_at,
              takeover_from_session_id, takeover_reason
            ) values (
              $1, $2, $3, $4, $5::inet,
              $6, $6, $7, null, $8, $9
            )
            on conflict (node_id) do update
               set session_id = excluded.session_id,
                   core_instance_id = excluded.core_instance_id,
                   certificate_id = excluded.certificate_id,
                   peer_ip = excluded.peer_ip,
                   connected_at = excluded.connected_at,
                   last_activity_at = excluded.last_activity_at,
                   lease_expires_at = excluded.lease_expires_at,
                   disconnected_at = null,
                   takeover_from_session_id = excluded.takeover_from_session_id,
                   takeover_reason = excluded.takeover_reason
            "#,
        )
        .bind(node_id)
        .bind(claim.session_id)
        .bind(claim.core_instance_id)
        .bind(certificate_id)
        .bind(claim.peer_ip.to_string())
        .bind(claim.connected_at)
        .bind(claim.lease_expires_at)
        .bind(replaced_session_id)
        .bind(takeover_reason.map(AgentSessionTakeoverReason::as_str))
        .execute(&mut *tx)
        .await?;

        let event_type = match takeover_reason {
            Some(AgentSessionTakeoverReason::CleanDisconnect) => "agent_session_reconnected",
            Some(_) => "agent_session_takeover",
            None => "agent_session_connected",
        };
        insert_agent_identity_audit(
            &mut tx,
            event_type,
            "agent-control",
            node_id,
            Some(claim.peer_ip),
            None,
            json!({
                "old_fingerprint_sha256": old_fingerprint.as_deref().map(hex_lower),
                "new_fingerprint_sha256": hex_lower(&claim.certificate_fingerprint_sha256),
                "old_session_id": replaced_session_id,
                "new_session_id": claim.session_id,
                "old_peer_ip": old_peer_ip,
                "new_peer_ip": claim.peer_ip,
                "reason": takeover_reason.map(AgentSessionTakeoverReason::as_str),
                "rotation_id": rotation_id,
            }),
            claim.connected_at,
        )
        .await?;
        tx.commit().await?;
        Ok(AgentControlSessionClaimOutcome::Claimed {
            certificate_id,
            replaced_session_id,
            takeover_reason,
            rotation_context,
        })
    }

    pub async fn activate_agent_management_rotation(
        &self,
        mut request: AgentManagementRotationActivationRequest,
    ) -> Result<AgentManagementRotationActivationOutcome, RepoError> {
        validate_management_rotation_activation_request(&request)?;
        let mut tx = self.pool.begin().await?;
        let identity_status = sqlx::query_scalar::<_, String>(
            "select status from agent_identities where node_id = $1 for update",
        )
        .bind(request.node_id)
        .fetch_optional(&mut *tx)
        .await?;
        let row = sqlx::query(
            r#"
            select r.state, r.authorized_until, r.consumed_by_session_id,
                   r.management_activated_at, r.management_activated_by_session_id,
                   r.old_management_certificate_id, r.new_management_certificate_id,
                   s.session_id, s.disconnected_at, s.lease_expires_at,
                   new_control.state as control_state,
                   new_control.not_before as control_not_before,
                   new_control.not_after as control_not_after,
                   new_control.fingerprint_sha256 as control_fingerprint_sha256,
                   new_management.not_before as management_not_before,
                   new_management.not_after as management_not_after,
                   new_management.fingerprint_sha256 as management_fingerprint_sha256,
                   old_management.state as old_management_state,
                   new_management.state as new_management_state
              from agent_certificate_rotations r
              join agent_control_sessions s
                on s.node_id = r.node_id and s.certificate_id = r.new_certificate_id
              join agent_certificates new_control
                on new_control.id = r.new_certificate_id and new_control.node_id = r.node_id
              join agent_management_certificates old_management
                on old_management.id = r.old_management_certificate_id
               and old_management.node_id = r.node_id
              join agent_management_certificates new_management
                on new_management.id = r.new_management_certificate_id
               and new_management.node_id = r.node_id
             where r.id = $1 and r.node_id = $2
             for update of r, s, new_control, old_management, new_management
            "#,
        )
        .bind(request.rotation_id)
        .bind(request.node_id)
        .fetch_optional(&mut *tx)
        .await?;
        request.activated_at = transaction_clock_timestamp(&mut tx).await?;
        let valid = if let Some(row) = &row {
            let state: String = row.try_get("state")?;
            identity_status.as_deref() == Some("active")
                && matches!(state.as_str(), "control_activated" | "management_activated")
                && row.try_get::<Uuid, _>("session_id")? == request.session_id
                && row.try_get::<Option<Uuid>, _>("consumed_by_session_id")?
                    == Some(request.session_id)
                && row
                    .try_get::<Option<DateTime<Utc>>, _>("disconnected_at")?
                    .is_none()
                && row.try_get::<DateTime<Utc>, _>("lease_expires_at")? > request.activated_at
                && row.try_get::<String, _>("control_state")? == "active"
                && row.try_get::<DateTime<Utc>, _>("control_not_before")? <= request.activated_at
                && row.try_get::<DateTime<Utc>, _>("control_not_after")? > request.activated_at
                && row.try_get::<DateTime<Utc>, _>("management_not_before")? <= request.activated_at
                && row.try_get::<DateTime<Utc>, _>("management_not_after")? > request.activated_at
                && row
                    .try_get::<Vec<u8>, _>("control_fingerprint_sha256")?
                    .as_slice()
                    == request.control_fingerprint_sha256.as_slice()
                && row
                    .try_get::<Vec<u8>, _>("management_fingerprint_sha256")?
                    .as_slice()
                    == request.management_fingerprint_sha256.as_slice()
        } else {
            false
        };
        if !valid {
            reject_agent_rotation_transition(
                &mut tx,
                request.node_id,
                request.session_id,
                request.rotation_id,
                "management_activation_mismatch",
                request.activated_at,
            )
            .await?;
            tx.commit().await?;
            return Ok(AgentManagementRotationActivationOutcome::Rejected);
        }
        let row = row.expect("validated rotation row is present");
        let state: String = row.try_get("state")?;
        if state == "management_activated" {
            let management_activated_at = row
                .try_get::<Option<DateTime<Utc>>, _>("management_activated_at")?
                .ok_or_else(|| {
                    RepoError::AgentIdentityInvariant(
                        "activated management rotation has no activation time".to_string(),
                    )
                })?;
            let certificate_expires_at = std::cmp::min(
                row.try_get("control_not_after")?,
                row.try_get("management_not_after")?,
            );
            let original_retirement_deadline = std::cmp::min(
                management_activated_at + AGENT_PREVIOUS_IDENTITY_RETIREMENT_WINDOW,
                certificate_expires_at,
            );
            // If every Activate command was lost for the complete first
            // retirement window, issue a fresh bounded command deadline. An
            // Agent that already persisted its activation audit ignores this
            // field and only replays the ACK, so the old key lifetime is never
            // extended after a successful local activation.
            let previous_identity_expires_at =
                if original_retirement_deadline > request.activated_at {
                    original_retirement_deadline
                } else {
                    std::cmp::min(
                        request.activated_at + AGENT_PREVIOUS_IDENTITY_RETIREMENT_WINDOW,
                        certificate_expires_at,
                    )
                };
            let context = AgentManagementRotationActivationContext {
                rotation_id: request.rotation_id,
                previous_identity_expires_at,
            };
            if row.try_get::<Option<Uuid>, _>("management_activated_by_session_id")?
                == Some(request.session_id)
                && row.try_get::<String, _>("old_management_state")? == "replaced"
                && row.try_get::<String, _>("new_management_state")? == "active"
            {
                tx.commit().await?;
                return Ok(AgentManagementRotationActivationOutcome::Recovered(context));
            }
            reject_agent_rotation_transition(
                &mut tx,
                request.node_id,
                request.session_id,
                request.rotation_id,
                "management_activation_state_mismatch",
                request.activated_at,
            )
            .await?;
            tx.commit().await?;
            return Ok(AgentManagementRotationActivationOutcome::Rejected);
        }
        if row.try_get::<String, _>("old_management_state")? != "active"
            || row.try_get::<String, _>("new_management_state")? != "pending_rotation"
        {
            return Err(RepoError::AgentIdentityInvariant(
                "management rotation certificate states changed while locked".to_string(),
            ));
        }
        let old_management_id: Uuid = row.try_get("old_management_certificate_id")?;
        let new_management_id: Uuid = row.try_get("new_management_certificate_id")?;
        let replaced = sqlx::query(
            "update agent_management_certificates set state = 'replaced', revoked_at = $1, revocation_reason = 'rotation_activated' where id = $2 and state = 'active'",
        )
        .bind(request.activated_at)
        .bind(old_management_id)
        .execute(&mut *tx)
        .await?;
        let activated = sqlx::query(
            "update agent_management_certificates set state = 'active', activated_at = $1 where id = $2 and state = 'pending_rotation'",
        )
        .bind(request.activated_at)
        .bind(new_management_id)
        .execute(&mut *tx)
        .await?;
        let advanced = sqlx::query(
            r#"
            update agent_certificate_rotations
               set state = 'management_activated',
                   management_activated_at = $1,
                   management_activated_by_session_id = $2
             where id = $3 and state = 'control_activated'
            "#,
        )
        .bind(request.activated_at)
        .bind(request.session_id)
        .bind(request.rotation_id)
        .execute(&mut *tx)
        .await?;
        if replaced.rows_affected() != 1
            || activated.rows_affected() != 1
            || advanced.rows_affected() != 1
        {
            return Err(RepoError::AgentIdentityInvariant(
                "management rotation changed while locked".to_string(),
            ));
        }
        let management_activated_at = DateTime::from_timestamp_micros(
            request.activated_at.timestamp_micros(),
        )
        .ok_or_else(|| {
            RepoError::AgentIdentityInvariant(
                "management rotation activation time is outside PostgreSQL range".to_string(),
            )
        })?;
        let context = AgentManagementRotationActivationContext {
            rotation_id: request.rotation_id,
            previous_identity_expires_at: std::cmp::min(
                management_activated_at + AGENT_PREVIOUS_IDENTITY_RETIREMENT_WINDOW,
                std::cmp::min(
                    row.try_get("control_not_after")?,
                    row.try_get("management_not_after")?,
                ),
            ),
        };
        insert_agent_identity_audit(
            &mut tx,
            "agent_management_certificate_activated",
            "agent-control",
            request.node_id,
            None,
            None,
            json!({
                "rotation_id": request.rotation_id,
                "session_id": request.session_id,
                "control_fingerprint_sha256": hex_lower(&request.control_fingerprint_sha256),
                "management_fingerprint_sha256": hex_lower(&request.management_fingerprint_sha256),
                "initial_takeover_authorized_until": row.try_get::<DateTime<Utc>, _>("authorized_until")?,
                "previous_identity_expires_at": context.previous_identity_expires_at,
            }),
            request.activated_at,
        )
        .await?;
        tx.commit().await?;
        Ok(AgentManagementRotationActivationOutcome::Activated(context))
    }

    pub async fn agent_management_certificate_fingerprints_for_session(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        _now: DateTime<Utc>,
    ) -> Result<Option<AgentManagementCertificateFingerprints>, RepoError> {
        if node_id.is_nil() || session_id.is_nil() {
            return Err(RepoError::AgentIdentityInvariant(
                "management target node and session identifiers must be non-nil".to_string(),
            ));
        }
        let rows = sqlx::query(
            r#"
            with decision as materialized (
              select clock_timestamp() as now
            ), session_state as (
              select exists (
                select 1
                  from agent_control_sessions s
                  join agent_certificates c
                    on c.id = s.certificate_id and c.node_id = s.node_id
                  join agent_identities i on i.node_id = s.node_id
                  cross join decision d
                 where s.node_id = $1
                   and s.session_id = $2
                   and s.disconnected_at is null
                   and s.lease_expires_at > d.now
                   and c.state = 'active'
                   and c.not_before <= d.now
                   and c.not_after > d.now
                   and i.status = 'active'
              ) as is_current
            ), pins as (
              select m.state, m.fingerprint_sha256
                from agent_management_certificates m
                cross join decision d
               where m.node_id = $1
                 and m.state in ('active', 'pending_rotation')
                 and m.not_before <= d.now
                 and m.not_after > d.now
            )
            select s.is_current, p.state, p.fingerprint_sha256
              from session_state s
              left join pins p on s.is_current
             order by case p.state when 'active' then 0 when 'pending_rotation' then 1 else 2 end
            "#,
        )
        .bind(node_id)
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        if !rows
            .first()
            .ok_or_else(|| {
                RepoError::AgentIdentityInvariant(
                    "management pin query returned no session state".to_string(),
                )
            })?
            .try_get::<bool, _>("is_current")?
        {
            return Ok(None);
        }
        let mut current_fingerprint = None;
        let mut rotating_fingerprint = None;
        for row in rows {
            let Some(state) = row.try_get::<Option<String>, _>("state")? else {
                continue;
            };
            let fingerprint = vec_to_sha256(
                row.try_get::<Option<Vec<u8>>, _>("fingerprint_sha256")?
                    .ok_or_else(|| {
                        RepoError::AgentIdentityInvariant(
                            "management pin row is missing its fingerprint".to_string(),
                        )
                    })?,
            )?;
            match state.as_str() {
                "active" if current_fingerprint.replace(fingerprint).is_none() => {}
                "pending_rotation" if rotating_fingerprint.replace(fingerprint).is_none() => {}
                _ => {
                    return Err(RepoError::AgentIdentityInvariant(
                        "management certificate pin set is ambiguous".to_string(),
                    ));
                }
            }
        }
        let Some(current_fingerprint_sha256) = current_fingerprint else {
            return Err(RepoError::AgentIdentityInvariant(
                "current Agent session has no active management certificate".to_string(),
            ));
        };
        Ok(Some(AgentManagementCertificateFingerprints {
            current_fingerprint_sha256,
            rotating_fingerprint_sha256: rotating_fingerprint,
        }))
    }

    pub async fn complete_agent_certificate_rotation(
        &self,
        mut acknowledgement: AgentCertificateRotationAcknowledgement,
    ) -> Result<CompleteAgentCertificateRotationOutcome, RepoError> {
        validate_agent_certificate_rotation_acknowledgement(&acknowledgement)?;
        let mut tx = self.pool.begin().await?;
        let identity_status = sqlx::query_scalar::<_, String>(
            "select status from agent_identities where node_id = $1 for update",
        )
        .bind(acknowledgement.node_id)
        .fetch_optional(&mut *tx)
        .await?;
        let row = sqlx::query(
            r#"
            select r.state, r.authorized_until, r.consumed_by_session_id,
                   r.management_activated_by_session_id, r.completed_by_session_id,
                   s.session_id, s.disconnected_at, s.lease_expires_at,
                   new_control.state as control_state,
                   new_control.not_before as control_not_before,
                   new_control.not_after as control_not_after,
                   new_control.fingerprint_sha256 as control_fingerprint_sha256,
                   new_management.state as management_state,
                   new_management.not_before as management_not_before,
                   new_management.not_after as management_not_after,
                   new_management.fingerprint_sha256 as management_fingerprint_sha256
              from agent_certificate_rotations r
              join agent_control_sessions s
                on s.node_id = r.node_id and s.certificate_id = r.new_certificate_id
              join agent_certificates new_control
                on new_control.id = r.new_certificate_id and new_control.node_id = r.node_id
              join agent_management_certificates new_management
                on new_management.id = r.new_management_certificate_id
               and new_management.node_id = r.node_id
             where r.id = $1 and r.node_id = $2
             for update of r, s, new_control, new_management
            "#,
        )
        .bind(acknowledgement.rotation_id)
        .bind(acknowledgement.node_id)
        .fetch_optional(&mut *tx)
        .await?;
        acknowledgement.acknowledged_at = transaction_clock_timestamp(&mut tx).await?;
        let valid = if let Some(row) = &row {
            let state: String = row.try_get("state")?;
            let current_new_identity = identity_status.as_deref() == Some("active")
                && row.try_get::<Uuid, _>("session_id")? == acknowledgement.session_id
                && row
                    .try_get::<Option<DateTime<Utc>>, _>("disconnected_at")?
                    .is_none()
                && row.try_get::<DateTime<Utc>, _>("lease_expires_at")?
                    > acknowledgement.acknowledged_at
                && row.try_get::<String, _>("control_state")? == "active"
                && row.try_get::<DateTime<Utc>, _>("control_not_before")?
                    <= acknowledgement.acknowledged_at
                && row.try_get::<DateTime<Utc>, _>("control_not_after")?
                    > acknowledgement.acknowledged_at
                && row.try_get::<String, _>("management_state")? == "active"
                && row.try_get::<DateTime<Utc>, _>("management_not_before")?
                    <= acknowledgement.acknowledged_at
                && row.try_get::<DateTime<Utc>, _>("management_not_after")?
                    > acknowledgement.acknowledged_at
                && row
                    .try_get::<Vec<u8>, _>("control_fingerprint_sha256")?
                    .as_slice()
                    == acknowledgement.control_fingerprint_sha256.as_slice()
                && row
                    .try_get::<Vec<u8>, _>("management_fingerprint_sha256")?
                    .as_slice()
                    == acknowledgement.management_fingerprint_sha256.as_slice();
            match state.as_str() {
                "management_activated" => {
                    current_new_identity
                        && row.try_get::<Option<Uuid>, _>("consumed_by_session_id")?
                            == Some(acknowledgement.session_id)
                        && row.try_get::<Option<Uuid>, _>("management_activated_by_session_id")?
                            == Some(acknowledgement.session_id)
                }
                "completed" => {
                    current_new_identity
                        && row
                            .try_get::<Option<Uuid>, _>("consumed_by_session_id")?
                            .is_some()
                        && row
                            .try_get::<Option<Uuid>, _>("management_activated_by_session_id")?
                            .is_some()
                        && row
                            .try_get::<Option<Uuid>, _>("completed_by_session_id")?
                            .is_some()
                }
                _ => false,
            }
        } else {
            false
        };
        if !valid {
            reject_agent_rotation_transition(
                &mut tx,
                acknowledgement.node_id,
                acknowledgement.session_id,
                acknowledgement.rotation_id,
                "activation_ack_mismatch",
                acknowledgement.acknowledged_at,
            )
            .await?;
            tx.commit().await?;
            return Ok(CompleteAgentCertificateRotationOutcome::Rejected);
        }
        let row = row.expect("validated rotation row is present");
        let state: String = row.try_get("state")?;
        if state == "completed" {
            tx.commit().await?;
            return Ok(CompleteAgentCertificateRotationOutcome::Recovered);
        }
        let completed = sqlx::query(
            r#"
            update agent_certificate_rotations
               set state = 'completed', completed_at = $1, completed_by_session_id = $2
             where id = $3 and state = 'management_activated'
            "#,
        )
        .bind(acknowledgement.acknowledged_at)
        .bind(acknowledgement.session_id)
        .bind(acknowledgement.rotation_id)
        .execute(&mut *tx)
        .await?;
        if completed.rows_affected() != 1 {
            return Err(RepoError::AgentIdentityInvariant(
                "rotation completion changed while locked".to_string(),
            ));
        }
        insert_agent_identity_audit(
            &mut tx,
            "agent_certificate_rotation_completed",
            "agent-control",
            acknowledgement.node_id,
            None,
            None,
            json!({
                "rotation_id": acknowledgement.rotation_id,
                "session_id": acknowledgement.session_id,
                "control_fingerprint_sha256": hex_lower(&acknowledgement.control_fingerprint_sha256),
                "management_fingerprint_sha256": hex_lower(&acknowledgement.management_fingerprint_sha256),
            }),
            acknowledgement.acknowledged_at,
        )
        .await?;
        tx.commit().await?;
        Ok(CompleteAgentCertificateRotationOutcome::Completed)
    }

    #[cfg(test)]
    pub async fn touch_agent_control_session(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        _now: DateTime<Utc>,
    ) -> Result<bool, RepoError> {
        let result = sqlx::query(
            r#"
            with decision as materialized (select clock_timestamp() as now)
            update agent_control_sessions s
               set last_activity_at = d.now,
                   lease_expires_at = d.now + interval '30 seconds'
              from agent_certificates c, agent_identities i, decision d
             where s.node_id = $1
               and s.session_id = $2
               and s.disconnected_at is null
               and s.lease_expires_at > d.now
               and c.id = s.certificate_id
               and c.node_id = s.node_id
               and c.state = 'active'
               and c.not_before <= d.now
               and c.not_after > d.now
               and i.node_id = s.node_id
               and i.status = 'active'
            "#,
        )
        .bind(node_id)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    #[cfg(test)]
    pub async fn release_agent_control_session(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        _now: DateTime<Utc>,
    ) -> Result<bool, RepoError> {
        let result = sqlx::query(
            r#"
            with decision as materialized (select clock_timestamp() as now)
            update agent_control_sessions s
               set disconnected_at = coalesce(s.disconnected_at, d.now),
                   last_activity_at = greatest(s.last_activity_at, d.now)
              from decision d
             where s.node_id = $1
               and s.session_id = $2
               and s.disconnected_at is null
            "#,
        )
        .bind(node_id)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Fences a disconnect against the durable session row and applies every
    /// resulting offline transition in the same transaction. This prevents an
    /// old Core from marking a node offline after a different Core has already
    /// taken over the Agent session.
    pub async fn close_agent_control_session_and_reclaim(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        _now: DateTime<Utc>,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await?;
        let current_session_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            select session_id
              from agent_control_sessions
             where node_id = $1
               and disconnected_at is null
             for update
            "#,
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;
        if current_session_id != Some(session_id) {
            tx.commit().await?;
            return Ok(false);
        }
        let now = transaction_clock_timestamp(&mut tx).await?;

        let released = sqlx::query(
            r#"
            update agent_control_sessions
               set disconnected_at = $1,
                   last_activity_at = greatest(last_activity_at, $1)
             where node_id = $2
               and session_id = $3
               and disconnected_at is null
            "#,
        )
        .bind(now)
        .bind(node_id)
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
        if released.rows_affected() != 1 {
            return Err(RepoError::AgentIdentityInvariant(
                "Agent control session changed while locked".to_string(),
            ));
        }

        let node_updated = sqlx::query(
            r#"
            update media_nodes
               set healthy = false,
                   control_connected = false,
                   updated_at = $1
             where id = $2
            "#,
        )
        .bind(now)
        .bind(node_id)
        .execute(&mut *tx)
        .await?;
        if node_updated.rows_affected() != 1 {
            return Err(RepoError::NodeNotFound(node_id));
        }

        let tasks = sqlx::query(
            r#"
            select id, status::text as status, current_attempt_no
              from tasks
             where assigned_node_id = $1
               and current_attempt_no > 0
               and status in ('DISPATCHING', 'STARTING', 'RUNNING', 'STOPPING', 'RECOVERING')
             order by updated_at asc
             for update
            "#,
        )
        .bind(node_id)
        .fetch_all(&mut *tx)
        .await?;
        for task in tasks {
            let task_id: Uuid = task.try_get("id")?;
            let attempt_no: i32 = task.try_get("current_attempt_no")?;
            let status = TaskStatus::from_str(&task.try_get::<String, _>("status")?)?;
            let reclaim_deadline_at = now
                + if status == TaskStatus::Dispatching {
                    DISPATCH_RECLAIM_GRACE
                } else {
                    RUNTIME_RECLAIM_GRACE
                };
            let updated = sqlx::query(
                r#"
                update tasks
                   set status = 'RECLAIMING'::task_status,
                       reclaim_deadline_at = $1,
                       updated_at = $2,
                       finished_at = null
                 where id = $3
                   and current_attempt_no = $4
                   and status = $5::task_status
                "#,
            )
            .bind(reclaim_deadline_at)
            .bind(now)
            .bind(task_id)
            .bind(attempt_no)
            .bind(status.as_str())
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(RepoError::AgentIdentityInvariant(format!(
                    "task {task_id} changed while Agent session row was locked"
                )));
            }
            self.insert_event(
                &mut tx,
                task_id,
                None,
                Some(attempt_no),
                EventSource::Core,
                "task_reclaiming_after_node_disconnect",
                "warn",
                json!({
                    "node_id": node_id,
                    "attempt_no": attempt_no,
                    "from": status,
                    "to": TaskStatus::Reclaiming,
                    "reclaim_deadline_at": reclaim_deadline_at,
                    "session_id": session_id,
                }),
            )
            .await?;
        }

        insert_agent_identity_audit(
            &mut tx,
            "agent_session_disconnected",
            "agent-control",
            node_id,
            None,
            None,
            json!({ "session_id": session_id }),
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn agent_control_session_is_current(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        _now: DateTime<Utc>,
    ) -> Result<bool, RepoError> {
        Ok(sqlx::query_scalar(
            r#"
            with decision as materialized (select clock_timestamp() as now)
            select exists (
              select 1
                from agent_control_sessions s
                join agent_certificates c on c.id = s.certificate_id
                join agent_identities i on i.node_id = s.node_id
                cross join decision d
               where s.node_id = $1
                 and s.session_id = $2
                 and s.disconnected_at is null
                 and s.lease_expires_at > d.now
                 and c.node_id = s.node_id
                 and c.state = 'active'
                 and c.not_before <= d.now
                 and c.not_after > d.now
                 and i.status = 'active'
            )
            "#,
        )
        .bind(node_id)
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn begin_agent_control_session_fence(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<sqlx::Transaction<'static, sqlx::Postgres>>, RepoError> {
        let mut tx = self.pool.begin().await?;
        if self
            .lock_current_agent_control_session(&mut tx, node_id, session_id, now)
            .await?
        {
            Ok(Some(tx))
        } else {
            tx.commit().await?;
            Ok(None)
        }
    }

    pub(super) async fn lock_current_agent_control_session(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        node_id: Uuid,
        session_id: Uuid,
        _now: DateTime<Utc>,
    ) -> Result<bool, RepoError> {
        // Lock only the session row. A takeover locks the certificate first and
        // then this row; avoiding a certificate row lock here preserves that
        // lock order while still serializing every inbound mutation against
        // takeover/close.
        Ok(sqlx::query_scalar::<_, i32>(
            r#"
            with decision as materialized (select clock_timestamp() as now)
            select 1
              from agent_control_sessions s
              join agent_certificates c on c.id = s.certificate_id
              join agent_identities i on i.node_id = s.node_id
              cross join decision d
             where s.node_id = $1
               and s.session_id = $2
               and s.disconnected_at is null
               and s.lease_expires_at > d.now
               and c.node_id = s.node_id
               and c.state = 'active'
               and c.not_before <= d.now
               and c.not_after > d.now
               and i.status = 'active'
             for share of s
            "#,
        )
        .bind(node_id)
        .bind(session_id)
        .fetch_optional(&mut **tx)
        .await?
        .is_some())
    }

    pub(super) async fn renew_current_agent_control_session(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        node_id: Uuid,
        session_id: Uuid,
        _now: DateTime<Utc>,
    ) -> Result<bool, RepoError> {
        // A single UPDATE acquires the exclusive row lock directly. Do not
        // take a shared lock and then upgrade it: concurrent heartbeats could
        // otherwise deadlock while both wait to upgrade their session lock.
        let renewed = sqlx::query(
            r#"
            with decision as materialized (select clock_timestamp() as now)
            update agent_control_sessions s
               set last_activity_at = d.now,
                   lease_expires_at = d.now + interval '30 seconds'
              from agent_certificates c, agent_identities i, decision d
             where s.node_id = $1
               and s.session_id = $2
               and s.disconnected_at is null
               and s.lease_expires_at > d.now
               and c.id = s.certificate_id
               and c.node_id = s.node_id
               and c.state = 'active'
               and c.not_before <= d.now
               and c.not_after > d.now
               and i.node_id = s.node_id
               and i.status = 'active'
            "#,
        )
        .bind(node_id)
        .bind(session_id)
        .execute(&mut **tx)
        .await?;
        Ok(renewed.rows_affected() == 1)
    }
}

async fn insert_agent_session_rejection_audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    claim: &AgentControlSessionClaim,
    failure: AgentCertificateAuthorizationFailure,
) -> Result<(), RepoError> {
    insert_agent_identity_audit(
        tx,
        "agent_certificate_replay_rejected",
        "agent-control",
        claim.registration.node_id,
        Some(claim.peer_ip),
        None,
        json!({
            "fingerprint_sha256": hex_lower(&claim.certificate_fingerprint_sha256),
            "session_id": claim.session_id,
            "peer_ip": claim.peer_ip,
            "reason": format!("{failure:?}").to_ascii_lowercase(),
        }),
        claim.connected_at,
    )
    .await
}

async fn insert_agent_duplicate_audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    claim: &AgentControlSessionClaim,
    existing_session_id: Uuid,
    existing_fingerprint: &[u8],
    existing_peer_ip: Option<&str>,
) -> Result<(), RepoError> {
    insert_agent_identity_audit(
        tx,
        "agent_session_rejected_duplicate",
        "agent-control",
        claim.registration.node_id,
        Some(claim.peer_ip),
        None,
        json!({
            "old_fingerprint_sha256": hex_lower(existing_fingerprint),
            "new_fingerprint_sha256": hex_lower(&claim.certificate_fingerprint_sha256),
            "old_session_id": existing_session_id,
            "new_session_id": claim.session_id,
            "old_peer_ip": existing_peer_ip,
            "new_peer_ip": claim.peer_ip,
            "reason": "healthy_session",
        }),
        claim.connected_at,
    )
    .await
}

async fn upsert_agent_registration_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    registration: &AgentRegistration,
    seen_at: DateTime<Utc>,
) -> Result<(), RepoError> {
    sqlx::query(
        r#"
        insert into media_nodes (
          id, node_name, hostname, labels, zlm_api_base, zlm_api_secret, agent_stream_addr,
          agent_http_base_url, zlm_rtmp_port, zlm_rtsp_port,
          output_mount_relative_prefix_mp4, output_mount_relative_prefix_hls,
          network_mode, interfaces, healthy, control_connected, last_seen_at,
          control_last_seen_at, created_at, updated_at
        ) values (
          $1, $2, $3, $4, $5, $6, $7,
          $8, $9, $10, $11, $12, $13, $14, true, true, $15, $15, $15, $15
        )
        on conflict (id) do update
           set node_name = excluded.node_name,
               hostname = excluded.hostname,
               labels = excluded.labels,
               zlm_api_base = excluded.zlm_api_base,
               zlm_api_secret = excluded.zlm_api_secret,
               agent_stream_addr = excluded.agent_stream_addr,
               agent_http_base_url = excluded.agent_http_base_url,
               zlm_rtmp_port = excluded.zlm_rtmp_port,
               zlm_rtsp_port = excluded.zlm_rtsp_port,
               output_mount_relative_prefix_mp4 = excluded.output_mount_relative_prefix_mp4,
               output_mount_relative_prefix_hls = excluded.output_mount_relative_prefix_hls,
               network_mode = excluded.network_mode,
               interfaces = excluded.interfaces,
               healthy = true,
               control_connected = true,
               last_seen_at = excluded.last_seen_at,
               control_last_seen_at = excluded.control_last_seen_at,
               updated_at = excluded.updated_at
        "#,
    )
    .bind(registration.node_id)
    .bind(&registration.node_name)
    .bind(&registration.hostname)
    .bind(serde_json::to_value(&registration.labels)?)
    .bind("")
    .bind("")
    .bind(&registration.agent_stream_addr)
    .bind("")
    .bind(i32::from(registration.zlm_rtmp_port))
    .bind(i32::from(registration.zlm_rtsp_port))
    .bind(&registration.output_mount_relative_prefix_mp4)
    .bind(&registration.output_mount_relative_prefix_hls)
    .bind(registration.network_mode.as_str())
    .bind(serde_json::to_value(&registration.interfaces)?)
    .bind(seen_at)
    .execute(&mut **tx)
    .await?;

    let zlm_server_id = registration.zlm_server_id.trim();
    if !zlm_server_id.is_empty() {
        sqlx::query(
            r#"
            insert into media_servers (server_id, node_id, last_seen_at, created_at, updated_at)
            values ($1, $2, $3, $3, $3)
            on conflict (server_id) do update
               set node_id = excluded.node_id,
                   last_seen_at = excluded.last_seen_at,
                   updated_at = excluded.updated_at
            "#,
        )
        .bind(zlm_server_id)
        .bind(registration.node_id)
        .bind(seen_at)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

fn validate_agent_enrollment_request(request: &AgentEnrollmentRequest) -> Result<(), RepoError> {
    if request.node_id.is_nil() {
        return Err(RepoError::AgentIdentityInvariant(
            "Agent identifier must be non-nil".to_string(),
        ));
    }
    if request.control_csr_public_key_sha256 == request.management_csr_public_key_sha256 {
        return Err(RepoError::AgentIdentityInvariant(
            "control and management CSRs must use different public keys".to_string(),
        ));
    }
    Ok(())
}

fn validate_agent_certificate_rotation_request(
    request: &AgentCertificateRotationRequest,
) -> Result<(), RepoError> {
    if request.rotation_id.is_nil() || request.node_id.is_nil() || request.session_id.is_nil() {
        return Err(RepoError::AgentIdentityInvariant(
            "rotation, node, and session identifiers must be non-nil".to_string(),
        ));
    }
    if request.control_csr_public_key_sha256 == request.management_csr_public_key_sha256 {
        return Err(RepoError::AgentIdentityInvariant(
            "rotation control and management CSRs must use different public keys".to_string(),
        ));
    }
    Ok(())
}

fn validate_management_rotation_activation_request(
    request: &AgentManagementRotationActivationRequest,
) -> Result<(), RepoError> {
    if request.rotation_id.is_nil() || request.node_id.is_nil() || request.session_id.is_nil() {
        return Err(RepoError::AgentIdentityInvariant(
            "management rotation activation identifiers must be non-nil".to_string(),
        ));
    }
    if request.control_fingerprint_sha256 == request.management_fingerprint_sha256 {
        return Err(RepoError::AgentIdentityInvariant(
            "control and management activation fingerprints must be distinct".to_string(),
        ));
    }
    Ok(())
}

fn validate_agent_certificate_rotation_acknowledgement(
    acknowledgement: &AgentCertificateRotationAcknowledgement,
) -> Result<(), RepoError> {
    if acknowledgement.rotation_id.is_nil()
        || acknowledgement.node_id.is_nil()
        || acknowledgement.session_id.is_nil()
    {
        return Err(RepoError::AgentIdentityInvariant(
            "rotation acknowledgement identifiers must be non-nil".to_string(),
        ));
    }
    if acknowledgement.control_fingerprint_sha256 == acknowledgement.management_fingerprint_sha256 {
        return Err(RepoError::AgentIdentityInvariant(
            "rotation acknowledgement fingerprints must be distinct".to_string(),
        ));
    }
    Ok(())
}

fn normalize_agent_certificate_rotation_issue_timestamps(
    issue: &mut AgentCertificateRotationIssue,
) -> Result<(), RepoError> {
    for certificate in [
        &mut issue.control_certificate,
        &mut issue.management_certificate,
    ] {
        certificate.not_before = DateTime::from_timestamp_micros(
            certificate.not_before.timestamp_micros(),
        )
        .ok_or_else(|| {
            RepoError::AgentIdentityInvariant(
                "rotation certificate not-before is outside PostgreSQL timestamp range".to_string(),
            )
        })?;
        certificate.not_after = DateTime::from_timestamp_micros(
            certificate.not_after.timestamp_micros(),
        )
        .ok_or_else(|| {
            RepoError::AgentIdentityInvariant(
                "rotation certificate not-after is outside PostgreSQL timestamp range".to_string(),
            )
        })?;
    }
    Ok(())
}

fn validate_agent_certificate_rotation_issue(
    request: &AgentCertificateRotationRequest,
    issue: &AgentCertificateRotationIssue,
) -> Result<(), RepoError> {
    validate_issued_enrollment_certificate(&issue.control_certificate, request.requested_at)?;
    validate_issued_enrollment_certificate(&issue.management_certificate, request.requested_at)?;
    if issue.control_certificate.public_key_sha256 != request.control_csr_public_key_sha256
        || issue.management_certificate.public_key_sha256
            != request.management_csr_public_key_sha256
    {
        return Err(RepoError::AgentIdentityInvariant(
            "issued rotation certificates do not match the requested CSRs".to_string(),
        ));
    }
    if issue.control_certificate.id == issue.management_certificate.id
        || issue.control_certificate.serial_number == issue.management_certificate.serial_number
        || issue.control_certificate.fingerprint_sha256
            == issue.management_certificate.fingerprint_sha256
    {
        return Err(RepoError::AgentIdentityInvariant(
            "rotation control and management certificates must be distinct".to_string(),
        ));
    }
    Ok(())
}

async fn insert_pending_agent_certificate(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    node_id: Uuid,
    certificate: &NewIssuedAgentCertificate,
    issued_at: DateTime<Utc>,
    management: bool,
) -> Result<(), RepoError> {
    let query = if management {
        r#"
        insert into agent_management_certificates (
          id, node_id, serial_number, fingerprint_sha256, public_key_sha256,
          certificate_pem, state, not_before, not_after, issued_at, activated_at,
          revoked_at, revocation_reason, issued_via
        ) values (
          $1, $2, $3, $4, $5, $6, 'pending_rotation', $7, $8, $9, null,
          null, null, 'rotation'
        )
        "#
    } else {
        r#"
        insert into agent_certificates (
          id, node_id, serial_number, fingerprint_sha256, public_key_sha256,
          certificate_pem, state, not_before, not_after, issued_at, activated_at,
          revoked_at, revocation_reason, issued_via
        ) values (
          $1, $2, $3, $4, $5, $6, 'pending_rotation', $7, $8, $9, null,
          null, null, 'rotation'
        )
        "#
    };
    sqlx::query(query)
        .bind(certificate.id)
        .bind(node_id)
        .bind(&certificate.serial_number)
        .bind(certificate.fingerprint_sha256.as_slice())
        .bind(certificate.public_key_sha256.as_slice())
        .bind(&certificate.certificate_pem)
        .bind(certificate.not_before)
        .bind(certificate.not_after)
        .bind(issued_at)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn recover_agent_certificate_rotation_bundle(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    rotation_id: Uuid,
    node_id: Uuid,
    control_id: Uuid,
    management_id: Uuid,
    authorized_until: DateTime<Utc>,
) -> Result<AgentCertificateRotationBundle, RepoError> {
    let row = sqlx::query(
        r#"
        select
          c.id as control_id,
          c.serial_number as control_serial_number,
          c.fingerprint_sha256 as control_fingerprint_sha256,
          c.public_key_sha256 as control_public_key_sha256,
          c.certificate_pem as control_certificate_pem,
          c.not_before as control_not_before,
          c.not_after as control_not_after,
          m.id as management_id,
          m.serial_number as management_serial_number,
          m.fingerprint_sha256 as management_fingerprint_sha256,
          m.public_key_sha256 as management_public_key_sha256,
          m.certificate_pem as management_certificate_pem,
          m.not_before as management_not_before,
          m.not_after as management_not_after
        from agent_certificates c
        join agent_management_certificates m on m.id = $2 and m.node_id = $3
        where c.id = $1 and c.node_id = $3
        "#,
    )
    .bind(control_id)
    .bind(management_id)
    .bind(node_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| {
        RepoError::AgentIdentityInvariant(
            "staged rotation recovery certificates are missing".to_string(),
        )
    })?;
    Ok(AgentCertificateRotationBundle {
        rotation_id,
        node_id,
        control_certificate: NewIssuedAgentCertificate {
            id: row.try_get("control_id")?,
            serial_number: row.try_get("control_serial_number")?,
            fingerprint_sha256: vec_to_sha256(row.try_get("control_fingerprint_sha256")?)?,
            public_key_sha256: vec_to_sha256(row.try_get("control_public_key_sha256")?)?,
            certificate_pem: row.try_get("control_certificate_pem")?,
            not_before: row.try_get("control_not_before")?,
            not_after: row.try_get("control_not_after")?,
        },
        management_certificate: NewIssuedAgentCertificate {
            id: row.try_get("management_id")?,
            serial_number: row.try_get("management_serial_number")?,
            fingerprint_sha256: vec_to_sha256(row.try_get("management_fingerprint_sha256")?)?,
            public_key_sha256: vec_to_sha256(row.try_get("management_public_key_sha256")?)?,
            certificate_pem: row.try_get("management_certificate_pem")?,
            not_before: row.try_get("management_not_before")?,
            not_after: row.try_get("management_not_after")?,
        },
        authorized_until,
    })
}

async fn expire_pending_agent_certificate_rotation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    rotation_id: Uuid,
    expired_at: DateTime<Utc>,
) -> Result<(), RepoError> {
    let row = sqlx::query(
        r#"
        update agent_certificate_rotations
           set state = 'expired'
         where id = $1 and state = 'pending'
         returning new_certificate_id, new_management_certificate_id
        "#,
    )
    .bind(rotation_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let control_id: Uuid = row.try_get("new_certificate_id")?;
    let management_id: Uuid = row.try_get("new_management_certificate_id")?;
    sqlx::query(
        "update agent_certificates set state = 'revoked', revoked_at = $1, revocation_reason = 'rotation_expired' where id = $2 and state = 'pending_rotation'",
    )
    .bind(expired_at)
    .bind(control_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "update agent_management_certificates set state = 'revoked', revoked_at = $1, revocation_reason = 'rotation_expired' where id = $2 and state = 'pending_rotation'",
    )
    .bind(expired_at)
    .bind(management_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn reject_agent_certificate_rotation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &AgentCertificateRotationRequest,
    reason: &'static str,
) -> Result<(), RepoError> {
    insert_agent_identity_audit(
        tx,
        "agent_certificate_rotation_rejected",
        "agent-control",
        request.node_id,
        Some(request.remote_ip),
        None,
        json!({
            "rotation_id": request.rotation_id,
            "session_id": request.session_id,
            "reason": reason,
        }),
        request.requested_at,
    )
    .await
}

async fn reject_agent_rotation_transition(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    node_id: Uuid,
    session_id: Uuid,
    rotation_id: Uuid,
    reason: &'static str,
    attempted_at: DateTime<Utc>,
) -> Result<(), RepoError> {
    insert_agent_identity_audit(
        tx,
        "agent_certificate_rotation_rejected",
        "agent-control",
        node_id,
        None,
        None,
        json!({
            "rotation_id": rotation_id,
            "session_id": session_id,
            "reason": reason,
        }),
        attempted_at,
    )
    .await
}

fn validate_issued_enrollment_certificate(
    certificate: &NewIssuedAgentCertificate,
    attempted_at: DateTime<Utc>,
) -> Result<(), RepoError> {
    if certificate.id.is_nil() {
        return Err(RepoError::AgentIdentityInvariant(
            "certificate identifier must be non-nil".to_string(),
        ));
    }
    if certificate.serial_number.trim().is_empty() || certificate.certificate_pem.trim().is_empty()
    {
        return Err(RepoError::AgentIdentityInvariant(
            "certificate serial number and PEM must not be empty".to_string(),
        ));
    }
    if certificate.not_before > attempted_at
        || certificate.not_after <= attempted_at
        || certificate.not_after - certificate.not_before > chrono::Duration::days(90)
    {
        return Err(RepoError::AgentIdentityInvariant(
            "certificate validity is outside enrollment policy".to_string(),
        ));
    }
    Ok(())
}

fn validate_agent_enrollment_bundle(
    request: &AgentEnrollmentRequest,
    bundle: &AgentEnrollmentBundle,
) -> Result<(), RepoError> {
    validate_issued_enrollment_certificate(&bundle.control_certificate, request.attempted_at)?;
    validate_issued_enrollment_certificate(&bundle.management_certificate, request.attempted_at)?;
    if bundle.node_id != request.node_id
        || bundle.control_certificate.public_key_sha256 != request.control_csr_public_key_sha256
        || bundle.management_certificate.public_key_sha256
            != request.management_csr_public_key_sha256
    {
        return Err(RepoError::AgentIdentityInvariant(
            "issued enrollment bundle does not match the request".to_string(),
        ));
    }
    if bundle.control_certificate.id == bundle.management_certificate.id
        || bundle.control_certificate.serial_number == bundle.management_certificate.serial_number
        || bundle.control_certificate.fingerprint_sha256
            == bundle.management_certificate.fingerprint_sha256
    {
        return Err(RepoError::AgentIdentityInvariant(
            "control and management certificates must be distinct".to_string(),
        ));
    }
    if bundle.agent_client_issuer_ca_pem.trim().is_empty()
        || bundle.control_plane_server_ca_pem.trim().is_empty()
        || bundle.management_client_ca_pem.trim().is_empty()
        || bundle.capability_jwt_public_key_pem.trim().is_empty()
        || bundle.capability_jwt_kid.trim().is_empty()
    {
        return Err(RepoError::AgentIdentityInvariant(
            "enrollment trust bundle must be complete".to_string(),
        ));
    }
    if bundle.agent_client_issuer_ca_pem == bundle.control_plane_server_ca_pem
        || bundle.agent_client_issuer_ca_pem == bundle.management_client_ca_pem
        || bundle.control_plane_server_ca_pem == bundle.management_client_ca_pem
    {
        return Err(RepoError::AgentIdentityInvariant(
            "enrollment trust roots must be distinct".to_string(),
        ));
    }
    Ok(())
}

fn normalize_enrollment_bundle_timestamps(
    bundle: &mut AgentEnrollmentBundle,
) -> Result<(), RepoError> {
    for certificate in [
        &mut bundle.control_certificate,
        &mut bundle.management_certificate,
    ] {
        certificate.not_before = DateTime::from_timestamp_micros(
            certificate.not_before.timestamp_micros(),
        )
        .ok_or_else(|| {
            RepoError::AgentIdentityInvariant(
                "certificate not-before is outside PostgreSQL timestamp range".to_string(),
            )
        })?;
        certificate.not_after = DateTime::from_timestamp_micros(
            certificate.not_after.timestamp_micros(),
        )
        .ok_or_else(|| {
            RepoError::AgentIdentityInvariant(
                "certificate not-after is outside PostgreSQL timestamp range".to_string(),
            )
        })?;
    }
    Ok(())
}

async fn recover_agent_enrollment_bundle(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    token_row: &sqlx::postgres::PgRow,
    node_id: Uuid,
) -> Result<AgentEnrollmentBundle, RepoError> {
    let control_id: Uuid = token_row.try_get("consumed_certificate_id")?;
    let management_id: Uuid = token_row.try_get("consumed_management_certificate_id")?;
    let row = sqlx::query(
        r#"
        select
          c.id as control_id,
          c.serial_number as control_serial_number,
          c.fingerprint_sha256 as control_fingerprint_sha256,
          c.public_key_sha256 as control_public_key_sha256,
          c.certificate_pem as control_certificate_pem,
          c.not_before as control_not_before,
          c.not_after as control_not_after,
          m.id as management_id,
          m.serial_number as management_serial_number,
          m.fingerprint_sha256 as management_fingerprint_sha256,
          m.public_key_sha256 as management_public_key_sha256,
          m.certificate_pem as management_certificate_pem,
          m.not_before as management_not_before,
          m.not_after as management_not_after
        from agent_certificates c
        join agent_management_certificates m on m.id = $2 and m.node_id = $3
        where c.id = $1 and c.node_id = $3
        "#,
    )
    .bind(control_id)
    .bind(management_id)
    .bind(node_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| {
        RepoError::AgentIdentityInvariant(
            "consumed enrollment recovery certificates are missing".to_string(),
        )
    })?;

    let bundle = AgentEnrollmentBundle {
        node_id,
        control_certificate: NewIssuedAgentCertificate {
            id: row.try_get("control_id")?,
            serial_number: row.try_get("control_serial_number")?,
            fingerprint_sha256: vec_to_sha256(row.try_get("control_fingerprint_sha256")?)?,
            public_key_sha256: vec_to_sha256(row.try_get("control_public_key_sha256")?)?,
            certificate_pem: row.try_get("control_certificate_pem")?,
            not_before: row.try_get("control_not_before")?,
            not_after: row.try_get("control_not_after")?,
        },
        management_certificate: NewIssuedAgentCertificate {
            id: row.try_get("management_id")?,
            serial_number: row.try_get("management_serial_number")?,
            fingerprint_sha256: vec_to_sha256(row.try_get("management_fingerprint_sha256")?)?,
            public_key_sha256: vec_to_sha256(row.try_get("management_public_key_sha256")?)?,
            certificate_pem: row.try_get("management_certificate_pem")?,
            not_before: row.try_get("management_not_before")?,
            not_after: row.try_get("management_not_after")?,
        },
        agent_client_issuer_ca_pem: token_row.try_get("agent_client_issuer_ca_pem")?,
        control_plane_server_ca_pem: token_row.try_get("control_plane_server_ca_pem")?,
        management_client_ca_pem: token_row.try_get("management_client_ca_pem")?,
        capability_jwt_public_key_pem: token_row.try_get("capability_jwt_public_key_pem")?,
        capability_jwt_kid: token_row.try_get("capability_jwt_kid")?,
    };
    validate_agent_enrollment_bundle(
        &AgentEnrollmentRequest {
            node_id,
            control_csr_public_key_sha256: bundle.control_certificate.public_key_sha256,
            management_csr_public_key_sha256: bundle.management_certificate.public_key_sha256,
            attempted_at: bundle.control_certificate.not_before,
            remote_ip: None,
            user_agent: None,
        },
        &bundle,
    )?;
    Ok(bundle)
}

fn vec_to_sha256(value: Vec<u8>) -> Result<[u8; 32], RepoError> {
    value.try_into().map_err(|_| {
        RepoError::AgentIdentityInvariant("persisted SHA-256 value is not 32 bytes".to_string())
    })
}

async fn transaction_clock_timestamp(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<DateTime<Utc>, RepoError> {
    Ok(sqlx::query_scalar("select clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?)
}

#[allow(clippy::too_many_arguments)]
async fn insert_agent_identity_audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event_type: &str,
    actor: &str,
    node_id: Uuid,
    remote_ip: Option<IpAddr>,
    user_agent: Option<&str>,
    payload: serde_json::Value,
    created_at: DateTime<Utc>,
) -> Result<(), RepoError> {
    sqlx::query(
        r#"
        insert into security_audit_events (
          id, event_type, actor, subject, remote_ip, user_agent, payload, created_at
        ) values ($1, $2, $3, $4, $5::inet, $6, $7, $8)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(event_type)
    .bind(actor)
    .bind(node_id.to_string())
    .bind(remote_ip.map(|value| value.to_string()))
    .bind(user_agent)
    .bind(payload)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use chrono::{Duration, TimeZone, Utc};
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use crate::test_database::{acquire_test_database_slot, config_from_env, finish_setup};

    use super::*;

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
                    .max_connections(24)
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

    #[test]
    fn identity_migration_declares_all_durable_state() {
        let migration = include_str!("../../../migrations/0014_agent_identity.sql");
        for required in [
            "create table agent_identities",
            "create table agent_enrollment_tokens",
            "create table agent_certificates",
            "create table agent_management_certificates",
            "create table agent_certificate_rotations",
            "create table agent_control_sessions",
            "control_csr_public_key_sha256",
            "management_csr_public_key_sha256",
            "certificate_pem text not null",
            "agent_client_issuer_ca_pem",
            "control_plane_server_ca_pem",
            "management_client_ca_pem",
            "capability_jwt_public_key_pem",
            "capability_jwt_kid",
            "agent_management_certificates_pending_rotation_node_uidx",
            "old_management_certificate_id",
            "new_management_certificate_id",
            "control_csr_public_key_sha256 bytea not null",
            "management_csr_public_key_sha256 bytea not null",
            "management_activated_at",
            "management_activated_by_session_id",
            "completed_at",
            "completed_by_session_id",
            "agent_certificate_rotations_pending_node_uidx",
            "interval '90 days'",
        ] {
            assert!(migration.contains(required), "0014 must contain {required}");
        }
        assert_eq!(migration.matches("interval '90 days'").count(), 2);
        assert!(!migration.contains("interval '91 days'"));
    }

    fn enrollment_with_ttl(ttl: Duration) -> NewAgentEnrollment {
        let created_at = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        NewAgentEnrollment {
            id: Uuid::now_v7(),
            node_id: Uuid::now_v7(),
            token_hash: [7; 32],
            created_by: "admin".to_string(),
            created_at,
            expires_at: created_at + ttl,
            remote_ip: None,
            user_agent: None,
        }
    }

    #[test]
    fn enrollment_repository_accepts_only_positive_ttl_up_to_ten_minutes() {
        validate_new_agent_enrollment(&enrollment_with_ttl(Duration::seconds(1)))
            .expect("positive TTL is valid");
        validate_new_agent_enrollment(&enrollment_with_ttl(Duration::minutes(10)))
            .expect("ten-minute TTL is valid");
        assert!(validate_new_agent_enrollment(&enrollment_with_ttl(Duration::zero())).is_err());
        assert!(
            validate_new_agent_enrollment(&enrollment_with_ttl(
                Duration::minutes(10) + Duration::seconds(1)
            ))
            .is_err()
        );
    }

    #[test]
    fn issued_certificate_repository_contract_caps_the_complete_leaf_lifetime_at_ninety_days() {
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let mut certificate = issued_certificate(unique_sha256(), now);
        certificate.not_after = certificate.not_before + Duration::days(90);
        validate_issued_enrollment_certificate(&certificate, now)
            .expect("an exact ninety-day leaf is valid");

        certificate.not_after += Duration::seconds(1);
        let error = validate_issued_enrollment_certificate(&certificate, now)
            .expect_err("ninety days plus one second must fail closed");
        assert!(error.to_string().contains("validity"));
    }

    #[test]
    fn control_session_claim_requires_exact_thirty_second_lease_and_global_ids() {
        let connected_at = Utc.with_ymd_and_hms(2026, 7, 12, 1, 2, 3).unwrap();
        let node_id = Uuid::now_v7();
        let claim = |lease: Duration| AgentControlSessionClaim {
            registration: sample_registration_for_claim(node_id),
            session_id: Uuid::now_v7(),
            core_instance_id: Uuid::now_v7(),
            certificate_fingerprint_sha256: [9; 32],
            peer_ip: "192.0.2.30".parse().unwrap(),
            connected_at,
            lease_expires_at: connected_at + lease,
        };

        validate_agent_control_session_claim(&claim(Duration::seconds(30)))
            .expect("exact policy lease is valid");
        assert!(validate_agent_control_session_claim(&claim(Duration::seconds(29))).is_err());
        assert!(validate_agent_control_session_claim(&claim(Duration::seconds(31))).is_err());
    }

    fn sample_registration_for_claim(node_id: Uuid) -> AgentRegistration {
        AgentRegistration {
            node_id,
            node_name: "agent-test".to_string(),
            agent_version: "test".to_string(),
            hostname: "agent-test".to_string(),
            labels: Vec::new(),
            interfaces: Vec::new(),
            zlm_api_base: String::new(),
            zlm_api_secret: String::new(),
            agent_stream_addr: "http://127.0.0.1".to_string(),
            agent_http_base_url: "http://127.0.0.1:8081".to_string(),
            zlm_rtmp_port: 1935,
            zlm_rtsp_port: 554,
            network_mode: media_domain::NetworkMode::Host,
            ffmpeg_bin: "ffmpeg".to_string(),
            ffprobe_bin: "ffprobe".to_string(),
            zlm_server_id: "agent-test".to_string(),
            output_mount_relative_prefix_mp4: String::new(),
            output_mount_relative_prefix_hls: String::new(),
        }
    }

    async fn seed_agent_certificate(
        pool: &sqlx::PgPool,
        node_id: Uuid,
        state: &'static str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<(Uuid, [u8; 32])> {
        sqlx::query(
            r#"
            insert into agent_identities (node_id, status, created_at, updated_at)
            values ($1, 'active', $2, $2)
            on conflict (node_id) do nothing
            "#,
        )
        .bind(node_id)
        .bind(now)
        .execute(pool)
        .await?;
        let certificate_id = Uuid::now_v7();
        let fingerprint = unique_sha256();
        let public_key = unique_sha256();
        sqlx::query(
            r#"
            insert into agent_certificates (
              id, node_id, serial_number, fingerprint_sha256, public_key_sha256,
              certificate_pem, state, not_before, not_after, issued_at, activated_at, issued_via
            ) values (
              $1, $2, $3, $4, $5,
              'test-certificate-pem', $6, $7, $8, $9,
              case when $6 = 'active' then $9 else null end,
              case when $6 = 'pending_rotation' then 'rotation' else 'enrollment' end
            )
            "#,
        )
        .bind(certificate_id)
        .bind(node_id)
        .bind(Uuid::now_v7().simple().to_string())
        .bind(fingerprint.as_slice())
        .bind(public_key.as_slice())
        .bind(state)
        .bind(now - Duration::minutes(5))
        .bind(now + Duration::days(90) - Duration::minutes(5))
        .bind(now)
        .execute(pool)
        .await?;
        Ok((certificate_id, fingerprint))
    }

    fn session_claim(
        node_id: Uuid,
        fingerprint: [u8; 32],
        session_id: Uuid,
        core_instance_id: Uuid,
        connected_at: DateTime<Utc>,
    ) -> AgentControlSessionClaim {
        AgentControlSessionClaim {
            registration: sample_registration_for_claim(node_id),
            session_id,
            core_instance_id,
            certificate_fingerprint_sha256: fingerprint,
            peer_ip: "192.0.2.30".parse().unwrap(),
            connected_at,
            lease_expires_at: connected_at + Duration::seconds(30),
        }
    }

    fn sample_heartbeat(now: DateTime<Utc>) -> media_domain::HeartbeatSnapshot {
        media_domain::HeartbeatSnapshot {
            node_time: now,
            cpu_percent: 1.0,
            mem_percent: 2.0,
            disk_percent: 3.0,
            upload_disk_total_bytes: 100,
            upload_disk_available_bytes: 90,
            upload_disk_used_percent: 10.0,
            running_tasks: 0,
            starting_tasks: 0,
            stopping_tasks: 0,
            orphaned_tasks: 0,
            runtime_slot_loads: Vec::new(),
            zlm_alive: true,
            ffmpeg_alive: true,
            artifact_cleanup_blocked: false,
            artifact_cleanup_block_reason: None,
            gpu_runtime: Vec::new(),
        }
    }

    async fn authorize_rotation(
        pool: &sqlx::PgPool,
        node_id: Uuid,
        old_certificate_id: Uuid,
        new_certificate_id: Uuid,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Uuid> {
        let rotation_id = Uuid::now_v7();
        let old_management_id = Uuid::now_v7();
        let new_management_id = Uuid::now_v7();
        for (id, state) in [
            (old_management_id, "active"),
            (new_management_id, "pending_rotation"),
        ] {
            sqlx::query(
                r#"
                insert into agent_management_certificates (
                  id, node_id, serial_number, fingerprint_sha256, public_key_sha256,
                  certificate_pem, state, not_before, not_after, issued_at, activated_at,
                  issued_via
                ) values (
                  $1, $2, $3, $4, $5, 'test-management-certificate-pem', $6,
                  $7, $8, $9, case when $6 = 'active' then $9 else null end,
                  case when $6 = 'active' then 'enrollment' else 'rotation' end
                )
                "#,
            )
            .bind(id)
            .bind(node_id)
            .bind(Uuid::now_v7().simple().to_string())
            .bind(unique_sha256().as_slice())
            .bind(unique_sha256().as_slice())
            .bind(state)
            .bind(now - Duration::minutes(5))
            .bind(now + Duration::days(90) - Duration::minutes(5))
            .bind(now)
            .execute(pool)
            .await?;
        }
        sqlx::query(
            r#"
            insert into agent_certificate_rotations (
              id, node_id, old_certificate_id, new_certificate_id,
              old_management_certificate_id, new_management_certificate_id,
              control_csr_public_key_sha256, management_csr_public_key_sha256,
              state, authorized_at, authorized_until
            ) values ($1, $2, $3, $4, $5, $6, $7, $8, 'pending', $9, $10)
            "#,
        )
        .bind(rotation_id)
        .bind(node_id)
        .bind(old_certificate_id)
        .bind(new_certificate_id)
        .bind(old_management_id)
        .bind(new_management_id)
        .bind(unique_sha256().as_slice())
        .bind(unique_sha256().as_slice())
        .bind(now)
        .bind(now + Duration::minutes(5))
        .execute(pool)
        .await?;
        Ok(rotation_id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn twenty_concurrent_session_claims_have_one_winner() -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(database.pool.clone()));
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let (_certificate_id, fingerprint) =
            seed_agent_certificate(&database.pool, node_id, "active", now).await?;

        let mut claimers = Vec::new();
        for _ in 0..20 {
            let repository = repository.clone();
            let session_id = Uuid::now_v7();
            claimers.push(tokio::spawn(async move {
                let outcome = repository
                    .claim_agent_control_session(session_claim(
                        node_id,
                        fingerprint,
                        session_id,
                        Uuid::now_v7(),
                        now,
                    ))
                    .await?;
                Ok::<_, RepoError>((session_id, outcome))
            }));
        }

        let mut winner = None;
        let mut duplicate_count = 0;
        for claimer in claimers {
            let (session_id, outcome) = claimer.await??;
            match outcome {
                AgentControlSessionClaimOutcome::Claimed { .. } => {
                    assert!(winner.replace(session_id).is_none());
                }
                AgentControlSessionClaimOutcome::DuplicateHealthy { .. } => {
                    duplicate_count += 1;
                }
                AgentControlSessionClaimOutcome::UnauthorizedCertificate(failure) => {
                    panic!("active certificate was unexpectedly rejected: {failure:?}");
                }
            }
        }
        assert_eq!(duplicate_count, 19);
        let winner = winner.expect("one session claim must win");
        assert!(
            repository
                .agent_control_session_is_current(node_id, winner, now + Duration::seconds(1))
                .await?
        );
        let session_count: i64 =
            sqlx::query_scalar("select count(*) from agent_control_sessions where node_id = $1")
                .bind(node_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(session_count, 1);
        let connected_audits: i64 = sqlx::query_scalar(
            "select count(*) from security_audit_events where event_type = 'agent_session_connected' and subject = $1",
        )
        .bind(node_id.to_string())
        .fetch_one(&database.pool)
        .await?;
        let duplicate_audits: i64 = sqlx::query_scalar(
            "select count(*) from security_audit_events where event_type = 'agent_session_rejected_duplicate' and subject = $1",
        )
        .bind(node_id.to_string())
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(connected_audits, 1);
        assert_eq!(duplicate_audits, 19);

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn lease_boundary_and_immediate_release_are_exactly_fenced() -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let (_certificate_id, fingerprint) =
            seed_agent_certificate(&database.pool, node_id, "active", now).await?;
        let first_session_id = Uuid::now_v7();
        assert!(matches!(
            repository
                .claim_agent_control_session(session_claim(
                    node_id,
                    fingerprint,
                    first_session_id,
                    Uuid::now_v7(),
                    now,
                ))
                .await?,
            AgentControlSessionClaimOutcome::Claimed { .. }
        ));
        assert!(matches!(
            repository
                .claim_agent_control_session(session_claim(
                    node_id,
                    fingerprint,
                    Uuid::now_v7(),
                    Uuid::now_v7(),
                    now + Duration::seconds(29),
                ))
                .await?,
            AgentControlSessionClaimOutcome::DuplicateHealthy { .. }
        ));

        sqlx::query(
            "update agent_control_sessions set connected_at = clock_timestamp() - interval '31 seconds', last_activity_at = clock_timestamp() - interval '31 seconds', lease_expires_at = clock_timestamp() - interval '1 second' where node_id = $1",
        )
        .bind(node_id)
        .execute(&database.pool)
        .await?;
        let replacement_session_id = Uuid::now_v7();
        assert!(matches!(
            repository
                .claim_agent_control_session(session_claim(
                    node_id,
                    fingerprint,
                    replacement_session_id,
                    Uuid::now_v7(),
                    now + Duration::seconds(30),
                ))
                .await?,
            AgentControlSessionClaimOutcome::Claimed {
                takeover_reason: Some(AgentSessionTakeoverReason::StaleTimeout),
                ..
            }
        ));
        assert!(
            !repository
                .release_agent_control_session(
                    node_id,
                    first_session_id,
                    now + Duration::seconds(30),
                )
                .await?
        );
        assert!(
            repository
                .release_agent_control_session(
                    node_id,
                    replacement_session_id,
                    now + Duration::seconds(30),
                )
                .await?,
            "release at connected_at must not violate the lease constraint"
        );

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn management_fence_linearizes_before_stale_session_takeover() -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let (_certificate_id, fingerprint) =
            seed_agent_certificate(&database.pool, node_id, "active", now).await?;
        let first_session_id = Uuid::now_v7();
        assert!(matches!(
            repository
                .claim_agent_control_session(session_claim(
                    node_id,
                    fingerprint,
                    first_session_id,
                    Uuid::now_v7(),
                    now,
                ))
                .await?,
            AgentControlSessionClaimOutcome::Claimed { .. }
        ));

        sqlx::query(
            "update agent_control_sessions set lease_expires_at = clock_timestamp() + interval '1 second' where node_id = $1 and session_id = $2",
        )
        .bind(node_id)
        .bind(first_session_id)
        .execute(&database.pool)
        .await?;
        let fence = repository
            .begin_agent_control_session_fence(
                node_id,
                first_session_id,
                now + Duration::seconds(1),
            )
            .await?
            .expect("current session must acquire a management fence");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let expired: bool = sqlx::query_scalar(
                    "select clock_timestamp() >= lease_expires_at from agent_control_sessions where node_id = $1 and session_id = $2",
                )
                .bind(node_id)
                .bind(first_session_id)
                .fetch_one(&database.pool)
                .await?;
                if expired {
                    break Ok::<(), RepoError>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        let replacement_session_id = Uuid::now_v7();
        let takeover_repository = repository.clone();
        let takeover = tokio::spawn(async move {
            takeover_repository
                .claim_agent_control_session(session_claim(
                    node_id,
                    fingerprint,
                    replacement_session_id,
                    Uuid::now_v7(),
                    now + Duration::days(1),
                ))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let waiting_on_lock: bool = sqlx::query_scalar(
                    r#"
                    select exists (
                      select 1
                        from pg_stat_activity
                       where datname = $1
                         and wait_event_type = 'Lock'
                         and query like '%from agent_control_sessions s%'
                         and query like '%for update of s, c%'
                    )
                    "#,
                )
                .bind(&database.database_name)
                .fetch_one(&database.admin_pool)
                .await?;
                if waiting_on_lock {
                    break Ok::<(), RepoError>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;

        fence.commit().await?;
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), takeover).await???,
            AgentControlSessionClaimOutcome::Claimed {
                takeover_reason: Some(AgentSessionTakeoverReason::StaleTimeout),
                ..
            }
        ));
        assert!(
            repository
                .begin_agent_control_session_fence(
                    node_id,
                    first_session_id,
                    now - Duration::days(1),
                )
                .await?
                .is_none(),
            "the replaced session must never reacquire a management fence"
        );

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_heartbeats_renew_without_lock_upgrade_deadlock() -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(database.pool.clone()));
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let (_certificate_id, fingerprint) =
            seed_agent_certificate(&database.pool, node_id, "active", now).await?;
        let session_id = Uuid::now_v7();
        assert!(matches!(
            repository
                .claim_agent_control_session(session_claim(
                    node_id,
                    fingerprint,
                    session_id,
                    Uuid::now_v7(),
                    now,
                ))
                .await?,
            AgentControlSessionClaimOutcome::Claimed { .. }
        ));
        assert!(
            repository
                .touch_agent_control_session(node_id, session_id, now)
                .await?
        );

        let mut heartbeats = Vec::new();
        for _ in 0..20 {
            let repository = repository.clone();
            let heartbeat = sample_heartbeat(Utc::now());
            heartbeats.push(tokio::spawn(async move {
                repository
                    .record_node_heartbeat_for_session(node_id, session_id, &heartbeat)
                    .await
            }));
        }
        for heartbeat in heartbeats {
            assert_eq!(heartbeat.await??, AgentSessionWriteOutcome::Applied);
        }
        let heartbeat_count: i64 =
            sqlx::query_scalar("select count(*) from node_heartbeats where node_id = $1")
                .bind(node_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(heartbeat_count, 20);

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn authorized_rotation_is_one_time_and_replaces_the_old_certificate() -> anyhow::Result<()>
    {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let (old_certificate_id, old_fingerprint) =
            seed_agent_certificate(&database.pool, node_id, "active", now).await?;
        let (new_certificate_id, new_fingerprint) =
            seed_agent_certificate(&database.pool, node_id, "pending_rotation", now).await?;
        let old_session_id = Uuid::now_v7();
        assert!(matches!(
            repository
                .claim_agent_control_session(session_claim(
                    node_id,
                    old_fingerprint,
                    old_session_id,
                    Uuid::now_v7(),
                    now,
                ))
                .await?,
            AgentControlSessionClaimOutcome::Claimed { .. }
        ));
        let rotation_id = authorize_rotation(
            &database.pool,
            node_id,
            old_certificate_id,
            new_certificate_id,
            now,
        )
        .await?;

        let new_session_id = Uuid::now_v7();
        assert!(matches!(
            repository
                .claim_agent_control_session(session_claim(
                    node_id,
                    new_fingerprint,
                    new_session_id,
                    Uuid::now_v7(),
                    now + Duration::seconds(1),
                ))
                .await?,
            AgentControlSessionClaimOutcome::Claimed {
                replaced_session_id: Some(replaced),
                takeover_reason: Some(AgentSessionTakeoverReason::CertificateRotation),
                ..
            } if replaced == old_session_id
        ));

        let states = sqlx::query(
            r#"
            select
              (select state from agent_certificates where id = $1) as old_state,
              (select state from agent_certificates where id = $2) as new_state,
              (select consumed_by_session_id from agent_certificate_rotations where id = $3)
                as consumed_by_session_id
            "#,
        )
        .bind(old_certificate_id)
        .bind(new_certificate_id)
        .bind(rotation_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(states.try_get::<String, _>("old_state")?, "replaced");
        assert_eq!(states.try_get::<String, _>("new_state")?, "active");
        assert_eq!(
            states.try_get::<Option<Uuid>, _>("consumed_by_session_id")?,
            Some(new_session_id)
        );
        assert!(matches!(
            repository
                .claim_agent_control_session(session_claim(
                    node_id,
                    old_fingerprint,
                    Uuid::now_v7(),
                    Uuid::now_v7(),
                    now + Duration::seconds(2),
                ))
                .await?,
            AgentControlSessionClaimOutcome::UnauthorizedCertificate(
                AgentCertificateAuthorizationFailure::Replaced
            )
        ));
        let audit_payload: serde_json::Value = sqlx::query_scalar(
            r#"
            select payload from security_audit_events
             where event_type = 'agent_session_takeover' and subject = $1
             order by created_at desc limit 1
            "#,
        )
        .bind(node_id.to_string())
        .fetch_one(&database.pool)
        .await?;
        for key in [
            "old_fingerprint_sha256",
            "new_fingerprint_sha256",
            "old_session_id",
            "new_session_id",
            "old_peer_ip",
            "new_peer_ip",
            "reason",
            "rotation_id",
        ] {
            assert!(audit_payload.get(key).is_some(), "missing audit key {key}");
        }
        let serialized_audit = audit_payload.to_string();
        assert!(!serialized_audit.contains("PRIVATE KEY"));
        assert!(!serialized_audit.contains("CERTIFICATE"));

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn rotation_failure_rolls_back_every_certificate_and_session_change() -> anyhow::Result<()>
    {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();

        let other_node_id = Uuid::now_v7();
        let (_other_certificate_id, other_fingerprint) =
            seed_agent_certificate(&database.pool, other_node_id, "active", now).await?;
        let globally_conflicting_session_id = Uuid::now_v7();
        repository
            .claim_agent_control_session(session_claim(
                other_node_id,
                other_fingerprint,
                globally_conflicting_session_id,
                Uuid::now_v7(),
                now,
            ))
            .await?;

        let node_id = Uuid::now_v7();
        let (old_certificate_id, old_fingerprint) =
            seed_agent_certificate(&database.pool, node_id, "active", now).await?;
        let (new_certificate_id, new_fingerprint) =
            seed_agent_certificate(&database.pool, node_id, "pending_rotation", now).await?;
        let old_session_id = Uuid::now_v7();
        repository
            .claim_agent_control_session(session_claim(
                node_id,
                old_fingerprint,
                old_session_id,
                Uuid::now_v7(),
                now,
            ))
            .await?;
        let rotation_id = authorize_rotation(
            &database.pool,
            node_id,
            old_certificate_id,
            new_certificate_id,
            now,
        )
        .await?;

        let error = repository
            .claim_agent_control_session(session_claim(
                node_id,
                new_fingerprint,
                globally_conflicting_session_id,
                Uuid::now_v7(),
                now + Duration::seconds(1),
            ))
            .await
            .expect_err("global session UUID conflict must fail the transaction");
        assert!(matches!(error, RepoError::Sqlx(_)));

        let certificate_states = sqlx::query(
            r#"
            select
              (select state from agent_certificates where id = $1) as old_state,
              (select revoked_at from agent_certificates where id = $1) as old_revoked_at,
              (select state from agent_certificates where id = $2) as new_state,
              (select activated_at from agent_certificates where id = $2) as new_activated_at,
              (select consumed_at from agent_certificate_rotations where id = $3) as consumed_at,
              (select consumed_by_session_id from agent_certificate_rotations where id = $3)
                as consumed_by_session_id,
              (select session_id from agent_control_sessions where node_id = $4) as session_id
            "#,
        )
        .bind(old_certificate_id)
        .bind(new_certificate_id)
        .bind(rotation_id)
        .bind(node_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            certificate_states.try_get::<String, _>("old_state")?,
            "active"
        );
        assert_eq!(
            certificate_states.try_get::<Option<DateTime<Utc>>, _>("old_revoked_at")?,
            None
        );
        assert_eq!(
            certificate_states.try_get::<String, _>("new_state")?,
            "pending_rotation"
        );
        assert_eq!(
            certificate_states.try_get::<Option<DateTime<Utc>>, _>("new_activated_at")?,
            None
        );
        assert_eq!(
            certificate_states.try_get::<Option<DateTime<Utc>>, _>("consumed_at")?,
            None
        );
        assert_eq!(
            certificate_states.try_get::<Option<Uuid>, _>("consumed_by_session_id")?,
            None
        );
        assert_eq!(
            certificate_states.try_get::<Uuid, _>("session_id")?,
            old_session_id
        );

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn old_core_close_cannot_mark_a_taken_over_session_offline() -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let (_certificate_id, fingerprint) =
            seed_agent_certificate(&database.pool, node_id, "active", now).await?;
        let old_session_id = Uuid::now_v7();
        repository
            .claim_agent_control_session(session_claim(
                node_id,
                fingerprint,
                old_session_id,
                Uuid::now_v7(),
                now,
            ))
            .await?;
        sqlx::query(
            "update agent_control_sessions set connected_at = clock_timestamp() - interval '31 seconds', last_activity_at = clock_timestamp() - interval '31 seconds', lease_expires_at = clock_timestamp() - interval '1 second' where node_id = $1",
        )
        .bind(node_id)
        .execute(&database.pool)
        .await?;
        let new_session_id = Uuid::now_v7();
        assert!(matches!(
            repository
                .claim_agent_control_session(session_claim(
                    node_id,
                    fingerprint,
                    new_session_id,
                    Uuid::now_v7(),
                    now + Duration::seconds(30),
                ))
                .await?,
            AgentControlSessionClaimOutcome::Claimed {
                takeover_reason: Some(AgentSessionTakeoverReason::StaleTimeout),
                ..
            }
        ));

        assert!(
            !repository
                .close_agent_control_session_and_reclaim(
                    node_id,
                    old_session_id,
                    now + Duration::seconds(31),
                )
                .await?
        );
        let node = sqlx::query(
            r#"
            select n.healthy, n.control_connected, s.session_id, s.disconnected_at
              from media_nodes n
              join agent_control_sessions s on s.node_id = n.id
             where n.id = $1
            "#,
        )
        .bind(node_id)
        .fetch_one(&database.pool)
        .await?;
        assert!(node.try_get::<bool, _>("healthy")?);
        assert!(node.try_get::<bool, _>("control_connected")?);
        assert_eq!(node.try_get::<Uuid, _>("session_id")?, new_session_id);
        assert_eq!(
            node.try_get::<Option<DateTime<Utc>>, _>("disconnected_at")?,
            None
        );

        database.cleanup().await?;
        Ok(())
    }

    fn unique_sha256() -> [u8; 32] {
        let mut value = [0_u8; 32];
        value[..16].copy_from_slice(Uuid::now_v7().as_bytes());
        value[16..].copy_from_slice(Uuid::now_v7().as_bytes());
        value
    }

    fn enrollment_request(node_id: Uuid, now: DateTime<Utc>) -> AgentEnrollmentRequest {
        let control_csr_public_key_sha256 = unique_sha256();
        let mut management_csr_public_key_sha256 = unique_sha256();
        if management_csr_public_key_sha256 == control_csr_public_key_sha256 {
            management_csr_public_key_sha256[0] ^= 1;
        }
        AgentEnrollmentRequest {
            node_id,
            control_csr_public_key_sha256,
            management_csr_public_key_sha256,
            attempted_at: now,
            remote_ip: Some("192.0.2.10".parse().unwrap()),
            user_agent: Some("repository-test".to_string()),
        }
    }

    fn issued_certificate(
        public_key_sha256: [u8; 32],
        now: DateTime<Utc>,
    ) -> NewIssuedAgentCertificate {
        let mut fingerprint_sha256 = [0_u8; 32];
        fingerprint_sha256[..16].copy_from_slice(Uuid::now_v7().as_bytes());
        fingerprint_sha256[16..].copy_from_slice(Uuid::now_v7().as_bytes());
        NewIssuedAgentCertificate {
            id: Uuid::now_v7(),
            serial_number: Uuid::now_v7().simple().to_string(),
            fingerprint_sha256,
            public_key_sha256,
            certificate_pem: format!(
                "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
                Uuid::now_v7().simple()
            ),
            not_before: now - Duration::minutes(5),
            not_after: now + Duration::days(90) - Duration::minutes(5),
        }
    }

    fn enrollment_bundle(request: &AgentEnrollmentRequest) -> AgentEnrollmentBundle {
        AgentEnrollmentBundle {
            node_id: request.node_id,
            control_certificate: issued_certificate(
                request.control_csr_public_key_sha256,
                request.attempted_at,
            ),
            management_certificate: issued_certificate(
                request.management_csr_public_key_sha256,
                request.attempted_at,
            ),
            agent_client_issuer_ca_pem: "agent-issuer-ca-pem".to_string(),
            control_plane_server_ca_pem: "control-plane-server-ca-pem".to_string(),
            management_client_ca_pem: "management-client-ca-pem".to_string(),
            capability_jwt_public_key_pem: "capability-public-key-pem".to_string(),
            capability_jwt_kid: "capability-kid".to_string(),
        }
    }

    fn wrong_node_request(node_id: Uuid, now: DateTime<Utc>) -> AgentEnrollmentRequest {
        enrollment_request(node_id, now)
    }

    async fn wait_for_enrollment_identity_lock_waiters(
        pool: &sqlx::PgPool,
        expected_waiters: i64,
    ) -> anyhow::Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let waiter_count: i64 = sqlx::query_scalar(
                    r#"
                    select count(*)
                      from pg_stat_activity
                     where datname = current_database()
                       and wait_event_type = 'Lock'
                       and query like '%select status from agent_identities where node_id = $1 for update%'
                    "#,
                )
                .fetch_one(pool)
                .await?;
                if waiter_count >= expected_waiters {
                    return Ok::<(), sqlx::Error>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out waiting for {expected_waiters} enrollment identity lock waiters"
            )
        })??;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_enrollment_replacement_and_old_token_consume_do_not_deadlock()
    -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(database.pool.clone()));
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let old_enrollment_id = Uuid::now_v7();
        let old_token_hash = unique_sha256();
        assert!(matches!(
            repository
                .create_agent_enrollment(NewAgentEnrollment {
                    id: old_enrollment_id,
                    node_id,
                    token_hash: old_token_hash,
                    created_by: "admin".to_string(),
                    created_at: now,
                    expires_at: now + Duration::minutes(10),
                    remote_ip: None,
                    user_agent: Some("repository-deadlock-test".to_string()),
                })
                .await?,
            CreateAgentEnrollmentOutcome::Created { .. }
        ));

        let mut identity_blocker = database.pool.begin().await?;
        sqlx::query("select node_id from agent_identities where node_id = $1 for update")
            .bind(node_id)
            .fetch_one(&mut *identity_blocker)
            .await?;

        let replacement_enrollment_id = Uuid::now_v7();
        let replacement_token_hash = unique_sha256();
        let create_repository = repository.clone();
        let mut create_task = tokio::spawn(async move {
            create_repository
                .create_agent_enrollment(NewAgentEnrollment {
                    id: replacement_enrollment_id,
                    node_id,
                    token_hash: replacement_token_hash,
                    created_by: "admin".to_string(),
                    created_at: now + Duration::seconds(1),
                    expires_at: now + Duration::minutes(10),
                    remote_ip: None,
                    user_agent: Some("repository-deadlock-test".to_string()),
                })
                .await
        });
        wait_for_enrollment_identity_lock_waiters(&database.pool, 1).await?;

        let consume_repository = repository.clone();
        let consume_request = enrollment_request(node_id, now + Duration::seconds(2));
        let issued_request = consume_request.clone();
        let mut consume_task = tokio::spawn(async move {
            consume_repository
                .consume_agent_enrollment(&old_token_hash, consume_request, |_| {
                    Ok::<_, RepoError>(enrollment_bundle(&issued_request))
                })
                .await
        });
        wait_for_enrollment_identity_lock_waiters(&database.pool, 2).await?;
        identity_blocker.commit().await?;

        let joined = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(&mut create_task, &mut consume_task)
        })
        .await;
        let (create_join, consume_join) = match joined {
            Ok(joined) => joined,
            Err(_) => {
                create_task.abort();
                consume_task.abort();
                let _ = create_task.await;
                let _ = consume_task.await;
                database.cleanup().await?;
                anyhow::bail!(
                    "enrollment replacement/consume race did not finish within 10 seconds"
                );
            }
        };
        let create_result = create_join?;
        let consume_result = consume_join?;

        let old_token_state = sqlx::query(
            "select consumed_at, revoked_at from agent_enrollment_tokens where id = $1",
        )
        .bind(old_enrollment_id)
        .fetch_one(&database.pool)
        .await?;
        let replacement_token_state = sqlx::query(
            "select consumed_at, revoked_at from agent_enrollment_tokens where id = $1",
        )
        .bind(replacement_enrollment_id)
        .fetch_optional(&database.pool)
        .await?;
        let identity_status: String =
            sqlx::query_scalar("select status from agent_identities where node_id = $1")
                .bind(node_id)
                .fetch_one(&database.pool)
                .await?;
        database.cleanup().await?;

        let create_outcome = create_result
            .map_err(|error| anyhow::anyhow!("replacement enrollment failed: {error}"))?;
        let consume_outcome = consume_result
            .map_err(|error| anyhow::anyhow!("old enrollment consume failed: {error}"))?;
        assert!(matches!(
            create_outcome,
            CreateAgentEnrollmentOutcome::Created { .. }
        ));
        assert_eq!(consume_outcome, ConsumeAgentEnrollmentOutcome::Invalid);
        assert_eq!(identity_status, "pending_enrollment");
        assert_eq!(
            old_token_state.try_get::<Option<DateTime<Utc>>, _>("consumed_at")?,
            None
        );
        assert!(
            old_token_state
                .try_get::<Option<DateTime<Utc>>, _>("revoked_at")?
                .is_some()
        );
        let replacement_token_state =
            replacement_token_state.expect("replacement token must be persisted");
        assert_eq!(
            replacement_token_state.try_get::<Option<DateTime<Utc>>, _>("consumed_at")?,
            None
        );
        assert_eq!(
            replacement_token_state.try_get::<Option<DateTime<Utc>>, _>("revoked_at")?,
            None
        );
        Ok(())
    }

    #[tokio::test]
    async fn consumed_enrollment_cannot_be_recovered_after_token_expiry() -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let token_hash = unique_sha256();
        repository
            .create_agent_enrollment(NewAgentEnrollment {
                id: Uuid::now_v7(),
                node_id,
                token_hash,
                created_by: "admin".to_string(),
                created_at: now,
                expires_at: now + Duration::minutes(10),
                remote_ip: None,
                user_agent: Some("repository-expired-recovery-test".to_string()),
            })
            .await?;
        let request = enrollment_request(node_id, now + Duration::seconds(1));
        let issued_request = request.clone();
        assert!(matches!(
            repository
                .consume_agent_enrollment(&token_hash, request.clone(), |_| {
                    Ok::<_, RepoError>(enrollment_bundle(&issued_request))
                })
                .await?,
            ConsumeAgentEnrollmentOutcome::Issued(_)
        ));

        sqlx::query(
            r#"
            with decision as materialized (select clock_timestamp() as now)
            update agent_enrollment_tokens
               set created_at = decision.now - interval '9 minutes',
                   expires_at = decision.now - interval '1 second'
              from decision
             where token_hash = $1
            "#,
        )
        .bind(token_hash.as_slice())
        .execute(&database.pool)
        .await?;
        let outcome = repository
            .consume_agent_enrollment(
                &token_hash,
                request,
                |_| -> Result<AgentEnrollmentBundle, RepoError> {
                    panic!("expired recovery must not invoke the issuer")
                },
            )
            .await?;
        database.cleanup().await?;

        assert_eq!(outcome, ConsumeAgentEnrollmentOutcome::Invalid);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn twenty_concurrent_enrollment_consumers_have_exactly_one_winner() -> anyhow::Result<()>
    {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(database.pool.clone()));
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let token_hash = [42_u8; 32];
        assert!(matches!(
            repository
                .create_agent_enrollment(NewAgentEnrollment {
                    id: Uuid::now_v7(),
                    node_id,
                    token_hash,
                    created_by: "admin".to_string(),
                    created_at: now,
                    expires_at: now + Duration::minutes(10),
                    remote_ip: Some("192.0.2.1".parse().unwrap()),
                    user_agent: Some("repository-test".to_string()),
                })
                .await?,
            CreateAgentEnrollmentOutcome::Created { .. }
        ));

        let request = enrollment_request(node_id, now + Duration::seconds(1));
        let issue_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut consumers = Vec::new();
        for _ in 0..20 {
            let repository = repository.clone();
            let request = request.clone();
            let issue_count = issue_count.clone();
            consumers.push(tokio::spawn(async move {
                repository
                    .consume_agent_enrollment(&token_hash, request.clone(), |_| {
                        issue_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok::<_, RepoError>(enrollment_bundle(&request))
                    })
                    .await
            }));
        }
        let mut issued = 0;
        let mut recovered = 0;
        let mut recovered_bundle = None;
        for consumer in consumers {
            match consumer.await?? {
                ConsumeAgentEnrollmentOutcome::Issued(bundle) => {
                    issued += 1;
                    if let Some(expected) = &recovered_bundle {
                        assert_eq!(&bundle, expected);
                    } else {
                        recovered_bundle = Some(bundle);
                    }
                }
                ConsumeAgentEnrollmentOutcome::Recovered(bundle) => {
                    recovered += 1;
                    if let Some(expected) = &recovered_bundle {
                        assert_eq!(&bundle, expected);
                    } else {
                        recovered_bundle = Some(bundle);
                    }
                }
                ConsumeAgentEnrollmentOutcome::Invalid => panic!("identical retry must recover"),
            }
        }
        assert_eq!(issued, 1);
        assert_eq!(recovered, 19);
        assert_eq!(issue_count.load(std::sync::atomic::Ordering::SeqCst), 1);

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
        let token_state = sqlx::query(
            "select consumed_at, consumed_certificate_id from agent_enrollment_tokens where token_hash = $1",
        )
        .bind(token_hash.as_slice())
        .fetch_one(&database.pool)
        .await?;
        assert!(
            token_state
                .try_get::<Option<DateTime<Utc>>, _>("consumed_at")?
                .is_some()
        );
        assert!(
            token_state
                .try_get::<Option<Uuid>, _>("consumed_certificate_id")?
                .is_some()
        );
        let leaked_secret: bool = sqlx::query_scalar(
            r#"
            select exists (
              select 1 from security_audit_events
               where payload::text like '%2a2a2a2a%'
                  or actor like '%2a2a2a2a%'
                  or coalesce(subject, '') like '%2a2a2a2a%'
            )
            "#,
        )
        .fetch_one(&database.pool)
        .await?;
        assert!(!leaked_secret);

        let second_node_id = Uuid::now_v7();
        let second_token_hash = [43_u8; 32];
        assert!(matches!(
            repository
                .create_agent_enrollment(NewAgentEnrollment {
                    id: Uuid::now_v7(),
                    node_id: second_node_id,
                    token_hash: second_token_hash,
                    created_by: "admin".to_string(),
                    created_at: now,
                    expires_at: now + Duration::minutes(10),
                    remote_ip: None,
                    user_agent: None,
                })
                .await?,
            CreateAgentEnrollmentOutcome::Created { .. }
        ));
        assert_eq!(
            repository
                .consume_agent_enrollment(
                    &second_token_hash,
                    wrong_node_request(Uuid::now_v7(), now + Duration::seconds(1)),
                    |_| -> Result<AgentEnrollmentBundle, RepoError> {
                        panic!("wrong-node request must not invoke issuer")
                    },
                )
                .await?,
            ConsumeAgentEnrollmentOutcome::Invalid,
            "wrong node must not consume the token"
        );
        let second_unused: bool = sqlx::query_scalar(
            "select consumed_at is null from agent_enrollment_tokens where token_hash = $1",
        )
        .bind(second_token_hash.as_slice())
        .fetch_one(&database.pool)
        .await?;
        assert!(second_unused);
        let second_request = enrollment_request(second_node_id, now + Duration::seconds(2));
        let second_bundle = enrollment_bundle(&second_request);
        let issued_bundle = match repository
            .consume_agent_enrollment(&second_token_hash, second_request.clone(), |_| {
                Ok::<_, RepoError>(second_bundle.clone())
            })
            .await?
        {
            ConsumeAgentEnrollmentOutcome::Issued(bundle) => bundle,
            other => panic!("expected issued bundle, got {other:?}"),
        };
        let recovered_bundle = match repository
            .consume_agent_enrollment(
                &second_token_hash,
                second_request.clone(),
                |_| -> Result<AgentEnrollmentBundle, RepoError> {
                    panic!("same consumed request must not invoke issuer")
                },
            )
            .await?
        {
            ConsumeAgentEnrollmentOutcome::Recovered(bundle) => bundle,
            other => panic!("expected recovered bundle, got {other:?}"),
        };
        assert_eq!(recovered_bundle, issued_bundle);

        let mut changed_control = second_request.clone();
        changed_control.control_csr_public_key_sha256[0] ^= 1;
        let mut changed_management = second_request.clone();
        changed_management.management_csr_public_key_sha256[0] ^= 1;
        for changed in [
            changed_control,
            changed_management,
            wrong_node_request(Uuid::now_v7(), now + Duration::seconds(3)),
        ] {
            let outcome = repository
                .consume_agent_enrollment(
                    &second_token_hash,
                    changed,
                    |_| -> Result<AgentEnrollmentBundle, RepoError> {
                        panic!("changed consumed request must not invoke issuer")
                    },
                )
                .await?;
            assert_eq!(outcome, ConsumeAgentEnrollmentOutcome::Invalid);
        }
        assert_eq!(
            repository
                .create_agent_enrollment(NewAgentEnrollment {
                    id: Uuid::now_v7(),
                    node_id: second_node_id,
                    token_hash: [45_u8; 32],
                    created_by: "admin".to_string(),
                    created_at: now + Duration::seconds(4),
                    expires_at: now + Duration::minutes(10),
                    remote_ip: None,
                    user_agent: None,
                })
                .await?,
            CreateAgentEnrollmentOutcome::IdentityAlreadyActive,
            "active identities require a separate recovery workflow"
        );

        let expired_node_id = Uuid::now_v7();
        let expired_token_hash = [44_u8; 32];
        assert!(matches!(
            repository
                .create_agent_enrollment(NewAgentEnrollment {
                    id: Uuid::now_v7(),
                    node_id: expired_node_id,
                    token_hash: expired_token_hash,
                    created_by: "admin".to_string(),
                    created_at: now - Duration::minutes(11),
                    expires_at: now - Duration::minutes(1),
                    remote_ip: None,
                    user_agent: None,
                })
                .await?,
            CreateAgentEnrollmentOutcome::Created { .. }
        ));
        sqlx::query(
            "with decision as materialized (select clock_timestamp() as now) update agent_enrollment_tokens set created_at = decision.now - interval '11 minutes', expires_at = decision.now - interval '1 minute' from decision where token_hash = $1",
        )
        .bind(expired_token_hash.as_slice())
        .execute(&database.pool)
        .await?;
        assert_eq!(
            repository
                .consume_agent_enrollment(
                    &expired_token_hash,
                    enrollment_request(expired_node_id, now),
                    |_| -> Result<AgentEnrollmentBundle, RepoError> {
                        panic!("expired request must not invoke issuer")
                    },
                )
                .await?,
            ConsumeAgentEnrollmentOutcome::Invalid,
            "expired token must fail"
        );
        let expired_certificate_count: i64 =
            sqlx::query_scalar("select count(*) from agent_certificates where node_id = $1")
                .bind(expired_node_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(expired_certificate_count, 0);

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn enrollment_transaction_failure_rolls_back_and_allows_retry() -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();

        let first_node = Uuid::now_v7();
        let first_token = [81_u8; 32];
        repository
            .create_agent_enrollment(NewAgentEnrollment {
                id: Uuid::now_v7(),
                node_id: first_node,
                token_hash: first_token,
                created_by: "admin".to_string(),
                created_at: now,
                expires_at: now + Duration::minutes(10),
                remote_ip: None,
                user_agent: None,
            })
            .await?;
        let first_request = enrollment_request(first_node, now + Duration::seconds(1));
        let first_bundle = enrollment_bundle(&first_request);
        let first_persisted = first_bundle.clone();
        let outcome = repository
            .consume_agent_enrollment(&first_token, first_request, |_| {
                Ok::<_, RepoError>(first_bundle)
            })
            .await?;
        assert!(matches!(outcome, ConsumeAgentEnrollmentOutcome::Issued(_)));

        let retry_node = Uuid::now_v7();
        let retry_token = [82_u8; 32];
        repository
            .create_agent_enrollment(NewAgentEnrollment {
                id: Uuid::now_v7(),
                node_id: retry_node,
                token_hash: retry_token,
                created_by: "admin".to_string(),
                created_at: now,
                expires_at: now + Duration::minutes(10),
                remote_ip: None,
                user_agent: None,
            })
            .await?;
        let retry_request = enrollment_request(retry_node, now + Duration::seconds(2));
        let mut colliding_bundle = enrollment_bundle(&retry_request);
        colliding_bundle.management_certificate.serial_number =
            first_persisted.management_certificate.serial_number;
        let failure = repository
            .consume_agent_enrollment(&retry_token, retry_request.clone(), |_| {
                Ok::<_, RepoError>(colliding_bundle)
            })
            .await;
        assert!(
            failure.is_err(),
            "management insert collision must abort transaction"
        );

        let state = sqlx::query(
            r#"
            select consumed_at,
                   (select count(*) from agent_certificates where node_id = $2) as control_count,
                   (select count(*) from agent_management_certificates where node_id = $2) as management_count
              from agent_enrollment_tokens
             where token_hash = $1
            "#,
        )
        .bind(retry_token.as_slice())
        .bind(retry_node)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            state.try_get::<Option<DateTime<Utc>>, _>("consumed_at")?,
            None
        );
        assert_eq!(state.try_get::<i64, _>("control_count")?, 0);
        assert_eq!(state.try_get::<i64, _>("management_count")?, 0);

        let retry_bundle = enrollment_bundle(&retry_request);
        let outcome = repository
            .consume_agent_enrollment(&retry_token, retry_request, |_| {
                Ok::<_, RepoError>(retry_bundle)
            })
            .await?;
        assert!(matches!(outcome, ConsumeAgentEnrollmentOutcome::Issued(_)));

        database.cleanup().await?;
        Ok(())
    }

    async fn enroll_rotation_test_identity(
        repository: &TaskRepository,
        node_id: Uuid,
        now: DateTime<Utc>,
        remaining_validity: Duration,
    ) -> anyhow::Result<(AgentEnrollmentBundle, Uuid)> {
        let token_hash = unique_sha256();
        repository
            .create_agent_enrollment(NewAgentEnrollment {
                id: Uuid::now_v7(),
                node_id,
                token_hash,
                created_by: "rotation-test".to_string(),
                created_at: now,
                expires_at: now + Duration::minutes(10),
                remote_ip: Some("192.0.2.40".parse().unwrap()),
                user_agent: Some("rotation-test".to_string()),
            })
            .await?;
        let request = enrollment_request(node_id, now + Duration::seconds(1));
        let mut bundle = enrollment_bundle(&request);
        bundle.control_certificate.not_after = now + remaining_validity;
        bundle.management_certificate.not_after = now + remaining_validity;
        let expected = bundle.clone();
        let outcome = repository
            .consume_agent_enrollment(&token_hash, request, |_| Ok::<_, RepoError>(bundle))
            .await?;
        assert!(matches!(outcome, ConsumeAgentEnrollmentOutcome::Issued(_)));

        let session_id = Uuid::now_v7();
        let claimed = repository
            .claim_agent_control_session(session_claim(
                node_id,
                expected.control_certificate.fingerprint_sha256,
                session_id,
                Uuid::now_v7(),
                now + Duration::seconds(2),
            ))
            .await?;
        assert!(matches!(
            claimed,
            AgentControlSessionClaimOutcome::Claimed { .. }
        ));
        Ok((expected, session_id))
    }

    fn rotation_request(
        rotation_id: Uuid,
        node_id: Uuid,
        session_id: Uuid,
        now: DateTime<Utc>,
    ) -> AgentCertificateRotationRequest {
        let control_csr_public_key_sha256 = unique_sha256();
        let mut management_csr_public_key_sha256 = unique_sha256();
        if management_csr_public_key_sha256 == control_csr_public_key_sha256 {
            management_csr_public_key_sha256[0] ^= 1;
        }
        AgentCertificateRotationRequest {
            rotation_id,
            node_id,
            session_id,
            control_csr_public_key_sha256,
            management_csr_public_key_sha256,
            requested_at: now,
            remote_ip: "192.0.2.40".parse().unwrap(),
        }
    }

    fn rotation_issue(request: &AgentCertificateRotationRequest) -> AgentCertificateRotationIssue {
        AgentCertificateRotationIssue {
            control_certificate: issued_certificate(
                request.control_csr_public_key_sha256,
                request.requested_at,
            ),
            management_certificate: issued_certificate(
                request.management_csr_public_key_sha256,
                request.requested_at,
            ),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rotation_staging_is_due_fenced_exactly_once_and_idempotent() -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(database.pool.clone()));
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let (_active, session_id) =
            enroll_rotation_test_identity(&repository, node_id, now, Duration::days(30)).await?;
        let request = rotation_request(Uuid::now_v7(), node_id, session_id, now);
        let issue_count = Arc::new(AtomicUsize::new(0));
        let mut callers = Vec::new();
        for _ in 0..20 {
            let repository = repository.clone();
            let request = request.clone();
            let issue_count = issue_count.clone();
            callers.push(tokio::spawn(async move {
                repository
                    .stage_agent_certificate_rotation(request.clone(), |decision_at| {
                        issue_count.fetch_add(1, Ordering::SeqCst);
                        let mut issued_request = request.clone();
                        issued_request.requested_at = decision_at;
                        Ok::<_, RepoError>(rotation_issue(&issued_request))
                    })
                    .await
            }));
        }
        let mut issued = 0;
        let mut recovered = 0;
        let mut expected_bundle = None;
        for caller in callers {
            match caller.await?? {
                StageAgentCertificateRotationOutcome::Issued(bundle) => {
                    issued += 1;
                    expected_bundle.get_or_insert(bundle);
                }
                StageAgentCertificateRotationOutcome::Recovered(bundle) => {
                    recovered += 1;
                    if let Some(expected) = &expected_bundle {
                        assert_eq!(&bundle, expected);
                    } else {
                        expected_bundle = Some(bundle);
                    }
                }
                StageAgentCertificateRotationOutcome::Rejected => {
                    panic!("identical eligible requests must issue or recover")
                }
                StageAgentCertificateRotationOutcome::Expired => {
                    panic!("fresh identical requests must not be expired")
                }
            }
        }
        assert_eq!(issued, 1);
        assert_eq!(recovered, 19);
        assert_eq!(issue_count.load(Ordering::SeqCst), 1);

        let persisted = sqlx::query(
            r#"
            select state, old_certificate_id, new_certificate_id,
                   old_management_certificate_id, new_management_certificate_id,
                   control_csr_public_key_sha256, management_csr_public_key_sha256,
                    authorized_at, authorized_until
              from agent_certificate_rotations
             where id = $1
            "#,
        )
        .bind(request.rotation_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(persisted.try_get::<String, _>("state")?, "pending");
        assert_eq!(
            persisted.try_get::<Vec<u8>, _>("control_csr_public_key_sha256")?,
            request.control_csr_public_key_sha256
        );
        assert_eq!(
            persisted.try_get::<Vec<u8>, _>("management_csr_public_key_sha256")?,
            request.management_csr_public_key_sha256
        );
        assert_eq!(
            persisted.try_get::<DateTime<Utc>, _>("authorized_until")?
                - persisted.try_get::<DateTime<Utc>, _>("authorized_at")?,
            AGENT_CERTIFICATE_ROTATION_DEADLINE
        );

        let mut changed = request.clone();
        changed.control_csr_public_key_sha256[0] ^= 1;
        assert_eq!(
            repository
                .stage_agent_certificate_rotation(changed, |_| -> Result<_, RepoError> {
                    panic!("changed CSR replay must not invoke issuer")
                })
                .await?,
            StageAgentCertificateRotationOutcome::Rejected
        );
        let mut expired = request.clone();
        expired.requested_at += Duration::minutes(5);
        sqlx::query(
            "with decision as materialized (select clock_timestamp() as now) update agent_certificate_rotations set authorized_at = decision.now - interval '6 minutes', authorized_until = decision.now - interval '1 minute' from decision where id = $1",
        )
        .bind(request.rotation_id)
        .execute(&database.pool)
        .await?;
        assert_eq!(
            repository
                .stage_agent_certificate_rotation(expired, |_| -> Result<_, RepoError> {
                    panic!("expired replay must not invoke issuer")
                })
                .await?,
            StageAgentCertificateRotationOutcome::Expired
        );

        let rotation_rows: i64 = sqlx::query_scalar(
            "select count(*) from agent_certificate_rotations where node_id = $1",
        )
        .bind(node_id)
        .fetch_one(&database.pool)
        .await?;
        let control_rows: i64 =
            sqlx::query_scalar("select count(*) from agent_certificates where node_id = $1")
                .bind(node_id)
                .fetch_one(&database.pool)
                .await?;
        let management_rows: i64 = sqlx::query_scalar(
            "select count(*) from agent_management_certificates where node_id = $1",
        )
        .bind(node_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(rotation_rows, 1);
        assert_eq!(control_rows, 2);
        assert_eq!(management_rows, 2);

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn rotation_staging_rejects_not_due_or_fenced_sessions_without_signing()
    -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let (_active, session_id) =
            enroll_rotation_test_identity(&repository, node_id, now, Duration::days(31)).await?;
        let request = rotation_request(Uuid::now_v7(), node_id, session_id, now);
        assert_eq!(
            repository
                .stage_agent_certificate_rotation(request.clone(), |_| -> Result<_, RepoError> {
                    panic!("not-due rotation must not invoke issuer")
                })
                .await?,
            StageAgentCertificateRotationOutcome::Rejected
        );
        let mut fenced = request;
        fenced.session_id = Uuid::now_v7();
        assert_eq!(
            repository
                .stage_agent_certificate_rotation(fenced, |_| -> Result<_, RepoError> {
                    panic!("fenced rotation must not invoke issuer")
                })
                .await?,
            StageAgentCertificateRotationOutcome::Rejected
        );

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn pending_control_takeover_returns_rotation_context_without_activating_management()
    -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let (active, old_session_id) =
            enroll_rotation_test_identity(&repository, node_id, now, Duration::days(20)).await?;
        let request = rotation_request(Uuid::now_v7(), node_id, old_session_id, now);
        let staged = match repository
            .stage_agent_certificate_rotation(request.clone(), |decision_at| {
                let mut issued_request = request.clone();
                issued_request.requested_at = decision_at;
                Ok::<_, RepoError>(rotation_issue(&issued_request))
            })
            .await?
        {
            StageAgentCertificateRotationOutcome::Issued(bundle) => bundle,
            other => panic!("expected staged rotation, got {other:?}"),
        };

        let new_session_id = Uuid::now_v7();
        let context = match repository
            .claim_agent_control_session(session_claim(
                node_id,
                staged.control_certificate.fingerprint_sha256,
                new_session_id,
                Uuid::now_v7(),
                request.requested_at + Duration::seconds(1),
            ))
            .await?
        {
            AgentControlSessionClaimOutcome::Claimed {
                replaced_session_id: Some(replaced),
                takeover_reason: Some(AgentSessionTakeoverReason::CertificateRotation),
                rotation_context: Some(context),
                ..
            } if replaced == old_session_id => context,
            other => panic!("expected authorized rotation takeover, got {other:?}"),
        };
        assert_eq!(context.rotation_id, request.rotation_id);
        assert_eq!(
            context.old_control_fingerprint_sha256,
            active.control_certificate.fingerprint_sha256
        );
        assert_eq!(
            context.new_control_fingerprint_sha256,
            staged.control_certificate.fingerprint_sha256
        );
        assert_eq!(
            context.old_management_fingerprint_sha256,
            active.management_certificate.fingerprint_sha256
        );
        assert_eq!(
            context.new_management_fingerprint_sha256,
            staged.management_certificate.fingerprint_sha256
        );
        assert_eq!(context.authorized_until, staged.authorized_until);

        let states = sqlx::query(
            r#"
            select
              (select state from agent_certificates where id = $1) as old_control_state,
              (select state from agent_certificates where id = $2) as new_control_state,
              (select state from agent_management_certificates where id = $3) as old_management_state,
              (select state from agent_management_certificates where id = $4) as new_management_state,
              (select state from agent_certificate_rotations where id = $5) as rotation_state,
              (select consumed_by_session_id from agent_certificate_rotations where id = $5)
                as consumed_by_session_id
            "#,
        )
        .bind(active.control_certificate.id)
        .bind(staged.control_certificate.id)
        .bind(active.management_certificate.id)
        .bind(staged.management_certificate.id)
        .bind(request.rotation_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            states.try_get::<String, _>("old_control_state")?,
            "replaced"
        );
        assert_eq!(states.try_get::<String, _>("new_control_state")?, "active");
        assert_eq!(
            states.try_get::<String, _>("old_management_state")?,
            "active"
        );
        assert_eq!(
            states.try_get::<String, _>("new_management_state")?,
            "pending_rotation"
        );
        assert_eq!(
            states.try_get::<String, _>("rotation_state")?,
            "control_activated"
        );
        assert_eq!(
            states.try_get::<Option<Uuid>, _>("consumed_by_session_id")?,
            Some(new_session_id)
        );

        database.cleanup().await?;
        Ok(())
    }

    struct TakenOverRotation {
        request: AgentCertificateRotationRequest,
        active: AgentEnrollmentBundle,
        staged: AgentCertificateRotationBundle,
        session_id: Uuid,
    }

    async fn stage_and_take_over_rotation(
        repository: &TaskRepository,
        node_id: Uuid,
        now: DateTime<Utc>,
    ) -> anyhow::Result<TakenOverRotation> {
        let (active, old_session_id) =
            enroll_rotation_test_identity(repository, node_id, now, Duration::days(20)).await?;
        let request = rotation_request(Uuid::now_v7(), node_id, old_session_id, now);
        let staged = match repository
            .stage_agent_certificate_rotation(request.clone(), |decision_at| {
                let mut issued_request = request.clone();
                issued_request.requested_at = decision_at;
                Ok::<_, RepoError>(rotation_issue(&issued_request))
            })
            .await?
        {
            StageAgentCertificateRotationOutcome::Issued(bundle) => bundle,
            other => panic!("expected staged rotation, got {other:?}"),
        };
        let session_id = Uuid::now_v7();
        let takeover = repository
            .claim_agent_control_session(session_claim(
                node_id,
                staged.control_certificate.fingerprint_sha256,
                session_id,
                Uuid::now_v7(),
                request.requested_at + Duration::seconds(1),
            ))
            .await?;
        assert!(matches!(
            takeover,
            AgentControlSessionClaimOutcome::Claimed {
                rotation_context: Some(_),
                ..
            }
        ));
        Ok(TakenOverRotation {
            request,
            active,
            staged,
            session_id,
        })
    }

    fn management_activation_request(
        rotation: &TakenOverRotation,
        activated_at: DateTime<Utc>,
    ) -> AgentManagementRotationActivationRequest {
        AgentManagementRotationActivationRequest {
            rotation_id: rotation.request.rotation_id,
            node_id: rotation.request.node_id,
            session_id: rotation.session_id,
            control_fingerprint_sha256: rotation.staged.control_certificate.fingerprint_sha256,
            management_fingerprint_sha256: rotation
                .staged
                .management_certificate
                .fingerprint_sha256,
            activated_at,
        }
    }

    #[tokio::test]
    async fn management_rotation_activation_is_pinned_fenced_and_idempotent() -> anyhow::Result<()>
    {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();
        let rotation = stage_and_take_over_rotation(&repository, Uuid::now_v7(), now).await?;
        let activation_time = rotation.staged.authorized_until + Duration::minutes(1);
        sqlx::query(
            "update agent_control_sessions set lease_expires_at = $1 where node_id = $2 and session_id = $3",
        )
        .bind(activation_time + Duration::seconds(30))
        .bind(rotation.request.node_id)
        .bind(rotation.session_id)
        .execute(&database.pool)
        .await?;
        let pending_pins = repository
            .agent_management_certificate_fingerprints_for_session(
                rotation.request.node_id,
                rotation.session_id,
                activation_time,
            )
            .await?
            .expect("current rotation session has management pins");
        assert_eq!(
            pending_pins.current_fingerprint_sha256,
            rotation.active.management_certificate.fingerprint_sha256
        );
        assert_eq!(
            pending_pins.rotating_fingerprint_sha256,
            Some(rotation.staged.management_certificate.fingerprint_sha256)
        );
        let mut wrong = management_activation_request(&rotation, activation_time);
        wrong.management_fingerprint_sha256[0] ^= 1;
        assert_eq!(
            repository.activate_agent_management_rotation(wrong).await?,
            AgentManagementRotationActivationOutcome::Rejected
        );
        let pending_state: String =
            sqlx::query_scalar("select state from agent_management_certificates where id = $1")
                .bind(rotation.staged.management_certificate.id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(pending_state, "pending_rotation");

        let request = management_activation_request(&rotation, activation_time);
        let context = match repository
            .activate_agent_management_rotation(request.clone())
            .await?
        {
            AgentManagementRotationActivationOutcome::Activated(context) => context,
            other => panic!("expected management activation, got {other:?}"),
        };
        assert_eq!(context.rotation_id, rotation.request.rotation_id);
        let management_activated_at: DateTime<Utc> = sqlx::query_scalar(
            "select management_activated_at from agent_certificate_rotations where id = $1",
        )
        .bind(rotation.request.rotation_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            context.previous_identity_expires_at,
            management_activated_at + AGENT_PREVIOUS_IDENTITY_RETIREMENT_WINDOW
        );
        assert_eq!(
            repository
                .activate_agent_management_rotation(request.clone())
                .await?,
            AgentManagementRotationActivationOutcome::Recovered(context)
        );
        sqlx::query(
            "update agent_certificate_rotations set management_activated_at = clock_timestamp() - interval '6 minutes' where id = $1",
        )
        .bind(rotation.request.rotation_id)
        .execute(&database.pool)
        .await?;
        let recovery_time: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let recovered_after_lost_command = match repository
            .activate_agent_management_rotation(AgentManagementRotationActivationRequest {
                activated_at: recovery_time,
                ..request
            })
            .await?
        {
            AgentManagementRotationActivationOutcome::Recovered(context) => context,
            other => panic!("expected late activation recovery, got {other:?}"),
        };
        let recovery_after: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        assert!(
            recovered_after_lost_command.previous_identity_expires_at > recovery_time
                && recovered_after_lost_command.previous_identity_expires_at
                    <= recovery_after + AGENT_PREVIOUS_IDENTITY_RETIREMENT_WINDOW,
            "a lost Activate command must never be retried with an expired retirement deadline"
        );
        let active_pins = repository
            .agent_management_certificate_fingerprints_for_session(
                rotation.request.node_id,
                rotation.session_id,
                activation_time,
            )
            .await?
            .expect("activated rotation session has management pins");
        assert_eq!(
            active_pins.current_fingerprint_sha256,
            rotation.staged.management_certificate.fingerprint_sha256
        );
        assert_eq!(active_pins.rotating_fingerprint_sha256, None);

        let states = sqlx::query(
            r#"
            select
              (select state from agent_management_certificates where id = $1) as old_state,
              (select state from agent_management_certificates where id = $2) as new_state,
              (select state from agent_certificate_rotations where id = $3) as rotation_state,
              (select management_activated_by_session_id
                 from agent_certificate_rotations where id = $3) as activation_session
            "#,
        )
        .bind(rotation.active.management_certificate.id)
        .bind(rotation.staged.management_certificate.id)
        .bind(rotation.request.rotation_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(states.try_get::<String, _>("old_state")?, "replaced");
        assert_eq!(states.try_get::<String, _>("new_state")?, "active");
        assert_eq!(
            states.try_get::<String, _>("rotation_state")?,
            "management_activated"
        );
        assert_eq!(
            states.try_get::<Option<Uuid>, _>("activation_session")?,
            Some(rotation.session_id)
        );

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn rotation_activation_ack_is_exact_and_recovers_on_a_later_new_certificate_session()
    -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();
        let rotation = stage_and_take_over_rotation(&repository, Uuid::now_v7(), now).await?;
        let activation_time = rotation.request.requested_at + Duration::seconds(2);
        repository
            .activate_agent_management_rotation(management_activation_request(
                &rotation,
                activation_time,
            ))
            .await?;

        let acknowledgement = AgentCertificateRotationAcknowledgement {
            rotation_id: rotation.request.rotation_id,
            node_id: rotation.request.node_id,
            session_id: rotation.session_id,
            control_fingerprint_sha256: rotation.staged.control_certificate.fingerprint_sha256,
            management_fingerprint_sha256: rotation
                .staged
                .management_certificate
                .fingerprint_sha256,
            acknowledged_at: activation_time + Duration::seconds(1),
        };
        let mut wrong = acknowledgement.clone();
        wrong.session_id = Uuid::now_v7();
        assert_eq!(
            repository
                .complete_agent_certificate_rotation(wrong)
                .await?,
            CompleteAgentCertificateRotationOutcome::Rejected
        );
        assert_eq!(
            repository
                .complete_agent_certificate_rotation(acknowledgement.clone())
                .await?,
            CompleteAgentCertificateRotationOutcome::Completed
        );
        assert_eq!(
            repository
                .complete_agent_certificate_rotation(acknowledgement)
                .await?,
            CompleteAgentCertificateRotationOutcome::Recovered
        );

        let completed = sqlx::query(
            "select state, completed_by_session_id from agent_certificate_rotations where id = $1",
        )
        .bind(rotation.request.rotation_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(completed.try_get::<String, _>("state")?, "completed");
        assert_eq!(
            completed.try_get::<Option<Uuid>, _>("completed_by_session_id")?,
            Some(rotation.session_id)
        );

        sqlx::query(
            "with decision as materialized (select clock_timestamp() as now) update agent_certificate_rotations set authorized_at = decision.now - interval '11 minutes', authorized_until = decision.now - interval '6 minutes', completed_at = decision.now - interval '6 minutes' from decision where id = $1",
        )
        .bind(rotation.request.rotation_id)
        .execute(&database.pool)
        .await?;
        let reconnect_at: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        assert!(
            repository
                .close_agent_control_session_and_reclaim(
                    rotation.request.node_id,
                    rotation.session_id,
                    reconnect_at - Duration::seconds(1),
                )
                .await?
        );
        let reconnect_session_id = Uuid::now_v7();
        assert!(matches!(
            repository
                .claim_agent_control_session(session_claim(
                    rotation.request.node_id,
                    rotation.staged.control_certificate.fingerprint_sha256,
                    reconnect_session_id,
                    Uuid::now_v7(),
                    reconnect_at,
                ))
                .await?,
            AgentControlSessionClaimOutcome::Claimed { .. }
        ));
        assert_eq!(
            repository
                .complete_agent_certificate_rotation(AgentCertificateRotationAcknowledgement {
                    rotation_id: rotation.request.rotation_id,
                    node_id: rotation.request.node_id,
                    session_id: reconnect_session_id,
                    control_fingerprint_sha256: rotation
                        .staged
                        .control_certificate
                        .fingerprint_sha256,
                    management_fingerprint_sha256: rotation
                        .staged
                        .management_certificate
                        .fingerprint_sha256,
                    acknowledged_at: reconnect_at,
                })
                .await?,
            CompleteAgentCertificateRotationOutcome::Recovered,
            "a completed ACK tombstone must survive reconnect and the five-minute rotation window"
        );
        let replay = |session_id, acknowledged_at| AgentCertificateRotationAcknowledgement {
            rotation_id: rotation.request.rotation_id,
            node_id: rotation.request.node_id,
            session_id,
            control_fingerprint_sha256: rotation.staged.control_certificate.fingerprint_sha256,
            management_fingerprint_sha256: rotation
                .staged
                .management_certificate
                .fingerprint_sha256,
            acknowledged_at,
        };
        assert_eq!(
            repository
                .complete_agent_certificate_rotation(replay(
                    rotation.session_id,
                    reconnect_at + Duration::seconds(2),
                ))
                .await?,
            CompleteAgentCertificateRotationOutcome::Rejected,
            "the completed rotation's prior session must remain fenced"
        );
        let mut wrong_control = replay(reconnect_session_id, reconnect_at + Duration::seconds(3));
        wrong_control.control_fingerprint_sha256[0] ^= 1;
        assert_eq!(
            repository
                .complete_agent_certificate_rotation(wrong_control)
                .await?,
            CompleteAgentCertificateRotationOutcome::Rejected
        );
        let mut wrong_management =
            replay(reconnect_session_id, reconnect_at + Duration::seconds(4));
        wrong_management.management_fingerprint_sha256[0] ^= 1;
        assert_eq!(
            repository
                .complete_agent_certificate_rotation(wrong_management)
                .await?,
            CompleteAgentCertificateRotationOutcome::Rejected
        );
        assert_eq!(
            repository
                .stage_agent_certificate_rotation(
                    AgentCertificateRotationRequest {
                        session_id: rotation.session_id,
                        requested_at: activation_time + Duration::seconds(2),
                        ..rotation.request
                    },
                    |_| -> Result<_, RepoError> {
                        panic!("completed rotation replay must not invoke issuer")
                    },
                )
                .await?,
            StageAgentCertificateRotationOutcome::Rejected
        );

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn incomplete_rotation_rebinds_after_ack_loss_and_replays_activation()
    -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();
        let rotation = stage_and_take_over_rotation(&repository, Uuid::now_v7(), now).await?;
        sqlx::query(
            "with decision as materialized (select clock_timestamp() as now) update agent_certificate_rotations set authorized_at = decision.now - interval '6 minutes', authorized_until = decision.now - interval '1 minute' from decision where id = $1",
        )
        .bind(rotation.request.rotation_id)
        .execute(&database.pool)
        .await?;
        let activation_time: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        repository
            .activate_agent_management_rotation(management_activation_request(
                &rotation,
                activation_time,
            ))
            .await?;
        assert!(
            repository
                .close_agent_control_session_and_reclaim(
                    rotation.request.node_id,
                    rotation.session_id,
                    activation_time + Duration::milliseconds(100),
                )
                .await?
        );

        let rebound_session_id = Uuid::now_v7();
        let rebound_at: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let context = match repository
            .claim_agent_control_session(session_claim(
                rotation.request.node_id,
                rotation.staged.control_certificate.fingerprint_sha256,
                rebound_session_id,
                Uuid::now_v7(),
                rebound_at,
            ))
            .await?
        {
            AgentControlSessionClaimOutcome::Claimed {
                takeover_reason: Some(AgentSessionTakeoverReason::CleanDisconnect),
                rotation_context: Some(context),
                ..
            } => context,
            other => panic!("expected incomplete rotation rebind, got {other:?}"),
        };
        assert_eq!(context.rotation_id, rotation.request.rotation_id);
        let rebound_rotation = TakenOverRotation {
            session_id: rebound_session_id,
            ..rotation
        };
        assert!(matches!(
            repository
                .activate_agent_management_rotation(management_activation_request(
                    &rebound_rotation,
                    rebound_at + Duration::milliseconds(100),
                ))
                .await?,
            AgentManagementRotationActivationOutcome::Recovered(_)
        ));
        assert_eq!(
            repository
                .complete_agent_certificate_rotation(AgentCertificateRotationAcknowledgement {
                    rotation_id: rebound_rotation.request.rotation_id,
                    node_id: rebound_rotation.request.node_id,
                    session_id: rebound_session_id,
                    control_fingerprint_sha256: rebound_rotation
                        .staged
                        .control_certificate
                        .fingerprint_sha256,
                    management_fingerprint_sha256: rebound_rotation
                        .staged
                        .management_certificate
                        .fingerprint_sha256,
                    acknowledged_at: rebound_at + Duration::milliseconds(200),
                })
                .await?,
            CompleteAgentCertificateRotationOutcome::Completed
        );

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn enrollment_expiry_and_audit_use_database_time_across_skewed_core_instances()
    -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let creator = TaskRepository::new(database.pool.clone());
        let consumer = TaskRepository::new(database.pool.clone());
        let database_before: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let node_id = Uuid::now_v7();
        let token_hash = unique_sha256();
        let slow_core_now = database_before - Duration::days(1);

        assert!(matches!(
            creator
                .create_agent_enrollment(NewAgentEnrollment {
                    id: Uuid::now_v7(),
                    node_id,
                    token_hash,
                    created_by: "skewed-admin".to_string(),
                    created_at: slow_core_now,
                    expires_at: slow_core_now + Duration::minutes(10),
                    remote_ip: Some("192.0.2.51".parse().unwrap()),
                    user_agent: Some("slow-core".to_string()),
                })
                .await?,
            CreateAgentEnrollmentOutcome::Created { .. }
        ));
        let database_after_create: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let token_times = sqlx::query(
            "select created_at, expires_at from agent_enrollment_tokens where token_hash = $1",
        )
        .bind(token_hash.as_slice())
        .fetch_one(&database.pool)
        .await?;
        let created_at: DateTime<Utc> = token_times.try_get("created_at")?;
        let expires_at: DateTime<Utc> = token_times.try_get("expires_at")?;
        assert!(created_at >= database_before && created_at <= database_after_create);
        assert_eq!(expires_at - created_at, Duration::minutes(10));

        let fast_core_now = database_before + Duration::days(1);
        let request = enrollment_request(node_id, fast_core_now);
        let mut issue_request = request.clone();
        issue_request.attempted_at = database_after_create;
        let bundle = enrollment_bundle(&issue_request);
        assert!(matches!(
            consumer
                .consume_agent_enrollment(&token_hash, request, |_| Ok::<_, RepoError>(bundle))
                .await?,
            ConsumeAgentEnrollmentOutcome::Issued(_)
        ));
        let database_after_consume: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let audit_times: Vec<DateTime<Utc>> = sqlx::query_scalar(
            r#"
            select created_at
              from security_audit_events
             where subject = $1
               and event_type in ('agent_enrollment_created', 'agent_certificate_issued')
             order by created_at
            "#,
        )
        .bind(node_id.to_string())
        .fetch_all(&database.pool)
        .await?;
        assert_eq!(audit_times.len(), 2);
        assert!(
            audit_times
                .iter()
                .all(|value| *value >= database_before && *value <= database_after_consume)
        );

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn session_claim_renew_and_takeover_use_database_time_across_skewed_core_instances()
    -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let first_core = TaskRepository::new(database.pool.clone());
        let second_core = TaskRepository::new(database.pool.clone());
        let database_now: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let node_id = Uuid::now_v7();
        let (_certificate_id, fingerprint) =
            seed_agent_certificate(&database.pool, node_id, "active", database_now).await?;
        let first_session = Uuid::now_v7();
        let first_before: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        assert!(matches!(
            first_core
                .claim_agent_control_session(session_claim(
                    node_id,
                    fingerprint,
                    first_session,
                    Uuid::now_v7(),
                    database_now - Duration::days(1),
                ))
                .await?,
            AgentControlSessionClaimOutcome::Claimed { .. }
        ));
        let first_after: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let first_row = sqlx::query(
            "select connected_at, lease_expires_at from agent_control_sessions where node_id = $1",
        )
        .bind(node_id)
        .fetch_one(&database.pool)
        .await?;
        let connected_at: DateTime<Utc> = first_row.try_get("connected_at")?;
        let lease_expires_at: DateTime<Utc> = first_row.try_get("lease_expires_at")?;
        assert!(connected_at >= first_before && connected_at <= first_after);
        assert_eq!(lease_expires_at - connected_at, Duration::seconds(30));

        let duplicate_session = Uuid::now_v7();
        assert!(matches!(
            second_core
                .claim_agent_control_session(session_claim(
                    node_id,
                    fingerprint,
                    duplicate_session,
                    Uuid::now_v7(),
                    database_now + Duration::days(1),
                ))
                .await?,
            AgentControlSessionClaimOutcome::DuplicateHealthy {
                existing_session_id
            } if existing_session_id == first_session
        ));

        let mut renew_tx = database.pool.begin().await?;
        assert!(
            second_core
                .renew_current_agent_control_session(
                    &mut renew_tx,
                    node_id,
                    first_session,
                    database_now + Duration::days(1),
                )
                .await?
        );
        renew_tx.commit().await?;
        let renewed_at: DateTime<Utc> = sqlx::query_scalar(
            "select last_activity_at from agent_control_sessions where node_id = $1",
        )
        .bind(node_id)
        .fetch_one(&database.pool)
        .await?;
        assert!(renewed_at >= first_after && renewed_at <= Utc::now() + Duration::seconds(1));

        sqlx::query(
            "update agent_control_sessions set connected_at = clock_timestamp() - interval '40 seconds', last_activity_at = clock_timestamp() - interval '40 seconds', lease_expires_at = clock_timestamp() - interval '1 second' where node_id = $1",
        )
        .bind(node_id)
        .execute(&database.pool)
        .await?;
        let takeover_before: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let takeover_session = Uuid::now_v7();
        assert!(matches!(
            second_core
                .claim_agent_control_session(session_claim(
                    node_id,
                    fingerprint,
                    takeover_session,
                    Uuid::now_v7(),
                    database_now - Duration::days(1),
                ))
                .await?,
            AgentControlSessionClaimOutcome::Claimed {
                replaced_session_id: Some(replaced),
                takeover_reason: Some(AgentSessionTakeoverReason::StaleTimeout),
                ..
            } if replaced == first_session
        ));
        let takeover_after: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let takeover_audit_at: DateTime<Utc> = sqlx::query_scalar(
            "select created_at from security_audit_events where subject = $1 and event_type = 'agent_session_takeover' order by created_at desc limit 1",
        )
        .bind(node_id.to_string())
        .fetch_one(&database.pool)
        .await?;
        assert!(takeover_audit_at >= takeover_before && takeover_audit_at <= takeover_after);

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn peer_rejection_audit_uses_database_time_across_skewed_core_instances()
    -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let database_before: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let node_id = Uuid::now_v7();
        repository
            .record_agent_peer_rejection(
                None,
                Some(node_id),
                unique_sha256(),
                "192.0.2.120".parse()?,
                "test_rejection",
                database_before + Duration::days(1),
            )
            .await?;
        let database_after: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let created_at: DateTime<Utc> = sqlx::query_scalar(
            "select created_at from security_audit_events where event_type = 'agent_peer_identity_rejected' and subject = $1",
        )
        .bind(node_id.to_string())
        .fetch_one(&database.pool)
        .await?;
        assert!(created_at >= database_before && created_at <= database_after);

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn clean_disconnect_reconnect_is_not_a_stale_timeout_takeover() -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let node_id = Uuid::now_v7();
        let (_certificate_id, fingerprint) =
            seed_agent_certificate(&database.pool, node_id, "active", now).await?;
        let first_session = Uuid::now_v7();
        assert!(matches!(
            repository
                .claim_agent_control_session(session_claim(
                    node_id,
                    fingerprint,
                    first_session,
                    Uuid::now_v7(),
                    now,
                ))
                .await?,
            AgentControlSessionClaimOutcome::Claimed { .. }
        ));
        let disconnect_before: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        assert!(
            repository
                .release_agent_control_session(
                    node_id,
                    first_session,
                    disconnect_before + Duration::days(1),
                )
                .await?
        );
        let disconnect_after: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let disconnected_at: DateTime<Utc> = sqlx::query_scalar(
            "select disconnected_at from agent_control_sessions where node_id = $1 and session_id = $2",
        )
        .bind(node_id)
        .bind(first_session)
        .fetch_one(&database.pool)
        .await?;
        assert!(disconnected_at >= disconnect_before && disconnected_at <= disconnect_after);

        let second_session = Uuid::now_v7();
        let reconnect = repository
            .claim_agent_control_session(session_claim(
                node_id,
                fingerprint,
                second_session,
                Uuid::now_v7(),
                now,
            ))
            .await?;
        match reconnect {
            AgentControlSessionClaimOutcome::Claimed {
                replaced_session_id: Some(replaced),
                takeover_reason: Some(reason),
                ..
            } => {
                assert_eq!(replaced, first_session);
                assert_eq!(reason.as_str(), "clean_disconnect");
            }
            other => panic!("expected clean reconnect, got {other:?}"),
        }
        let session = sqlx::query(
            "select takeover_from_session_id, takeover_reason from agent_control_sessions where node_id = $1",
        )
        .bind(node_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            session.try_get::<Option<Uuid>, _>("takeover_from_session_id")?,
            Some(first_session)
        );
        assert_eq!(
            session
                .try_get::<Option<String>, _>("takeover_reason")?
                .as_deref(),
            Some("clean_disconnect")
        );
        let audit_payload: serde_json::Value = sqlx::query_scalar(
            "select payload from security_audit_events where subject = $1 and event_type = 'agent_session_reconnected' order by created_at desc limit 1",
        )
        .bind(node_id.to_string())
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(audit_payload["reason"], json!("clean_disconnect"));

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn rotation_transitions_and_management_pins_use_database_time_across_skewed_cores()
    -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let first_core = TaskRepository::new(database.pool.clone());
        let second_core = TaskRepository::new(database.pool.clone());
        let database_before: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let node_id = Uuid::now_v7();
        let (active, old_session_id) = enroll_rotation_test_identity(
            &first_core,
            node_id,
            database_before,
            Duration::days(20),
        )
        .await?;
        let request = rotation_request(
            Uuid::now_v7(),
            node_id,
            old_session_id,
            database_before - Duration::days(1),
        );
        let mut issue_request = request.clone();
        issue_request.requested_at = database_before;
        let issue = rotation_issue(&issue_request);
        let staged = match second_core
            .stage_agent_certificate_rotation(request.clone(), |_| Ok::<_, RepoError>(issue))
            .await?
        {
            StageAgentCertificateRotationOutcome::Issued(bundle) => bundle,
            other => panic!("database-time staging must issue, got {other:?}"),
        };
        let database_after_stage: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let staged_times = sqlx::query(
            "select authorized_at, authorized_until from agent_certificate_rotations where id = $1",
        )
        .bind(request.rotation_id)
        .fetch_one(&database.pool)
        .await?;
        let authorized_at: DateTime<Utc> = staged_times.try_get("authorized_at")?;
        let authorized_until: DateTime<Utc> = staged_times.try_get("authorized_until")?;
        assert!(authorized_at >= database_before && authorized_at <= database_after_stage);
        assert_eq!(
            authorized_until - authorized_at,
            AGENT_CERTIFICATE_ROTATION_DEADLINE
        );

        let new_session_id = Uuid::now_v7();
        assert!(matches!(
            first_core
                .claim_agent_control_session(session_claim(
                    node_id,
                    staged.control_certificate.fingerprint_sha256,
                    new_session_id,
                    Uuid::now_v7(),
                    database_before + Duration::days(1),
                ))
                .await?,
            AgentControlSessionClaimOutcome::Claimed {
                takeover_reason: Some(AgentSessionTakeoverReason::CertificateRotation),
                ..
            }
        ));
        assert!(matches!(
            second_core
                .activate_agent_management_rotation(AgentManagementRotationActivationRequest {
                    rotation_id: request.rotation_id,
                    node_id,
                    session_id: new_session_id,
                    control_fingerprint_sha256: staged.control_certificate.fingerprint_sha256,
                    management_fingerprint_sha256: staged.management_certificate.fingerprint_sha256,
                    activated_at: database_before + Duration::days(1),
                })
                .await?,
            AgentManagementRotationActivationOutcome::Activated(_)
        ));
        assert_eq!(
            first_core
                .complete_agent_certificate_rotation(AgentCertificateRotationAcknowledgement {
                    rotation_id: request.rotation_id,
                    node_id,
                    session_id: new_session_id,
                    control_fingerprint_sha256: staged.control_certificate.fingerprint_sha256,
                    management_fingerprint_sha256: staged.management_certificate.fingerprint_sha256,
                    acknowledged_at: database_before - Duration::days(1),
                })
                .await?,
            CompleteAgentCertificateRotationOutcome::Completed
        );
        let pins = second_core
            .agent_management_certificate_fingerprints_for_session(
                node_id,
                new_session_id,
                database_before + Duration::days(1),
            )
            .await?
            .expect("database-current session must return management pins");
        assert_eq!(
            pins.current_fingerprint_sha256,
            staged.management_certificate.fingerprint_sha256
        );
        assert_eq!(pins.rotating_fingerprint_sha256, None);
        assert_ne!(
            pins.current_fingerprint_sha256,
            active.management_certificate.fingerprint_sha256
        );
        let database_after: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let audit_times: Vec<DateTime<Utc>> = sqlx::query_scalar(
            r#"
            select created_at
              from security_audit_events
             where subject = $1
               and event_type in (
                 'agent_certificate_rotation_staged',
                 'agent_management_certificate_activated',
                 'agent_certificate_rotation_completed'
               )
            "#,
        )
        .bind(node_id.to_string())
        .fetch_all(&database.pool)
        .await?;
        assert_eq!(audit_times.len(), 3);
        assert!(
            audit_times
                .iter()
                .all(|time| *time >= database_before && *time <= database_after)
        );

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn enrollment_preflight_is_token_first_lock_free_and_audit_free() -> anyhow::Result<()> {
        let Some(database) = TestDatabase::maybe_new().await? else {
            return Ok(());
        };
        let repository = TaskRepository::new(database.pool.clone());
        let now = Utc::now();
        let node_id = Uuid::now_v7();
        let enrollment_id = Uuid::now_v7();
        let token_hash = [41_u8; 32];
        repository
            .create_agent_enrollment(NewAgentEnrollment {
                id: enrollment_id,
                node_id,
                token_hash,
                created_by: "admin".to_string(),
                created_at: now,
                expires_at: now + Duration::minutes(10),
                remote_ip: None,
                user_agent: None,
            })
            .await?;
        let audit_before: i64 = sqlx::query_scalar("select count(*) from security_audit_events")
            .fetch_one(&database.pool)
            .await?;

        assert_eq!(
            repository
                .preflight_agent_enrollment(enrollment_id, &token_hash, node_id)
                .await?,
            AgentEnrollmentPreflightOutcome::Admissible
        );
        assert_eq!(
            repository
                .preflight_agent_enrollment(Uuid::now_v7(), &[99_u8; 32], node_id)
                .await?,
            AgentEnrollmentPreflightOutcome::Invalid
        );
        assert_eq!(
            repository
                .preflight_agent_enrollment(enrollment_id, &token_hash, Uuid::now_v7())
                .await?,
            AgentEnrollmentPreflightOutcome::Invalid
        );
        let direct_invalid = repository
            .consume_agent_enrollment(
                &[99_u8; 32],
                AgentEnrollmentRequest {
                    node_id,
                    control_csr_public_key_sha256: [1_u8; 32],
                    management_csr_public_key_sha256: [2_u8; 32],
                    attempted_at: now,
                    remote_ip: Some("192.0.2.99".parse()?),
                    user_agent: Some("audit-amplification-probe".to_string()),
                },
                |_| -> Result<AgentEnrollmentBundle, RepoError> {
                    panic!("invalid token must not invoke certificate issuer")
                },
            )
            .await?;
        assert_eq!(direct_invalid, ConsumeAgentEnrollmentOutcome::Invalid);

        let mut identity_lock = database.pool.begin().await?;
        sqlx::query("select 1 from agent_identities where node_id = $1 for update")
            .bind(node_id)
            .fetch_one(&mut *identity_lock)
            .await?;
        let while_identity_locked = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            repository.preflight_agent_enrollment(enrollment_id, &token_hash, node_id),
        )
        .await??;
        assert_eq!(
            while_identity_locked,
            AgentEnrollmentPreflightOutcome::Admissible
        );
        identity_lock.rollback().await?;

        let audit_after: i64 = sqlx::query_scalar("select count(*) from security_audit_events")
            .fetch_one(&database.pool)
            .await?;
        assert_eq!(audit_after, audit_before);
        database.cleanup().await?;
        Ok(())
    }
}
