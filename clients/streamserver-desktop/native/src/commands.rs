use crate::{
    api::{ApiClient, wrap_api_response},
    diagnostics, discovery, media_player,
    models::{NativeError, NativeRequest, ServerProfile, json_object, require_server},
    secure_store,
};
use serde_json::{Value, json};

pub async fn dispatch_json(input: &str) -> Result<Value, NativeError> {
    let request = serde_json::from_str::<NativeRequest>(input)?;
    let api = ApiClient::new()?;
    dispatch(api, request).await
}

async fn dispatch(api: ApiClient, request: NativeRequest) -> Result<Value, NativeError> {
    // Flutter 侧通过 op 字符串进入 native 层；这里保持薄路由，只做参数提取、
    // token 传递和少量本机能力分发，业务语义仍在 Core API 或子模块中实现。
    match request.op.as_str() {
        "server_profile.normalize" => {
            let server = require_server(&request)?;
            Ok(json!({ "base_url": server.normalized_base_url()? }))
        }
        "auth.login" => {
            let server = require_server(&request)?;
            api.request_json(
                &server,
                "POST",
                "/api/v1/auth/login",
                None,
                request.body,
                None,
            )
            .await
        }
        "auth.refresh" => {
            let server = require_server(&request)?;
            api.request_json(
                &server,
                "POST",
                "/api/v1/auth/refresh",
                None,
                Some(json!({ "refresh_token": request.refresh_token.unwrap_or_default() })),
                None,
            )
            .await
        }
        "auth.logout" => {
            let server = require_server(&request)?;
            api.request_json(
                &server,
                "POST",
                "/api/v1/auth/logout",
                None,
                Some(json!({ "refresh_token": request.refresh_token.unwrap_or_default() })),
                request.access_token.as_deref(),
            )
            .await
        }
        "auth.me" => {
            let server = require_server(&request)?;
            api.request_json(
                &server,
                "GET",
                "/api/v1/me",
                None,
                None,
                request.access_token.as_deref(),
            )
            .await
        }
        "api.request" => {
            // 通用 API 代理会尝试用 refresh token 刷新 access token；
            // 登录、登出等固定端点保留在上面的显式分支中，方便前端区分响应形态。
            let server = require_server(&request)?;
            let method = request.method.as_deref().unwrap_or("GET");
            let path = request
                .path
                .as_deref()
                .ok_or_else(|| NativeError::InvalidRequest("path is required".to_string()))?;
            let response = api
                .request_with_refresh(
                    &server,
                    method,
                    path,
                    request.query.as_ref(),
                    request.body,
                    request.access_token.as_deref(),
                    request.refresh_token.as_deref(),
                )
                .await?;
            Ok(wrap_api_response(response))
        }
        "upload.media" => {
            let server = require_server(&request)?;
            let file_path = request
                .file_path
                .as_deref()
                .ok_or_else(|| NativeError::InvalidRequest("file_path is required".to_string()))?;
            api.upload_media(
                &server,
                file_path,
                request.query.as_ref(),
                request.access_token.as_deref(),
            )
            .await
        }
        "secure_store.read" => {
            // secure_store 操作只接受 key/value，避免前端把完整请求体直接写入本机密钥存储。
            let key = request
                .key
                .as_deref()
                .ok_or_else(|| NativeError::InvalidRequest("key is required".to_string()))?;
            Ok(json!({ "value": secure_store::read(key)? }))
        }
        "secure_store.write" => {
            let key = request
                .key
                .as_deref()
                .ok_or_else(|| NativeError::InvalidRequest("key is required".to_string()))?;
            let value = request.value.as_deref().unwrap_or_default();
            secure_store::write(key, value)?;
            Ok(json!({ "written": true }))
        }
        "secure_store.delete" => {
            let key = request
                .key
                .as_deref()
                .ok_or_else(|| NativeError::InvalidRequest("key is required".to_string()))?;
            secure_store::delete(key)?;
            Ok(json!({ "deleted": true }))
        }
        "diagnostics.probe" => {
            let server = require_server(&request)?;
            Ok(diagnostics::probe(&api, &server, request.access_token.as_deref()).await)
        }
        "server_discovery.scan" => discovery::scan(request.body.as_ref()).await,
        "server_discovery.probe" => discovery::probe(request.body.as_ref()).await,
        "media_player.probe" => Ok(media_player::probe()),
        "media_player.validate_url" => {
            let url = media_url_from_body(&request)?;
            media_player::validate_url(&url)
        }
        "media_player.open" => {
            // 播放器能力只处理已经由前端/Core 生成的媒体 URL，不在 native 层重新做鉴权。
            let url = media_url_from_body(&request)?;
            let requested_player = request
                .body
                .as_ref()
                .and_then(|value| value.get("player"))
                .and_then(Value::as_str);
            media_player::open(&url, requested_player)
        }
        "media_player.open_external" => {
            let url = media_url_from_body(&request)?;
            let requested_player = request
                .body
                .as_ref()
                .and_then(|value| value.get("player"))
                .and_then(Value::as_str);
            media_player::open(&url, requested_player)
        }
        "media_player.stop" => {
            let session_id = request
                .body
                .as_ref()
                .and_then(|value| value.get("session_id"))
                .and_then(Value::as_str)
                .ok_or_else(|| NativeError::InvalidRequest("session_id is required".to_string()))?;
            media_player::stop(session_id)
        }
        "media_player.snapshot" => {
            let session_id = request
                .body
                .as_ref()
                .and_then(|value| value.get("session_id"))
                .and_then(Value::as_str)
                .ok_or_else(|| NativeError::InvalidRequest("session_id is required".to_string()))?;
            let output_path = request
                .body
                .as_ref()
                .and_then(|value| value.get("output_path"))
                .and_then(Value::as_str);
            media_player::snapshot(session_id, output_path)
        }
        "version" => Ok(json_object([
            (
                "name",
                Value::String("streamserver-desktop-native".to_string()),
            ),
            (
                "version",
                Value::String(env!("CARGO_PKG_VERSION").to_string()),
            ),
        ])),
        other => Err(NativeError::InvalidRequest(format!(
            "unsupported operation: {other}"
        ))),
    }
}

fn media_url_from_body(request: &NativeRequest) -> Result<String, NativeError> {
    request
        .body
        .as_ref()
        .and_then(|value| value.get("url"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| NativeError::InvalidRequest("body.url is required".to_string()))
}

#[allow(dead_code)]
fn _server_from_parts(base_url: &str) -> ServerProfile {
    ServerProfile {
        id: "default".to_string(),
        name: "default".to_string(),
        base_url: base_url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatches_version() {
        let value = dispatch_json(r#"{"op":"version"}"#).await.unwrap();
        assert_eq!(value["name"], "streamserver-desktop-native");
    }

    #[tokio::test]
    async fn rejects_unknown_operation() {
        assert!(dispatch_json(r#"{"op":"missing"}"#).await.is_err());
    }

    #[tokio::test]
    async fn rejects_invalid_manual_discovery_protocol() {
        let result = dispatch_json(
            r#"{"op":"server_discovery.probe","body":{"protocol":"ftp","host":"127.0.0.1","port":8080}}"#,
        )
        .await;
        assert!(result.is_err());
    }
}
