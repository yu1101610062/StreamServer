use std::{net::IpAddr, sync::Arc, time::Duration};

use media_rpc::control_plane::{
    ZlmDebugError, ZlmDebugOperation, ZlmDebugRequest, ZlmDebugResponse, ZlmDebugResponseStatus,
    ZlmSnapshotPayload, zlm_debug_request::Parameters, zlm_debug_response::Payload,
};
use reqwest::{Client, Url, redirect::Policy};
use tokio::sync::Semaphore;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::config::AgentSettings;

const ZLM_DEBUG_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ZLM_DEBUG_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const ZLM_DEBUG_MAX_CONCURRENCY: usize = 4;
const ZLM_DEBUG_MAX_JSON_BYTES: usize = 256 * 1024;
const ZLM_DEBUG_MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct ZlmDebugExecutor {
    client: Client,
    base_url: Url,
    secret: Arc<Zeroizing<String>>,
    concurrency: Arc<Semaphore>,
}

impl ZlmDebugExecutor {
    pub(crate) fn new(settings: &AgentSettings) -> anyhow::Result<Self> {
        Self::build(settings, ZLM_DEBUG_REQUEST_TIMEOUT)
    }

    fn build(settings: &AgentSettings, request_timeout: Duration) -> anyhow::Result<Self> {
        let base_url = Url::parse(settings.zlm_api_base.trim())?;
        anyhow::ensure!(
            base_url.scheme() == "http",
            "ZLM debug API must use local HTTP"
        );
        anyhow::ensure!(
            base_url.username().is_empty() && base_url.password().is_none(),
            "ZLM debug API base must not contain URL credentials"
        );
        anyhow::ensure!(
            base_url.path() == "/" && base_url.query().is_none() && base_url.fragment().is_none(),
            "ZLM debug API base must not contain a path, query, or fragment"
        );
        let host = base_url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("ZLM debug API must have an IP host"))?;
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host)
            .parse::<IpAddr>()?;
        anyhow::ensure!(host.is_loopback(), "ZLM debug API must use a loopback IP");
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(ZLM_DEBUG_CONNECT_TIMEOUT)
            .timeout(request_timeout)
            .build()?;
        Ok(Self {
            client,
            base_url,
            secret: Arc::new(Zeroizing::new(settings.zlm_api_secret.clone())),
            concurrency: Arc::new(Semaphore::new(ZLM_DEBUG_MAX_CONCURRENCY)),
        })
    }

    pub(crate) async fn execute(&self, request: ZlmDebugRequest) -> ZlmDebugResponse {
        let request_id = request.request_id.clone();
        let operation = request.operation;
        let plan = match plan_request(&request) {
            Ok(plan) => plan,
            Err(()) => {
                return failed(
                    request_id,
                    operation,
                    "INVALID_REQUEST",
                    "invalid ZLM debug request",
                    false,
                );
            }
        };
        let _permit = match self.concurrency.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return failed(
                    request_id,
                    operation,
                    "BUSY",
                    "too many local ZLM requests are active",
                    false,
                );
            }
        };

        let mut url = match self.base_url.join(plan.path) {
            Ok(url) => url,
            Err(_) => {
                return failed(
                    request_id,
                    operation,
                    "ZLM_UNAVAILABLE",
                    "local ZLM request failed",
                    false,
                );
            }
        };
        {
            let mut query = url.query_pairs_mut();
            if !self.secret.is_empty() {
                query.append_pair("secret", self.secret.as_str());
            }
            for (key, value) in &plan.parameters {
                query.append_pair(key, value);
            }
        }
        let mut response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(error) => {
                let (code, message) = if error.is_timeout() {
                    ("UPSTREAM_TIMEOUT", "local ZLM request timed out")
                } else {
                    ("ZLM_UNAVAILABLE", "local ZLM request failed")
                };
                return failed(request_id, operation, code, message, false);
            }
        };
        if !response.status().is_success() {
            return failed(
                request_id,
                operation,
                "UPSTREAM_HTTP_STATUS",
                "local ZLM returned an unsuccessful HTTP status",
                false,
            );
        }

        let limit = match plan.response_kind {
            ResponseKind::Json => ZLM_DEBUG_MAX_JSON_BYTES,
            ResponseKind::Snapshot => ZLM_DEBUG_MAX_SNAPSHOT_BYTES,
        };
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return failed(
                request_id,
                operation,
                "RESPONSE_TOO_LARGE",
                "local ZLM response exceeded the size limit",
                true,
            );
        }
        let content_type = if plan.response_kind == ResponseKind::Snapshot {
            match response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(safe_snapshot_content_type)
            {
                Some(value) => Some(value),
                None => {
                    return failed(
                        request_id,
                        operation,
                        "INVALID_UPSTREAM_RESPONSE",
                        "local ZLM returned an invalid response",
                        false,
                    );
                }
            }
        } else {
            None
        };
        let body = match read_bounded_body(&mut response, limit).await {
            Ok(body) => body,
            Err(ReadBodyError::TooLarge) => {
                return failed(
                    request_id,
                    operation,
                    "RESPONSE_TOO_LARGE",
                    "local ZLM response exceeded the size limit",
                    true,
                );
            }
            Err(ReadBodyError::Unavailable) => {
                return failed(
                    request_id,
                    operation,
                    "ZLM_UNAVAILABLE",
                    "local ZLM response failed",
                    false,
                );
            }
        };
        let payload = match plan.response_kind {
            ResponseKind::Json => match String::from_utf8(body) {
                Ok(body) if serde_json::from_str::<serde_json::Value>(&body).is_ok() => {
                    Payload::JsonPayload(body)
                }
                _ => {
                    return failed(
                        request_id,
                        operation,
                        "INVALID_UPSTREAM_RESPONSE",
                        "local ZLM returned an invalid response",
                        false,
                    );
                }
            },
            ResponseKind::Snapshot => Payload::Snapshot(ZlmSnapshotPayload {
                content_type: content_type.expect("snapshot content type was validated"),
                data: body,
            }),
        };
        succeeded(request_id, operation, payload)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseKind {
    Json,
    Snapshot,
}

