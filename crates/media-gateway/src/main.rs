use std::{net::SocketAddr, path::PathBuf};

use media_gateway::{GatewayConfig, GatewayState, build_app};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let bind_addr = env_or("MEDIA_GATEWAY_BIND_ADDR", "127.0.0.1:18081");
    let public_base_url = env_or("MEDIA_GATEWAY_PUBLIC_BASE_URL", "http://127.0.0.1:18081");
    let work_root = PathBuf::from(env_or("MEDIA_GATEWAY_WORK_ROOT", "/data/media/work"));

    let state = GatewayState::new(GatewayConfig {
        public_base_url,
        work_root,
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
