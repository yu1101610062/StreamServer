use std::{env::VarError, ffi::OsString, net::SocketAddr, path::Path, str::FromStr};

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentEnvironment {
    Development,
    Production,
}

impl AgentEnvironment {
    pub(crate) fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "development" => Ok(Self::Development),
            "production" => Ok(Self::Production),
            _ => anyhow::bail!("STREAMSERVER_ENV must be exactly `development` or `production`"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Production => "production",
        }
    }
}

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
    #[serde(default = "default_public_media_addr")]
    pub public_media_addr: String,
    #[serde(default)]
    pub public_media_expose: bool,
    #[serde(default)]
    pub public_media_tls_cert_path: String,
    #[serde(default)]
    pub public_media_tls_key_path: String,
    #[serde(default = "default_management_addr")]
    pub management_addr: String,
    #[serde(default = "default_management_tls_cert_path")]
    pub management_tls_cert_path: String,
    #[serde(default = "default_management_tls_key_path")]
    pub management_tls_key_path: String,
    #[serde(default = "default_management_tls_client_ca_path")]
    pub management_tls_client_ca_path: String,
    #[serde(default = "default_management_capability_jwt_public_key_path")]
    pub management_capability_jwt_public_key_path: String,
    #[serde(default = "default_management_max_concurrency")]
    pub management_max_concurrency: usize,
    #[serde(default = "default_management_chunk_idle_timeout_sec")]
    pub management_chunk_idle_timeout_sec: u64,
    #[serde(default = "default_zlm_hook_addr")]
    pub zlm_hook_addr: String,
    #[serde(default)]
    pub zlm_hook_shared_secret: String,
    #[serde(default = "default_zlm_hook_queue_capacity")]
    pub zlm_hook_queue_capacity: usize,
    #[serde(default = "default_zlm_hook_timeout_sec")]
    pub zlm_hook_timeout_sec: u64,
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
    #[serde(default = "default_identity_dir")]
    pub identity_dir: String,
    #[serde(default)]
    pub tls_domain_name: String,
    #[serde(default = "default_ffmpeg_bin")]
    pub ffmpeg_bin: String,
    #[serde(default = "default_ffprobe_bin")]
    pub ffprobe_bin: String,
    #[serde(default = "default_zlm_api_base")]
    pub zlm_api_base: String,
    #[serde(default = "default_zlm_rtmp_port")]
    pub zlm_rtmp_port: u16,
    #[serde(default = "default_zlm_rtsp_port")]
    pub zlm_rtsp_port: u16,
    #[serde(default)]
    pub zlm_api_secret: String,
    #[serde(default)]
    pub zlm_auto_close_on_no_reader_enabled: bool,
    #[serde(default = "default_allow_enhanced_rtmp_expose")]
    pub allow_enhanced_rtmp_expose: bool,
    #[serde(default = "default_mp4_record_segment_sec")]
    pub mp4_record_segment_sec: u32,
    #[serde(default = "default_hls_record_segment_sec")]
    pub hls_record_segment_sec: u32,
    #[serde(default = "default_agent_stream_addr")]
    pub agent_stream_addr: String,
    #[serde(default)]
    pub primary_interface_name: String,
    #[serde(default)]
    pub primary_interface_ip: String,
    #[serde(default = "default_output_mount_relative_prefix")]
    pub output_mount_relative_prefix_mp4: String,
    #[serde(default = "default_output_mount_relative_prefix")]
    pub output_mount_relative_prefix_hls: String,
    #[serde(default = "default_zlm_output_mp4_root")]
    pub zlm_output_mp4_root: String,
    #[serde(default = "default_zlm_output_hls_root")]
    pub zlm_output_hls_root: String,
    #[serde(default)]
    pub multicast_interface_name: String,
    #[serde(default)]
    pub multicast_interface_ip: String,
    #[serde(default = "default_network_mode")]
    pub network_mode: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub max_live_runtime_slots: u32,
    #[serde(default)]
    pub max_vod_runtime_slots: u32,
    #[serde(default = "default_runtime_manager_start_limit")]
    pub runtime_manager_start_limit: usize,
    #[serde(default = "default_runtime_manager_stop_limit")]
    pub runtime_manager_stop_limit: usize,
    #[serde(default = "default_runtime_manager_recording_limit")]
    pub runtime_manager_recording_limit: usize,
    #[serde(default = "default_runtime_manager_adopt_limit")]
    pub runtime_manager_adopt_limit: usize,
    #[serde(default = "default_work_root")]
    pub work_root: String,
    #[serde(default = "default_upload_max_bytes")]
    pub upload_max_bytes: u64,
    #[serde(default = "default_upload_allowed_extensions")]
    pub upload_allowed_extensions: Vec<String>,
    #[serde(default = "default_upload_probe_timeout_sec")]
    pub upload_probe_timeout_sec: u64,
    #[serde(default)]
    pub public_media_base_url: String,
    #[serde(default = "default_acceleration_mode")]
    pub acceleration_mode: String,
    #[serde(default = "default_runtime_log_tail_bytes")]
    pub runtime_log_tail_bytes: usize,
    #[serde(default = "default_runtime_log_max_file_bytes")]
    pub runtime_log_max_file_bytes: u64,
    #[serde(default = "default_runtime_log_retention_days")]
    pub runtime_log_retention_days: u64,
    #[serde(default)]
    pub artifact_cleanup: AgentArtifactCleanupSettings,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentArtifactCleanupSettings {
    #[serde(default = "default_artifact_cleanup_enabled")]
    pub enabled: bool,
    #[serde(default = "default_artifact_cleanup_threshold_percent")]
    pub threshold_percent: f64,
    #[serde(default = "default_artifact_cleanup_strategy")]
    pub strategy: String,
    #[serde(default = "default_artifact_cleanup_check_interval_sec")]
    pub check_interval_sec: u64,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            public_media_addr: default_public_media_addr(),
            public_media_expose: false,
            public_media_tls_cert_path: String::new(),
            public_media_tls_key_path: String::new(),
            management_addr: default_management_addr(),
            management_tls_cert_path: default_management_tls_cert_path(),
            management_tls_key_path: default_management_tls_key_path(),
            management_tls_client_ca_path: default_management_tls_client_ca_path(),
            management_capability_jwt_public_key_path:
                default_management_capability_jwt_public_key_path(),
            management_max_concurrency: default_management_max_concurrency(),
            management_chunk_idle_timeout_sec: default_management_chunk_idle_timeout_sec(),
            zlm_hook_addr: default_zlm_hook_addr(),
            zlm_hook_shared_secret: String::new(),
            zlm_hook_queue_capacity: default_zlm_hook_queue_capacity(),
            zlm_hook_timeout_sec: default_zlm_hook_timeout_sec(),
            node_id: String::new(),
            node_name: default_node_name(),
            core_endpoint: default_core_endpoint(),
            cert_path: default_cert_path(),
            key_path: default_key_path(),
            ca_path: default_ca_path(),
            identity_dir: default_identity_dir(),
            tls_domain_name: String::new(),
            ffmpeg_bin: default_ffmpeg_bin(),
            ffprobe_bin: default_ffprobe_bin(),
            zlm_api_base: default_zlm_api_base(),
            zlm_rtmp_port: default_zlm_rtmp_port(),
            zlm_rtsp_port: default_zlm_rtsp_port(),
            zlm_api_secret: String::new(),
            zlm_auto_close_on_no_reader_enabled: false,
            allow_enhanced_rtmp_expose: default_allow_enhanced_rtmp_expose(),
            mp4_record_segment_sec: default_mp4_record_segment_sec(),
            hls_record_segment_sec: default_hls_record_segment_sec(),
            agent_stream_addr: default_agent_stream_addr(),
            primary_interface_name: String::new(),
            primary_interface_ip: String::new(),
            output_mount_relative_prefix_mp4: default_output_mount_relative_prefix(),
            output_mount_relative_prefix_hls: default_output_mount_relative_prefix(),
            zlm_output_mp4_root: default_zlm_output_mp4_root(),
            zlm_output_hls_root: default_zlm_output_hls_root(),
            multicast_interface_name: String::new(),
            multicast_interface_ip: String::new(),
            network_mode: default_network_mode(),
            labels: Vec::new(),
            max_live_runtime_slots: 0,
            max_vod_runtime_slots: 0,
            runtime_manager_start_limit: default_runtime_manager_start_limit(),
            runtime_manager_stop_limit: default_runtime_manager_stop_limit(),
            runtime_manager_recording_limit: default_runtime_manager_recording_limit(),
            runtime_manager_adopt_limit: default_runtime_manager_adopt_limit(),
            work_root: default_work_root(),
            upload_max_bytes: default_upload_max_bytes(),
            upload_allowed_extensions: default_upload_allowed_extensions(),
            upload_probe_timeout_sec: default_upload_probe_timeout_sec(),
            public_media_base_url: String::new(),
            acceleration_mode: default_acceleration_mode(),
            runtime_log_tail_bytes: default_runtime_log_tail_bytes(),
            runtime_log_max_file_bytes: default_runtime_log_max_file_bytes(),
            runtime_log_retention_days: default_runtime_log_retention_days(),
            artifact_cleanup: AgentArtifactCleanupSettings::default(),
        }
    }
}