struct RequestPlan {
    path: &'static str,
    parameters: Vec<(&'static str, String)>,
    response_kind: ResponseKind,
}

fn plan_request(request: &ZlmDebugRequest) -> Result<RequestPlan, ()> {
    let request_id = Uuid::parse_str(&request.request_id).map_err(|_| ())?;
    if request_id.is_nil() || request_id.to_string() != request.request_id {
        return Err(());
    }
    let operation = ZlmDebugOperation::try_from(request.operation).map_err(|_| ())?;
    let (path, parameters, response_kind) = match (operation, request.parameters.as_ref()) {
        (ZlmDebugOperation::ListMedia, Some(Parameters::MediaFilter(filter))) => {
            let mut parameters = Vec::new();
            append_optional(&mut parameters, "schema", &filter.schema, 512)?;
            append_optional(&mut parameters, "vhost", &filter.vhost, 512)?;
            append_optional(&mut parameters, "app", &filter.app, 512)?;
            append_optional(&mut parameters, "stream", &filter.stream, 512)?;
            ("/index/api/getMediaList", parameters, ResponseKind::Json)
        }
        (ZlmDebugOperation::ListSessions, None) => {
            ("/index/api/getAllSession", Vec::new(), ResponseKind::Json)
        }
        (ZlmDebugOperation::ListPlayers, None) => (
            "/index/api/getMediaPlayerList",
            Vec::new(),
            ResponseKind::Json,
        ),
        (ZlmDebugOperation::GetStatistic, None) => {
            ("/index/api/getStatistic", Vec::new(), ResponseKind::Json)
        }
        (ZlmDebugOperation::GetThreadsLoad, None) => {
            ("/index/api/getThreadsLoad", Vec::new(), ResponseKind::Json)
        }
        (ZlmDebugOperation::GetWorkThreadsLoad, None) => (
            "/index/api/getWorkThreadsLoad",
            Vec::new(),
            ResponseKind::Json,
        ),
        (ZlmDebugOperation::KickSession, Some(Parameters::KickSession(parameters))) => {
            validate_required(&parameters.session_id, 512)?;
            (
                "/index/api/kick_session",
                vec![("id", parameters.session_id.clone())],
                ResponseKind::Json,
            )
        }
        (ZlmDebugOperation::KickSessions, Some(Parameters::KickSessions(parameters))) => {
            if parameters.local_port > u32::from(u16::MAX) {
                return Err(());
            }
            let mut query = Vec::new();
            if parameters.local_port > 0 {
                query.push(("local_port", parameters.local_port.to_string()));
            }
            if !parameters.peer_ip.is_empty() {
                validate_required(&parameters.peer_ip, 128)?;
                parameters.peer_ip.parse::<IpAddr>().map_err(|_| ())?;
                query.push(("peer_ip", parameters.peer_ip.clone()));
            }
            ("/index/api/kick_sessions", query, ResponseKind::Json)
        }
        (ZlmDebugOperation::CloseStream, Some(Parameters::CloseStream(parameters))) => {
            for value in [
                &parameters.schema,
                &parameters.vhost,
                &parameters.app,
                &parameters.stream,
            ] {
                validate_required(value, 512)?;
            }
            (
                "/index/api/close_streams",
                vec![
                    ("schema", parameters.schema.clone()),
                    ("vhost", parameters.vhost.clone()),
                    ("app", parameters.app.clone()),
                    ("stream", parameters.stream.clone()),
                    ("force", parameters.force.to_string()),
                ],
                ResponseKind::Json,
            )
        }
        (ZlmDebugOperation::Snapshot, Some(Parameters::Snapshot(parameters))) => {
            validate_required(&parameters.source_url, 8192)?;
            if parameters.timeout_sec == 0
                || parameters.timeout_sec > 300
                || parameters.expire_sec == 0
                || parameters.expire_sec > 3600
            {
                return Err(());
            }
            (
                "/index/api/getSnap",
                vec![
                    ("url", parameters.source_url.clone()),
                    ("timeout_sec", parameters.timeout_sec.to_string()),
                    ("expire_sec", parameters.expire_sec.to_string()),
                ],
                ResponseKind::Snapshot,
            )
        }
        _ => return Err(()),
    };
    Ok(RequestPlan {
        path,
        parameters,
        response_kind,
    })
}

fn safe_snapshot_content_type(value: &str) -> Option<String> {
    if value != value.trim() {
        return None;
    }
    let normalized = value.to_ascii_lowercase();
    match normalized.as_str() {
        "image/jpeg" | "image/png" | "image/webp" | "image/gif" => Some(normalized),
        _ => None,
    }
}

fn append_optional(
    parameters: &mut Vec<(&'static str, String)>,
    key: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ()> {
    if !value.is_empty() {
        validate_required(value, max_bytes)?;
        parameters.push((key, value.to_string()));
    }
    Ok(())
}

fn validate_required(value: &str, max_bytes: usize) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > max_bytes
        || value != value.trim()
        || value.contains(['\0', '\r', '\n'])
    {
        Err(())
    } else {
        Ok(())
    }
}

enum ReadBodyError {
    TooLarge,
    Unavailable,
}

async fn read_bounded_body(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ReadBodyError> {
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|_| ReadBodyError::Unavailable)?;
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(ReadBodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
}

fn succeeded(request_id: String, operation: i32, payload: Payload) -> ZlmDebugResponse {
    ZlmDebugResponse {
        request_id,
        operation,
        status: ZlmDebugResponseStatus::Succeeded as i32,
        payload: Some(payload),
        truncated: false,
    }
}

fn failed(
    request_id: String,
    operation: i32,
    code: &str,
    message: &str,
    truncated: bool,
) -> ZlmDebugResponse {
    ZlmDebugResponse {
        request_id,
        operation,
        status: ZlmDebugResponseStatus::Failed as i32,
        payload: Some(Payload::Error(ZlmDebugError {
            code: code.to_string(),
            message: message.to_string(),
        })),
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, convert::Infallible, sync::Arc};

    use axum::{
        Router,
        body::{Body, Bytes},
        extract::{OriginalUri, State},
        http::{Response, StatusCode, header},
        response::Redirect,
    };
    use media_rpc::control_plane::{
        ZlmCloseStreamParameters, ZlmKickSessionParameters, ZlmKickSessionsParameters,
        ZlmMediaFilter, ZlmSnapshotParameters, zlm_debug_request::Parameters,
    };
    use tokio::sync::{Semaphore, mpsc};
    use tokio_stream::once;

    use super::*;

    async fn zlm_stub(
        State(sender): State<mpsc::UnboundedSender<reqwest::Url>>,
        OriginalUri(uri): OriginalUri,
    ) -> Response<Body> {
        let url = reqwest::Url::parse(&format!("http://127.0.0.1{uri}"))
            .expect("captured URI must be valid");
        sender.send(url.clone()).expect("capture request");
        if url.path() == "/index/api/getSnap" {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "image/jpeg")
                .body(Body::from(vec![1_u8, 2, 3, 4]))
                .unwrap()
        } else {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"code":0}"#))
                .unwrap()
        }
    }

    fn request(operation: ZlmDebugOperation, parameters: Option<Parameters>) -> ZlmDebugRequest {
        ZlmDebugRequest {
            request_id: Uuid::now_v7().to_string(),
            operation: operation as i32,
            parameters,
        }
    }

    fn query(url: &reqwest::Url) -> HashMap<String, String> {
        url.query_pairs().into_owned().collect()
    }

    fn assert_error(response: &ZlmDebugResponse, expected_code: &str, truncated: bool) {
        assert_eq!(response.status, ZlmDebugResponseStatus::Failed as i32);
        assert_eq!(response.truncated, truncated);
        match response.payload.as_ref() {
            Some(Payload::Error(error)) => assert_eq!(error.code, expected_code),
            payload => panic!("expected typed error payload, got {payload:?}"),
        }
    }

    #[tokio::test]
    async fn every_typed_operation_maps_to_one_fixed_local_zlm_api() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (capture_tx, mut capture_rx) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(zlm_stub).with_state(capture_tx),
            )
            .await
            .unwrap();
        });
        let settings = AgentSettings {
            zlm_api_base: format!("http://{address}"),
            zlm_api_secret: "agent-held-secret".to_string(),
            ..AgentSettings::default()
        };
        let executor = ZlmDebugExecutor::new(&settings).unwrap();
        let cases = vec![
            (
                request(
                    ZlmDebugOperation::ListMedia,
                    Some(Parameters::MediaFilter(ZlmMediaFilter {
                        schema: "rtsp".to_string(),
                        vhost: "__defaultVhost__".to_string(),
                        app: "live".to_string(),
                        stream: "camera".to_string(),
                    })),
                ),
                "/index/api/getMediaList",
                vec![
                    ("schema", "rtsp"),
                    ("vhost", "__defaultVhost__"),
                    ("app", "live"),
                    ("stream", "camera"),
                ],
                false,
            ),
            (
                request(ZlmDebugOperation::ListSessions, None),
                "/index/api/getAllSession",
                vec![],
                false,
            ),
            (
                request(ZlmDebugOperation::ListPlayers, None),
                "/index/api/getMediaPlayerList",
                vec![],
                false,
            ),
            (
                request(ZlmDebugOperation::GetStatistic, None),
                "/index/api/getStatistic",
                vec![],
                false,
            ),
            (
                request(ZlmDebugOperation::GetThreadsLoad, None),
                "/index/api/getThreadsLoad",
                vec![],
                false,
            ),
            (
                request(ZlmDebugOperation::GetWorkThreadsLoad, None),
                "/index/api/getWorkThreadsLoad",
                vec![],
                false,
            ),
            (
                request(
                    ZlmDebugOperation::KickSession,
                    Some(Parameters::KickSession(ZlmKickSessionParameters {
                        session_id: "session-7".to_string(),
                    })),
                ),
                "/index/api/kick_session",
                vec![("id", "session-7")],
                false,
            ),
            (
                request(
                    ZlmDebugOperation::KickSessions,
                    Some(Parameters::KickSessions(ZlmKickSessionsParameters {
                        local_port: 554,
                        peer_ip: "192.0.2.80".to_string(),
                    })),
                ),
                "/index/api/kick_sessions",
                vec![("local_port", "554"), ("peer_ip", "192.0.2.80")],
                false,
            ),
            (
                request(
                    ZlmDebugOperation::CloseStream,
                    Some(Parameters::CloseStream(ZlmCloseStreamParameters {
                        schema: "rtsp".to_string(),
                        vhost: "__defaultVhost__".to_string(),
                        app: "live".to_string(),
                        stream: "camera".to_string(),
                        force: true,
                    })),
                ),
                "/index/api/close_streams",
                vec![
                    ("schema", "rtsp"),
                    ("vhost", "__defaultVhost__"),
                    ("app", "live"),
                    ("stream", "camera"),
                    ("force", "true"),
                ],
                false,
            ),
            (
                request(
                    ZlmDebugOperation::Snapshot,
                    Some(Parameters::Snapshot(ZlmSnapshotParameters {
                        source_url: "rtsp://camera.example/live".to_string(),
                        timeout_sec: 10,
                        expire_sec: 30,
                    })),
                ),
                "/index/api/getSnap",
                vec![
                    ("url", "rtsp://camera.example/live"),
                    ("timeout_sec", "10"),
                    ("expire_sec", "30"),
                ],
                true,
            ),
        ];

        for (request, expected_path, expected_query, snapshot) in cases {
            let response = executor.execute(request).await;
            assert_eq!(response.status, ZlmDebugResponseStatus::Succeeded as i32);
            assert_eq!(
                matches!(response.payload, Some(Payload::Snapshot(_))),
                snapshot
            );
            let captured = capture_rx.recv().await.expect("ZLM request");
            assert_eq!(captured.path(), expected_path);
            let captured_query = query(&captured);
            assert_eq!(
                captured_query.get("secret").map(String::as_str),
                Some("agent-held-secret")
            );
            let expected_query_len = expected_query.len();
            for (key, value) in expected_query {
                assert_eq!(captured_query.get(key).map(String::as_str), Some(value));
            }
            assert_eq!(captured_query.len(), expected_query_len + 1);
        }

        server.abort();
    }

    #[test]
    fn executor_rejects_every_non_loopback_or_ambiguous_base_url() {
        let rejected = [
            "https://127.0.0.1:8080",
            "http://localhost:8080",
            "http://example.test:8080",
            "http://192.0.2.10:8080",
            "http://0.0.0.0:8080",
            "http://user:password@127.0.0.1:8080",
            "http://127.0.0.1:8080/index/api",
            "http://127.0.0.1:8080?target=elsewhere",
            "http://127.0.0.1:8080#fragment",
        ];
        for base_url in rejected {
            let settings = AgentSettings {
                zlm_api_base: base_url.to_string(),
                ..AgentSettings::default()
            };
            assert!(
                ZlmDebugExecutor::new(&settings).is_err(),
                "unsafe base URL was accepted: {base_url}"
            );
        }

        for base_url in ["http://127.0.0.1:8080", "http://[::1]:8080"] {
            let settings = AgentSettings {
                zlm_api_base: base_url.to_string(),
                ..AgentSettings::default()
            };
            assert!(
                ZlmDebugExecutor::new(&settings).is_ok(),
                "loopback base URL was rejected: {base_url}"
            );
        }
    }

    #[test]
    fn snapshot_content_type_allowlist_contains_only_safe_raster_formats() {
        for (value, expected) in [
            ("image/jpeg", "image/jpeg"),
            ("IMAGE/PNG", "image/png"),
            ("image/webp", "image/webp"),
            ("image/gif", "image/gif"),
        ] {
            assert_eq!(safe_snapshot_content_type(value).as_deref(), Some(expected));
        }
        for value in [
            "image/svg+xml",
            "application/xml",
            "text/html",
            "image/jpeg; charset=utf-8",
            " image/jpeg",
            "image/jpeg ",
        ] {
            assert_eq!(safe_snapshot_content_type(value), None, "accepted {value}");
        }
    }

    #[tokio::test]
    async fn executor_rejects_invalid_typed_parameters_before_network_access() {
        let settings = AgentSettings {
            zlm_api_base: "http://127.0.0.1:1".to_string(),
            ..AgentSettings::default()
        };
        let executor = ZlmDebugExecutor::new(&settings).unwrap();
        let mut invalid_request_id = request(ZlmDebugOperation::ListSessions, None);
        invalid_request_id.request_id = Uuid::nil().to_string();
        let invalid = vec![
            invalid_request_id,
            ZlmDebugRequest {
                request_id: Uuid::now_v7().to_string(),
                operation: 999,
                parameters: None,
            },
            request(
                ZlmDebugOperation::ListSessions,
                Some(Parameters::MediaFilter(ZlmMediaFilter::default())),
            ),
            request(ZlmDebugOperation::ListMedia, None),
            request(
                ZlmDebugOperation::KickSession,
                Some(Parameters::KickSession(ZlmKickSessionParameters {
                    session_id: "\n".to_string(),
                })),
            ),
            request(
                ZlmDebugOperation::KickSessions,
                Some(Parameters::KickSessions(ZlmKickSessionsParameters {
                    local_port: u32::from(u16::MAX) + 1,
                    peer_ip: String::new(),
                })),
            ),
            request(
                ZlmDebugOperation::KickSessions,
                Some(Parameters::KickSessions(ZlmKickSessionsParameters {
                    local_port: 0,
                    peer_ip: "not-an-ip".to_string(),
                })),
            ),
            request(
                ZlmDebugOperation::CloseStream,
                Some(Parameters::CloseStream(ZlmCloseStreamParameters {
                    schema: "rtsp".to_string(),
                    vhost: "__defaultVhost__".to_string(),
                    app: String::new(),
                    stream: "camera".to_string(),
                    force: false,
                })),
            ),
            request(
                ZlmDebugOperation::Snapshot,
                Some(Parameters::Snapshot(ZlmSnapshotParameters {
                    source_url: "rtsp://camera.example/live".to_string(),
                    timeout_sec: 0,
                    expire_sec: 30,
                })),
            ),
        ];

        for invalid_request in invalid {
            let response = executor.execute(invalid_request).await;
            assert_error(&response, "INVALID_REQUEST", false);
        }
    }

    async fn oversized_stub(OriginalUri(uri): OriginalUri) -> Response<Body> {
        let (content_type, length) = if uri.path() == "/index/api/getSnap" {
            ("image/jpeg", ZLM_DEBUG_MAX_SNAPSHOT_BYTES + 1)
        } else {
            ("application/json", ZLM_DEBUG_MAX_JSON_BYTES + 1)
        };
        let stream = once(Ok::<Bytes, Infallible>(Bytes::from(vec![b'x'; length])));
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from_stream(stream))
            .unwrap()
    }

    #[tokio::test]
    async fn chunked_json_and_snapshot_responses_are_bounded_while_reading() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(oversized_stub))
                .await
                .unwrap();
        });
        let settings = AgentSettings {
            zlm_api_base: format!("http://{address}"),
            ..AgentSettings::default()
        };
        let executor = ZlmDebugExecutor::new(&settings).unwrap();

        let json = executor
            .execute(request(ZlmDebugOperation::ListSessions, None))
            .await;
        assert_error(&json, "RESPONSE_TOO_LARGE", true);
        let snapshot = executor
            .execute(request(
                ZlmDebugOperation::Snapshot,
                Some(Parameters::Snapshot(ZlmSnapshotParameters {
                    source_url: "rtsp://camera.example/live".to_string(),
                    timeout_sec: 10,
                    expire_sec: 30,
                })),
            ))
            .await;
        assert_error(&snapshot, "RESPONSE_TOO_LARGE", true);

        server.abort();
    }

    #[derive(Clone)]
    struct BlockingState {
        entered: mpsc::UnboundedSender<()>,
        release: Arc<Semaphore>,
    }

    async fn blocking_stub(State(state): State<BlockingState>) -> Response<Body> {
        state.entered.send(()).unwrap();
        let _permit = state.release.acquire().await.unwrap();
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"code":0}"#))
            .unwrap()
    }

    #[tokio::test]
    async fn executor_fails_fast_when_all_four_local_request_slots_are_active() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let server_release = release.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .fallback(blocking_stub)
                    .with_state(BlockingState {
                        entered: entered_tx,
                        release: server_release,
                    }),
            )
            .await
            .unwrap();
        });
        let settings = AgentSettings {
            zlm_api_base: format!("http://{address}"),
            ..AgentSettings::default()
        };
        let executor = ZlmDebugExecutor::new(&settings).unwrap();
        let mut active = Vec::new();
        for _ in 0..ZLM_DEBUG_MAX_CONCURRENCY {
            let executor = executor.clone();
            active.push(tokio::spawn(async move {
                executor
                    .execute(request(ZlmDebugOperation::ListSessions, None))
                    .await
            }));
        }
        for _ in 0..ZLM_DEBUG_MAX_CONCURRENCY {
            tokio::time::timeout(Duration::from_secs(1), entered_rx.recv())
                .await
                .expect("local request did not enter ZLM")
                .expect("ZLM server stopped");
        }

        let busy = executor
            .execute(request(ZlmDebugOperation::ListSessions, None))
            .await;
        assert_error(&busy, "BUSY", false);
        release.add_permits(ZLM_DEBUG_MAX_CONCURRENCY);
        for task in active {
            let response = task.await.unwrap();
            assert_eq!(response.status, ZlmDebugResponseStatus::Succeeded as i32);
        }

        server.abort();
    }

    async fn slow_stub() -> Response<Body> {
        tokio::time::sleep(Duration::from_secs(1)).await;
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"code":0}"#))
            .unwrap()
    }

    #[tokio::test]
    async fn executor_returns_typed_timeout_without_waiting_for_slow_zlm() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(slow_stub))
                .await
                .unwrap();
        });
        let settings = AgentSettings {
            zlm_api_base: format!("http://{address}"),
            ..AgentSettings::default()
        };
        let executor = ZlmDebugExecutor::build(&settings, Duration::from_millis(25)).unwrap();

        let response = executor
            .execute(request(ZlmDebugOperation::ListSessions, None))
            .await;
        assert_error(&response, "UPSTREAM_TIMEOUT", false);

        server.abort();
    }

    #[tokio::test]
    async fn executor_never_follows_zlm_redirects() {
        async fn redirect_stub() -> Redirect {
            Redirect::temporary("http://192.0.2.10/private")
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(redirect_stub))
                .await
                .unwrap();
        });
        let settings = AgentSettings {
            zlm_api_base: format!("http://{address}"),
            ..AgentSettings::default()
        };
        let executor = ZlmDebugExecutor::new(&settings).unwrap();

        let response = executor
            .execute(request(ZlmDebugOperation::ListSessions, None))
            .await;
        assert_error(&response, "UPSTREAM_HTTP_STATUS", false);

        server.abort();
    }

    #[tokio::test]
    async fn snapshot_rejects_active_or_non_raster_content_types() {
        async fn svg_stub() -> Response<Body> {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "image/svg+xml")
                .body(Body::from("<svg><script>alert(1)</script></svg>"))
                .unwrap()
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(svg_stub))
                .await
                .unwrap();
        });
        let settings = AgentSettings {
            zlm_api_base: format!("http://{address}"),
            ..AgentSettings::default()
        };
        let executor = ZlmDebugExecutor::new(&settings).unwrap();

        let response = executor
            .execute(request(
                ZlmDebugOperation::Snapshot,
                Some(Parameters::Snapshot(ZlmSnapshotParameters {
                    source_url: "rtsp://camera.example/live".to_string(),
                    timeout_sec: 10,
                    expire_sec: 30,
                })),
            ))
            .await;
        assert_error(&response, "INVALID_UPSTREAM_RESPONSE", false);

        server.abort();
    }
}
