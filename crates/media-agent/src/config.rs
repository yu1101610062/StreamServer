use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Settings {
    pub environment: String,
    pub logging: LoggingSettings,
    pub agent: AgentSettings,
}

#[derive(Debug, Clone, Deserialize)]
struct FileSettings {
    #[serde(default)]
    logging: LoggingSettings,
    #[serde(default)]
    agent: AgentSettings,
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
pub struct AgentSettings {
    #[serde(default = "default_http_addr")]
    pub http_addr: String,
    #[serde(default)]
    pub node_id: String,
    #[serde(default = "default_node_name")]
    pub node_name: String,
    #[serde(default = "default_core_endpoint")]
    pub core_endpoint: String,
    #[serde(default = "default_cert_path")]
    pub cert_path: String,
    #[serde(default = "default_key_path")]
    pub key_path: String,
    #[serde(default = "default_ca_path")]
    pub ca_path: String,
    #[serde(default)]
    pub tls_domain_name: String,
    #[serde(default = "default_ffmpeg_bin")]
    pub ffmpeg_bin: String,
    #[serde(default = "default_ffprobe_bin")]
    pub ffprobe_bin: String,
    #[serde(default = "default_zlm_api_base")]
    pub zlm_api_base: String,
    #[serde(default)]
    pub zlm_api_secret: String,
    #[serde(default)]
    pub zlm_auto_close_on_no_reader_enabled: bool,
    #[serde(default = "default_agent_stream_addr")]
    pub agent_stream_addr: String,
    #[serde(default)]
    pub primary_interface_name: String,
    #[serde(default)]
    pub primary_interface_ip: String,
    #[serde(default)]
    pub multicast_interface_name: String,
    #[serde(default)]
    pub multicast_interface_ip: String,
    #[serde(default = "default_network_mode")]
    pub network_mode: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default = "default_max_runtime_slots")]
    pub max_runtime_slots: u32,
    #[serde(default = "default_work_root")]
    pub work_root: String,
    #[serde(default = "default_acceleration_mode")]
    pub acceleration_mode: String,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            http_addr: default_http_addr(),
            node_id: String::new(),
            node_name: default_node_name(),
            core_endpoint: default_core_endpoint(),
            cert_path: default_cert_path(),
            key_path: default_key_path(),
            ca_path: default_ca_path(),
            tls_domain_name: String::new(),
            ffmpeg_bin: default_ffmpeg_bin(),
            ffprobe_bin: default_ffprobe_bin(),
            zlm_api_base: default_zlm_api_base(),
            zlm_api_secret: String::new(),
            zlm_auto_close_on_no_reader_enabled: false,
            agent_stream_addr: default_agent_stream_addr(),
            primary_interface_name: String::new(),
            primary_interface_ip: String::new(),
            multicast_interface_name: String::new(),
            multicast_interface_ip: String::new(),
            network_mode: default_network_mode(),
            labels: Vec::new(),
            max_runtime_slots: default_max_runtime_slots(),
            work_root: default_work_root(),
            acceleration_mode: default_acceleration_mode(),
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
            agent: file_settings.agent,
        };
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.agent.http_addr.trim().is_empty(),
            "agent.http_addr must not be empty"
        );
        anyhow::ensure!(
            self.agent.node_id.trim().is_empty()
                || uuid::Uuid::parse_str(self.agent.node_id.trim()).is_ok(),
            "AGENT_NODE_ID must be a valid UUID when provided"
        );
        anyhow::ensure!(
            !self.agent.node_name.trim().is_empty(),
            "AGENT_NODE_NAME must not be empty"
        );
        anyhow::ensure!(
            !self.agent.core_endpoint.trim().is_empty(),
            "AGENT_CORE_ENDPOINT must not be empty"
        );
        if self.agent.core_endpoint.starts_with("https://") {
            anyhow::ensure!(
                !self.agent.cert_path.trim().is_empty(),
                "AGENT_CERT_PATH must not be empty when AGENT_CORE_ENDPOINT uses https"
            );
            anyhow::ensure!(
                !self.agent.key_path.trim().is_empty(),
                "AGENT_KEY_PATH must not be empty when AGENT_CORE_ENDPOINT uses https"
            );
            anyhow::ensure!(
                !self.agent.ca_path.trim().is_empty(),
                "AGENT_CA_PATH must not be empty when AGENT_CORE_ENDPOINT uses https"
            );
        }
        anyhow::ensure!(
            !self.agent.ffmpeg_bin.trim().is_empty(),
            "FFMPEG_BIN must not be empty"
        );
        anyhow::ensure!(
            !self.agent.ffprobe_bin.trim().is_empty(),
            "FFPROBE_BIN must not be empty"
        );
        anyhow::ensure!(
            !self.agent.agent_stream_addr.trim().is_empty(),
            "AGENT_STREAM_ADDR must not be empty"
        );
        anyhow::ensure!(
            matches!(
                self.agent.network_mode.trim(),
                "bridge" | "host" | "macvlan"
            ),
            "AGENT_NETWORK_MODE must be one of bridge/host/macvlan"
        );
        anyhow::ensure!(
            !self.agent.work_root.trim().is_empty(),
            "WORK_ROOT must not be empty"
        );
        anyhow::ensure!(
            matches!(self.agent.acceleration_mode.trim(), "cpu" | "gpu"),
            "AGENT_ACCELERATION_MODE must be one of cpu/gpu"
        );

        Ok(())
    }
}