impl Default for AgentArtifactCleanupSettings {
    fn default() -> Self {
        Self {
            enabled: default_artifact_cleanup_enabled(),
            threshold_percent: default_artifact_cleanup_threshold_percent(),
            strategy: default_artifact_cleanup_strategy(),
            check_interval_sec: default_artifact_cleanup_check_interval_sec(),
        }
    }
}

impl Settings {
    pub fn load() -> anyhow::Result<Self> {
        let environment =
            environment_from_var(std::env::var("STREAMSERVER_ENV").map(OsString::from))?
                .as_str()
                .to_string();
        let builder = config::Config::builder()
            .add_source(config::File::with_name("config/base").required(false))
            .add_source(config::File::with_name(&format!("config/{environment}")).required(false));

        let config = builder.build()?;
        reject_legacy_listener_config(&config)?;
        reject_legacy_runtime_slot_config(&config)?;
        require_runtime_slot_config(&config)?;
        let mut file_settings = config.try_deserialize::<FileSettings>()?;
        apply_env_overrides(&mut file_settings)?;

        let settings = Self {
            environment,
            logging: file_settings.logging,
            agent: file_settings.agent,
        };
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> anyhow::Result<()> {
        let environment = self.environment_kind()?;
        let public_media_addr =
            parse_listener_addr("AGENT_PUBLIC_MEDIA_ADDR", &self.agent.public_media_addr)?;
        let management_addr =
            parse_listener_addr("AGENT_MANAGEMENT_ADDR", &self.agent.management_addr)?;
        let zlm_hook_addr = parse_listener_addr("AGENT_ZLM_HOOK_ADDR", &self.agent.zlm_hook_addr)?;
        let public_tls_fields = [
            self.agent.public_media_tls_cert_path.trim(),
            self.agent.public_media_tls_key_path.trim(),
        ];
        let public_tls_configured = public_tls_fields.iter().all(|value| !value.is_empty());
        anyhow::ensure!(
            public_tls_fields.iter().all(|value| value.is_empty()) || public_tls_configured,
            "AGENT_PUBLIC_MEDIA_TLS_CERT_PATH and AGENT_PUBLIC_MEDIA_TLS_KEY_PATH must be configured together"
        );
        if environment == AgentEnvironment::Production && !public_media_addr.ip().is_loopback() {
            anyhow::ensure!(
                self.agent.public_media_expose,
                "production non-loopback AGENT_PUBLIC_MEDIA_ADDR requires AGENT_PUBLIC_MEDIA_EXPOSE=true"
            );
            anyhow::ensure!(
                public_tls_configured,
                "production non-loopback public media listener requires TLS"
            );
        }
        if environment == AgentEnvironment::Development {
            let management_tls_fields = [
                self.agent.management_tls_cert_path.trim(),
                self.agent.management_tls_key_path.trim(),
                self.agent.management_tls_client_ca_path.trim(),
                self.agent.management_capability_jwt_public_key_path.trim(),
            ];
            anyhow::ensure!(
                management_tls_fields.iter().all(|value| !value.is_empty()),
                "development Agent management listener requires server cert/key, Core client CA, and capability JWT public key"
            );
            anyhow::ensure!(
                management_tls_fields
                    .iter()
                    .all(|value| Path::new(value).is_absolute()),
                "development Agent management TLS and capability key paths must be absolute"
            );
        }
        anyhow::ensure!(
            (1..=32).contains(&self.agent.management_max_concurrency),
            "AGENT_MANAGEMENT_MAX_CONCURRENCY must be between 1 and 32"
        );
        anyhow::ensure!(
            (1..=300).contains(&self.agent.management_chunk_idle_timeout_sec),
            "AGENT_MANAGEMENT_CHUNK_IDLE_TIMEOUT_SEC must be between 1 and 300"
        );
        anyhow::ensure!(
            management_addr.port() > 0,
            "AGENT_MANAGEMENT_ADDR port must be greater than 0"
        );
        anyhow::ensure!(
            zlm_hook_addr.ip().is_loopback() && zlm_hook_addr.port() > 0,
            "AGENT_ZLM_HOOK_ADDR must be a loopback listener with a non-zero port"
        );
        let zlm_hook_secret = self.agent.zlm_hook_shared_secret.as_bytes();
        anyhow::ensure!(
            !zlm_hook_secret.is_empty()
                && zlm_hook_secret.len() <= 256
                && zlm_hook_secret.iter().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'-')
                }),
            "ZLM_HOOK_SHARED_SECRET must be a non-empty URL-safe token of at most 256 bytes"
        );
        if environment == AgentEnvironment::Production {
            anyhow::ensure!(
                zlm_hook_secret.len() >= 32,
                "production ZLM_HOOK_SHARED_SECRET must contain at least 32 bytes"
            );
        }
        anyhow::ensure!(
            (1..=1024).contains(&self.agent.zlm_hook_queue_capacity),
            "AGENT_ZLM_HOOK_QUEUE_CAPACITY must be between 1 and 1024"
        );
        anyhow::ensure!(
            (1..=4).contains(&self.agent.zlm_hook_timeout_sec),
            "AGENT_ZLM_HOOK_TIMEOUT_SEC must be between 1 and 4 so Agent fails before ZLM's 5-second hook timeout"
        );
        if !self.agent.node_id.trim().is_empty() {
            let configured_node = uuid::Uuid::parse_str(self.agent.node_id.trim())
                .map_err(|_| anyhow::anyhow!("AGENT_NODE_ID must be a valid UUID when provided"))?;
            anyhow::ensure!(
                !configured_node.is_nil(),
                "AGENT_NODE_ID must not be the nil UUID"
            );
        }
        anyhow::ensure!(
            !self.agent.node_name.trim().is_empty(),
            "AGENT_NODE_NAME must not be empty"
        );
        anyhow::ensure!(
            !self.agent.core_endpoint.trim().is_empty(),
            "AGENT_CORE_ENDPOINT must not be empty"
        );
        anyhow::ensure!(
            self.agent.identity_dir.trim().is_empty()
                || Path::new(self.agent.identity_dir.trim()).is_absolute(),
            "AGENT_IDENTITY_DIR must be an absolute path when provided"
        );
        if environment == AgentEnvironment::Production {
            anyhow::ensure!(
                self.agent.core_endpoint.starts_with("https://"),
                "production AGENT_CORE_ENDPOINT must use https with Agent mTLS"
            );
            anyhow::ensure!(
                !self.agent.identity_dir.trim().is_empty()
                    && Path::new(self.agent.identity_dir.trim()).is_absolute(),
                "production AGENT_IDENTITY_DIR must be an absolute path"
            );
        }
        if self.agent.core_endpoint.starts_with("https://")
            && self.agent.identity_dir.trim().is_empty()
        {
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
            self.agent.zlm_rtmp_port > 0,
            "ZLM_RTMP_PORT must be a valid port greater than 0"
        );
        anyhow::ensure!(
            self.agent.zlm_rtsp_port > 0,
            "ZLM_RTSP_PORT must be a valid port greater than 0"
        );
        anyhow::ensure!(
            matches!(
                self.agent.network_mode.trim(),
                "bridge" | "host" | "macvlan"
            ),
            "AGENT_NETWORK_MODE must be one of bridge/host/macvlan"
        );
        anyhow::ensure!(
            !self.agent.zlm_output_mp4_root.trim().is_empty()
                && Path::new(self.agent.zlm_output_mp4_root.trim()).is_absolute(),
            "ZLM_OUTPUT_MP4_ROOT must be an absolute path"
        );
        anyhow::ensure!(
            !self.agent.zlm_output_hls_root.trim().is_empty()
                && Path::new(self.agent.zlm_output_hls_root.trim()).is_absolute(),
            "ZLM_OUTPUT_HLS_ROOT must be an absolute path"
        );
        anyhow::ensure!(
            !self.agent.work_root.trim().is_empty(),
            "WORK_ROOT must not be empty"
        );
        anyhow::ensure!(
            self.agent.upload_max_bytes > 0,
            "UPLOAD_MAX_BYTES must be greater than 0"
        );
        anyhow::ensure!(
            !self.agent.upload_allowed_extensions.is_empty(),
            "UPLOAD_ALLOWED_EXTENSIONS must not be empty"
        );
        anyhow::ensure!(
            self.agent
                .upload_allowed_extensions
                .iter()
                .all(|value| !value.trim().is_empty()
                    && !value.contains('/')
                    && !value.contains('\\')
                    && !value.contains('.')),
            "UPLOAD_ALLOWED_EXTENSIONS entries must be bare extensions"
        );
        anyhow::ensure!(
            self.agent.upload_probe_timeout_sec > 0,
            "UPLOAD_PROBE_TIMEOUT_SEC must be greater than 0"
        );
        anyhow::ensure!(
            self.agent.runtime_log_tail_bytes > 0,
            "AGENT_RUNTIME_LOG_TAIL_BYTES must be greater than 0"
        );
        anyhow::ensure!(
            self.agent.runtime_log_max_file_bytes > 0,
            "AGENT_RUNTIME_LOG_MAX_FILE_BYTES must be greater than 0"
        );
        anyhow::ensure!(
            self.agent.runtime_log_retention_days > 0,
            "AGENT_RUNTIME_LOG_RETENTION_DAYS must be greater than 0"
        );
        anyhow::ensure!(
            matches!(self.agent.acceleration_mode.trim(), "cpu" | "gpu"),
            "AGENT_ACCELERATION_MODE must be one of cpu/gpu"
        );
        anyhow::ensure!(
            self.agent.mp4_record_segment_sec > 0,
            "AGENT_MP4_RECORD_SEGMENT_SEC must be greater than 0"
        );
        anyhow::ensure!(
            matches!(self.agent.hls_record_segment_sec, 30 | 60),
            "AGENT_HLS_RECORD_SEGMENT_SEC must be one of 30/60"
        );
        anyhow::ensure!(
            self.agent.artifact_cleanup.threshold_percent.is_finite()
                && self.agent.artifact_cleanup.threshold_percent >= 0.0
                && self.agent.artifact_cleanup.threshold_percent <= 100.0,
            "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT must be between 0 and 100"
        );
        anyhow::ensure!(
            matches!(
                self.agent.artifact_cleanup.strategy.trim(),
                "delete_oldest_then_reject" | "reject_only"
            ),
            "AGENT_ARTIFACT_CLEANUP_STRATEGY must be one of delete_oldest_then_reject/reject_only"
        );
        anyhow::ensure!(
            self.agent.artifact_cleanup.check_interval_sec > 0,
            "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC must be greater than 0"
        );

        Ok(())
    }

    pub(crate) fn environment_kind(&self) -> anyhow::Result<AgentEnvironment> {
        AgentEnvironment::parse(&self.environment)
    }
}

