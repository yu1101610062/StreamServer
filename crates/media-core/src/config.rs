use std::{net::SocketAddr, str::FromStr};

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
    #[serde(default = "default_http_tls_cert_path")]
    pub http_tls_cert_path: String,
    #[serde(default = "default_http_tls_key_path")]
    pub http_tls_key_path: String,
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
    pub insecure_dev: bool,
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
    #[serde(default = "default_callback_timeout_ms")]
    pub callback_timeout_ms: u64,
    #[serde(default = "default_callback_max_attempts")]
    pub callback_max_attempts: u32,
    #[serde(default = "default_callback_initial_backoff_ms")]
    pub callback_initial_backoff_ms: u64,
    #[serde(default = "default_callback_max_backoff_ms")]
    pub callback_max_backoff_ms: u64,
    #[serde(default = "default_callback_settle_delay_ms")]
    pub callback_settle_delay_ms: u64,
    #[serde(default)]
    pub callback_shared_secret: String,
    #[serde(default = "default_storage_allowlist")]
    pub storage_allowlist: Vec<String>,
    #[serde(default)]
    pub source_gateway_base_url: String,
    #[serde(default = "default_source_gateway_prefetch_poll_ms")]
    pub source_gateway_prefetch_poll_ms: u64,
    #[serde(default = "default_source_gateway_prefetch_timeout_ms")]
    pub source_gateway_prefetch_timeout_ms: u64,
}

impl Default for CoreSettings {
    fn default() -> Self {
        Self {
            http_addr: default_core_http_addr(),
            http_tls_cert_path: default_http_tls_cert_path(),
            http_tls_key_path: default_http_tls_key_path(),
            grpc_addr: default_core_grpc_addr(),
            grpc_tls_cert_path: default_grpc_tls_cert_path(),
            grpc_tls_key_path: default_grpc_tls_key_path(),
            grpc_tls_client_ca_path: default_grpc_tls_client_ca_path(),
            database_url: String::new(),
            auth_mode: AuthMode::Disabled,
            insecure_dev: false,
            jwt_public_key: String::new(),
            auth_jwt_private_key_path: String::new(),
            auth_jwt_public_key_path: String::new(),
            auth_access_token_ttl: default_auth_access_token_ttl(),
            auth_refresh_token_ttl: default_auth_refresh_token_ttl(),
            hook_shared_secret: String::new(),
            hook_source_allowlist: Vec::new(),
            zlm_auto_close_on_no_reader_enabled: false,
            callback_timeout_ms: default_callback_timeout_ms(),
            callback_max_attempts: default_callback_max_attempts(),
            callback_initial_backoff_ms: default_callback_initial_backoff_ms(),
            callback_max_backoff_ms: default_callback_max_backoff_ms(),
            callback_settle_delay_ms: default_callback_settle_delay_ms(),
            callback_shared_secret: String::new(),
            storage_allowlist: default_storage_allowlist(),
            source_gateway_base_url: String::new(),
            source_gateway_prefetch_poll_ms: default_source_gateway_prefetch_poll_ms(),
            source_gateway_prefetch_timeout_ms: default_source_gateway_prefetch_timeout_ms(),
        }
    }
}

impl Settings {
    pub fn load_with_insecure_dev(insecure_dev: bool) -> anyhow::Result<Self> {
        Self::load_internal(insecure_dev, true)
    }

    pub fn load_for_auth_cli() -> anyhow::Result<Self> {
        Self::load_internal(false, false)
    }

    fn load_internal(insecure_dev: bool, validate_listener_security: bool) -> anyhow::Result<Self> {
        let environment = match std::env::var("STREAMSERVER_ENV") {
            Ok(value) => value,
            Err(_) => "development".into(),
        };
        let environment = environment.trim().to_string();
        let builder = config::Config::builder()
            .add_source(config::File::with_name("config/base").required(false))
            .add_source(config::File::with_name(&format!("config/{environment}")).required(false));

        let mut file_settings = builder.build()?.try_deserialize::<FileSettings>()?;
        apply_env_overrides(&mut file_settings)?;
        if insecure_dev {
            file_settings.core.insecure_dev = true;
        }

        let settings = Self {
            environment,
            logging: file_settings.logging,
            core: file_settings.core,
        };

        settings.validate(validate_listener_security)?;
        Ok(settings)
    }