fn apply_env_overrides(settings: &mut FileSettings) {
    if let Some(value) = env("AGENT_HTTP_ADDR") {
        settings.agent.http_addr = value;
    }
    if let Some(value) = env("AGENT_NODE_ID") {
        settings.agent.node_id = value;
    }
    if let Some(value) = env("AGENT_NODE_NAME") {
        settings.agent.node_name = value;
    }
    if let Some(value) = env("AGENT_CORE_ENDPOINT") {
        settings.agent.core_endpoint = value;
    }
    if let Some(value) = env("AGENT_CERT_PATH") {
        settings.agent.cert_path = value;
    }
    if let Some(value) = env("AGENT_KEY_PATH") {
        settings.agent.key_path = value;
    }
    if let Some(value) = env("AGENT_CA_PATH") {
        settings.agent.ca_path = value;
    }
    if let Some(value) = env("AGENT_TLS_DOMAIN_NAME") {
        settings.agent.tls_domain_name = value;
    }
    if let Some(value) = env("FFMPEG_BIN") {
        settings.agent.ffmpeg_bin = value;
    }
    if let Some(value) = env("FFPROBE_BIN") {
        settings.agent.ffprobe_bin = value;
    }
    if let Some(value) = env("ZLM_API_BASE") {
        settings.agent.zlm_api_base = value;
    }
    if let Some(value) = env("ZLM_API_SECRET") {
        settings.agent.zlm_api_secret = value;
    }
    if let Some(value) = env("ZLM_AUTO_CLOSE_ON_NO_READER_ENABLED") {
        settings.agent.zlm_auto_close_on_no_reader_enabled =
            matches!(value.as_str(), "1" | "true" | "TRUE" | "yes");
    }
    if let Some(value) = env("AGENT_STREAM_ADDR") {
        settings.agent.agent_stream_addr = value;
    }
    if let Some(value) = env("AGENT_PRIMARY_INTERFACE_NAME") {
        settings.agent.primary_interface_name = value;
    }
    if let Some(value) = env("AGENT_PRIMARY_INTERFACE_IP") {
        settings.agent.primary_interface_ip = value;
    }
    if let Some(value) = env("AGENT_MULTICAST_INTERFACE_NAME") {
        settings.agent.multicast_interface_name = value;
    }
    if let Some(value) = env("AGENT_MULTICAST_INTERFACE_IP") {
        settings.agent.multicast_interface_ip = value;
    }
    if let Some(value) = env("AGENT_NETWORK_MODE") {
        settings.agent.network_mode = value;
    }
    if let Some(value) = env("AGENT_LABELS") {
        settings.agent.labels = split_csv(&value);
    }
    if let Some(value) = env("AGENT_MAX_RUNTIME_SLOTS") {
        settings.agent.max_runtime_slots = value.parse().unwrap_or(default_max_runtime_slots());
    }
    if let Some(value) = env("WORK_ROOT") {
        settings.agent.work_root = value;
    }
    if let Some(value) = env("AGENT_ACCELERATION_MODE") {
        settings.agent.acceleration_mode = value;
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

fn default_http_addr() -> String {
    "0.0.0.0:8081".to_string()
}

fn default_node_name() -> String {
    "local-agent".to_string()
}

fn default_core_endpoint() -> String {
    "http://127.0.0.1:50051".to_string()
}

fn default_cert_path() -> String {
    "certs/agent.pem".to_string()
}

fn default_key_path() -> String {
    "certs/agent.key".to_string()
}

fn default_ca_path() -> String {
    "certs/ca.pem".to_string()
}

fn default_ffmpeg_bin() -> String {
    "ffmpeg".to_string()
}

fn default_ffprobe_bin() -> String {
    "ffprobe".to_string()
}

fn default_zlm_api_base() -> String {
    "http://127.0.0.1:8080".to_string()
}

fn default_agent_stream_addr() -> String {
    "http://127.0.0.1:8081".to_string()
}

fn default_network_mode() -> String {
    "bridge".to_string()
}

fn default_max_runtime_slots() -> u32 {
    4
}

fn default_work_root() -> String {
    "/data/media/work".to_string()
}

fn default_acceleration_mode() -> String {
    "cpu".to_string()
}
