use crate::{
    api::ApiClient,
    media_player,
    models::{NativeError, ServerProfile},
    secure_store,
};
use serde_json::{Value, json};
use std::time::Instant;

pub async fn probe(api: &ApiClient, server: &ServerProfile, access_token: Option<&str>) -> Value {
    let live = probe_path(api, server, "GET", "/health/live", None).await;
    let ready = probe_path(api, server, "GET", "/health/ready", None).await;
    let me = probe_path(api, server, "GET", "/api/v1/me", access_token).await;
    json!({
        "base_url": server.base_url,
        "health_live": live,
        "health_ready": ready,
        "current_session": me,
        "media_player": media_player::probe(),
        "secure_store": secure_store::probe(),
    })
}

async fn probe_path(
    api: &ApiClient,
    server: &ServerProfile,
    method: &str,
    path: &str,
    access_token: Option<&str>,
) -> Value {
    let started = Instant::now();
    match api
        .request_json(server, method, path, None, None, access_token)
        .await
    {
        Ok(payload) => json!({
            "ok": true,
            "latency_ms": started.elapsed().as_millis(),
            "payload": payload,
        }),
        Err(error) => json!({
            "ok": false,
            "latency_ms": started.elapsed().as_millis(),
            "error": error_to_json(error),
        }),
    }
}

fn error_to_json(error: NativeError) -> Value {
    match error {
        NativeError::Http {
            status,
            message,
            details,
        } => json!({ "kind": "http", "status": status, "message": message, "details": details }),
        other => json!({ "kind": "native", "message": other.to_string() }),
    }
}