    fn validate(&self, validate_listener_security: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.core.http_addr.trim().is_empty(),
            "core.http_addr must not be empty"
        );
        anyhow::ensure!(
            !self.core.grpc_addr.trim().is_empty(),
            "core.grpc_addr must not be empty"
        );
        if validate_listener_security {
            validate_security_policy(&self.environment, &self.core)?;
        }
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
        anyhow::ensure!(
            self.core.callback_timeout_ms > 0,
            "CALLBACK_TIMEOUT_MS must be positive"
        );
        anyhow::ensure!(
            self.core.callback_max_attempts > 0,
            "CALLBACK_MAX_ATTEMPTS must be positive"
        );
        anyhow::ensure!(
            self.core.callback_initial_backoff_ms > 0,
            "CALLBACK_INITIAL_BACKOFF_MS must be positive"
        );
        anyhow::ensure!(
            self.core.callback_max_backoff_ms >= self.core.callback_initial_backoff_ms,
            "CALLBACK_MAX_BACKOFF_MS must be greater than or equal to CALLBACK_INITIAL_BACKOFF_MS"
        );
        anyhow::ensure!(
            self.core.callback_settle_delay_ms > 0,
            "CALLBACK_SETTLE_DELAY_MS must be positive"
        );
        if !self.core.source_gateway_base_url.trim().is_empty() {
            anyhow::ensure!(
                self.core.source_gateway_prefetch_poll_ms > 0,
                "SOURCE_GATEWAY_PREFETCH_POLL_MS must be positive"
            );
            anyhow::ensure!(
                self.core.source_gateway_prefetch_timeout_ms
                    >= self.core.source_gateway_prefetch_poll_ms,
                "SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS must be greater than or equal to SOURCE_GATEWAY_PREFETCH_POLL_MS"
            );
        }
        Ok(())
    }
}

pub(crate) fn validate_security_policy(
    environment: &str,
    core: &CoreSettings,
) -> anyhow::Result<()> {
    let http_addr = parse_bind_addr("CORE_HTTP_ADDR", &core.http_addr)?;
    let grpc_addr = parse_bind_addr("CORE_GRPC_ADDR", &core.grpc_addr)?;
    let environment = environment.trim();
    let is_production = environment.eq_ignore_ascii_case("production");
    let is_development = environment.eq_ignore_ascii_case("development");

    let http_tls_fields = [
        core.http_tls_cert_path.trim(),
        core.http_tls_key_path.trim(),
    ];
    let http_tls_configured = http_tls_fields.iter().all(|value| !value.is_empty());
    anyhow::ensure!(
        http_tls_fields.iter().all(|value| value.is_empty()) || http_tls_configured,
        "CORE_HTTP_TLS_CERT_PATH and CORE_HTTP_TLS_KEY_PATH must be set together"
    );

    let grpc_tls_fields = [
        core.grpc_tls_cert_path.trim(),
        core.grpc_tls_key_path.trim(),
        core.grpc_tls_client_ca_path.trim(),
    ];
    let grpc_mtls_configured = grpc_tls_fields.iter().all(|value| !value.is_empty());
    anyhow::ensure!(
        grpc_tls_fields.iter().all(|value| value.is_empty()) || grpc_mtls_configured,
        "CORE_GRPC_TLS_CERT_PATH, CORE_GRPC_TLS_KEY_PATH and CORE_GRPC_TLS_CLIENT_CA_PATH must all be set together"
    );

    if is_production {
        anyhow::ensure!(
            core.auth_mode != AuthMode::Disabled,
            "production requires AUTH_MODE other than disabled; configure local_password or external_jwt"
        );
    }

    if core.insecure_dev {
        anyhow::ensure!(
            is_development,
            "--insecure-dev is allowed only in development; remove it or set STREAMSERVER_ENV=development"
        );
        anyhow::ensure!(
            http_addr.ip().is_loopback() && grpc_addr.ip().is_loopback(),
            "--insecure-dev requires loopback HTTP and gRPC addresses"
        );
    }

    anyhow::ensure!(
        http_tls_configured || http_addr.ip().is_loopback(),
        "non-loopback CORE_HTTP_ADDR requires HTTP TLS; configure CORE_HTTP_TLS_CERT_PATH and CORE_HTTP_TLS_KEY_PATH"
    );

    if is_production {
        anyhow::ensure!(
            grpc_mtls_configured,
            "production requires gRPC mTLS; configure CORE_GRPC_TLS_CERT_PATH, CORE_GRPC_TLS_KEY_PATH and CORE_GRPC_TLS_CLIENT_CA_PATH"
        );
    } else if !grpc_mtls_configured {
        anyhow::ensure!(
            grpc_addr.ip().is_loopback(),
            "non-loopback CORE_GRPC_ADDR requires gRPC mTLS"
        );
        anyhow::ensure!(
            is_development && core.insecure_dev,
            "development plaintext gRPC requires --insecure-dev and a loopback CORE_GRPC_ADDR"
        );
    }

    Ok(())
}

fn parse_bind_addr(name: &str, value: &str) -> anyhow::Result<SocketAddr> {
    value
        .trim()
        .parse::<SocketAddr>()
        .map_err(|error| anyhow::anyhow!("{name} must be an IP socket address: {error}"))
}

