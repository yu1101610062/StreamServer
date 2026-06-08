use crate::models::{NativeError, ServerProfile};
use reqwest::{
    Client, Method,
    header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Map, Value, json};
use std::{path::Path, time::Duration};

#[derive(Clone)]
pub struct ApiClient {
    http: Client,
}

#[derive(Debug)]
pub struct ApiResponse {
    pub payload: Value,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

impl ApiClient {
    pub fn new() -> Result<Self, NativeError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent("StreamServerDesktop/0.1")
            .build()?;
        Ok(Self { http })
    }

    pub async fn request_json(
        &self,
        server: &ServerProfile,
        method: &str,
        path: &str,
        query: Option<&Map<String, Value>>,
        body: Option<Value>,
        access_token: Option<&str>,
    ) -> Result<Value, NativeError> {
        let url = build_url(server, path, query)?;
        let method = method
            .parse::<Method>()
            .map_err(|error| NativeError::InvalidRequest(format!("invalid method: {error}")))?;
        let mut headers = HeaderMap::new();
        if let Some(token) = access_token.filter(|value| !value.trim().is_empty()) {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|error| NativeError::InvalidRequest(error.to_string()))?,
            );
        }
        if method == Method::POST && path.trim_matches('/') == "api/v1/tasks" {
            headers.insert(
                HeaderName::from_static("idempotency-key"),
                HeaderValue::from_str(&uuid::Uuid::now_v7().to_string())
                    .map_err(|error| NativeError::InvalidRequest(error.to_string()))?,
            );
        }

        let mut request = self.http.request(method, url).headers(headers);
        if let Some(body) = body {
            request = request.json(&body);
        }
        self.parse_response(request.send().await?).await
    }

    pub async fn request_with_refresh(
        &self,
        server: &ServerProfile,
        method: &str,
        path: &str,
        query: Option<&Map<String, Value>>,
        body: Option<Value>,
        access_token: Option<&str>,
        refresh_token: Option<&str>,
    ) -> Result<ApiResponse, NativeError> {
        match self
            .request_json(server, method, path, query, body.clone(), access_token)
            .await
        {
            Ok(payload) => Ok(ApiResponse {
                payload,
                access_token: None,
                refresh_token: None,
            }),
            Err(NativeError::Http { status: 403, .. })
                if refresh_token
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false)
                    && path != "/api/v1/auth/refresh" =>
            {
                let tokens = self
                    .request_json(
                        server,
                        "POST",
                        "/api/v1/auth/refresh",
                        None,
                        Some(json!({ "refresh_token": refresh_token.unwrap_or_default() })),
                        None,
                    )
                    .await?;
                let new_access = tokens
                    .get("access_token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| NativeError::Http {
                        status: 403,
                        message: "refresh response did not contain access_token".to_string(),
                        details: Some(tokens.clone()),
                    })?
                    .to_string();
                let new_refresh = tokens
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                let payload = self
                    .request_json(server, method, path, query, body, Some(&new_access))
                    .await?;
                Ok(ApiResponse {
                    payload,
                    access_token: Some(new_access),
                    refresh_token: new_refresh,
                })
            }
            Err(error) => Err(error),
        }
    }

    pub async fn upload_media(
        &self,
        server: &ServerProfile,
        file_path: &str,
        query: Option<&Map<String, Value>>,
        access_token: Option<&str>,
    ) -> Result<Value, NativeError> {
        let path = Path::new(file_path);
        if !path.exists() {
            return Err(NativeError::InvalidRequest(format!(
                "upload file was not found: {file_path}"
            )));
        }
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("upload.bin")
            .to_string();
        let bytes = tokio::fs::read(path).await?;
        let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name);
        let form = reqwest::multipart::Form::new().part("file", part);
        let url = build_url(server, "/api/v1/uploads/media", query)?;
        let mut request = self.http.post(url).multipart(form);
        if let Some(token) = access_token.filter(|value| !value.trim().is_empty()) {
            request = request.bearer_auth(token);
        }
        self.parse_response(request.send().await?).await
    }

    async fn parse_response(&self, response: reqwest::Response) -> Result<Value, NativeError> {
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let payload = if status == reqwest::StatusCode::NO_CONTENT {
            Value::Null
        } else if content_type.contains("application/json") {
            response.json::<Value>().await?
        } else {
            Value::String(response.text().await?)
        };

        if status.is_success() {
            return Ok(payload);
        }
        let message = payload
            .get("message")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        Err(NativeError::Http {
            status: status.as_u16(),
            message,
            details: Some(payload),
        })
    }
}

pub fn build_url(
    server: &ServerProfile,
    path: &str,
    query: Option<&Map<String, Value>>,
) -> Result<String, NativeError> {
    if path.starts_with("http://") || path.starts_with("https://") {
        return Err(NativeError::InvalidRequest(
            "path must be relative to the StreamServer base URL".to_string(),
        ));
    }
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let mut url = format!("{}{}", server.normalized_base_url()?, path);
    let query_string = query_to_string(query);
    if !query_string.is_empty() {
        url.push('?');
        url.push_str(&query_string);
    }
    Ok(url)
}

fn query_to_string(query: Option<&Map<String, Value>>) -> String {
    let Some(query) = query else {
        return String::new();
    };
    query
        .iter()
        .filter_map(|(key, value)| query_value(value).map(|value| (key, value)))
        .map(|(key, value)| format!("{}={}", encode_query(key), encode_query(&value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn query_value(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(value) if value.is_empty() => None,
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => Some(value.to_string()),
    }
}

fn encode_query(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            b' ' => vec!['+'],
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

pub fn wrap_api_response(response: ApiResponse) -> Value {
    json!({
        "payload": response.payload,
        "access_token": response.access_token,
        "refresh_token": response.refresh_token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn server() -> ServerProfile {
        ServerProfile {
            id: "local".to_string(),
            name: "local".to_string(),
            base_url: "http://127.0.0.1:8080/".to_string(),
        }
    }

    #[test]
    fn builds_url_with_query() {
        let mut query = Map::new();
        query.insert("keyword".to_string(), json!("camera 01"));
        query.insert("empty".to_string(), json!(""));
        let url = build_url(&server(), "/api/v1/tasks", Some(&query)).unwrap();
        assert_eq!(url, "http://127.0.0.1:8080/api/v1/tasks?keyword=camera+01");
    }

    #[test]
    fn rejects_absolute_path() {
        assert!(build_url(&server(), "https://example.com/api", None).is_err());
    }
}
