use std::{io, path::Path, sync::Arc};

use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Extension, FromRef, Multipart, Path as AxumPath, Request, State},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use rustls::{RootCertStore, ServerConfig};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_stream::StreamExt;

use crate::{
    AppState, agent_metadata, live_health,
    management_auth::{
        AgentWriteOperation, CapabilityVerifier, CoreClientCertificateVerifier, DeleteJtiError,
        DeleteJtiStore, ManagementAuthError, VerifiedCapability,
    },
    ready_health, upload,
};

const MAX_MULTIPART_FRAMING_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct ManagementAdmission {
    semaphore: Arc<Semaphore>,
}

impl ManagementAdmission {
    pub(crate) fn new(limit: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(limit > 0, "management concurrency limit must be positive");
        Ok(Self {
            semaphore: Arc::new(Semaphore::new(limit)),
        })
    }

    pub(crate) fn try_enter(&self) -> Result<OwnedSemaphorePermit, StatusCode> {
        self.semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ManagementState {
    app: AppState,
    verifier: Arc<CapabilityVerifier>,
    delete_jti_store: Arc<DeleteJtiStore>,
    admission: ManagementAdmission,
}

impl ManagementState {
    pub(crate) fn new(
        app: AppState,
        verifier: CapabilityVerifier,
        delete_jti_root: impl AsRef<Path>,
        max_concurrency: usize,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            app,
            verifier: Arc::new(verifier),
            delete_jti_store: Arc::new(
                DeleteJtiStore::new(delete_jti_root)
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?,
            ),
            admission: ManagementAdmission::new(max_concurrency)?,
        })
    }

    pub(crate) fn delete_jti_store(&self) -> &DeleteJtiStore {
        &self.delete_jti_store
    }
}

impl FromRef<ManagementState> for AppState {
    fn from_ref(state: &ManagementState) -> Self {
        state.app.clone()
    }
}

pub(crate) fn public_router(state: AppState) -> Router {
    Router::new()
        .route("/health/live", get(live_health))
        .route("/health/ready", get(ready_health))
        .route("/health/metadata", get(agent_metadata))
        .route("/media/{*path}", get(upload::serve_media_file))
        .with_state(state)
}

pub(crate) fn management_router(state: ManagementState) -> Router {
    let upload_body_limit =
        usize::try_from(multipart_request_limit(state.app.upload.max_bytes)).unwrap_or(usize::MAX);
    let protected_write_routes = Router::new()
        .route(
            "/internal/uploads/media",
            post(management_upload).layer(DefaultBodyLimit::max(upload_body_limit)),
        )
        .route("/internal/uploads/media/{*path}", delete(management_delete))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            authorize_and_admit,
        ));

    Router::new()
        .route("/internal/health/ready", get(ready_health))
        .merge(protected_write_routes)
        .with_state(state)
}

