use std::{net::SocketAddr, path::PathBuf, str::FromStr, time::Duration};

use media_gateway::{GatewayConfig, GatewayRuntimeConfig, GatewayState, build_app};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let bind_addr = env_or("MEDIA_GATEWAY_BIND_ADDR", "127.0.0.1:18081");
    let public_base_url = env_or("MEDIA_GATEWAY_PUBLIC_BASE_URL", "http://127.0.0.1:18081");
    let work_root = PathBuf::from(env_or("MEDIA_GATEWAY_WORK_ROOT", "/data/media/work"));
    let gateway_ffmpeg_bin = std::env::var("MEDIA_GATEWAY_FFMPEG_BIN").ok();
    let agent_ffmpeg_bin = std::env::var("FFMPEG_BIN").ok();
    let ffmpeg_bin = resolve_ffmpeg_bin(gateway_ffmpeg_bin.as_deref(), agent_ffmpeg_bin.as_deref());
    let mut runtime = runtime_config_from_env()?;
    runtime.ffprobe_bin = Some(resolve_ffprobe_bin(
        std::env::var("MEDIA_GATEWAY_FFPROBE_BIN").ok().as_deref(),
        &ffmpeg_bin,
    ));

    let state = GatewayState::with_runtime_config(
        GatewayConfig {
            public_base_url,
            work_root,
            ffmpeg_bin,
        },
        runtime.clone(),
    );
    let app = build_app(state);
    let addr: SocketAddr = bind_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(
        listen_addr = %listener.local_addr()?,
        max_queued_prefetches = runtime.max_queued_prefetches,
        max_active_downloads = runtime.max_active_downloads,
        max_active_ffmpeg = runtime.max_active_ffmpeg,
        max_active_relays = runtime.max_active_relays,
        "media-gateway ready"
    );
    axum::serve(listener, app).await?;
    Ok(())
}

fn runtime_config_from_env() -> anyhow::Result<GatewayRuntimeConfig> {
    let defaults = GatewayRuntimeConfig::default();
    let config = GatewayRuntimeConfig {
        max_queued_prefetches: env_parse(
            "MEDIA_GATEWAY_MAX_QUEUED_PREFETCHES",
            defaults.max_queued_prefetches,
        )?,
        max_active_downloads: env_parse(
            "MEDIA_GATEWAY_MAX_ACTIVE_DOWNLOADS",
            defaults.max_active_downloads,
        )?,
        max_active_ffmpeg: env_parse(
            "MEDIA_GATEWAY_MAX_ACTIVE_FFMPEG",
            defaults.max_active_ffmpeg,
        )?,
        prefetch_queue_timeout: Duration::from_millis(env_parse(
            "MEDIA_GATEWAY_PREFETCH_QUEUE_TIMEOUT_MS",
            defaults.prefetch_queue_timeout.as_millis() as u64,
        )?),
        prefetch_execution_timeout: Duration::from_millis(env_parse(
            "MEDIA_GATEWAY_PREFETCH_EXECUTION_TIMEOUT_MS",
            defaults.prefetch_execution_timeout.as_millis() as u64,
        )?),
        source_connect_timeout: Duration::from_millis(env_parse(
            "MEDIA_GATEWAY_SOURCE_CONNECT_TIMEOUT_MS",
            defaults.source_connect_timeout.as_millis() as u64,
        )?),
        source_read_idle_timeout: Duration::from_millis(env_parse(
            "MEDIA_GATEWAY_SOURCE_READ_IDLE_TIMEOUT_MS",
            defaults.source_read_idle_timeout.as_millis() as u64,
        )?),
        max_prefetch_records: env_parse(
            "MEDIA_GATEWAY_MAX_PREFETCH_RECORDS",
            defaults.max_prefetch_records,
        )?,
        prefetch_terminal_retention: Duration::from_secs(env_parse(
            "MEDIA_GATEWAY_PREFETCH_TERMINAL_RETENTION_SEC",
            defaults.prefetch_terminal_retention.as_secs(),
        )?),
        relay_cancel_wait: Duration::from_millis(env_parse(
            "MEDIA_GATEWAY_RELAY_CANCEL_WAIT_MS",
            defaults.relay_cancel_wait.as_millis() as u64,
        )?),
        prefetch_cancel_wait: Duration::from_millis(env_parse(
            "MEDIA_GATEWAY_PREFETCH_CANCEL_WAIT_MS",
            defaults.prefetch_cancel_wait.as_millis() as u64,
        )?),
        cancel_tombstone_ttl: Duration::from_secs(env_parse(
            "MEDIA_GATEWAY_CANCEL_TOMBSTONE_TTL_SEC",
            defaults.cancel_tombstone_ttl.as_secs(),
        )?),
        max_active_relays: env_parse(
            "MEDIA_GATEWAY_MAX_ACTIVE_RELAYS",
            defaults.max_active_relays,
        )?,
        max_relay_registrations: env_parse(
            "MEDIA_GATEWAY_MAX_RELAY_REGISTRATIONS",
            defaults.max_relay_registrations,
        )?,
        relay_reconnect_grace: Duration::from_secs(env_parse(
            "MEDIA_GATEWAY_RELAY_RECONNECT_GRACE_SEC",
            defaults.relay_reconnect_grace.as_secs(),
        )?),
        relay_unopened_ttl: Duration::from_secs(env_parse(
            "MEDIA_GATEWAY_RELAY_UNOPENED_TTL_SEC",
            defaults.relay_unopened_ttl.as_secs(),
        )?),
        ffprobe_bin: defaults.ffprobe_bin,
    };
    anyhow::ensure!(
        config.max_queued_prefetches > 0,
        "MEDIA_GATEWAY_MAX_QUEUED_PREFETCHES must be greater than 0"
    );
    anyhow::ensure!(
        config.max_active_downloads > 0,
        "MEDIA_GATEWAY_MAX_ACTIVE_DOWNLOADS must be greater than 0"
    );
    anyhow::ensure!(
        config.max_active_ffmpeg > 0,
        "MEDIA_GATEWAY_MAX_ACTIVE_FFMPEG must be greater than 0"
    );
    anyhow::ensure!(
        config.max_prefetch_records
            >= config.max_queued_prefetches
                + config.max_active_downloads
                + config.max_active_ffmpeg,
        "MEDIA_GATEWAY_MAX_PREFETCH_RECORDS must cover queued and active prefetches"
    );
    anyhow::ensure!(
        !config.source_connect_timeout.is_zero(),
        "MEDIA_GATEWAY_SOURCE_CONNECT_TIMEOUT_MS must be greater than 0"
    );
    anyhow::ensure!(
        !config.source_read_idle_timeout.is_zero(),
        "MEDIA_GATEWAY_SOURCE_READ_IDLE_TIMEOUT_MS must be greater than 0"
    );
    anyhow::ensure!(
        config.max_active_relays > 0,
        "MEDIA_GATEWAY_MAX_ACTIVE_RELAYS must be greater than 0"
    );
    anyhow::ensure!(
        config.max_relay_registrations >= config.max_active_relays,
        "MEDIA_GATEWAY_MAX_RELAY_REGISTRATIONS must cover active relays"
    );
    Ok(config)
}

