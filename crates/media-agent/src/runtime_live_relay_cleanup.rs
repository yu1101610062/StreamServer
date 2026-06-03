//! Live relay 清理：封装停止 ZLM stream proxy 和关闭 ZLM 流的收尾动作。
//!
//! 这里只处理 ZLM API 清理顺序，不负责 runtime 状态迁移、事件投递或进程信号。

use media_domain::RuntimeHandle;
use reqwest::Client;

use crate::{
    config::AgentSettings,
    runtime_metadata::{StreamBinding, zlm_proxy_key_from_handle},
    runtime_zlm::{build_close_stream_params, call_zlm_api},
};

pub(crate) async fn cleanup_live_relay_runtime(
    client: &Client,
    settings: &AgentSettings,
    handle: &RuntimeHandle,
    binding: &StreamBinding,
) {
    if let Some(proxy_key) = zlm_proxy_key_from_handle(handle) {
        let _ = call_zlm_api(
            client,
            settings,
            "/index/api/delStreamProxy",
            &[("key".to_string(), proxy_key)],
        )
        .await;
    }
    let _ = call_zlm_api(
        client,
        settings,
        "/index/api/close_streams",
        &build_close_stream_params(binding, true),
    )
    .await;
}
