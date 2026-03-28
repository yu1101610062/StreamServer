use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::LoggingSettings;

pub fn init(settings: &LoggingSettings) {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(settings.level.clone()));

    if settings.json {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().json())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().compact())
            .init();
    }
}
