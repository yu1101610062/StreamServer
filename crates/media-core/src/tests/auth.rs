use super::*;
use crate::config::{AuthMode, CoreSettings};

const ED25519_PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIMAlSI3/XdPzRT72Rw08g6NnTnJ2eaq1JoJoW5Vlbm/T\n-----END PRIVATE KEY-----";
const ED25519_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAA5Q5gilpT0f2fcLhC7l30Wou7Ng/gESlFWWx8z6TGJw=\n-----END PUBLIC KEY-----";

fn disabled_auth_config() -> AuthConfig {
    AuthConfig::from_settings(&CoreSettings::default()).expect("disabled auth config")
}

#[test]
fn disabled_auth_returns_admin() {
    let config = disabled_auth_config();
    let principal = config
        .verify_session_claims(&HeaderMap::new())
        .expect("disabled auth should identify the built-in principal");
    principal
        .require_permission(ApiPermission::DebugRead)
        .expect("disabled auth should authorize");

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
            .require_permission(ApiPermission::RecordRead)
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
        .issue("alice", ApiRole::Admin, Some(7), true, Duration::minutes(5))
        .expect("token should issue");
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {}", issued.token).parse().expect("header"),
    );
    let principal = verifier.verify(&headers).expect("token should verify");
    assert_eq!(principal.subject(), "alice");
    assert_eq!(principal.role(), ApiRole::Admin);
    assert_eq!(principal.credential_version(), Some(7));
    assert!(principal.must_change_password());
    assert!(
        principal
            .require_permission(ApiPermission::TaskRead)
            .is_err(),
        "must-change access tokens must not receive business permissions"
    );
}

#[test]
fn verifier_accepts_external_tokens_without_local_credential_claims() {
    #[derive(serde::Serialize)]
    struct LegacyExternalClaims<'a> {
        sub: &'a str,
        role: ApiRole,
        jti: &'a str,
        iat: i64,
        nbf: i64,
        exp: i64,
    }

    let now = Utc::now().timestamp();
    let token = encode(
        &Header::new(Algorithm::EdDSA),
        &LegacyExternalClaims {
            sub: "external-admin",
            role: ApiRole::Admin,
            jti: "legacy-external-token",
            iat: now,
            nbf: now,
            exp: now + 300,
        },
        &EncodingKey::from_ed_pem(ED25519_PRIVATE_KEY.as_bytes()).expect("private key"),
    )
    .expect("legacy external token");
    let verifier = JwtVerifier::from_public_key_pem(ED25519_PUBLIC_KEY).expect("public key");
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {token}").parse().expect("header"),
    );

    let principal = verifier
        .verify(&headers)
        .expect("claims added for local auth must remain optional for external JWTs");
    assert_eq!(principal.credential_version(), None);
    assert!(!principal.must_change_password());
    principal
        .require_permission(ApiPermission::TaskRead)
        .expect("legacy external JWT permissions remain unchanged");
}

#[test]
fn external_auth_ignores_local_credential_claim_names() {
    let signer = JwtSigner::from_private_key_pem(ED25519_PRIVATE_KEY).expect("private key");
    let issued = signer
        .issue(
            "external-admin",
            ApiRole::Admin,
            Some(99),
            true,
            Duration::minutes(5),
        )
        .expect("token should issue");
    let config = AuthConfig::from_settings(&CoreSettings {
        auth_mode: AuthMode::ExternalJwt,
        jwt_public_key: ED25519_PUBLIC_KEY.to_string(),
        ..CoreSettings::default()
    })
    .expect("external JWT config");
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {}", issued.token).parse().expect("header"),
    );

    let principal = config
        .verify_session_claims(&headers)
        .expect("external token should verify");
    assert_eq!(principal.credential_version(), None);
    assert!(!principal.must_change_password());
    principal
        .require_permission(ApiPermission::TaskRead)
        .expect("local credential claim names must not change external JWT permissions");
}

#[test]
fn local_auth_honors_namespaced_credential_claims() {
    let key_dir = tempfile::tempdir().expect("temporary key directory");
    let private_key_path = key_dir.path().join("jwt-private.pem");
    let public_key_path = key_dir.path().join("jwt-public.pem");
    std::fs::write(&private_key_path, ED25519_PRIVATE_KEY).expect("private key file");
    std::fs::write(&public_key_path, ED25519_PUBLIC_KEY).expect("public key file");
    let config = AuthConfig::from_settings(&CoreSettings {
        auth_mode: AuthMode::LocalPassword,
        auth_jwt_private_key_path: private_key_path.to_string_lossy().to_string(),
        auth_jwt_public_key_path: public_key_path.to_string_lossy().to_string(),
        ..CoreSettings::default()
    })
    .expect("local JWT config");
    let issued = config
        .issue_access_token("local-admin", ApiRole::Admin, 7, true)
        .expect("local token");
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {}", issued.token).parse().expect("header"),
    );

    let principal = config
        .verify_session_claims(&headers)
        .expect("local token should verify");
    assert_eq!(principal.credential_version(), Some(7));
    assert!(principal.must_change_password());
    assert!(
        principal
            .require_permission(ApiPermission::TaskRead)
            .is_err(),
        "local must-change claims must remove business permissions"
    );
}
