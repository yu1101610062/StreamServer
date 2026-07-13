use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State, rejection::JsonRejection},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

pub(crate) const ZLM_HOOK_MAX_BODY_BYTES: usize = 256 * 1024;
const BOUNDARY_FIELDS: [&str; 5] = [
    "secret",
    "mediaServerId",
    "media_server_id",
    "server_id",
    "serverId",
];
const ALLOWED_HOOKS: [&str; 9] = [
    "on_publish",
    "on_rtp_server_timeout",
    "on_record_mp4",
    "on_record_ts",
    "on_record_hls",
    "on_stream_none_reader",
    "on_stream_not_found",
    "on_server_keepalive",
    "on_server_started",
];

#[derive(Debug)]
pub(crate) struct ZlmHookRelayResponse {
    pub(crate) http_status: StatusCode,
    pub(crate) body_json: String,
}

#[derive(Debug)]
pub(crate) struct ZlmHookRelayRequest {
    pub(crate) request_id: String,
    pub(crate) hook_name: String,
    pub(crate) body_json: String,
    response: oneshot::Sender<ZlmHookRelayResponse>,
}

impl ZlmHookRelayRequest {
    pub(crate) fn new(
        request_id: String,
        hook_name: String,
        body_json: String,
    ) -> (Self, oneshot::Receiver<ZlmHookRelayResponse>) {
        let (response, receiver) = oneshot::channel();
        (
            Self {
                request_id,
                hook_name,
                body_json,
                response,
            },
            receiver,
        )
    }

    pub(crate) fn response_is_closed(&self) -> bool {
        self.response.is_closed()
    }

    pub(crate) fn respond(
        self,
        http_status: StatusCode,
        body_json: String,
    ) -> Result<(), ZlmHookRelayResponse> {
        self.response.send(ZlmHookRelayResponse {
            http_status,
            body_json,
        })
    }
}

#[derive(Clone)]
pub(crate) struct ZlmHookRelay {
    sender: mpsc::Sender<ZlmHookRelayRequest>,
}

pub(crate) type ZlmHookRequestReceiver = mpsc::Receiver<ZlmHookRelayRequest>;

pub(crate) fn zlm_hook_channel(capacity: usize) -> (ZlmHookRelay, ZlmHookRequestReceiver) {
    let (sender, receiver) = mpsc::channel(capacity);
    (ZlmHookRelay { sender }, receiver)
}

#[derive(Clone)]
pub(crate) struct ZlmHookIngress {
    expected_secret: Arc<str>,
    relay: ZlmHookRelay,
    response_timeout: Duration,
}

impl ZlmHookIngress {
    pub(crate) fn new(
        expected_secret: String,
        relay: ZlmHookRelay,
        response_timeout: Duration,
    ) -> Self {
        Self {
            expected_secret: Arc::from(expected_secret),
            relay,
            response_timeout,
        }
    }
}

#[derive(Debug, Deserialize)]
struct HookSecretQuery {
    secret: String,
}

#[derive(Debug, Serialize)]
struct LocalErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}

pub(crate) fn zlm_hook_router(state: ZlmHookIngress) -> Router {
    Router::new()
        .route("/internal/zlm-hooks/{hook_name}", post(receive_zlm_hook))
        .layer(DefaultBodyLimit::max(ZLM_HOOK_MAX_BODY_BYTES))
        .with_state(state)
}

