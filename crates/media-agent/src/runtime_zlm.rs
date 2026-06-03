//! ZLM API 辅助：封装 ZLMediaKit HTTP API 调用、流状态解析、RTP server 查询和录制参数构造。

use std::time::Duration;

use reqwest::{Client, Url};
use serde_json::Value;
use tokio::time::sleep;

use crate::{
    config::AgentSettings,
    runtime::{
        ExecutorError, LiveRelayRecording, StartupProbe, StreamBinding, ZlmMediaStatus,
        ZlmRecordKind,
    },
};

pub(crate) const ZLM_RUNTIME_VHOST: &str = "__defaultVhost__";
const PROCESS_RECOVERY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_REALTIME_MP4_RECORD_SEGMENT_SEC: u32 = 7_200;

pub(crate) async fn start_live_relay_recording(
    client: &Client,
    settings: &AgentSettings,
    binding: &StreamBinding,
    recording: &LiveRelayRecording,
) -> Result<(), ExecutorError> {
    for kind in &recording.formats {
        call_zlm_api(
            client,
            settings,
            "/index/api/startRecord",
            &build_record_api_params(binding, recording, kind),
        )
        .await?;
    }
    Ok(())
}

pub(crate) async fn stop_live_relay_recording(
    client: &Client,
    settings: &AgentSettings,
    binding: &StreamBinding,
    recording: &LiveRelayRecording,
) -> Result<(), ExecutorError> {
    for kind in &recording.formats {
        call_zlm_api(
            client,
            settings,
            "/index/api/stopRecord",
            &build_record_api_params(binding, recording, kind),
        )
        .await?;
    }
    Ok(())
}

pub(crate) async fn zlm_stream_online(
    client: &Client,
    settings: &AgentSettings,
    target: &StartupProbe,
) -> anyhow::Result<bool> {
    let status = zlm_stream_status(client, settings, target).await?;
    Ok(status.is_some())
}

pub(crate) async fn zlm_stream_status(
    client: &Client,
    settings: &AgentSettings,
    target: &StartupProbe,
) -> anyhow::Result<Option<ZlmMediaStatus>> {
    let url = build_zlm_url(settings, "/index/api/getMediaList")?;
    let response = client.get(url).send().await?.error_for_status()?;
    let body: Value = response.json().await?;
    Ok(zlm_stream_status_in_body(&body, target))
}

pub(crate) async fn zlm_rtp_server_port(
    client: &Client,
    settings: &AgentSettings,
    stream_id: &str,
) -> Result<Option<u16>, ExecutorError> {
    let body = call_zlm_api(client, settings, "/index/api/listRtpServer", &[]).await?;
    Ok(body
        .get("data")
        .and_then(Value::as_array)
        .and_then(|servers| {
            servers.iter().find_map(|entry| {
                let matches_stream =
                    entry.get("stream_id").and_then(Value::as_str) == Some(stream_id);
                if !matches_stream {
                    return None;
                }
                entry
                    .get("port")
                    .and_then(Value::as_u64)
                    .and_then(|value| u16::try_from(value).ok())
            })
        }))
}

pub(crate) async fn zlm_stream_binding_by_stream_id(
    client: &Client,
    settings: &AgentSettings,
    stream_id: &str,
) -> anyhow::Result<Option<StreamBinding>> {
    let url = build_zlm_url(settings, "/index/api/getMediaList")?;
    let response = client.get(url).send().await?.error_for_status()?;
    let body: Value = response.json().await?;
    Ok(body
        .get("data")
        .and_then(Value::as_array)
        .and_then(|media| {
            media.iter().find_map(|entry| {
                if entry.get("stream").and_then(Value::as_str) != Some(stream_id) {
                    return None;
                }
                Some(StreamBinding {
                    schema: entry
                        .get("schema")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    vhost: entry
                        .get("vhost")
                        .and_then(Value::as_str)
                        .unwrap_or(ZLM_RUNTIME_VHOST)
                        .to_string(),
                    app: entry.get("app").and_then(Value::as_str)?.to_string(),
                    stream: entry.get("stream").and_then(Value::as_str)?.to_string(),
                })
            })
        }))
}

pub(crate) async fn wait_for_zlm_api_ready(
    client: &Client,
    settings: &AgentSettings,
    timeout: Duration,
) -> bool {
    let started_at = tokio::time::Instant::now();
    loop {
        if zlm_api_ready(client, settings).await {
            return true;
        }
        if started_at.elapsed() >= timeout {
            return false;
        }
        sleep(PROCESS_RECOVERY_POLL_INTERVAL).await;
    }
}

async fn zlm_api_ready(client: &Client, settings: &AgentSettings) -> bool {
    let Ok(url) = build_zlm_url(settings, "/index/api/version") else {
        return false;
    };
    match client.get(url).send().await {
        Ok(response) => response.error_for_status().is_ok(),
        Err(_) => false,
    }
}

