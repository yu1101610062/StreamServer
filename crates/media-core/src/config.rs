use chrono::Duration;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    Disabled,
    ExternalJwt,
    LocalPassword,
}

impl Default for AuthMode {
    fn default() -> Self {
        Self::Disabled
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub environment: String,
    pub logging: LoggingSettings,
    pub core: CoreSettings,
}

#[derive(Debug, Clone, Deserialize)]
struct FileSettings {
    #[serde(default)]
    logging: LoggingSettings,
    #[serde(default)]
    core: CoreSettings,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingSettings {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub json: bool,
}

impl Default for LoggingSettings {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            json: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CoreSettings {
    #[serde(default = "default_core_http_addr")]
    pub http_addr: String,
    #[serde(default = "default_core_grpc_addr")]
    pub grpc_addr: String,
    #[serde(default = "default_grpc_tls_cert_path")]
    pub grpc_tls_cert_path: String,
    #[serde(default = "default_grpc_tls_key_path")]
    pub grpc_tls_key_path: String,
    #[serde(default = "default_grpc_tls_client_ca_path")]
    pub grpc_tls_client_ca_path: String,
    #[serde(default)]
    pub database_url: String,
    #[serde(default)]
    pub auth_mode: AuthMode,
    #[serde(default)]
    pub jwt_public_key: String,
    #[serde(default)]
    pub auth_jwt_private_key_path: String,
    #[serde(default)]
    pub auth_jwt_public_key_path: String,
    #[serde(default = "default_auth_access_token_ttl")]
    pub auth_access_token_ttl: String,
    #[serde(default = "default_auth_refresh_token_ttl")]
    pub auth_refresh_token_ttl: String,
    #[serde(default)]
    pub hook_shared_secret: String,
    #[serde(default)]
    pub hook_source_allowlist: Vec<String>,
    #[serde(default)]
    pub zlm_auto_close_on_no_reader_enabled: bool,
    #[serde(default = "default_storage_allowlist")]
    pub storage_allowlist: Vec<String>,
}

impl Default for CoreSettings {
    fn default() -> Self {
        Self {
            http_addr: default_core_http_addr(),
            grpc_addr: default_core_grpc_addr(),
            grpc_tls_cert_path: default_grpc_tls_cert_path(),
            grpc_tls_key_path: default_grpc_tls_key_path(),
            grpc_tls_client_ca_path: default_grpc_tls_client_ca_path(),
            database_url: String::new(),
            auth_mode: AuthMode::Disabled,
            jwt_public_key: String::new(),
            auth_jwt_private_key_path: String::new(),
            auth_jwt_public_key_path: String::new(),
            auth_access_token_ttl: default_auth_access_token_ttl(),
            auth_refresh_token_ttl: default_auth_refresh_token_ttl(),
            hook_shared_secret: String::new(),
            hook_source_allowlist: Vec::new(),
            zlm_auto_close_on_no_reader_enabled: false,
            storage_allowlist: default_storage_allowlist(),
        }
    }
}

impl Settings {
    pub fn load() -> anyhow::Result<Self> {
        let environment =
            std::env::var("STREAMSERVER_ENV").unwrap_or_else(|_| "development".into());
        let builder = config::Config::builder()
            .add_source(config::File::with_name("config/base").required(false))
            .add_source(config::File::with_name(&format!("config/{environment}")).required(false));

        let mut file_settings = builder.build()?.try_deserialize::<FileSettings>()?;
        apply_env_overrides(&mut file_settings);

        let settings = Self {
            environment,
            logging: file_settings.logging,
            core: file_settings.core,
        };

        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.core.http_addr.trim().is_empty(),
            "core.http_addr must not be empty"
        );
        anyhow::ensure!(
            !self.core.grpc_addr.trim().is_empty(),
            "core.grpc_addr must not be empty"
        );
        anyhow::ensure!(
            !self.core.database_url.trim().is_empty(),
            "DATABASE_URL must be configured"
        );
        match self.core.auth_mode {
            AuthMode::Disabled => {}
            AuthMode::ExternalJwt => {
                anyhow::ensure!(
                    !self.core.jwt_public_key.trim().is_empty(),
                    "JWT_PUBLIC_KEY must be configured when auth mode is external_jwt"
                );
            }
            AuthMode::LocalPassword => {
                anyhow::ensure!(
                    !self.core.auth_jwt_private_key_path.trim().is_empty(),
                    "AUTH_JWT_PRIVATE_KEY_PATH must be configured when auth mode is local_password"
                );
                anyhow::ensure!(
                    !self.core.auth_jwt_public_key_path.trim().is_empty(),
                    "AUTH_JWT_PUBLIC_KEY_PATH must be configured when auth mode is local_password"
                );
                parse_duration_spec(&self.core.auth_access_token_ttl)
                    .map_err(|error| anyhow::anyhow!("invalid AUTH_ACCESS_TOKEN_TTL: {error}"))?;
                parse_duration_spec(&self.core.auth_refresh_token_ttl)
                    .map_err(|error| anyhow::anyhow!("invalid AUTH_REFRESH_TOKEN_TTL: {error}"))?;
            }
        }
        anyhow::ensure!(
            !self.core.storage_allowlist.is_empty(),
            "storage allowlist must not be empty"
        );
        let tls_fields = [
            self.core.grpc_tls_cert_path.trim(),
            self.core.grpc_tls_key_path.trim(),
            self.core.grpc_tls_client_ca_path.trim(),
        ];
        if tls_fields.iter().any(|value| !value.is_empty()) {
            anyhow::ensure!(
                tls_fields.iter().all(|value| !value.is_empty()),
                "CORE_GRPC_TLS_CERT_PATH, CORE_GRPC_TLS_KEY_PATH and CORE_GRPC_TLS_CLIENT_CA_PATH must all be set together"
            );
        }

