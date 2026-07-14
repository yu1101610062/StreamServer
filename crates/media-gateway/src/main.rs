use std::{net::SocketAddr, path::PathBuf};

use media_gateway::{GatewayConfig, GatewayState, build_app};
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

    let state = GatewayState::new(GatewayConfig {
        public_base_url,
        work_root,
        ffmpeg_bin,
    });
    let app = build_app(state);
    let addr: SocketAddr = bind_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(listen_addr = %listener.local_addr()?, "media-gateway ready");
    axum::serve(listener, app).await?;
    Ok(())
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

fn nonempty_trimmed(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::resolve_ffmpeg_bin;
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
    }
}
