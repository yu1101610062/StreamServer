use std::{fs, sync::Arc};

use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng as PasswordOsRng},
};
use axum::http::HeaderMap;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, decode_header, encode,
};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    config::{AuthMode, CoreSettings, parse_duration_spec},
    error::AppError,
};

#[derive(Clone)]
pub struct AuthConfig {
    mode: AuthMode,
    verifier: Option<Arc<JwtVerifier>>,
    signer: Option<Arc<JwtSigner>>,
    access_token_ttl: Duration,
    refresh_token_ttl: Duration,
}

impl AuthConfig {
    #[cfg(test)]
    pub fn from_public_key(enabled: bool, pem: &str) -> anyhow::Result<Self> {
        if enabled {
            Ok(Self {
                mode: AuthMode::ExternalJwt,
                verifier: Some(Arc::new(JwtVerifier::from_public_key_pem(pem)?)),
                signer: None,
                access_token_ttl: Duration::minutes(15),
                refresh_token_ttl: Duration::days(7),
            })
        } else {
            Ok(Self::disabled())
        }
    }

    pub fn from_settings(settings: &CoreSettings) -> anyhow::Result<Self> {
        match settings.auth_mode {
            AuthMode::Disabled => Ok(Self {
                mode: AuthMode::Disabled,
                verifier: None,
                signer: None,
                access_token_ttl: Duration::minutes(15),
                refresh_token_ttl: Duration::days(7),
            }),
            AuthMode::ExternalJwt => Ok(Self {
                mode: AuthMode::ExternalJwt,
                verifier: Some(Arc::new(JwtVerifier::from_public_key_pem(
                    &settings.jwt_public_key,
                )?)),
                signer: None,
                access_token_ttl: Duration::minutes(15),
                refresh_token_ttl: Duration::days(7),
            }),
            AuthMode::LocalPassword => {
                let public_pem = fs::read_to_string(&settings.auth_jwt_public_key_path)?;
                let private_pem = fs::read_to_string(&settings.auth_jwt_private_key_path)?;
                Ok(Self {
                    mode: AuthMode::LocalPassword,
                    verifier: Some(Arc::new(JwtVerifier::from_public_key_pem(&public_pem)?)),
                    signer: Some(Arc::new(JwtSigner::from_private_key_pem(&private_pem)?)),
                    access_token_ttl: parse_duration_spec(&settings.auth_access_token_ttl)?,
                    refresh_token_ttl: parse_duration_spec(&settings.auth_refresh_token_ttl)?,
                })
            }
        }
    }

    #[cfg(test)]
    pub fn disabled() -> Self {
        Self {
            mode: AuthMode::Disabled,
            verifier: None,
            signer: None,
            access_token_ttl: Duration::minutes(15),
            refresh_token_ttl: Duration::days(7),
        }
    }

    pub fn session(&self, headers: &HeaderMap) -> Result<AuthenticatedPrincipal, AppError> {
        if self.mode == AuthMode::Disabled {
            return Ok(AuthenticatedPrincipal::disabled_admin());
        }

        let verifier = self.verifier.as_ref().ok_or_else(|| {
            AppError::Internal("auth is enabled but verifier is missing".to_string())
        })?;
        verifier.verify(headers)
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

    pub fn enabled(&self) -> bool {
        self.mode != AuthMode::Disabled
    }

    pub fn mode(&self) -> AuthMode {
        self.mode
    }

    pub fn supports_local_login(&self) -> bool {
        self.mode == AuthMode::LocalPassword
    }

    pub fn refresh_token_ttl(&self) -> Duration {
        self.refresh_token_ttl
    }

    pub fn issue_access_token(
        &self,
        subject: &str,
        role: ApiRole,
    ) -> anyhow::Result<IssuedAccessToken> {
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("access token signing is not available"))?;
        signer.issue(subject, role, self.access_token_ttl)
    }
}

#[derive(Debug, Clone)]
pub struct IssuedAccessToken {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrincipalKind {
    Disabled,
    User,
    Machine,
}

struct JwtVerifier {
    rsa_key: Option<DecodingKey>,
    ed_key: Option<DecodingKey>,
}

struct JwtSigner {
    algorithm: Algorithm,
    key: EncodingKey,
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("mode", &self.mode)
            .field("verifier", &self.verifier.is_some())
            .field("signer", &self.signer.is_some())
            .finish()
    }
}