fn apply_env_overrides(settings: &mut FileSettings) -> anyhow::Result<()> {
    // 环境变量覆盖只做单字段解析；TLS、鉴权和回调退避的组合约束
    // 继续集中在 validate() 中处理，避免覆盖阶段提前耦合业务规则。
    if let Some(value) = env("CORE_HTTP_ADDR") {
        settings.core.http_addr = value;
    }
    if let Some(value) = env("CORE_HTTP_TLS_CERT_PATH") {
        settings.core.http_tls_cert_path = value;
    }
    if let Some(value) = env("CORE_HTTP_TLS_KEY_PATH") {
        settings.core.http_tls_key_path = value;
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
            other => anyhow::bail!("unsupported AUTH_MODE: {other}"),
        };
    } else if let Some(value) = env("AUTH_ENABLED") {
        settings.core.auth_mode = if matches!(value.as_str(), "1" | "true" | "TRUE" | "yes") {
            AuthMode::ExternalJwt
        } else {
            AuthMode::Disabled
        };
    }
    if let Some(value) = env("CORE_INSECURE_DEV") {
        settings.core.insecure_dev = parse_bool_env("CORE_INSECURE_DEV", &value)?;
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
    // 回调重试参数直接影响后台任务调度，非法值必须变成启动期配置错误。
    if let Some(value) = env("CALLBACK_TIMEOUT_MS") {
        settings.core.callback_timeout_ms = parse_required_env("CALLBACK_TIMEOUT_MS", &value)?;
    }
    if let Some(value) = env("CALLBACK_MAX_ATTEMPTS") {
        settings.core.callback_max_attempts = parse_required_env("CALLBACK_MAX_ATTEMPTS", &value)?;
    }
    if let Some(value) = env("CALLBACK_INITIAL_BACKOFF_MS") {
        settings.core.callback_initial_backoff_ms =
            parse_required_env("CALLBACK_INITIAL_BACKOFF_MS", &value)?;
    }
    if let Some(value) = env("CALLBACK_MAX_BACKOFF_MS") {
        settings.core.callback_max_backoff_ms =
            parse_required_env("CALLBACK_MAX_BACKOFF_MS", &value)?;
    }
    if let Some(value) = env("CALLBACK_SETTLE_DELAY_MS") {
        settings.core.callback_settle_delay_ms =
            parse_required_env("CALLBACK_SETTLE_DELAY_MS", &value)?;
    }
    if let Some(value) = env("CALLBACK_SHARED_SECRET") {
        settings.core.callback_shared_secret = value;
    }
    if let Some(value) = env("STORAGE_ALLOWLIST") {
        settings.core.storage_allowlist = split_csv(&value);
    }
    if let Some(value) = env("SOURCE_GATEWAY_BASE_URL") {
        settings.core.source_gateway_base_url = value;
    }
    if let Some(value) = env("SOURCE_GATEWAY_PREFETCH_POLL_MS") {
        settings.core.source_gateway_prefetch_poll_ms =
            parse_required_env("SOURCE_GATEWAY_PREFETCH_POLL_MS", &value)?;
    }
    if let Some(value) = env("SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS") {
        settings.core.source_gateway_prefetch_timeout_ms =
            parse_required_env("SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS", &value)?;
    }
    if let Some(value) = env("LOG_LEVEL") {
        settings.logging.level = value;
    }
    if let Some(value) = env("LOG_JSON") {
        settings.logging.json = matches!(value.as_str(), "1" | "true" | "TRUE" | "yes");
    }
    Ok(())
}

fn env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        }
        Err(_) => None,
    }
}

fn parse_required_env<T>(name: &str, value: &str) -> anyhow::Result<T>
where
    T: FromStr,
{
    T::from_str(value).map_err(|_| anyhow::anyhow!("{name} must be an integer"))
}

fn parse_bool_env(name: &str, value: &str) -> anyhow::Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        _ => anyhow::bail!("{name} must be true or false"),
    }
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
    "127.0.0.1:8080".to_string()
}

fn default_http_tls_cert_path() -> String {
    String::new()
}

fn default_http_tls_key_path() -> String {
    String::new()
}

fn default_core_grpc_addr() -> String {
    "127.0.0.1:50051".to_string()
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
    vec!["/data/media/work".to_string(), "/data/zlm/www".to_string()]
}

fn default_callback_timeout_ms() -> u64 {
    5_000
}

fn default_callback_max_attempts() -> u32 {
    8
}

fn default_callback_initial_backoff_ms() -> u64 {
    5_000
}

fn default_callback_max_backoff_ms() -> u64 {
    300_000
}

fn default_callback_settle_delay_ms() -> u64 {
    8_000
}

fn default_source_gateway_prefetch_poll_ms() -> u64 {
    1_000
}

fn default_source_gateway_prefetch_timeout_ms() -> u64 {
    600_000
}

pub fn parse_duration_spec(value: &str) -> anyhow::Result<Duration> {
    let trimmed = value.trim();
    anyhow::ensure!(!trimmed.is_empty(), "duration must not be empty");
    anyhow::ensure!(trimmed.len() >= 2, "duration is too short");

    let (number, unit) = trimmed.split_at(trimmed.len() - 1);
    let amount = i64::from_str(number.trim())
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
