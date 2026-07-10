use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::get,
};
use media_gateway::{GatewayConfig, GatewayState, build_app, safe_target_path};
use serde_json::{Value, json};
use tower::util::ServiceExt;

#[tokio::test]
async fn relay_requires_registered_task_and_token() -> anyhow::Result<()> {
    async fn upstream() -> &'static str {
        "relay-bytes"
    }

    let upstream = spawn_server(Router::new().route("/live.flv", get(upstream))).await?;
    let temp = test_temp_dir()?;
    let state = GatewayState::new(GatewayConfig {
        public_base_url: "http://media:18080".to_string(),
        work_root: temp.clone(),
    });
    let app = build_app(state);
    let task_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000111")?;

    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/relays")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "task_id": task_id,
                        "source_url": format!("{upstream}/live.flv")
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(create.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&to_bytes(create.into_body(), usize::MAX).await?)?;
    let relay_url = body["relay_url"].as_str().expect("relay_url");
    assert!(relay_url.starts_with("http://media:18080/relay/"));
    let token = relay_url.split("token=").nth(1).expect("token");

    let rejected = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/relay/{task_id}?url={upstream}/live.flv"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

    let accepted = app
        .oneshot(
            Request::builder()
                .uri(format!("/relay/{task_id}?token={token}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(accepted.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(accepted.into_body(), usize::MAX).await?.as_ref(),
        b"relay-bytes"
    );
    Ok(())
}

#[test]
fn prefetch_target_path_stays_under_work_root_and_not_uploads() -> anyhow::Result<()> {
    let temp = test_temp_dir()?;

    assert_eq!(
        safe_target_path(&temp, "imports/task/source.mp4")?,
        temp.join("imports/task/source.mp4")
    );
    assert!(safe_target_path(&temp, "../source.mp4").is_err());
    assert!(safe_target_path(&temp, "/tmp/source.mp4").is_err());
    assert!(safe_target_path(&temp, "uploads/node/source.mp4").is_err());
    Ok(())
}

#[tokio::test]
async fn prefetch_downloads_http_source_to_shared_storage_path() -> anyhow::Result<()> {
    async fn source() -> &'static str {
        "prefetch-bytes"
    }

    let upstream = spawn_server(Router::new().route("/archive.mp4", get(source))).await?;
    let temp = test_temp_dir()?;
    let state = GatewayState::new(GatewayConfig {
        public_base_url: "http://media:18080".to_string(),
        work_root: temp.clone(),
    });
    let app = build_app(state);
    let task_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000222")?;

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/prefetch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "task_id": task_id,
                        "source_url": format!("{upstream}/archive.mp4"),
                        "target_path": "imports/00000000-0000-0000-0000-000000000222/source.mp4"
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(created.status(), StatusCode::ACCEPTED);

    let status = wait_prefetch_ready(app, task_id).await?;
    assert_eq!(status["status"], "ready");
    assert_eq!(
        status["source_url"],
        "imports/00000000-0000-0000-0000-000000000222/source.mp4"
    );
    assert_eq!(
        std::fs::read(temp.join("imports/00000000-0000-0000-0000-000000000222/source.mp4"))?,
        b"prefetch-bytes"
    );
    Ok(())
}

fn test_temp_dir() -> anyhow::Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("streamserver-gateway-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

async fn spawn_server(app: Router) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("stub server should run");
    });
    Ok(format!("http://{addr}"))
}

async fn wait_prefetch_ready(app: Router, task_id: uuid::Uuid) -> anyhow::Result<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/prefetch/{task_id}"))
                    .body(Body::empty())?,
            )
            .await?;
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
        if body["status"] == "ready" {
            return Ok(body);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for prefetch: {body}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