        Ok(())
    }
}

fn apply_env_overrides(settings: &mut FileSettings) {
    if let Some(value) = env("CORE_HTTP_ADDR") {
        settings.core.http_addr = value;
    }
    if let Some(value) = env("CORE_GRPC_ADDR") {
        settings.core.grpc_addr = value;
    }
    if let Some(value) = env("CORE_GRPC_TLS_CERT_PATH") {
        settings.core.grpc_tls_cert_path = value;
    }
    if let Some(value) = env("CORE_GRPC_TLS_KEY_PATH") {
        settings.core.grpc_tls_key_path = value;
    }
    if let Some(value) = env("CORE_GRPC_TLS_CLIENT_CA_PATH") {
        settings.core.grpc_tls_client_ca_path = value;
    }
    if let Some(value) = env("DATABASE_URL") {
        settings.core.database_url = value;
    }
    if let Some(value) = env("AUTH_MODE") {
        settings.core.auth_mode = match value.as_str() {
            "disabled" => AuthMode::Disabled,
            "external_jwt" => AuthMode::ExternalJwt,
            "local_password" => AuthMode::LocalPassword,
            other => panic!("unsupported AUTH_MODE: {other}"),
        };
    } else if let Some(value) = env("AUTH_ENABLED") {
        settings.core.auth_mode = if matches!(value.as_str(), "1" | "true" | "TRUE" | "yes") {
            AuthMode::ExternalJwt
        } else {
            AuthMode::Disabled
        };
    }
    if let Some(value) = env("JWT_PUBLIC_KEY") {
        settings.core.jwt_public_key = value;
    }
    if let Some(value) = env("AUTH_JWT_PRIVATE_KEY_PATH") {
        settings.core.auth_jwt_private_key_path = value;
    }
    if let Some(value) = env("AUTH_JWT_PUBLIC_KEY_PATH") {
        settings.core.auth_jwt_public_key_path = value;
    }
    if let Some(value) = env("AUTH_ACCESS_TOKEN_TTL") {
        settings.core.auth_access_token_ttl = value;
    }
    if let Some(value) = env("AUTH_REFRESH_TOKEN_TTL") {
        settings.core.auth_refresh_token_ttl = value;
    }
    if let Some(value) = env("HOOK_SHARED_SECRET") {
        settings.core.hook_shared_secret = value;
    }
    if let Some(value) = env("HOOK_SOURCE_ALLOWLIST") {
        settings.core.hook_source_allowlist = split_csv(&value);
    }
    if let Some(value) = env("CORE_ZLM_AUTO_CLOSE_ON_NO_READER_ENABLED") {
        settings.core.zlm_auto_close_on_no_reader_enabled =
            matches!(value.as_str(), "1" | "true" | "TRUE" | "yes");
    }
    if let Some(value) = env("STORAGE_ALLOWLIST") {
        settings.core.storage_allowlist = split_csv(&value);
    }
    if let Some(value) = env("LOG_LEVEL") {
        settings.logging.level = value;
    }
    if let Some(value) = env("LOG_JSON") {
        settings.logging.json = matches!(value.as_str(), "1" | "true" | "TRUE" | "yes");
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_core_http_addr() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_core_grpc_addr() -> String {
    "0.0.0.0:50051".to_string()
}

fn default_grpc_tls_cert_path() -> String {
    String::new()
}

fn default_grpc_tls_key_path() -> String {
    String::new()
}

fn default_grpc_tls_client_ca_path() -> String {
    String::new()
}

fn default_auth_access_token_ttl() -> String {
    "15m".to_string()
}

fn default_auth_refresh_token_ttl() -> String {
    "7d".to_string()
}

fn default_storage_allowlist() -> Vec<String> {
    vec![
        "/data/media/work".to_string(),
        "/data/zlm/record".to_string(),
        "/data/zlm/www".to_string(),
    ]
}

pub fn parse_duration_spec(value: &str) -> anyhow::Result<Duration> {
    let trimmed = value.trim();
    anyhow::ensure!(!trimmed.is_empty(), "duration must not be empty");
    anyhow::ensure!(trimmed.len() >= 2, "duration is too short");

    let (number, unit) = trimmed.split_at(trimmed.len() - 1);
    let amount: i64 = number
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("duration amount must be an integer"))?;
    anyhow::ensure!(amount > 0, "duration amount must be positive");

    let duration = match unit {
        "s" | "S" => Duration::seconds(amount),
        "m" | "M" => Duration::minutes(amount),
        "h" | "H" => Duration::hours(amount),
        "d" | "D" => Duration::days(amount),
        _ => anyhow::bail!("unsupported duration unit {unit}; use s, m, h or d"),
    };
    Ok(duration)
}
