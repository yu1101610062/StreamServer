use std::sync::Arc;

use axum::http::HeaderMap;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};

use crate::error::AppError;

#[derive(Clone)]
pub struct AuthConfig {
    enabled: bool,
    verifier: Option<Arc<JwtVerifier>>,
}

impl AuthConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            verifier: None,
        }
    }

    pub fn from_public_key(enabled: bool, pem: &str) -> anyhow::Result<Self> {
        if !enabled {
            return Ok(Self::disabled());
        }

        let verifier = JwtVerifier::from_public_key_pem(pem)?;
        Ok(Self {
            enabled: true,
            verifier: Some(Arc::new(verifier)),
        })
    }

    pub fn authorize(
        &self,
        headers: &HeaderMap,
        permission: ApiPermission,
    ) -> Result<AuthenticatedPrincipal, AppError> {
        let principal = self.session(headers)?;

        principal.require_permission(permission)?;
        Ok(principal)
    }

    pub fn session(&self, headers: &HeaderMap) -> Result<AuthenticatedPrincipal, AppError> {
        if !self.enabled {
            return Ok(AuthenticatedPrincipal::platform_admin("auth_disabled"));
        }

        let verifier = self.verifier.as_ref().ok_or_else(|| {
            AppError::Internal("auth is enabled but verifier is missing".to_string())
        })?;
        verifier.verify(headers)
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

struct JwtVerifier {
    rsa_key: Option<DecodingKey>,
    ed_key: Option<DecodingKey>,
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("enabled", &self.enabled)
            .field("verifier", &self.verifier.is_some())
            .finish()
    }
}

impl JwtVerifier {
    fn from_public_key_pem(pem: &str) -> anyhow::Result<Self> {
        let pem = pem.trim();
        anyhow::ensure!(
            !pem.is_empty(),
            "JWT_PUBLIC_KEY must not be empty when auth is enabled"
        );

        let rsa_key = DecodingKey::from_rsa_pem(pem.as_bytes()).ok();
        let ed_key = DecodingKey::from_ed_pem(pem.as_bytes()).ok();
        anyhow::ensure!(
            rsa_key.is_some() || ed_key.is_some(),
            "JWT_PUBLIC_KEY must be a valid RSA or Ed25519 public key in PEM format"
        );

        Ok(Self { rsa_key, ed_key })
    }

    fn verify(&self, headers: &HeaderMap) -> Result<AuthenticatedPrincipal, AppError> {
        let token = extract_bearer_token(headers)?;
        let header = decode_header(token).map_err(|error| {
            AppError::Forbidden(format!("invalid bearer token header: {error}"))
        })?;
        let algorithm = header.alg;
        let key = match algorithm {
            Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => self.rsa_key.as_ref(),
            Algorithm::EdDSA => self.ed_key.as_ref(),
            _ => None,
        }
        .ok_or_else(|| {
            AppError::Forbidden(format!(
                "unsupported JWT algorithm {algorithm:?} for configured public key"
            ))
        })?;

        let mut validation = Validation::new(algorithm);
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.required_spec_claims.clear();

        let claims = decode::<JwtClaims>(token, key, &validation)
            .map_err(|error| AppError::Forbidden(format!("invalid bearer token: {error}")))?
            .claims;

        AuthenticatedPrincipal::from_claims(claims)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiPermission {
    TaskRead,
    TaskWrite,
    TemplateRead,
    TemplateWrite,
    RecordRead,
    NodeRead,
    DebugRead,
}

impl ApiPermission {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TaskRead => "task_read",
            Self::TaskWrite => "task_write",
            Self::TemplateRead => "template_read",
            Self::TemplateWrite => "template_write",
            Self::RecordRead => "record_read",
            Self::NodeRead => "node_read",
            Self::DebugRead => "debug_read",
        }
    }
}