impl JwtVerifier {
    fn from_public_key_pem(pem: &str) -> anyhow::Result<Self> {
        let pem = pem.trim();
        anyhow::ensure!(
            !pem.is_empty(),
            "JWT public key must not be empty when auth is enabled"
        );

        let rsa_key = DecodingKey::from_rsa_pem(pem.as_bytes()).ok();
        let ed_key = DecodingKey::from_ed_pem(pem.as_bytes()).ok();
        anyhow::ensure!(
            rsa_key.is_some() || ed_key.is_some(),
            "JWT public key must be a valid RSA or Ed25519 public key in PEM format"
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
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.required_spec_claims = ["exp", "iat", "nbf", "sub"]
            .into_iter()
            .map(str::to_string)
            .collect();
        validation.leeway = 30;

        let claims = decode::<JwtClaims>(token, key, &validation)
            .map_err(|error| AppError::Forbidden(format!("invalid bearer token: {error}")))?
            .claims;

        AuthenticatedPrincipal::from_claims(claims)
    }
}

impl JwtSigner {
    fn from_private_key_pem(pem: &str) -> anyhow::Result<Self> {
        let pem = pem.trim();
        anyhow::ensure!(
            !pem.is_empty(),
            "JWT private key must not be empty when local auth is enabled"
        );

        if let Ok(key) = EncodingKey::from_rsa_pem(pem.as_bytes()) {
            return Ok(Self {
                algorithm: Algorithm::RS256,
                key,
            });
        }
        if let Ok(key) = EncodingKey::from_ed_pem(pem.as_bytes()) {
            return Ok(Self {
                algorithm: Algorithm::EdDSA,
                key,
            });
        }

        anyhow::bail!("JWT private key must be a valid RSA or Ed25519 private key in PEM format")
    }

    fn issue(
        &self,
        subject: &str,
        role: ApiRole,
        ttl: Duration,
    ) -> anyhow::Result<IssuedAccessToken> {
        let now = Utc::now();
        let expires_at = now + ttl;
        let claims = JwtClaims {
            sub: subject.to_string(),
            role,
            jti: Uuid::now_v7().to_string(),
            iat: now.timestamp(),
            nbf: now.timestamp(),
            exp: expires_at.timestamp(),
        };
        let mut header = Header::new(self.algorithm);
        header.typ = Some("JWT".to_string());
        let token = encode(&header, &claims, &self.key)?;
        Ok(IssuedAccessToken { token, expires_at })
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
    SecurityWrite,
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
            Self::SecurityWrite => "security_write",
        }
    }
}