fn apply_env_overrides(settings: &mut FileSettings) -> anyhow::Result<()> {
    // 环境变量覆盖发生在文件配置反序列化之后、validate 之前；这里只负责把文本
    // 转成字段值，跨字段依赖仍统一交给 validate()。
    reject_legacy_agent_http_addr(env("AGENT_HTTP_ADDR"))?;
    if let Some(value) = env("AGENT_PUBLIC_MEDIA_ADDR") {
        settings.agent.public_media_addr = value;
    }
    if let Some(value) = env("AGENT_PUBLIC_MEDIA_EXPOSE") {
        settings.agent.public_media_expose = parse_bool(&value);
    }
    if let Some(value) = env("AGENT_PUBLIC_MEDIA_TLS_CERT_PATH") {
        settings.agent.public_media_tls_cert_path = value;
    }
    if let Some(value) = env("AGENT_PUBLIC_MEDIA_TLS_KEY_PATH") {
        settings.agent.public_media_tls_key_path = value;
    }
    if let Some(value) = env("AGENT_MANAGEMENT_ADDR") {
        settings.agent.management_addr = value;
    }
    if let Some(value) = env("AGENT_MANAGEMENT_TLS_CERT_PATH") {
        settings.agent.management_tls_cert_path = value;
    }
    if let Some(value) = env("AGENT_MANAGEMENT_TLS_KEY_PATH") {
        settings.agent.management_tls_key_path = value;
    }
    if let Some(value) = env("AGENT_MANAGEMENT_TLS_CLIENT_CA_PATH") {
        settings.agent.management_tls_client_ca_path = value;
    }
    if let Some(value) = env("AGENT_MANAGEMENT_CAPABILITY_JWT_PUBLIC_KEY_PATH") {
        settings.agent.management_capability_jwt_public_key_path = value;
    }
    if let Some(value) = env("AGENT_MANAGEMENT_MAX_CONCURRENCY") {
        settings.agent.management_max_concurrency =
            parse_required_env("AGENT_MANAGEMENT_MAX_CONCURRENCY", &value)?;
    }
    if let Some(value) = env("AGENT_MANAGEMENT_CHUNK_IDLE_TIMEOUT_SEC") {
        settings.agent.management_chunk_idle_timeout_sec =
            parse_required_env("AGENT_MANAGEMENT_CHUNK_IDLE_TIMEOUT_SEC", &value)?;
    }
    if let Some(value) = env("AGENT_ZLM_HOOK_ADDR") {
        settings.agent.zlm_hook_addr = value;
    }
    if let Some(value) = env("ZLM_HOOK_SHARED_SECRET") {
        settings.agent.zlm_hook_shared_secret = value;
    }
    if let Some(value) = env("AGENT_ZLM_HOOK_QUEUE_CAPACITY") {
        settings.agent.zlm_hook_queue_capacity =
            parse_required_env("AGENT_ZLM_HOOK_QUEUE_CAPACITY", &value)?;
    }
    if let Some(value) = env("AGENT_ZLM_HOOK_TIMEOUT_SEC") {
        settings.agent.zlm_hook_timeout_sec =
            parse_required_env("AGENT_ZLM_HOOK_TIMEOUT_SEC", &value)?;
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
    if let Some(value) = env("AGENT_IDENTITY_DIR") {
        settings.agent.identity_dir = value;
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
    if let Some(value) = env("ZLM_RTMP_PORT") {
        settings.agent.zlm_rtmp_port = parse_env_or_default(&value, default_zlm_rtmp_port);
    }
    if let Some(value) = env("ZLM_RTSP_PORT") {
        settings.agent.zlm_rtsp_port = parse_env_or_default(&value, default_zlm_rtsp_port);
    }
    if let Some(value) = env("ZLM_API_SECRET") {
        settings.agent.zlm_api_secret = value;
    }
    if let Some(value) = env("ZLM_AUTO_CLOSE_ON_NO_READER_ENABLED") {
        settings.agent.zlm_auto_close_on_no_reader_enabled =
            matches!(value.as_str(), "1" | "true" | "TRUE" | "yes");
    }
    if let Some(value) = env("AGENT_ALLOW_ENHANCED_RTMP_EXPOSE") {
        settings.agent.allow_enhanced_rtmp_expose =
            matches!(value.as_str(), "1" | "true" | "TRUE" | "yes");
    }
    if let Some(value) = env("AGENT_MP4_RECORD_SEGMENT_SEC") {
        settings.agent.mp4_record_segment_sec =
            parse_env_or_default(&value, default_mp4_record_segment_sec);
    }
    if let Some(value) = env("AGENT_HLS_RECORD_SEGMENT_SEC") {
        settings.agent.hls_record_segment_sec =
            parse_env_or_default(&value, default_hls_record_segment_sec);
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
    if let Some(value) = env("OUTPUT_MOUNT_RELATIVE_PREFIX_MP4") {
        settings.agent.output_mount_relative_prefix_mp4 = value;
    }
    if let Some(value) = env("OUTPUT_MOUNT_RELATIVE_PREFIX_HLS") {
        settings.agent.output_mount_relative_prefix_hls = value;
    }
    if let Some(value) = env("ZLM_OUTPUT_MP4_ROOT") {
        settings.agent.zlm_output_mp4_root = value;
    }
    if let Some(value) = env("ZLM_OUTPUT_HLS_ROOT") {
        settings.agent.zlm_output_hls_root = value;
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
    if env("AGENT_MAX_RUNTIME_SLOTS").is_some() {
        anyhow::bail!(
            "AGENT_MAX_RUNTIME_SLOTS has been removed; use AGENT_MAX_LIVE_RUNTIME_SLOTS and AGENT_MAX_VOD_RUNTIME_SLOTS"
        );
    }
    if let Some(value) = env("AGENT_MAX_LIVE_RUNTIME_SLOTS") {
        settings.agent.max_live_runtime_slots =
            parse_required_env("AGENT_MAX_LIVE_RUNTIME_SLOTS", &value)?;
    }
    if let Some(value) = env("AGENT_MAX_VOD_RUNTIME_SLOTS") {
        settings.agent.max_vod_runtime_slots =
            parse_required_env("AGENT_MAX_VOD_RUNTIME_SLOTS", &value)?;
    }
    if let Some(value) = env("AGENT_RUNTIME_MANAGER_START_LIMIT") {
        settings.agent.runtime_manager_start_limit =
            parse_env_or_default(&value, default_runtime_manager_start_limit);
    }
    if let Some(value) = env("AGENT_RUNTIME_MANAGER_STOP_LIMIT") {
        settings.agent.runtime_manager_stop_limit =
            parse_env_or_default(&value, default_runtime_manager_stop_limit);
    }
    if let Some(value) = env("AGENT_RUNTIME_MANAGER_RECORDING_LIMIT") {
        settings.agent.runtime_manager_recording_limit =
            parse_env_or_default(&value, default_runtime_manager_recording_limit);
    }
    if let Some(value) = env("AGENT_RUNTIME_MANAGER_ADOPT_LIMIT") {
        settings.agent.runtime_manager_adopt_limit =
            parse_env_or_default(&value, default_runtime_manager_adopt_limit);
    }
    if let Some(value) = env("WORK_ROOT") {
        settings.agent.work_root = value;
    }
    // 上传限制属于安全边界，解析失败必须让配置加载失败，不能静默退回默认值。
    if let Some(value) = env("UPLOAD_MAX_BYTES") {
        settings.agent.upload_max_bytes = parse_required_env("UPLOAD_MAX_BYTES", &value)?;
    }
    if let Some(value) = env("UPLOAD_ALLOWED_EXTENSIONS") {
        settings.agent.upload_allowed_extensions = split_csv(&value)
            .into_iter()
            .map(|value| value.trim_start_matches('.').to_ascii_lowercase())
            .collect();
    }
    if let Some(value) = env("UPLOAD_PROBE_TIMEOUT_SEC") {
        settings.agent.upload_probe_timeout_sec =
            parse_required_env("UPLOAD_PROBE_TIMEOUT_SEC", &value)?;
    }
    if let Some(value) = env("PUBLIC_MEDIA_BASE_URL") {
        settings.agent.public_media_base_url = value;
    }
    if let Some(value) = env("AGENT_ACCELERATION_MODE") {
        settings.agent.acceleration_mode = value;
    }
    if let Some(value) = env("AGENT_RUNTIME_LOG_TAIL_BYTES") {
        settings.agent.runtime_log_tail_bytes = usize::try_from(parse_required_env::<u64>(
            "AGENT_RUNTIME_LOG_TAIL_BYTES",
            &value,
        )?)
        .unwrap_or(usize::MAX);
    }
    if let Some(value) = env("AGENT_RUNTIME_LOG_MAX_FILE_BYTES") {
        settings.agent.runtime_log_max_file_bytes =
            parse_required_env("AGENT_RUNTIME_LOG_MAX_FILE_BYTES", &value)?;
    }
    if let Some(value) = env("AGENT_RUNTIME_LOG_RETENTION_DAYS") {
        settings.agent.runtime_log_retention_days =
            parse_required_env("AGENT_RUNTIME_LOG_RETENTION_DAYS", &value)?;
    }
    if let Some(value) = env("AGENT_ARTIFACT_CLEANUP_ENABLED") {
        settings.agent.artifact_cleanup.enabled =
            matches!(value.as_str(), "1" | "true" | "TRUE" | "yes");
    }
    if let Some(value) = env("AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT") {
        settings.agent.artifact_cleanup.threshold_percent =
            parse_env_or_default(&value, default_artifact_cleanup_threshold_percent);
    }
    if let Some(value) = env("AGENT_ARTIFACT_CLEANUP_STRATEGY") {
        settings.agent.artifact_cleanup.strategy = value;
    }
    if let Some(value) = env("AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC") {
        settings.agent.artifact_cleanup.check_interval_sec =
            parse_env_or_default(&value, default_artifact_cleanup_check_interval_sec);
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

fn reject_legacy_agent_http_addr(value: Option<String>) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.is_none(),
        "AGENT_HTTP_ADDR is no longer supported; configure AGENT_PUBLIC_MEDIA_ADDR and AGENT_MANAGEMENT_ADDR"
    );
    Ok(())
}

fn environment_from_var(value: Result<OsString, VarError>) -> anyhow::Result<AgentEnvironment> {
    match value {
        Ok(value) => AgentEnvironment::parse(
            value
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("STREAMSERVER_ENV must be valid UTF-8"))?,
        ),
        Err(VarError::NotPresent) => Ok(AgentEnvironment::Development),
        Err(VarError::NotUnicode(_)) => {
            anyhow::bail!("STREAMSERVER_ENV must be valid UTF-8")
        }
    }
}

fn parse_listener_addr(name: &str, value: &str) -> anyhow::Result<SocketAddr> {
    let addr = value
        .trim()
        .parse::<SocketAddr>()
        .map_err(|_| anyhow::anyhow!("{name} must be an IP socket address"))?;
    anyhow::ensure!(addr.port() > 0, "{name} port must be greater than 0");
    Ok(addr)
}

fn parse_bool(value: &str) -> bool {
    matches!(value, "1" | "true" | "TRUE" | "yes")
}

fn config_key_present(config: &config::Config, key: &str) -> bool {
    config.get::<config::Value>(key).is_ok()
}

fn reject_legacy_listener_config(config: &config::Config) -> anyhow::Result<()> {
    anyhow::ensure!(
        !config_key_present(config, "agent.http_addr"),
        "agent.http_addr has been removed; use agent.public_media_addr and agent.management_addr"
    );
    Ok(())
}

fn reject_legacy_runtime_slot_config(config: &config::Config) -> anyhow::Result<()> {
    if config_key_present(config, "agent.max_runtime_slots") {
        anyhow::bail!(
            "agent.max_runtime_slots has been removed; use agent.max_live_runtime_slots and agent.max_vod_runtime_slots"
        );
    }
    if env("AGENT_MAX_RUNTIME_SLOTS").is_some() {
        anyhow::bail!(
            "AGENT_MAX_RUNTIME_SLOTS has been removed; use AGENT_MAX_LIVE_RUNTIME_SLOTS and AGENT_MAX_VOD_RUNTIME_SLOTS"
        );
    }
    Ok(())
}

fn require_runtime_slot_config(config: &config::Config) -> anyhow::Result<()> {
    let has_live = config_key_present(config, "agent.max_live_runtime_slots")
        || env("AGENT_MAX_LIVE_RUNTIME_SLOTS").is_some();
    let has_vod = config_key_present(config, "agent.max_vod_runtime_slots")
        || env("AGENT_MAX_VOD_RUNTIME_SLOTS").is_some();
    anyhow::ensure!(
        has_live,
        "agent.max_live_runtime_slots or AGENT_MAX_LIVE_RUNTIME_SLOTS must be provided"
    );
    anyhow::ensure!(
        has_vod,
        "agent.max_vod_runtime_slots or AGENT_MAX_VOD_RUNTIME_SLOTS must be provided"
    );
    Ok(())
}

fn parse_required_env<T>(name: &str, value: &str) -> anyhow::Result<T>
where
    T: FromStr,
{
    T::from_str(value).map_err(|_| anyhow::anyhow!("{name} must be an integer"))
}

fn parse_env_or_default<T>(value: &str, default: impl FnOnce() -> T) -> T
where
    T: FromStr,
{
    // 兼容旧部署脚本：这些字段历史上解析失败会回落默认值，暂不改变行为。
    match T::from_str(value) {
        Ok(value) => value,
        Err(_) => default(),
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

fn default_public_media_addr() -> String {
    "127.0.0.1:8081".to_string()
}

fn default_management_addr() -> String {
    "0.0.0.0:8443".to_string()
}

fn default_management_tls_cert_path() -> String {
    "/var/lib/streamserver/agent/identity/management-server-cert.pem".to_string()
}

fn default_management_tls_key_path() -> String {
    "/var/lib/streamserver/agent/identity/management-server-key.pem".to_string()
}

fn default_management_tls_client_ca_path() -> String {
    "/var/lib/streamserver/agent/identity/management-client-ca.pem".to_string()
}

fn default_management_capability_jwt_public_key_path() -> String {
    "/var/lib/streamserver/agent/identity/capability-jwt-public-key.pem".to_string()
}

fn default_management_max_concurrency() -> usize {
    4
}

fn default_management_chunk_idle_timeout_sec() -> u64 {
    30
}

fn default_zlm_hook_addr() -> String {
    "127.0.0.1:18082".to_string()
}

fn default_zlm_hook_queue_capacity() -> usize {
    64
}

fn default_zlm_hook_timeout_sec() -> u64 {
    4
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

fn default_identity_dir() -> String {
    "/var/lib/streamserver/agent/identity".to_string()
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

fn default_zlm_rtmp_port() -> u16 {
    1935
}

fn default_zlm_rtsp_port() -> u16 {
    554
}

fn default_agent_stream_addr() -> String {
    "http://127.0.0.1:8081".to_string()
}

fn default_output_mount_relative_prefix() -> String {
    "output".to_string()
}

fn default_network_mode() -> String {
    "bridge".to_string()
}

fn default_runtime_manager_start_limit() -> usize {
    8
}

fn default_runtime_manager_stop_limit() -> usize {
    16
}

fn default_runtime_manager_recording_limit() -> usize {
    12
}

fn default_runtime_manager_adopt_limit() -> usize {
    1
}

fn default_work_root() -> String {
    "/data/media/work".to_string()
}

fn default_zlm_output_mp4_root() -> String {
    "/data/zlm/www/output/mp4".to_string()
}

fn default_zlm_output_hls_root() -> String {
    "/data/zlm/www/output/hls".to_string()
}

fn default_upload_max_bytes() -> u64 {
    10 * 1024 * 1024 * 1024
}

fn default_upload_allowed_extensions() -> Vec<String> {
    [
        "mp4", "mov", "m4v", "mkv", "webm", "ts", "m2ts", "mts", "flv",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn default_upload_probe_timeout_sec() -> u64 {
    30
}

fn default_acceleration_mode() -> String {
    "cpu".to_string()
}

fn default_runtime_log_tail_bytes() -> usize {
    8 * 1024
}

fn default_runtime_log_max_file_bytes() -> u64 {
    128 * 1024 * 1024
}

fn default_runtime_log_retention_days() -> u64 {
    7
}

fn default_allow_enhanced_rtmp_expose() -> bool {
    true
}

fn default_mp4_record_segment_sec() -> u32 {
    7_200
}

fn default_hls_record_segment_sec() -> u32 {
    60
}

fn default_artifact_cleanup_enabled() -> bool {
    true
}

fn default_artifact_cleanup_threshold_percent() -> f64 {
    85.0
}

fn default_artifact_cleanup_strategy() -> String {
    "delete_oldest_then_reject".to_string()
}

fn default_artifact_cleanup_check_interval_sec() -> u64 {
    30
}

#[cfg(test)]
mod tests {
    use std::{env::VarError, ffi::OsString};

    use super::*;

    fn production_settings() -> Settings {
        let mut settings = Settings {
            environment: "production".to_string(),
            logging: LoggingSettings::default(),
            agent: AgentSettings::default(),
        };
        settings.agent.zlm_hook_shared_secret = "0123456789abcdef0123456789abcdef".to_string();
        settings
    }

    #[test]
    fn production_requires_https_control_plane() {
        let settings = production_settings();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn production_requires_absolute_identity_directory() {
        let mut settings = production_settings();
        settings.agent.core_endpoint = "https://core.example.test:50051".to_string();
        settings.agent.identity_dir = "relative/identity".to_string();
        assert!(settings.validate().is_err());
        settings.agent.identity_dir = "/var/lib/streamserver/agent/identity".to_string();
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn environment_is_a_fail_closed_allowlist() {
        for invalid in ["prod", "Production", "production ", "staging"] {
            let mut settings = production_settings();
            settings.environment = invalid.to_string();
            assert!(
                settings.validate().is_err(),
                "unexpectedly accepted environment {invalid:?}"
            );
        }
    }

    #[test]
    fn enrolled_identity_does_not_require_legacy_tls_paths() {
        let mut settings = production_settings();
        settings.agent.core_endpoint = "https://core.example.test:50051".to_string();
        settings.agent.identity_dir = "/var/lib/streamserver/agent/identity".to_string();
        settings.agent.cert_path.clear();
        settings.agent.key_path.clear();
        settings.agent.ca_path.clear();
        settings.agent.management_tls_cert_path.clear();
        settings.agent.management_tls_key_path.clear();
        settings.agent.management_tls_client_ca_path.clear();
        settings
            .agent
            .management_capability_jwt_public_key_path
            .clear();
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn explicit_development_node_id_must_not_be_nil() {
        let mut settings = production_settings();
        settings.environment = "development".to_string();
        settings.agent.node_id = uuid::Uuid::nil().to_string();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn missing_environment_defaults_to_development_but_non_unicode_is_rejected() {
        assert_eq!(
            environment_from_var(Err(VarError::NotPresent)).unwrap(),
            AgentEnvironment::Development
        );

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            let invalid = OsString::from_vec(vec![0xff, 0xfe]);
            let error = environment_from_var(Err(VarError::NotUnicode(invalid))).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("STREAMSERVER_ENV must be valid UTF-8")
            );
        }
    }

    #[test]
    fn public_listener_defaults_to_loopback_and_management_has_safe_limits() {
        let settings = AgentSettings::default();

        assert_eq!(settings.public_media_addr, "127.0.0.1:8081");
        assert!(!settings.public_media_expose);
        assert_eq!(settings.management_addr, "0.0.0.0:8443");
        assert_eq!(settings.management_max_concurrency, 4);
        assert_eq!(settings.management_chunk_idle_timeout_sec, 30);
        assert_eq!(settings.zlm_hook_addr, "127.0.0.1:18082");
        assert_eq!(settings.zlm_hook_queue_capacity, 64);
        assert_eq!(settings.zlm_hook_timeout_sec, 4);
    }

    #[test]
    fn zlm_hook_listener_is_loopback_and_production_secret_is_strong() {
        let mut settings = production_settings();
        settings.agent.core_endpoint = "https://core.example.test:50051".to_string();

        settings.agent.zlm_hook_addr = "0.0.0.0:18082".to_string();
        assert!(settings.validate().is_err());
        settings.agent.zlm_hook_addr = "127.0.0.1:18082".to_string();

        settings.agent.zlm_hook_shared_secret.clear();
        assert!(settings.validate().is_err());
        settings.agent.zlm_hook_shared_secret = "short-secret".to_string();
        assert!(settings.validate().is_err());
        settings.agent.zlm_hook_shared_secret = "0123456789abcdef0123456789abcdef".to_string();
        assert!(settings.validate().is_ok());

        settings.agent.zlm_hook_queue_capacity = 0;
        assert!(settings.validate().is_err());
        settings.agent.zlm_hook_queue_capacity = 64;
        settings.agent.zlm_hook_timeout_sec = 0;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn zlm_hook_relay_timeout_stays_below_zlm_timeout_in_every_environment() {
        let mut production = production_settings();
        production.agent.core_endpoint = "https://core.example.test:50051".to_string();
        production.agent.zlm_hook_timeout_sec = 5;
        let production_error = production.validate().unwrap_err();
        assert!(
            production_error
                .to_string()
                .contains("AGENT_ZLM_HOOK_TIMEOUT_SEC must be between 1 and 4")
        );

        let mut development = production;
        development.environment = "development".to_string();
        development.agent.zlm_hook_shared_secret = "development-hook-secret".to_string();
        let development_error = development.validate().unwrap_err();
        assert!(
            development_error
                .to_string()
                .contains("AGENT_ZLM_HOOK_TIMEOUT_SEC must be between 1 and 4")
        );
    }

    #[test]
    fn legacy_agent_http_addr_override_is_rejected_instead_of_opening_a_bypass_listener() {
        assert!(reject_legacy_agent_http_addr(None).is_ok());
        let error = reject_legacy_agent_http_addr(Some("0.0.0.0:8081".to_string())).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("AGENT_HTTP_ADDR is no longer supported")
        );
    }

    #[test]
    fn legacy_agent_http_addr_file_key_is_rejected_instead_of_silently_ignored() {
        let config = config::Config::builder()
            .set_override("agent.http_addr", "0.0.0.0:8081")
            .unwrap()
            .build()
            .unwrap();

        assert!(reject_legacy_listener_config(&config).is_err());
    }

    #[test]
    fn production_non_loopback_public_listener_requires_explicit_exposure_and_tls() {
        let mut settings = production_settings();
        settings.agent.core_endpoint = "https://core.example.test:50051".to_string();
        settings.agent.public_media_addr = "0.0.0.0:8081".to_string();
        settings.agent.public_media_expose = false;
        settings.agent.public_media_tls_cert_path.clear();
        settings.agent.public_media_tls_key_path.clear();
        assert!(settings.validate().is_err());

        settings.agent.public_media_expose = true;
        assert!(settings.validate().is_err());

        settings.agent.public_media_tls_cert_path = "/etc/streamserver/public.pem".to_string();
        settings.agent.public_media_tls_key_path = "/etc/streamserver/public.key".to_string();
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn management_listener_always_requires_complete_mtls_material() {
        let mut settings = production_settings();
        settings.environment = "development".to_string();
        settings.agent.management_tls_cert_path.clear();
        settings.agent.management_tls_key_path.clear();
        settings.agent.management_tls_client_ca_path.clear();
        settings
            .agent
            .management_capability_jwt_public_key_path
            .clear();
        assert!(settings.validate().is_err());

        settings.agent.management_tls_cert_path =
            "/var/lib/streamserver/agent/identity/management-cert.pem".to_string();
        settings.agent.management_tls_key_path =
            "/var/lib/streamserver/agent/identity/management-key.pem".to_string();
        settings.agent.management_tls_client_ca_path =
            "/var/lib/streamserver/agent/identity/ca-cert.pem".to_string();
        settings.agent.management_capability_jwt_public_key_path =
            "/var/lib/streamserver/agent/identity/capability-jwt-public.pem".to_string();
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn management_port_and_limits_are_fail_closed() {
        let mut settings = production_settings();
        settings.agent.core_endpoint = "https://core.example.test:50051".to_string();

        settings.agent.management_addr = "0.0.0.0:0".to_string();
        assert!(settings.validate().is_err());
        settings.agent.management_addr = "0.0.0.0:8443".to_string();

        settings.agent.management_max_concurrency = 0;
        assert!(settings.validate().is_err());
        settings.agent.management_max_concurrency = 4;

        settings.agent.management_chunk_idle_timeout_sec = 0;
        assert!(settings.validate().is_err());
    }
}
