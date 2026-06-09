use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NativeError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("http {status}: {message}")]
    Http {
        status: u16,
        message: String,
        details: Option<Value>,
    },
    #[error("io error: {0}")]
    Io(String),
    #[error("media player error: {0}")]
    MediaPlayer(String),
    #[error("secure store error: {0}")]
    SecureStore(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("serialization error: {0}")]
    Serialization(String),
}

impl From<reqwest::Error> for NativeError {
    fn from(value: reqwest::Error) -> Self {
        Self::Network(value.to_string())
    }
}

impl From<std::io::Error> for NativeError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

impl From<serde_json::Error> for NativeError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value.to_string())
    }
}

#[derive(Debug, Deserialize)]
pub struct NativeRequest {
    pub op: String,
    pub server: Option<ServerProfile>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub query: Option<Map<String, Value>>,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerProfile {
    pub id: String,
    pub name: String,
    pub base_url: String,
}

impl ServerProfile {
    pub fn normalized_base_url(&self) -> Result<String, NativeError> {
        normalize_base_url(&self.base_url)
    }
}

#[derive(Debug, Serialize)]
pub struct NativeEnvelope<'a> {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<NativeErrorEnvelope>,
}

#[derive(Debug, Serialize)]
pub struct NativeErrorEnvelope {
    pub kind: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl From<NativeError> for NativeErrorEnvelope {
    fn from(value: NativeError) -> Self {
        match value {
            NativeError::Http {
                status,
                message,
                details,
            } => Self {
                kind: "http",
                message,
                status: Some(status),
                details,
            },
            NativeError::InvalidRequest(message) => Self {
                kind: "invalid_request",
                message,
                status: None,
                details: None,
            },
            NativeError::Io(message) => Self {
                kind: "io",
                message,
                status: None,
                details: None,
            },
            NativeError::MediaPlayer(message) => Self {
                kind: "media_player",
                message,
                status: None,
                details: None,
            },
            NativeError::SecureStore(message) => Self {
                kind: "secure_store",
                message,
                status: None,
                details: None,
            },
            NativeError::Network(message) => Self {
                kind: "network",
                message,
                status: None,
                details: None,
            },
            NativeError::Serialization(message) => Self {
                kind: "serialization",
                message,
                status: None,
                details: None,
            },
        }
    }
}

pub fn require_server(request: &NativeRequest) -> Result<ServerProfile, NativeError> {
    request
        .server
        .clone()
        .ok_or_else(|| NativeError::InvalidRequest("server profile is required".to_string()))
}

pub fn normalize_base_url(value: &str) -> Result<String, NativeError> {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(NativeError::InvalidRequest(
            "server base_url is empty".to_string(),
        ));
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(NativeError::InvalidRequest(
            "server base_url must start with http:// or https://".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

pub fn json_object(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_base_url() {
        assert_eq!(
            normalize_base_url(" http://127.0.0.1:8080/ ").unwrap(),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn rejects_invalid_base_url() {
        assert!(normalize_base_url("127.0.0.1:8080").is_err());
    }
}
