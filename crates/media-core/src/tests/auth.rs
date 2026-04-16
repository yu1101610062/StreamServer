use super::*;
use crate::config::CoreSettings;

const ED25519_PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIMAlSI3/XdPzRT72Rw08g6NnTnJ2eaq1JoJoW5Vlbm/T\n-----END PRIVATE KEY-----";
const ED25519_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAA5Q5gilpT0f2fcLhC7l30Wou7Ng/gESlFWWx8z6TGJw=\n-----END PUBLIC KEY-----";

fn disabled_auth_config() -> AuthConfig {
    AuthConfig::from_settings(&CoreSettings::default()).expect("disabled auth config")
}

#[test]
fn disabled_auth_returns_admin() {
    let config = disabled_auth_config();
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
