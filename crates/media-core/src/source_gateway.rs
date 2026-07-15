use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use media_domain::{InputKind, SourceMode, TaskSpec};
use reqwest::{Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{sync::Mutex, time::sleep};
use tracing::warn;
use uuid::Uuid;

use crate::config::CoreSettings;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GatewayAction {
    Relay {
        task_id: Uuid,
        source_url: String,
        source_kind: InputKind,
    },
    Prefetch {
        task_id: Uuid,
        source_url: String,
        target_path: String,
        source_kind: InputKind,
        start_offset_sec: Option<u32>,
        duration_sec: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GatewayActionResult {
    Relay { relay_url: String },
    Prefetch { source_url: String },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GatewayPreparation {
    NotRequired,
    Pending { poll_after: Duration },
    Ready(Box<TaskSpec>),
}

#[derive(Debug, Error)]
pub(crate) enum SourceGatewayError {
    #[error("{0}")]
    InvalidSpec(String),
    #[error("gateway action/result mismatch")]
    ActionMismatch,
    #[error("source gateway request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("source gateway rejected task: {0}")]
    Rejected(String),
}

#[derive(Debug, Clone)]
pub(crate) struct SourceGatewayClient {
    http: reqwest::Client,
    base_url: Url,
    prefetch_poll_interval: Duration,
    submitted_prefetches: Arc<Mutex<HashSet<Uuid>>>,
    prefetch_poll_hints: Arc<Mutex<HashMap<Uuid, Duration>>>,
}

#[derive(Debug, Serialize)]
struct RelayRequest {
    task_id: Uuid,
    source_url: String,
    source_kind: InputKind,
}

#[derive(Debug, Deserialize)]
struct RelayResponse {
    relay_url: String,
}

#[derive(Debug, Serialize)]
struct PrefetchRequest {
    task_id: Uuid,
    source_url: String,
    target_path: String,
    source_kind: InputKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_offset_sec: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_sec: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct PrefetchResponse {
    status: String,
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    poll_after_ms: Option<u64>,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    failure_reason: Option<String>,
    #[serde(default)]
    time_slice_applied: bool,
}

impl SourceGatewayClient {
    pub(crate) fn from_settings(
        settings: &CoreSettings,
    ) -> Result<Option<Self>, SourceGatewayError> {
        if settings.source_gateway_base_url.trim().is_empty() {
            return Ok(None);
        }
        Self::new(
            &settings.source_gateway_base_url,
            settings.source_gateway_tls_insecure_skip_verify,
            Duration::from_millis(settings.source_gateway_prefetch_poll_ms),
            Duration::from_millis(settings.source_gateway_prefetch_timeout_ms),
        )
        .map(Some)
    }

    pub(crate) fn new(
        base_url: &str,
        tls_insecure_skip_verify: bool,
        prefetch_poll_interval: Duration,
        prefetch_timeout: Duration,
    ) -> Result<Self, SourceGatewayError> {
        Self::build(
            base_url,
            tls_insecure_skip_verify,
            true,
            prefetch_poll_interval,
            prefetch_timeout,
        )
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(base_url: &str) -> Result<Self, SourceGatewayError> {
        Self::build(
            base_url,
            false,
            false,
            Duration::from_millis(10),
            Duration::from_secs(2),
        )
    }

    fn build(
        base_url: &str,
        tls_insecure_skip_verify: bool,
        require_https: bool,
        prefetch_poll_interval: Duration,
        _prefetch_timeout: Duration,
    ) -> Result<Self, SourceGatewayError> {
        let base_url = normalize_base_url(base_url, require_https)?;
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none());
        if require_https {
            builder = builder.https_only(true);
        }
        if tls_insecure_skip_verify {
            builder = builder
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true);
            let host = base_url.host_str().unwrap_or("unknown-host");
            warn!("SOURCE_GATEWAY TLS verification is disabled for {host}");
        }
        Ok(Self {
            http: builder.build()?,
            base_url,
            prefetch_poll_interval,
            submitted_prefetches: Arc::new(Mutex::new(HashSet::new())),
            prefetch_poll_hints: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    #[cfg(test)]
    pub(crate) async fn prepare_task_spec(
        &self,
        task_id: Uuid,
        spec: &TaskSpec,
    ) -> Result<Option<TaskSpec>, SourceGatewayError> {
        match self.prepare_task_spec_once(task_id, spec).await? {
            GatewayPreparation::NotRequired | GatewayPreparation::Pending { .. } => Ok(None),
            GatewayPreparation::Ready(spec) => Ok(Some(*spec)),
        }
    }

    pub(crate) async fn prepare_task_spec_once(
        &self,
        task_id: Uuid,
        spec: &TaskSpec,
    ) -> Result<GatewayPreparation, SourceGatewayError> {
        let Some(action) = plan_gateway_action(spec, task_id) else {
            return Ok(GatewayPreparation::NotRequired);
        };
        let Some(result) = self.execute_action(&action).await? else {
            let poll_after = self.prefetch_poll_after(task_id).await?;
            return Ok(GatewayPreparation::Pending { poll_after });
        };
        let mut rewritten = spec.clone();
        apply_gateway_result(&mut rewritten, action, result)?;
        rewritten
            .validate()
            .map_err(|error| SourceGatewayError::InvalidSpec(error.to_string()))?;
        Ok(GatewayPreparation::Ready(Box::new(rewritten)))
    }

    pub(crate) async fn cancel_task(&self, task_id: Uuid) -> Result<(), SourceGatewayError> {
        let endpoint = self.endpoint(&format!("/api/tasks/{task_id}"))?;
        let mut last_error = None;
        for attempt in 1..=3 {
            match self.http.delete(endpoint.clone()).send().await {
                Ok(response)
                    if response.status().is_success()
                        || response.status() == reqwest::StatusCode::NOT_FOUND =>
                {
                    self.submitted_prefetches.lock().await.remove(&task_id);
                    self.prefetch_poll_hints.lock().await.remove(&task_id);
                    return Ok(());
                }
                Ok(response) => {
                    last_error = Some(format!("gateway cancel returned {}", response.status()));
                }
                Err(error) => last_error = Some(error.to_string()),
            }
            if attempt < 3 {
                sleep(Duration::from_millis(250)).await;
            }
        }
        Err(SourceGatewayError::Rejected(
            last_error.unwrap_or_else(|| "gateway cancel failed".to_string()),
        ))
    }

    pub(crate) async fn reset_task(&self, task_id: Uuid) -> Result<(), SourceGatewayError> {
        let response = self
            .http
            .post(self.endpoint(&format!("/api/tasks/{task_id}/reset"))?)
            .send()
            .await?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            self.submitted_prefetches.lock().await.remove(&task_id);
            self.prefetch_poll_hints.lock().await.remove(&task_id);
            Ok(())
        } else {
            Err(SourceGatewayError::Rejected(format!(
                "gateway reset returned {}",
                response.status()
            )))
        }
    }

    async fn execute_action(
        &self,
        action: &GatewayAction,
    ) -> Result<Option<GatewayActionResult>, SourceGatewayError> {
        match action {
            GatewayAction::Relay {
                task_id,
                source_url,
                source_kind,
            } => {
                let response: RelayResponse = self
                    .http
                    .post(self.endpoint("/api/relays")?)
                    .json(&RelayRequest {
                        task_id: *task_id,
                        source_url: source_url.clone(),
                        source_kind: *source_kind,
                    })
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                Ok(Some(GatewayActionResult::Relay {
                    relay_url: response.relay_url,
                }))
            }
            GatewayAction::Prefetch {
                task_id,
                source_url,
                target_path,
                source_kind,
                start_offset_sec,
                duration_sec,
            } => {
                let submitted = self.submitted_prefetches.lock().await.contains(task_id);
                let response = if submitted {
                    let response = self
                        .http
                        .get(self.endpoint(&format!("/api/prefetch/{task_id}"))?)
                        .send()
                        .await?;
                    if response.status() == reqwest::StatusCode::NOT_FOUND {
                        self.submitted_prefetches.lock().await.remove(task_id);
                        self.http
                            .post(self.endpoint("/api/prefetch")?)
                            .json(&PrefetchRequest {
                                task_id: *task_id,
                                source_url: source_url.clone(),
                                target_path: target_path.clone(),
                                source_kind: *source_kind,
                                start_offset_sec: *start_offset_sec,
                                duration_sec: *duration_sec,
                            })
                            .send()
                            .await?
                    } else {
                        response
                    }
                } else {
                    self.http
                        .post(self.endpoint("/api/prefetch")?)
                        .json(&PrefetchRequest {
                            task_id: *task_id,
                            source_url: source_url.clone(),
                            target_path: target_path.clone(),
                            source_kind: *source_kind,
                            start_offset_sec: *start_offset_sec,
                            duration_sec: *duration_sec,
                        })
                        .send()
                        .await?
                };
                let response: PrefetchResponse = response.error_for_status()?.json().await?;
                self.submitted_prefetches.lock().await.insert(*task_id);
                let time_slice_requested = start_offset_sec.is_some() || duration_sec.is_some();
                self.map_prefetch_response(*task_id, response, time_slice_requested)
                    .await
            }
        }
    }

    async fn map_prefetch_response(
        &self,
        task_id: Uuid,
        response: PrefetchResponse,
        time_slice_requested: bool,
    ) -> Result<Option<GatewayActionResult>, SourceGatewayError> {
        match response.status.as_str() {
            "ready" => {
                if time_slice_requested && !response.time_slice_applied {
                    return Err(SourceGatewayError::Rejected(
                        "ready prefetch response did not attest the requested time slice"
                            .to_string(),
                    ));
                }
                let source_url = response.source_url.ok_or_else(|| {
                    SourceGatewayError::Rejected(
                        "ready prefetch response is missing source_url".to_string(),
                    )
                })?;
                self.submitted_prefetches.lock().await.remove(&task_id);
                self.prefetch_poll_hints.lock().await.remove(&task_id);
                Ok(Some(GatewayActionResult::Prefetch { source_url }))
            }
            "failed" | "canceled" => {
                self.submitted_prefetches.lock().await.remove(&task_id);
                self.prefetch_poll_hints.lock().await.remove(&task_id);
                Err(SourceGatewayError::Rejected(
                    response
                        .failure_reason
                        .unwrap_or_else(|| format!("prefetch {}", response.status)),
                ))
            }
            "pending" => {
                let poll_after = response
                    .poll_after_ms
                    .map(Duration::from_millis)
                    .unwrap_or_else(|| match response.phase.as_deref() {
                        Some("queued") => Duration::from_secs(30),
                        _ => self.prefetch_poll_interval,
                    });
                self.prefetch_poll_hints
                    .lock()
                    .await
                    .insert(task_id, poll_after);
                Ok(None)
            }
            status => Err(SourceGatewayError::Rejected(format!(
                "unknown prefetch status {status}"
            ))),
        }
    }

    async fn prefetch_poll_after(&self, task_id: Uuid) -> Result<Duration, SourceGatewayError> {
        Ok(self
            .prefetch_poll_hints
            .lock()
            .await
            .get(&task_id)
            .copied()
            .unwrap_or(self.prefetch_poll_interval))
    }

    fn endpoint(&self, path: &str) -> Result<Url, SourceGatewayError> {
        let relative_path = path.strip_prefix('/').unwrap_or(path);
        if relative_path.is_empty()
            || !relative_path.starts_with("api/")
            || relative_path.starts_with("//")
            || relative_path.contains(['\\', '?', '#'])
            || relative_path.split('/').any(|segment| segment == "..")
        {
            return Err(SourceGatewayError::InvalidSpec(format!(
                "invalid source gateway endpoint {path}"
            )));
        }
        let endpoint = self.base_url.join(relative_path).map_err(|error| {
            SourceGatewayError::InvalidSpec(format!(
                "invalid source gateway endpoint {path}: {error}"
            ))
        })?;
        if endpoint.origin() != self.base_url.origin()
            || !endpoint.path().starts_with(self.base_url.path())
        {
            return Err(SourceGatewayError::InvalidSpec(format!(
                "source gateway endpoint escaped configured base url: {path}"
            )));
        }
        Ok(endpoint)
    }
}

fn normalize_base_url(base_url: &str, require_https: bool) -> Result<Url, SourceGatewayError> {
    let mut base_url = Url::parse(base_url.trim()).map_err(|error| {
        SourceGatewayError::InvalidSpec(format!("invalid source gateway url: {error}"))
    })?;
    if require_https && base_url.scheme() != "https" {
        return Err(SourceGatewayError::InvalidSpec(
            "source gateway base url must use https".to_string(),
        ));
    }
    if base_url.host_str().is_none() {
        return Err(SourceGatewayError::InvalidSpec(
            "source gateway base url must include a host".to_string(),
        ));
    }
    if !base_url.username().is_empty() || base_url.password().is_some() {
        return Err(SourceGatewayError::InvalidSpec(
            "source gateway base url must not include credentials".to_string(),
        ));
    }
    if base_url.query().is_some() || base_url.fragment().is_some() {
        return Err(SourceGatewayError::InvalidSpec(
            "source gateway base url must not include a query or fragment".to_string(),
        ));
    }
    if !base_url.path().ends_with('/') {
        base_url = Url::parse(&format!("{base_url}/")).map_err(|error| {
            SourceGatewayError::InvalidSpec(format!(
                "failed to normalize source gateway base url: {error}"
            ))
        })?;
    }
    Ok(base_url)
}

pub(crate) fn plan_gateway_action(spec: &TaskSpec, task_id: Uuid) -> Option<GatewayAction> {
    let kind = spec.input.kind?;
    let source_url = spec.input.url.as_ref()?.trim();
    if !source_url.starts_with("http://") && !source_url.starts_with("https://") {
        return None;
    }

    match (kind, spec.input.source_mode) {
        (InputKind::HttpFlv, Some(SourceMode::Live))
        | (InputKind::HttpTs | InputKind::Hls, Some(SourceMode::Live)) => {
            Some(GatewayAction::Relay {
                task_id,
                source_url: source_url.to_string(),
                source_kind: kind,
            })
        }
        (InputKind::HttpMp4, Some(SourceMode::Vod))
        | (InputKind::HttpTs | InputKind::Hls, Some(SourceMode::Vod)) => {
            Some(GatewayAction::Prefetch {
                task_id,
                source_url: source_url.to_string(),
                target_path: default_prefetch_target_path(task_id, kind),
                source_kind: kind,
                start_offset_sec: spec.input.start_offset_sec.filter(|value| *value > 0),
                duration_sec: spec.record.duration_sec,
            })
        }
        _ => None,
    }
}

pub(crate) fn apply_gateway_result(
    spec: &mut TaskSpec,
    action: GatewayAction,
    result: GatewayActionResult,
) -> Result<(), SourceGatewayError> {
    match (action, result) {
        (GatewayAction::Relay { .. }, GatewayActionResult::Relay { relay_url }) => {
            if relay_url.trim().is_empty() {
                return Err(SourceGatewayError::InvalidSpec(
                    "relay_url must not be empty".to_string(),
                ));
            }
            spec.input.url = Some(relay_url);
        }
        (GatewayAction::Prefetch { .. }, GatewayActionResult::Prefetch { source_url }) => {
            if source_url.trim().is_empty() || source_url.starts_with("uploads/") {
                return Err(SourceGatewayError::InvalidSpec(
                    "prefetch source_url must be a non-upload relative path".to_string(),
                ));
            }
            spec.input.kind = Some(InputKind::File);
            spec.input.source_mode = Some(SourceMode::Vod);
            spec.input.url = Some(source_url);
        }
        _ => return Err(SourceGatewayError::ActionMismatch),
    }
    spec.input.start_offset_sec = None;
    Ok(())
}

fn default_prefetch_target_path(task_id: Uuid, kind: InputKind) -> String {
    let ext = match kind {
        InputKind::Hls => "m3u8",
        InputKind::HttpTs => "ts",
        InputKind::HttpMp4 => "mp4",
        _ => unreachable!("only HTTP VOD inputs use prefetch targets"),
    };
    format!("imports/{task_id}/source.{ext}")
}

#[cfg(test)]
mod tests {
    use std::{
        net::TcpListener as StdTcpListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::{
        Router,
        http::{StatusCode, header},
        routing::{any, get},
    };
    use axum_server::{Handle, tls_rustls::RustlsConfig};
    use rcgen::{CertificateParams, KeyPair};
    use rustls::ServerConfig;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use time::OffsetDateTime;
    use tokio::{task::JoinHandle, time::timeout};

    use super::*;

    #[test]
    fn endpoint_preserves_configured_gateway_path_prefix() -> anyhow::Result<()> {
        let client = SourceGatewayClient::new_for_test("http://172.21.26.25/bohui/media/")?;
        let task_id = Uuid::nil();

        for (path, expected) in [
            ("/api/relays", "http://172.21.26.25/bohui/media/api/relays"),
            (
                "/api/prefetch",
                "http://172.21.26.25/bohui/media/api/prefetch",
            ),
            (
                "/api/prefetch/00000000-0000-0000-0000-000000000000",
                "http://172.21.26.25/bohui/media/api/prefetch/00000000-0000-0000-0000-000000000000",
            ),
        ] {
            assert_eq!(client.endpoint(path)?.as_str(), expected);
        }
        assert_eq!(
            client.endpoint(&format!("/api/relays/{task_id}"))?.as_str(),
            "http://172.21.26.25/bohui/media/api/relays/00000000-0000-0000-0000-000000000000"
        );
        Ok(())
    }

    #[test]
    fn production_gateway_client_requires_https_and_fixed_internal_paths() -> anyhow::Result<()> {
        let error = SourceGatewayClient::new(
            "http://172.21.26.25/bohui/media/",
            false,
            Duration::from_secs(1),
            Duration::from_secs(2),
        )
        .expect_err("production Source Gateway must reject plaintext HTTP");
        assert!(error.to_string().contains("must use https"));

        let client = SourceGatewayClient::new_for_test("http://127.0.0.1/base/")?;
        for path in [
            "https://attacker.invalid/api/relays",
            "/../api/relays",
            "/relay/token",
            "/api/relays?target=https://attacker.invalid",
        ] {
            assert!(
                client.endpoint(path).is_err(),
                "accepted unsafe path {path}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn insecure_gateway_tls_switch_is_scoped_to_the_gateway_client() -> anyhow::Result<()> {
        let app = Router::new().route(
            "/bohui/media/api/healthz",
            get(|| async { StatusCode::NO_CONTENT }),
        );
        let (base_url, handle, server) = spawn_invalid_tls_gateway(app)?;

        let strict = SourceGatewayClient::new(
            &base_url,
            false,
            Duration::from_millis(10),
            Duration::from_secs(1),
        )?;
        assert!(
            strict
                .http
                .get(strict.endpoint("/api/healthz")?)
                .send()
                .await
                .is_err(),
            "strict Source Gateway client accepted an expired, untrusted and hostname-mismatched certificate"
        );

        let insecure = SourceGatewayClient::new(
            &base_url,
            true,
            Duration::from_millis(10),
            Duration::from_secs(1),
        )?;
        let response = insecure
            .http
            .get(insecure.endpoint("/api/healthz")?)
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        handle.graceful_shutdown(Some(Duration::from_secs(1)));
        timeout(Duration::from_secs(3), server).await??;
        Ok(())
    }

    #[tokio::test]
    async fn gateway_client_never_follows_redirects() -> anyhow::Result<()> {
        let attacker_hits = Arc::new(AtomicUsize::new(0));
        let hits = attacker_hits.clone();
        let attacker = Router::new().fallback(any(move || {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }
        }));
        let attacker_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let attacker_url = format!("http://{}/stolen", attacker_listener.local_addr()?);
        let attacker_server = tokio::spawn(async move {
            axum::serve(attacker_listener, attacker).await.unwrap();
        });

        let redirect_location = attacker_url.clone();
        let gateway = Router::new().route(
            "/bohui/media/api/healthz",
            get(move || {
                let redirect_location = redirect_location.clone();
                async move {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [(header::LOCATION, redirect_location)],
                    )
                }
            }),
        );
        let (base_url, handle, server) = spawn_invalid_tls_gateway(gateway)?;
        let client = SourceGatewayClient::new(
            &base_url,
            true,
            Duration::from_millis(10),
            Duration::from_secs(1),
        )?;
        let response = client
            .http
            .get(client.endpoint("/api/healthz")?)
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        tokio::task::yield_now().await;
        assert_eq!(attacker_hits.load(Ordering::SeqCst), 0);

        handle.graceful_shutdown(Some(Duration::from_secs(1)));
        timeout(Duration::from_secs(3), server).await??;
        attacker_server.abort();
        Ok(())
    }

    fn spawn_invalid_tls_gateway(
        app: Router,
    ) -> anyhow::Result<(String, Handle<std::net::SocketAddr>, JoinHandle<()>)> {
        let key_pair = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec!["gateway.invalid".to_string()])?;
        params.not_before = OffsetDateTime::from_unix_timestamp(1_577_836_800)?;
        params.not_after = OffsetDateTime::from_unix_timestamp(1_609_459_200)?;
        let certificate = params.self_signed(&key_pair)?;
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(certificate.der().to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der())),
            )?;
        let listener = StdTcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let handle = Handle::new();
        let server_handle = handle.clone();
        let server = tokio::spawn(async move {
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
        Ok((format!("https://{address}/bohui/media/"), handle, server))
    }

    #[tokio::test]
    async fn prefetch_submission_is_nonblocking_and_honors_gateway_poll_hints() -> anyhow::Result<()>
    {
        use axum::{Json, extract::State, routing::post};
        use serde_json::json;

        #[derive(Clone, Default)]
        struct StubState {
            posts: Arc<AtomicUsize>,
            gets: Arc<AtomicUsize>,
        }

        async fn submit(State(state): State<StubState>) -> Json<serde_json::Value> {
            state.posts.fetch_add(1, Ordering::SeqCst);
            Json(json!({
                "status": "pending",
                "phase": "queued",
                "queue_position": 126,
                "poll_after_ms": 30000,
                "time_slice_applied": false
            }))
        }

        async fn poll(State(state): State<StubState>) -> Json<serde_json::Value> {
            let call = state.gets.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Json(json!({
                    "status": "pending",
                    "phase": "running",
                    "poll_after_ms": 5000,
                    "time_slice_applied": false
                }))
            } else {
                Json(json!({
                    "status": "ready",
                    "source_url": "imports/00000000-0000-0000-0000-000000009001/source.mp4",
                    "time_slice_applied": false
                }))
            }
        }

        let stub = StubState::default();
        let app = Router::new()
            .route("/api/prefetch", post(submit))
            .route("/api/prefetch/{task_id}", get(poll))
            .with_state(stub.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = SourceGatewayClient::new_for_test(&base)?;
        let task_id = Uuid::from_u128(0x9001);
        let action = GatewayAction::Prefetch {
            task_id,
            source_url: "http://customer.example/source.mp4".to_string(),
            target_path: format!("imports/{task_id}/source.mp4"),
            source_kind: InputKind::HttpMp4,
            start_offset_sec: None,
            duration_sec: None,
        };

        let started = std::time::Instant::now();
        assert_eq!(client.execute_action(&action).await?, None);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            client.prefetch_poll_after(task_id).await?,
            Duration::from_secs(30)
        );
        assert_eq!(stub.posts.load(Ordering::SeqCst), 1);
        assert_eq!(stub.gets.load(Ordering::SeqCst), 0);

        assert_eq!(client.execute_action(&action).await?, None);
        assert_eq!(
            client.prefetch_poll_after(task_id).await?,
            Duration::from_secs(5)
        );
        assert_eq!(stub.posts.load(Ordering::SeqCst), 1);
        assert_eq!(stub.gets.load(Ordering::SeqCst), 1);

        assert_eq!(
            client.execute_action(&action).await?,
            Some(GatewayActionResult::Prefetch {
                source_url: format!("imports/{task_id}/source.mp4")
            })
        );
        assert_eq!(stub.posts.load(Ordering::SeqCst), 1);
        assert_eq!(stub.gets.load(Ordering::SeqCst), 2);
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn unified_gateway_cancel_retries_three_times_through_prefixed_base_url()
    -> anyhow::Result<()> {
        use axum::{extract::State, routing::delete};

        async fn cancel(State(calls): State<Arc<AtomicUsize>>) -> StatusCode {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::NO_CONTENT
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/bohui/media/api/tasks/{task_id}", delete(cancel))
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}/bohui/media/", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        SourceGatewayClient::new_for_test(&base)?
            .cancel_task(Uuid::from_u128(0x9002))
            .await?;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        server.abort();
        Ok(())
    }
}