pub(crate) async fn call_zlm_api(
    client: &Client,
    settings: &AgentSettings,
    path: &str,
    params: &[(String, String)],
) -> Result<Value, ExecutorError> {
    let mut url = build_zlm_url(settings, path)?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in params {
            query.append_pair(key, value);
        }
    }
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| ExecutorError::ApiCall(error.to_string()))?
        .error_for_status()
        .map_err(|error| ExecutorError::ApiCall(error.to_string()))?;
    let body: Value = response
        .json()
        .await
        .map_err(|error| ExecutorError::ApiCall(error.to_string()))?;
    ensure_zlm_success(path, body)
}

fn build_zlm_url(settings: &AgentSettings, path: &str) -> Result<Url, ExecutorError> {
    let mut url = Url::parse(settings.zlm_api_base.trim())
        .map_err(|error| ExecutorError::ApiCall(error.to_string()))?
        .join(path)
        .map_err(|error| ExecutorError::ApiCall(error.to_string()))?;
    if !settings.zlm_api_secret.trim().is_empty() {
        url.query_pairs_mut()
            .append_pair("secret", settings.zlm_api_secret.trim());
    }
    Ok(url)
}

fn ensure_zlm_success(path: &str, body: Value) -> Result<Value, ExecutorError> {
    match body.get("code").and_then(Value::as_i64) {
        Some(0) | None => Ok(body),
        Some(code) => Err(ExecutorError::ApiCall(format!(
            "{path} returned code {code}: {}",
            body.get("msg")
                .and_then(Value::as_str)
                .unwrap_or("unknown ZLM error")
        ))),
    }
}

pub(crate) fn zlm_stream_status_in_body(
    body: &Value,
    target: &StartupProbe,
) -> Option<ZlmMediaStatus> {
    body.get("data")
        .and_then(Value::as_array)
        .and_then(|media| {
            media.iter().find_map(|entry| {
                if entry.get("app").and_then(Value::as_str) != Some(target.app.as_str())
                    || entry.get("stream").and_then(Value::as_str) != Some(target.stream.as_str())
                    || entry.get("vhost").and_then(Value::as_str) != Some(target.vhost.as_str())
                    || !target.schema.as_deref().is_none_or(|schema| {
                        entry.get("schema").and_then(Value::as_str) == Some(schema)
                    })
                {
                    return None;
                }

                let binding = StreamBinding {
                    schema: entry
                        .get("schema")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    vhost: entry
                        .get("vhost")
                        .and_then(Value::as_str)
                        .unwrap_or(ZLM_RUNTIME_VHOST)
                        .to_string(),
                    app: entry.get("app").and_then(Value::as_str)?.to_string(),
                    stream: entry.get("stream").and_then(Value::as_str)?.to_string(),
                };
                Some(ZlmMediaStatus { binding })
            })
        })
}

pub(crate) fn extract_zlm_proxy_key(body: &Value) -> Option<String> {
    body.get("data")
        .and_then(|data| data.get("key").or_else(|| data.get("proxy_key")))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(crate) fn extract_zlm_local_port(body: &Value) -> Option<u16> {
    body.get("port")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .or_else(|| {
            body.get("data")
                .and_then(|data| data.get("port"))
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
        })
}

pub(crate) fn build_record_api_params(
    binding: &StreamBinding,
    recording: &LiveRelayRecording,
    kind: &ZlmRecordKind,
) -> Vec<(String, String)> {
    let customized_path = recording
        .root_path_for_kind(kind)
        .expect("recording root path must exist for format")
        .to_string();
    let mut params = vec![
        ("type".to_string(), zlm_record_kind_code(kind).to_string()),
        ("vhost".to_string(), binding.vhost.clone()),
        ("app".to_string(), binding.app.clone()),
        ("stream".to_string(), binding.stream.clone()),
        ("customized_path".to_string(), customized_path),
    ];
    if let Some(schema) = &binding.schema {
        params.push(("schema".to_string(), schema.clone()));
    }
    if matches!(kind, ZlmRecordKind::Mp4) {
        params.push((
            "max_second".to_string(),
            mp4_record_max_second(recording).to_string(),
        ));
    }
    params
}

fn mp4_record_max_second(recording: &LiveRelayRecording) -> u32 {
    recording
        .segment_sec
        .filter(|value| *value > 0)
        .or(recording.duration_sec.filter(|value| *value > 0))
        .unwrap_or(DEFAULT_REALTIME_MP4_RECORD_SEGMENT_SEC)
}

pub(crate) fn build_close_stream_params(
    binding: &StreamBinding,
    force: bool,
) -> Vec<(String, String)> {
    let mut params = vec![
        ("vhost".to_string(), binding.vhost.clone()),
        ("app".to_string(), binding.app.clone()),
        ("stream".to_string(), binding.stream.clone()),
        (
            "force".to_string(),
            if force { "1" } else { "0" }.to_string(),
        ),
    ];
    if let Some(schema) = &binding.schema {
        params.push(("schema".to_string(), schema.clone()));
    }
    params
}

fn zlm_record_kind_code(kind: &ZlmRecordKind) -> u8 {
    match kind {
        ZlmRecordKind::Hls => 0,
        ZlmRecordKind::Mp4 => 1,
    }
}