const ALL_PERMISSIONS: [ApiPermission; 7] = [
    ApiPermission::TaskRead,
    ApiPermission::TaskWrite,
    ApiPermission::TemplateRead,
    ApiPermission::TemplateWrite,
    ApiPermission::RecordRead,
    ApiPermission::NodeRead,
    ApiPermission::DebugRead,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiRole {
    PlatformAdmin,
    TenantUser,
    AuditUser,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedPrincipal {
    subject: String,
    role: ApiRole,
    tenant_id: Option<String>,
}

impl AuthenticatedPrincipal {
    fn platform_admin(subject: &str) -> Self {
        Self {
            subject: subject.to_string(),
            role: ApiRole::PlatformAdmin,
            tenant_id: None,
        }
    }

    fn from_claims(claims: JwtClaims) -> Result<Self, AppError> {
        if claims.sub.trim().is_empty() {
            return Err(AppError::Forbidden(
                "bearer token missing subject".to_string(),
            ));
        }
        if !matches!(claims.role, ApiRole::PlatformAdmin)
            && claims
                .tenant_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
        {
            return Err(AppError::Forbidden(
                "tenant-scoped bearer token missing tenant_id".to_string(),
            ));
        }

        Ok(Self {
            subject: claims.sub.trim().to_string(),
            role: claims.role,
            tenant_id: claims
                .tenant_id
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        })
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn role(&self) -> ApiRole {
        self.role
    }

    pub fn tenant_id(&self) -> Option<&str> {
        self.tenant_id.as_deref()
    }

    pub fn require_permission(&self, permission: ApiPermission) -> Result<(), AppError> {
        let allowed = match permission {
            ApiPermission::TaskRead => true,
            ApiPermission::TaskWrite => !matches!(self.role, ApiRole::AuditUser),
            ApiPermission::TemplateRead => true,
            ApiPermission::TemplateWrite => matches!(self.role, ApiRole::PlatformAdmin),
            ApiPermission::RecordRead => true,
            ApiPermission::NodeRead | ApiPermission::DebugRead => {
                matches!(self.role, ApiRole::PlatformAdmin)
            }
        };

        if allowed {
            Ok(())
        } else {
            Err(AppError::Forbidden(format!(
                "role {:?} is not allowed to access this endpoint",
                self.role
            )))
        }
    }

    pub fn ensure_tenant_access(&self, tenant_id: &str) -> Result<(), AppError> {
        if matches!(self.role, ApiRole::PlatformAdmin) {
            return Ok(());
        }

        match self.tenant_id.as_deref() {
            Some(current) if current == tenant_id => Ok(()),
            _ => Err(AppError::Forbidden(format!(
                "principal is not allowed to access tenant {tenant_id}"
            ))),
        }
    }

    pub fn granted_permissions(&self) -> Vec<ApiPermission> {
        ALL_PERMISSIONS
            .iter()
            .copied()
            .filter(|permission| self.require_permission(*permission).is_ok())
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct JwtClaims {
    sub: String,
    role: ApiRole,
    #[serde(default)]
    tenant_id: Option<String>,
}

fn extract_bearer_token(headers: &HeaderMap) -> Result<&str, AppError> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .ok_or_else(|| AppError::Forbidden("missing Authorization header".to_string()))?;
    let value = header
        .to_str()
        .map_err(|_| AppError::Forbidden("Authorization header must be valid UTF-8".to_string()))?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::Forbidden("Authorization header must use Bearer token".to_string())
        })?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSA_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDRNk+CElS+M3My1DbTUInl9aeU\nYCLza8Uftij7kPTApECFQcy1em6CZwb+PDHjjtFB2i8Ncfbx+dt2S6CbJHSF0dDB\n+GoiaVaYolB9XoQODqA7LXTy/D4e9jdNJQgDVXlzXsTm4k3v1CnC1As7RfUkgdM/\npsbfsbeai7RULN2NnQIDAQAB\n-----END PUBLIC KEY-----";

    #[test]
    fn disabled_auth_returns_platform_admin() {
        let config = AuthConfig::disabled();
        let principal = config
            .authorize(&HeaderMap::new(), ApiPermission::DebugRead)
            .expect("auth should be bypassed");

        assert_eq!(principal.role(), ApiRole::PlatformAdmin);
        assert_eq!(principal.subject(), "auth_disabled");
    }

    #[test]
    fn tenant_user_must_match_tenant() {
        let principal = AuthenticatedPrincipal {
            subject: "alice".to_string(),
            role: ApiRole::TenantUser,
            tenant_id: Some("tenant-a".to_string()),
        };

        assert!(principal.ensure_tenant_access("tenant-a").is_ok());
        assert!(principal.ensure_tenant_access("tenant-b").is_err());
        assert!(
            principal
                .require_permission(ApiPermission::TaskWrite)
                .is_ok()
        );
        assert!(
            principal
                .require_permission(ApiPermission::TemplateWrite)
                .is_err()
        );
    }

    #[test]
    fn auth_config_rejects_empty_pem_when_enabled() {
        let error = AuthConfig::from_public_key(true, "").expect_err("empty key should fail");
        assert!(
            error
                .to_string()
                .contains("JWT_PUBLIC_KEY must not be empty")
        );
    }

    #[test]
    fn auth_config_accepts_valid_rsa_pem() {
        let config =
            AuthConfig::from_public_key(true, RSA_PUBLIC_KEY).expect("rsa pem should load");

        assert!(config.enabled);
        assert!(config.verifier.is_some());
    }
}