fn env_parse<T>(name: &str, default: T) -> anyhow::Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value
            .trim()
            .parse()
            .map_err(|error| anyhow::anyhow!("{name} must be an integer: {error}")),
        _ => Ok(default),
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn resolve_ffmpeg_bin(gateway_value: Option<&str>, agent_value: Option<&str>) -> PathBuf {
    gateway_value
        .and_then(nonempty_trimmed)
        .or_else(|| agent_value.and_then(nonempty_trimmed))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

fn resolve_ffprobe_bin(gateway_value: Option<&str>, ffmpeg_bin: &std::path::Path) -> PathBuf {
    gateway_value
        .and_then(nonempty_trimmed)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            ffmpeg_bin
                .parent()
                .unwrap_or_else(|| std::path::Path::new(""))
                .join("ffprobe")
        })
}

fn nonempty_trimmed(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::{resolve_ffmpeg_bin, resolve_ffprobe_bin};
    use std::path::PathBuf;

    #[test]
    fn gateway_ffmpeg_path_prefers_gateway_then_agent_then_path_default() {
        assert_eq!(
            resolve_ffmpeg_bin(Some(" /gateway/ffmpeg "), Some("/agent/ffmpeg")),
            PathBuf::from("/gateway/ffmpeg")
        );
        assert_eq!(
            resolve_ffmpeg_bin(Some(" "), Some(" /agent/ffmpeg ")),
            PathBuf::from("/agent/ffmpeg")
        );
        assert_eq!(resolve_ffmpeg_bin(None, None), PathBuf::from("ffmpeg"));
        assert_eq!(
            resolve_ffprobe_bin(None, std::path::Path::new("/runtime/bin/ffmpeg")),
            PathBuf::from("/runtime/bin/ffprobe")
        );
        assert_eq!(
            resolve_ffprobe_bin(Some(" /custom/ffprobe "), std::path::Path::new("ffmpeg")),
            PathBuf::from("/custom/ffprobe")
        );
    }
}