const ALL_PERMISSIONS: [ApiPermission; 8] = [
    ApiPermission::TaskRead,
    ApiPermission::TaskWrite,
    ApiPermission::TemplateRead,
    ApiPermission::TemplateWrite,
    ApiPermission::RecordRead,
    ApiPermission::NodeRead,
    ApiPermission::DebugRead,
    ApiPermission::SecurityWrite,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum ApiRole {
    #[serde(rename = "admin", alias = "platform_admin")]
    Admin,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedPrincipal {
    subject: String,
    role: ApiRole,
    kind: PrincipalKind,
    must_change_password: bool,
}

impl AuthenticatedPrincipal {
    fn disabled_admin() -> Self {
        Self {
            subject: "auth_disabled".to_string(),
            role: ApiRole::Admin,
            kind: PrincipalKind::Disabled,
            must_change_password: false,
        }
    }

    fn from_claims(claims: JwtClaims) -> Result<Self, AppError> {
        if claims.sub.trim().is_empty() {
            return Err(AppError::Forbidden(
                "bearer token missing subject".to_string(),
            ));
        }

        Ok(Self {
            subject: claims.sub.trim().to_string(),
            role: claims.role,
            kind: PrincipalKind::User,
            must_change_password: false,
        })
    }

    pub fn machine_allowlisted(subject: &str) -> Self {
        Self {
            subject: subject.to_string(),
            role: ApiRole::Admin,
            kind: PrincipalKind::Machine,
            must_change_password: false,
        }
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn role(&self) -> ApiRole {
        self.role
    }

    pub fn must_change_password(&self) -> bool {
        self.must_change_password
    }

    pub fn is_machine(&self) -> bool {
        self.kind == PrincipalKind::Machine
    }

    pub fn require_permission(&self, permission: ApiPermission) -> Result<(), AppError> {
        let allowed = match self.kind {
            PrincipalKind::Disabled | PrincipalKind::User => true,
            PrincipalKind::Machine => matches!(
                permission,
                ApiPermission::TaskRead
                    | ApiPermission::TaskWrite
                    | ApiPermission::TemplateRead
                    | ApiPermission::TemplateWrite
                    | ApiPermission::RecordRead
            ),
        };

        if allowed {
            Ok(())
        } else {
            Err(AppError::Forbidden(format!(
                "principal is not allowed to access {}",
                permission.as_str()
            )))
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

#[derive(Debug, Deserialize, Serialize)]
struct JwtClaims {
    sub: String,
    role: ApiRole,
    jti: String,
    iat: i64,
    nbf: i64,
    exp: i64,
}

pub fn extract_bearer_token(headers: &HeaderMap) -> Result<&str, AppError> {
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

pub fn maybe_extract_bearer_token(headers: &HeaderMap) -> Result<Option<&str>, AppError> {
    match headers.get(axum::http::header::AUTHORIZATION) {
        Some(_) => extract_bearer_token(headers).map(Some),
        None => Ok(None),
    }
}

pub fn hash_refresh_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    BASE64_STANDARD.encode(digest)
}

pub fn generate_refresh_token() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    BASE64_STANDARD.encode(bytes)
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    anyhow::ensure!(!password.trim().is_empty(), "password must not be empty");
    anyhow::ensure!(
        password.chars().count() >= 8,
        "password must be at least 8 characters"
    );
    let salt = SaltString::generate(&mut PasswordOsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| anyhow::anyhow!("failed to hash password: {error}"))?
        .to_string())
}

pub fn verify_password(password_hash: &str, password: &str) -> anyhow::Result<bool> {
    let parsed = PasswordHash::new(password_hash)
        .map_err(|error| anyhow::anyhow!("invalid password hash: {error}"))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED25519_PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIMAlSI3/XdPzRT72Rw08g6NnTnJ2eaq1JoJoW5Vlbm/T\n-----END PRIVATE KEY-----";
    const ED25519_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAA5Q5gilpT0f2fcLhC7l30Wou7Ng/gESlFWWx8z6TGJw=\n-----END PUBLIC KEY-----";

    #[test]
    fn disabled_auth_returns_admin() {
        let config = AuthConfig::disabled();
        let principal = config
            .authorize(&HeaderMap::new(), ApiPermission::DebugRead)
            .expect("auth should be bypassed");

        assert_eq!(principal.role(), ApiRole::Admin);
        assert_eq!(principal.subject(), "auth_disabled");
    }

    #[test]
    fn machine_principal_has_limited_permissions() {
        let principal = AuthenticatedPrincipal::machine_allowlisted("10.0.0.5");
        assert!(
            principal
                .require_permission(ApiPermission::TaskWrite)
                .is_ok()
        );
        assert!(
            principal
                .require_permission(ApiPermission::TemplateRead)
                .is_ok()
        );
        assert!(
            principal
                .require_permission(ApiPermission::NodeRead)
                .is_err()
        );
        assert!(
            principal
                .require_permission(ApiPermission::DebugRead)
                .is_err()
        );
    }

    #[test]
    fn signer_and_verifier_round_trip() {
        let signer = JwtSigner::from_private_key_pem(ED25519_PRIVATE_KEY).expect("private key");
        let verifier = JwtVerifier::from_public_key_pem(ED25519_PUBLIC_KEY).expect("public key");
        let issued = signer
            .issue("alice", ApiRole::Admin, Duration::minutes(5))
            .expect("token should issue");
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", issued.token).parse().expect("header"),
        );
        let principal = verifier.verify(&headers).expect("token should verify");
        assert_eq!(principal.subject(), "alice");
        assert_eq!(principal.role(), ApiRole::Admin);
    }
}