async fn receive_zlm_hook(
    State(state): State<ZlmHookIngress>,
    Path(hook_name): Path<String>,
    query: Result<Query<HookSecretQuery>, axum::extract::rejection::QueryRejection>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    if !ALLOWED_HOOKS.contains(&hook_name.as_str()) {
        return local_error(
            StatusCode::NOT_FOUND,
            "ZLM_HOOK_NOT_ALLOWED",
            "unsupported ZLMediaKit hook",
        );
    }

    let Ok(Query(query)) = query else {
        return local_error(
            StatusCode::UNAUTHORIZED,
            "ZLM_HOOK_UNAUTHORIZED",
            "missing hook authentication",
        );
    };
    if !constant_time_secret_eq(&state.expected_secret, &query.secret) {
        return local_error(
            StatusCode::UNAUTHORIZED,
            "ZLM_HOOK_UNAUTHORIZED",
            "invalid hook authentication",
        );
    }

    let Json(Value::Object(mut payload)) = (match payload {
        Ok(payload) => payload,
        Err(error) if error.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return local_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "ZLM_HOOK_BODY_TOO_LARGE",
                "hook JSON body exceeds 256 KiB",
            );
        }
        Err(_) => {
            return local_error(
                StatusCode::BAD_REQUEST,
                "ZLM_HOOK_INVALID_JSON",
                "hook body must be a JSON object",
            );
        }
    }) else {
        return local_error(
            StatusCode::BAD_REQUEST,
            "ZLM_HOOK_INVALID_JSON",
            "hook body must be a JSON object",
        );
    };
    if hook_name == "on_server_started" {
        // Locked ZLMediaKit versions report the complete mINI snapshot here,
        // including api.secret and hook URLs with query credentials. The event
        // itself is sufficient for restart reconciliation, so none of that
        // configuration is allowed to cross the Agent/Core trust boundary.
        payload.clear();
    } else {
        strip_boundary_fields(&mut payload);
    }
    let body_json = match serde_json::to_string(&payload) {
        Ok(body) => body,
        Err(_) => {
            return local_error(
                StatusCode::BAD_REQUEST,
                "ZLM_HOOK_INVALID_JSON",
                "hook body could not be normalized",
            );
        }
    };

    let request_id = Uuid::now_v7().to_string();
    let (request, response_rx) = ZlmHookRelayRequest::new(request_id, hook_name, body_json);
    match state.relay.sender.try_send(request) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            return local_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "ZLM_HOOK_QUEUE_FULL",
                "hook relay queue is full",
            );
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            return local_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "ZLM_HOOK_RELAY_UNAVAILABLE",
                "hook relay is unavailable",
            );
        }
    }

    match tokio::time::timeout(state.response_timeout, response_rx).await {
        Ok(Ok(response)) => relay_response(response),
        Ok(Err(_)) => local_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "ZLM_HOOK_CONTROL_DISCONNECTED",
            "control-plane session disconnected",
        ),
        Err(_) => local_error(
            StatusCode::GATEWAY_TIMEOUT,
            "ZLM_HOOK_RESPONSE_TIMEOUT",
            "Core did not answer the hook request in time",
        ),
    }
}

fn strip_boundary_fields(payload: &mut Map<String, Value>) {
    payload.retain(|field, _| !BOUNDARY_FIELDS.contains(&field.as_str()));
    for value in payload.values_mut() {
        strip_boundary_value(value);
    }
}

fn strip_boundary_value(value: &mut Value) {
    match value {
        Value::Object(payload) => strip_boundary_fields(payload),
        Value::Array(values) => values.iter_mut().for_each(strip_boundary_value),
        _ => {}
    }
}

fn constant_time_secret_eq(expected: &str, provided: &str) -> bool {
    let expected = Sha256::digest(expected.as_bytes());
    let provided = Sha256::digest(provided.as_bytes());
    bool::from(expected.ct_eq(&provided))
}

fn relay_response(response: ZlmHookRelayResponse) -> Response {
    (
        response.http_status,
        [(header::CONTENT_TYPE, "application/json")],
        response.body_json,
    )
        .into_response()
}

