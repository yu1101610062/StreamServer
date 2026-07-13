use std::time::Duration;

use media_domain::{InputKind, SourceMode, TaskSpec};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use crate::config::CoreSettings;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GatewayAction {
    Relay {
        task_id: Uuid,
        source_url: String,
    },
    Prefetch {
        task_id: Uuid,
        source_url: String,
        target_path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GatewayActionResult {
    Relay { relay_url: String },
    Prefetch { source_url: String },
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
    #[error("source gateway prefetch timed out")]
    PrefetchTimeout,
}

#[derive(Debug, Clone)]
pub(crate) struct SourceGatewayClient {
    http: reqwest::Client,
    base_url: Url,
    prefetch_poll_interval: Duration,
    prefetch_timeout: Duration,
}

#[derive(Debug, Serialize)]
struct RelayRequest {
    task_id: Uuid,
    source_url: String,
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
}

#[derive(Debug, Deserialize)]
struct PrefetchResponse {
    status: String,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    failure_reason: Option<String>,
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
            Duration::from_millis(settings.source_gateway_prefetch_poll_ms),
            Duration::from_millis(settings.source_gateway_prefetch_timeout_ms),
        )
        .map(Some)
    }

    pub(crate) fn new(
        base_url: &str,
        prefetch_poll_interval: Duration,
        prefetch_timeout: Duration,
    ) -> Result<Self, SourceGatewayError> {
        Ok(Self {
            http: reqwest::Client::new(),
            base_url: Url::parse(base_url.trim()).map_err(|error| {
                SourceGatewayError::InvalidSpec(format!("invalid source gateway url: {error}"))
            })?,
            prefetch_poll_interval,
            prefetch_timeout,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(base_url: &str) -> Result<Self, SourceGatewayError> {
        Self::new(base_url, Duration::from_millis(10), Duration::from_secs(2))
    }

    pub(crate) async fn prepare_task_spec(
        &self,
        task_id: Uuid,
        spec: &TaskSpec,
    ) -> Result<Option<TaskSpec>, SourceGatewayError> {
        let Some(action) = plan_gateway_action(spec, task_id) else {
            return Ok(None);
        };
        let result = self.execute_action(&action).await?;
        let mut rewritten = spec.clone();
        apply_gateway_result(&mut rewritten, action, result)?;
        rewritten
            .validate()
            .map_err(|error| SourceGatewayError::InvalidSpec(error.to_string()))?;
        Ok(Some(rewritten))
    }

    pub(crate) async fn delete_relay(&self, task_id: Uuid) -> Result<(), SourceGatewayError> {
        let response = self
            .http
            .delete(self.endpoint(&format!("/api/relays/{task_id}"))?)
            .send()
            .await?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(SourceGatewayError::Rejected(format!(
                "delete relay returned {}",
                response.status()
            )))
        }
    }

    async fn execute_action(
        &self,
        action: &GatewayAction,
    ) -> Result<GatewayActionResult, SourceGatewayError> {
        match action {
            GatewayAction::Relay {
                task_id,
                source_url,
            } => {
                let response: RelayResponse = self
                    .http
                    .post(self.endpoint("/api/relays")?)
                    .json(&RelayRequest {
                        task_id: *task_id,
                        source_url: source_url.clone(),
                    })
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                Ok(GatewayActionResult::Relay {
                    relay_url: response.relay_url,
                })
            }
            GatewayAction::Prefetch {
                task_id,
                source_url,
                target_path,
            } => {
                let response: PrefetchResponse = self
                    .http
                    .post(self.endpoint("/api/prefetch")?)
                    .json(&PrefetchRequest {
                        task_id: *task_id,
                        source_url: source_url.clone(),
                        target_path: target_path.clone(),
                    })
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                self.wait_for_prefetch(*task_id, response).await
            }
        }
    }

    async fn wait_for_prefetch(
        &self,
        task_id: Uuid,
        mut response: PrefetchResponse,
    ) -> Result<GatewayActionResult, SourceGatewayError> {
        let deadline = Instant::now() + self.prefetch_timeout;
        loop {
            match response.status.as_str() {
                "ready" => {
                    let source_url = response.source_url.ok_or_else(|| {
                        SourceGatewayError::Rejected(
                            "ready prefetch response is missing source_url".to_string(),
                        )
                    })?;
                    return Ok(GatewayActionResult::Prefetch { source_url });
                }
                "failed" => {
                    return Err(SourceGatewayError::Rejected(
                        response
                            .failure_reason
                            .unwrap_or_else(|| "prefetch failed".to_string()),
                    ));
                }
                _ => {
                    if Instant::now() >= deadline {
                        return Err(SourceGatewayError::PrefetchTimeout);
                    }
                    sleep(self.prefetch_poll_interval).await;
                    response = self
                        .http
                        .get(self.endpoint(&format!("/api/prefetch/{task_id}"))?)
                        .send()
                        .await?
                        .error_for_status()?
                        .json()
                        .await?;
                }
            }
        }
    }

    fn endpoint(&self, path: &str) -> Result<Url, SourceGatewayError> {
        self.base_url.join(path).map_err(|error| {
            SourceGatewayError::InvalidSpec(format!(
                "invalid source gateway endpoint {path}: {error}"
            ))
        })
    }
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
            })
        }
        (InputKind::HttpMp4, Some(SourceMode::Vod))
        | (InputKind::HttpTs | InputKind::Hls, Some(SourceMode::Vod)) => {
            Some(GatewayAction::Prefetch {
                task_id,
                source_url: source_url.to_string(),
                target_path: default_prefetch_target_path(task_id, kind, source_url),
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
            Ok(())
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
            Ok(())
        }
        _ => Err(SourceGatewayError::ActionMismatch),
    }
}

fn default_prefetch_target_path(task_id: Uuid, kind: InputKind, source_url: &str) -> String {
    let ext = source_url
        .split('?')
        .next()
        .and_then(|path| path.rsplit('/').next())
        .and_then(|name| name.rsplit_once('.').map(|(_, ext)| ext))
        .filter(|ext| !ext.trim().is_empty())
        .unwrap_or(match kind {
            InputKind::Hls => "m3u8",
            InputKind::HttpTs => "ts",
            _ => "mp4",
        });
    format!("imports/{task_id}/source.{ext}")
}