pub(crate) fn build_management_tls_config(
    server_certificate_pem: &[u8],
    server_private_key_pem: &[u8],
    core_client_ca_pem: &[u8],
) -> anyhow::Result<Arc<ServerConfig>> {
    let certificates = CertificateDer::pem_slice_iter(server_certificate_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| anyhow::anyhow!("management server certificate PEM is invalid"))?;
    anyhow::ensure!(
        !certificates.is_empty(),
        "management server certificate PEM is empty"
    );
    let mut keys = PrivateKeyDer::pem_slice_iter(server_private_key_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| anyhow::anyhow!("management server private key PEM is invalid"))?;
    anyhow::ensure!(
        keys.len() == 1,
        "management server private key PEM must contain exactly one key"
    );
    let private_key = keys.remove(0);

    let ca_certificates = CertificateDer::pem_slice_iter(core_client_ca_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| anyhow::anyhow!("Core client CA PEM is invalid"))?;
    anyhow::ensure!(
        ca_certificates.len() == 1,
        "Core client CA PEM must contain exactly one dedicated root"
    );
    let mut roots = RootCertStore::empty();
    for certificate in ca_certificates {
        roots
            .add(certificate)
            .map_err(|_| anyhow::anyhow!("Core client CA certificate is invalid"))?;
    }
    let verifier = Arc::new(CoreClientCertificateVerifier::new(roots)?);
    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificates, private_key)
        .map_err(|error| anyhow::anyhow!("management server certificate/key mismatch: {error}"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

async fn management_upload(
    State(state): State<ManagementState>,
    Extension(capability): Extension<VerifiedCapability>,
    headers: axum::http::HeaderMap,
    multipart: Multipart,
) -> Response {
    let expected_prefix = format!("uploads/{}/", state.app.node_id);
    if capability.authorize_path(&expected_prefix).is_err() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut app = state.app.clone();
    app.upload.max_bytes = app.upload.max_bytes.min(capability.max_bytes);
    match upload::upload_media(State(app), headers, multipart).await {
        Ok(response) => response.into_response(),
        Err(error) => error.into_response(),
    }
}

async fn management_delete(
    State(state): State<ManagementState>,
    Extension(capability): Extension<VerifiedCapability>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let relative = match upload::normalize_media_relative_path(&path) {
        Ok(relative) => relative,
        Err(error) => return error.into_response(),
    };
    let normalized_path = upload::path_to_url(&relative);
    if capability.authorize_path(&normalized_path).is_err() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let target = state.app.upload.work_root.join(&relative);
    match tokio::fs::symlink_metadata(&target).await {
        Ok(metadata) if metadata.len() > capability.max_bytes => {
            return StatusCode::FORBIDDEN.into_response();
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    match state.delete_jti_store().consume(
        capability.jti,
        state.app.node_id,
        &normalized_path,
        capability.expires_at,
        chrono::Utc::now(),
    ) {
        Ok(()) => {}
        Err(DeleteJtiError::Replay) => return StatusCode::CONFLICT.into_response(),
        Err(DeleteJtiError::ExpiredCapability) => {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Err(DeleteJtiError::Capacity | DeleteJtiError::ClockRollback) => {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Err(DeleteJtiError::Io(_) | DeleteJtiError::Encoding(_)) => {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    match upload::delete_media_file(State(state.app), AxumPath(path)).await {
        Ok(status) => status.into_response(),
        Err(error) => error.into_response(),
    }
}

async fn authorize_and_admit(
    State(state): State<ManagementState>,
    mut request: Request,
    next: Next,
) -> Response {
    let operation = match *request.method() {
        Method::POST => AgentWriteOperation::Upload,
        Method::DELETE => AgentWriteOperation::Delete,
        _ => return StatusCode::METHOD_NOT_ALLOWED.into_response(),
    };
    let Some(token) = bearer_token(request.headers()) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let capability = match state.verifier.verify_operation(
        token,
        operation,
        state.app.upload.max_bytes,
        chrono::Utc::now(),
    ) {
        Ok(capability) => capability,
        Err(ManagementAuthError::InvalidCapability) => {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Err(ManagementAuthError::ForbiddenCapability) => {
            return StatusCode::FORBIDDEN.into_response();
        }
    };
    let request_limit = (operation == AgentWriteOperation::Upload)
        .then(|| multipart_request_limit(capability.max_bytes.min(state.app.upload.max_bytes)));
    if let Some(request_limit) = request_limit {
        if let Some(length) = request.headers().get(header::CONTENT_LENGTH) {
            let Some(length) = length
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
            else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            if length > request_limit {
                return StatusCode::PAYLOAD_TOO_LARGE.into_response();
            }
        }
    }
    let Ok(_permit) = state.admission.try_enter() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if let Some(request_limit) = request_limit {
        bound_upload_request_body(
            &mut request,
            state.app.upload.chunk_idle_timeout,
            request_limit,
        );
    }
    request.extensions_mut().insert(capability);
    next.run(request).await
}

fn multipart_request_limit(file_limit: u64) -> u64 {
    file_limit.saturating_add(MAX_MULTIPART_FRAMING_BYTES)
}

fn bound_upload_request_body(request: &mut Request, idle_timeout: std::time::Duration, max: u64) {
    let body = std::mem::replace(request.body_mut(), Body::empty());
    let mut observed = 0_u64;
    let stream = body
        .into_data_stream()
        .timeout(idle_timeout)
        .map(move |result| match result {
            Ok(Ok(bytes)) => {
                observed = observed.saturating_add(bytes.len() as u64);
                if observed > max {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "multipart request body exceeds the total size limit",
                    ))
                } else {
                    Ok(bytes)
                }
            }
            Ok(Err(error)) => Err(io::Error::other(error)),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "multipart request body exceeded the chunk idle timeout",
            )),
        });
    *request.body_mut() = Body::from_stream(stream);
}

fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?;
    (!token.is_empty() && token.trim() == token && !token.contains(char::is_whitespace))
        .then_some(token)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        convert::Infallible,
        io::Cursor,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{
        body::{Body, Bytes},
        http::{Request, StatusCode, header},
    };
    use chrono::{Duration as ChronoDuration, Utc};
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose, PKCS_ED25519, SanType,
    };
    use rustls::{ClientConfig, ClientConnection, ServerConnection};
    use rustls_pki_types::{PrivateKeyDer, ServerName};
    use tempfile::tempdir;
    use tokio_stream::{StreamExt, once};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::{
        AgentReadiness, AppState,
        management_auth::{
            AgentWriteCapabilityClaims, CAPABILITY_ISSUER, CAPABILITY_SUBJECT,
            CAPABILITY_TOKEN_TYPE, CapabilityVerifier,
        },
        upload::UploadConfig,
    };

    fn app_state(work_root: &std::path::Path) -> AppState {
        AppState {
            started_at: chrono::Utc::now(),
            environment: "production".to_string(),
            readiness: AgentReadiness {
                ffmpeg_available: true,
                ffprobe_available: true,
                work_root_exists: true,
                zlm_hook_listener_available: true,
            },
            node_id: Uuid::now_v7(),
            upload: UploadConfig {
                work_root: work_root.to_path_buf(),
                max_bytes: 1024 * 1024,
                allowed_extensions: BTreeSet::from(["mp4".to_string()]),
                probe_timeout: Duration::from_secs(1),
                ffprobe_bin: "/definitely/missing/ffprobe".to_string(),
                public_media_base_url: None,
                chunk_idle_timeout: Duration::from_millis(50),
            },
        }
    }

    fn management_state(work_root: &std::path::Path) -> (ManagementState, EncodingKey, Uuid) {
        let app = app_state(work_root);
        let node_id = app.node_id;
        let key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let private_key = EncodingKey::from_ed_pem(key.serialize_pem().as_bytes()).unwrap();
        let verifier = CapabilityVerifier::from_ed25519_public_pem(
            &key.public_key_pem(),
            "test-kid",
            app.node_id,
        )
        .unwrap();
        (
            ManagementState::new(app, verifier, work_root.join("delete-jti"), 4).unwrap(),
            private_key,
            node_id,
        )
    }

    fn capability_token(key: &EncodingKey, node_id: Uuid, operation: &str, path: &str) -> String {
        let now = Utc::now();
        let claims = AgentWriteCapabilityClaims {
            iss: CAPABILITY_ISSUER.to_string(),
            sub: CAPABILITY_SUBJECT.to_string(),
            aud: format!("agent:{node_id}"),
            op: operation.to_string(),
            path: path.to_string(),
            max_bytes: 1024,
            jti: Uuid::now_v7().to_string(),
            iat: now.timestamp(),
            nbf: now.timestamp(),
            exp: (now + ChronoDuration::seconds(60)).timestamp(),
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(CAPABILITY_TOKEN_TYPE.to_string());
        header.kid = Some("test-kid".to_string());
        encode(&header, &claims, key).unwrap()
    }

    fn management_tls_material() -> (String, String, String) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca = ca_params.self_signed(&ca_key).unwrap();

        let server_key = KeyPair::generate().unwrap();
        let mut server_params = CertificateParams::default();
        server_params.distinguished_name = DistinguishedName::new();
        server_params.is_ca = IsCa::NoCa;
        server_params.subject_alt_names = vec![SanType::DnsName("localhost".try_into().unwrap())];
        server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server = server_params.signed_by(&server_key, &ca, &ca_key).unwrap();
        (server.pem(), server_key.serialize_pem(), ca.pem())
    }

    struct TestAuthority {
        certificate: rcgen::Certificate,
        key: KeyPair,
    }

    fn test_authority() -> TestAuthority {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        TestAuthority {
            certificate: params.self_signed(&key).unwrap(),
            key,
        }
    }

    fn signed_server(authority: &TestAuthority) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.is_ca = IsCa::NoCa;
        params.subject_alt_names = vec![SanType::DnsName("localhost".try_into().unwrap())];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params
            .signed_by(&key, &authority.certificate, &authority.key)
            .unwrap();
        (certificate.pem(), key.serialize_pem())
    }

    fn signed_core_client(authority: &TestAuthority, core_id: Uuid) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.is_ca = IsCa::NoCa;
        params.subject_alt_names = vec![SanType::URI(
            format!("spiffe://streamserver/core/{core_id}")
                .try_into()
                .unwrap(),
        )];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let certificate = params
            .signed_by(&key, &authority.certificate, &authority.key)
            .unwrap();
        (certificate.pem(), key.serialize_pem())
    }

    fn tls_client_config(
        server_ca: &TestAuthority,
        certificate_chain_pem: &[String],
        private_key_pem: &str,
    ) -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(server_ca.certificate.der().to_vec()))
            .unwrap();
        let certificates = certificate_chain_pem
            .iter()
            .flat_map(|pem| CertificateDer::pem_slice_iter(pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let mut private_keys = PrivateKeyDer::pem_slice_iter(private_key_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let mut config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(certificates, private_keys.remove(0))
            .unwrap();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Arc::new(config)
    }

    fn complete_tls_handshake(
        server_config: Arc<ServerConfig>,
        client_config: Arc<ClientConfig>,
    ) -> Result<(), String> {
        let server_name = ServerName::try_from("localhost").unwrap().to_owned();
        let mut server = ServerConnection::new(server_config).map_err(|error| error.to_string())?;
        let mut client =
            ClientConnection::new(client_config, server_name).map_err(|error| error.to_string())?;
        for _ in 0..32 {
            if client.wants_write() {
                let mut bytes = Vec::new();
                client
                    .write_tls(&mut bytes)
                    .map_err(|error| error.to_string())?;
                server
                    .read_tls(&mut Cursor::new(bytes))
                    .map_err(|error| error.to_string())?;
                server
                    .process_new_packets()
                    .map_err(|error| error.to_string())?;
            }
            if server.wants_write() {
                let mut bytes = Vec::new();
                server
                    .write_tls(&mut bytes)
                    .map_err(|error| error.to_string())?;
                client
                    .read_tls(&mut Cursor::new(bytes))
                    .map_err(|error| error.to_string())?;
                client
                    .process_new_packets()
                    .map_err(|error| error.to_string())?;
            }
            if !server.is_handshaking() && !client.is_handshaking() {
                return Ok(());
            }
        }
        Err("TLS handshake did not complete".to_string())
    }

    #[test]
    fn management_tls_config_is_mtls_only_and_rejects_empty_ca() {
        let (certificate, key, ca) = management_tls_material();
        let config =
            build_management_tls_config(certificate.as_bytes(), key.as_bytes(), ca.as_bytes())
                .unwrap();

        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        assert!(build_management_tls_config(certificate.as_bytes(), key.as_bytes(), b"").is_err());
        let duplicated_ca = format!("{ca}{ca}");
        assert!(
            build_management_tls_config(
                certificate.as_bytes(),
                key.as_bytes(),
                duplicated_ca.as_bytes(),
            )
            .is_err(),
            "management listener must trust exactly one dedicated client root"
        );
    }

    #[test]
    fn management_tls_handshake_accepts_only_dedicated_direct_core_identity() {
        let server_ca = test_authority();
        let management_client_ca = test_authority();
        let agent_issuer_ca = test_authority();
        let control_server_ca = test_authority();
        let (server_certificate, server_key) = signed_server(&server_ca);
        let server_config = build_management_tls_config(
            server_certificate.as_bytes(),
            server_key.as_bytes(),
            management_client_ca.certificate.pem().as_bytes(),
        )
        .unwrap();
        let core_id = Uuid::now_v7();

        let (valid_certificate, valid_key) = signed_core_client(&management_client_ca, core_id);
        assert!(
            complete_tls_handshake(
                server_config.clone(),
                tls_client_config(&server_ca, &[valid_certificate], &valid_key),
            )
            .is_ok()
        );

        for untrusted_authority in [&agent_issuer_ca, &control_server_ca] {
            let (certificate, key) = signed_core_client(untrusted_authority, core_id);
            assert!(
                complete_tls_handshake(
                    server_config.clone(),
                    tls_client_config(&server_ca, &[certificate], &key),
                )
                .is_err()
            );
        }

        let intermediate_key = KeyPair::generate().unwrap();
        let mut intermediate_params = CertificateParams::default();
        intermediate_params.distinguished_name = DistinguishedName::new();
        intermediate_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        intermediate_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let intermediate_certificate = intermediate_params
            .signed_by(
                &intermediate_key,
                &management_client_ca.certificate,
                &management_client_ca.key,
            )
            .unwrap();
        let intermediate_authority = TestAuthority {
            certificate: intermediate_certificate,
            key: intermediate_key,
        };
        let (delegated_leaf, delegated_key) = signed_core_client(&intermediate_authority, core_id);
        assert!(
            complete_tls_handshake(
                server_config,
                tls_client_config(
                    &server_ca,
                    &[delegated_leaf, intermediate_authority.certificate.pem()],
                    &delegated_key,
                ),
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn public_router_never_mounts_internal_write_routes() {
        let temp = tempdir().unwrap();
        let response = public_router(app_state(temp.path()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/uploads/media")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn management_readiness_is_available_after_mtls_without_capability_jwt() {
        let temp = tempdir().unwrap();
        let (state, _, _) = management_state(temp.path());
        let response = management_router(state)
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/internal/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn management_auth_rejects_before_polling_request_body() {
        let temp = tempdir().unwrap();
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let stream =
            once(Ok::<Bytes, Infallible>(Bytes::from_static(b"untrusted"))).map(move |chunk| {
                observed.fetch_add(1, Ordering::SeqCst);
                chunk
            });
        let (state, _, _) = management_state(temp.path());
        let admission = state.admission.clone();
        let response = management_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/uploads/media")
                    .header(
                        header::CONTENT_TYPE,
                        "multipart/form-data; boundary=streamserver",
                    )
                    .body(Body::from_stream(stream))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(admission.semaphore.available_permits(), 4);
    }

    #[tokio::test]
    async fn upload_path_scope_is_rejected_before_polling_body() {
        let temp = tempdir().unwrap();
        let (state, key, node_id) = management_state(temp.path());
        let token = capability_token(&key, node_id, "upload", "uploads/wrong-node/");
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let stream =
            once(Ok::<Bytes, Infallible>(Bytes::from_static(b"untrusted"))).map(move |chunk| {
                observed.fetch_add(1, Ordering::SeqCst);
                chunk
            });
        let response = management_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/uploads/media")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(
                        header::CONTENT_TYPE,
                        "multipart/form-data; boundary=streamserver",
                    )
                    .body(Body::from_stream(stream))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn multipart_total_limit_counts_preamble_and_non_file_fields() {
        let temp = tempdir().unwrap();
        let (state, key, node_id) = management_state(temp.path());
        let token = capability_token(&key, node_id, "upload", &format!("uploads/{node_id}/"));
        let oversized_preamble = vec![b'x'; 1024 * 1024 + 64 * 1024 + 1];
        let response = management_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/uploads/media")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(
                        header::CONTENT_TYPE,
                        "multipart/form-data; boundary=streamserver",
                    )
                    .header(header::CONTENT_LENGTH, oversized_preamble.len())
                    .body(Body::from(oversized_preamble))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn chunked_multipart_total_limit_covers_a_non_file_field_before_its_boundary() {
        let temp = tempdir().unwrap();
        let (state, key, node_id) = management_state(temp.path());
        let token = capability_token(&key, node_id, "upload", &format!("uploads/{node_id}/"));
        let mut oversized =
            b"--streamserver\r\nContent-Disposition: form-data; name=\"metadata\"\r\n\r\n".to_vec();
        oversized.resize(1024 * 1024 + 64 * 1024 + 1, b'x');
        let prefix = once(Ok::<Bytes, Infallible>(Bytes::from(oversized)));
        let stalled = tokio_stream::pending::<Result<Bytes, Infallible>>();
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            management_router(state).oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/uploads/media")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(
                        header::CONTENT_TYPE,
                        "multipart/form-data; boundary=streamserver",
                    )
                    .body(Body::from_stream(prefix.chain(stalled)))
                    .unwrap(),
            ),
        )
        .await
        .expect("chunked non-file field bypassed the multipart total limit")
        .unwrap();
        assert!(!response.status().is_success());
    }

    #[tokio::test]
    async fn multipart_idle_timeout_starts_before_the_first_file_field() {
        let temp = tempdir().unwrap();
        let (state, key, node_id) = management_state(temp.path());
        let token = capability_token(&key, node_id, "upload", &format!("uploads/{node_id}/"));
        let prefix = once(Ok::<Bytes, Infallible>(Bytes::from_static(
            b"--streamserver\r\nContent-Disposition: form-data; name=\"metadata\"\r\n\r\npartial",
        )));
        let stalled = tokio_stream::pending::<Result<Bytes, Infallible>>();
        let body = Body::from_stream(prefix.chain(stalled));
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            management_router(state).oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/uploads/media")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(
                        header::CONTENT_TYPE,
                        "multipart/form-data; boundary=streamserver",
                    )
                    .body(body)
                    .unwrap(),
            ),
        )
        .await
        .expect("multipart preamble was not covered by the idle timeout")
        .unwrap();
        assert!(!response.status().is_success());
    }

    #[tokio::test]
    async fn delete_capability_jti_is_consumed_before_file_mutation_and_replay_is_409() {
        let temp = tempdir().unwrap();
        let (state, key, node_id) = management_state(temp.path());
        let relative = format!("uploads/{node_id}/clip.mp4");
        let target = temp.path().join(&relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"media").unwrap();
        let token = capability_token(&key, node_id, "delete", &relative);
        let router = management_router(state);

        let request = || {
            Request::builder()
                .method("DELETE")
                .uri(format!("/internal/uploads/media/{relative}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap()
        };
        let first = router.clone().oneshot(request()).await.unwrap();
        let replay = router.oneshot(request()).await.unwrap();

        assert_eq!(first.status(), StatusCode::NO_CONTENT);
        assert_eq!(replay.status(), StatusCode::CONFLICT);
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn delete_capability_expiring_after_authorization_returns_401() {
        let temp = tempdir().unwrap();
        let (state, key, node_id) = management_state(temp.path());
        let now = Utc::now();
        let high_watermark = now + ChronoDuration::seconds(4);
        state
            .delete_jti_store()
            .consume(
                Uuid::now_v7(),
                node_id,
                "uploads/advance-clock.mp4",
                high_watermark + ChronoDuration::seconds(60),
                high_watermark,
            )
            .unwrap();
        let relative = format!("uploads/{node_id}/expired-between-checks.mp4");
        let exp = (high_watermark - ChronoDuration::seconds(5)).timestamp();
        let claims = AgentWriteCapabilityClaims {
            iss: CAPABILITY_ISSUER.to_string(),
            sub: CAPABILITY_SUBJECT.to_string(),
            aud: format!("agent:{node_id}"),
            op: "delete".to_string(),
            path: relative.clone(),
            max_bytes: 1024,
            jti: Uuid::now_v7().to_string(),
            iat: exp - 60,
            nbf: exp - 60,
            exp,
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(CAPABILITY_TOKEN_TYPE.to_string());
        header.kid = Some("test-kid".to_string());
        let token = encode(&header, &claims, &key).unwrap();

        let response = management_router(state)
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/internal/uploads/media/{relative}"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn fifth_concurrent_management_request_is_rejected_with_503() {
        let admission = ManagementAdmission::new(4).unwrap();
        let guards = (0..4)
            .map(|_| admission.try_enter().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            admission.try_enter().unwrap_err(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop(guards);
        assert!(admission.try_enter().is_ok());
    }
}