fn local_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (status, Json(LocalErrorBody { code, message })).into_response()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::{ZLM_HOOK_MAX_BODY_BYTES, ZlmHookIngress, zlm_hook_channel, zlm_hook_router};

    fn request(path: &str, body: impl Into<Body>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(body.into())
            .unwrap()
    }

    fn test_router(timeout: Duration) -> (Router, super::ZlmHookRequestReceiver) {
        let (relay, receiver) = zlm_hook_channel(2);
        let ingress = ZlmHookIngress::new(
            "0123456789abcdef0123456789abcdef".to_string(),
            relay,
            timeout,
        );
        (zlm_hook_router(ingress), receiver)
    }

    #[test]
    fn ingress_is_not_wrapped_in_a_trace_layer_that_records_query_secrets() {
        let main = include_str!("main.rs");
        let declaration = main
            .split("let zlm_hook_app =")
            .nth(1)
            .and_then(|tail| tail.split(';').next())
            .expect("main must construct the ZLM hook app");
        assert!(!declaration.contains("TraceLayer"));
    }

    #[test]
    fn ingress_exposes_only_the_fixed_native_zlm_hook_set() {
        assert_eq!(
            super::ALLOWED_HOOKS,
            [
                "on_publish",
                "on_rtp_server_timeout",
                "on_record_mp4",
                "on_record_ts",
                "on_record_hls",
                "on_stream_none_reader",
                "on_stream_not_found",
                "on_server_keepalive",
                "on_server_started",
            ]
        );
    }

    #[tokio::test]
    async fn ingress_rejects_unknown_hooks_and_wrong_secrets_without_relaying() {
        let (router, mut receiver) = test_router(Duration::from_secs(1));

        let response = router
            .clone()
            .oneshot(request(
                "/internal/zlm-hooks/not_a_hook?secret=0123456789abcdef0123456789abcdef",
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = router
            .oneshot(request("/internal/zlm-hooks/on_publish?secret=wrong", "{}"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn ingress_strips_boundary_identity_fields_before_relaying() {
        let (router, mut receiver) = test_router(Duration::from_secs(1));
        let request = tokio::spawn(
            router.oneshot(request(
                "/internal/zlm-hooks/on_publish?secret=0123456789abcdef0123456789abcdef",
                json!({
                    "secret": "must-not-cross",
                    "mediaServerId": "forged",
                    "media_server_id": "forged",
                    "server_id": "forged",
                    "serverId": "forged",
                    "nested": {"secret": "must-not-cross", "serverId": "forged", "ok": true},
                    "port": 1935
                })
                .to_string(),
            )),
        );

        let command = receiver.recv().await.unwrap();
        assert_eq!(command.hook_name, "on_publish");
        let body: Value = serde_json::from_str(&command.body_json).unwrap();
        assert_eq!(body, json!({"nested": {"ok": true}, "port": 1935}));
        command
            .respond(
                StatusCode::ACCEPTED,
                r#"{"code":0,"msg":"success"}"#.to_string(),
            )
            .unwrap();

        let response = request.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), br#"{"code":0,"msg":"success"}"#);
    }

    #[tokio::test]
    async fn server_started_never_relays_the_zlm_ini_snapshot() {
        let (router, mut receiver) = test_router(Duration::from_secs(1));
        let request = tokio::spawn(router.oneshot(request(
            "/internal/zlm-hooks/on_server_started?secret=0123456789abcdef0123456789abcdef",
            json!({
                "hook.on_publish": "http://127.0.0.1:18082/internal/zlm-hooks/on_publish?secret=LEAK",
                "api.secret": "LEAK",
                "general.mediaServerId": "forged",
                "http.port": "18080"
            })
            .to_string(),
        )));

        let command = receiver.recv().await.unwrap();
        assert_eq!(command.hook_name, "on_server_started");
        assert_eq!(command.body_json, "{}");
        assert!(!command.body_json.contains("LEAK"));
        command
            .respond(StatusCode::OK, r#"{"code":0}"#.to_string())
            .unwrap();
        assert_eq!(request.await.unwrap().unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ingress_returns_json_errors_for_oversize_queue_full_and_timeout() {
        let (router, _receiver) = test_router(Duration::from_secs(1));
        for invalid_body in ["not-json", "[]"] {
            let response = router
                .clone()
                .oneshot(request(
                    "/internal/zlm-hooks/on_publish?secret=0123456789abcdef0123456789abcdef",
                    invalid_body,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(response.headers()["content-type"], "application/json");
        }

        let (router, _receiver) = test_router(Duration::from_millis(20));
        let oversized = vec![b'x'; ZLM_HOOK_MAX_BODY_BYTES + 1];
        let response = router
            .clone()
            .oneshot(request(
                "/internal/zlm-hooks/on_publish?secret=0123456789abcdef0123456789abcdef",
                oversized,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(response.headers()["content-type"], "application/json");

        let (relay, _receiver) = zlm_hook_channel(1);
        let ingress = ZlmHookIngress::new(
            "0123456789abcdef0123456789abcdef".to_string(),
            relay,
            Duration::from_secs(1),
        );
        let router = zlm_hook_router(ingress);
        let occupied = tokio::spawn(router.clone().oneshot(request(
            "/internal/zlm-hooks/on_publish?secret=0123456789abcdef0123456789abcdef",
            "{}",
        )));
        tokio::time::sleep(Duration::from_millis(10)).await;
        let response = router
            .oneshot(request(
                "/internal/zlm-hooks/on_publish?secret=0123456789abcdef0123456789abcdef",
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        occupied.abort();

        let (router, mut receiver) = test_router(Duration::from_millis(20));
        let response_task = tokio::spawn(router.oneshot(request(
            "/internal/zlm-hooks/on_publish?secret=0123456789abcdef0123456789abcdef",
            "{}",
        )));
        let _command = receiver.recv().await.unwrap();
        let response = response_task.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(response.headers()["content-type"], "application/json");
    }
}
