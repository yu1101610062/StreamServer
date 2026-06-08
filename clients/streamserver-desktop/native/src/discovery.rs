use crate::models::NativeError;
use if_addrs::{IfAddr, get_if_addrs};
use reqwest::{Client, Url};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    net::Ipv4Addr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::Semaphore, task::JoinSet};

const DEFAULT_PORTS: &[u16] = &[8080, 80];
const DEFAULT_TIMEOUT_MS: u64 = 220;
const DEFAULT_CONCURRENCY: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Candidate {
    interface_ip: Ipv4Addr,
    host: Ipv4Addr,
    port: u16,
}

pub async fn scan(body: Option<&Value>) -> Result<Value, NativeError> {
    let timeout_ms = body
        .and_then(|value| value.get("timeout_ms"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let ports = ports_from_body(body);
    let candidates = scan_candidates(body, &ports)?;
    let direct_base_urls = direct_base_urls_from_body(body, &ports);
    let client = Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .connect_timeout(Duration::from_millis(timeout_ms))
        .user_agent("StreamServerDesktopDiscovery/0.1")
        .build()?;
    let semaphore = Arc::new(Semaphore::new(DEFAULT_CONCURRENCY));
    let mut tasks = JoinSet::new();
    for candidate in candidates.iter().cloned() {
        let client = client.clone();
        let semaphore = Arc::clone(&semaphore);
        tasks.spawn(async move {
            let _permit = semaphore.acquire_owned().await.ok()?;
            probe_candidate(&client, candidate, timeout_ms).await
        });
    }
    for base_url in direct_base_urls.iter().cloned() {
        let client = client.clone();
        let semaphore = Arc::clone(&semaphore);
        tasks.spawn(async move {
            let _permit = semaphore.acquire_owned().await.ok()?;
            probe_direct_base_url(&client, base_url).await
        });
    }

    let mut found = Vec::new();
    while let Some(result) = tasks.join_next().await {
        if let Ok(Some(value)) = result {
            found.push(value);
        }
    }
    found.sort_by(|left, right| base_url_of(left).cmp(base_url_of(right)));
    found.dedup_by(|left, right| base_url_of(left) == base_url_of(right));
    Ok(json!({
        "ports": ports,
        "timeout_ms": timeout_ms,
        "candidates": candidates.len(),
        "direct_candidates": direct_base_urls.len(),
        "items": found,
    }))
}

pub async fn probe(body: Option<&Value>) -> Result<Value, NativeError> {
    let body = body
        .and_then(Value::as_object)
        .ok_or_else(|| NativeError::InvalidRequest("body is required".to_string()))?;
    let timeout_ms = body
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(1500);
    let base_url = if let Some(base_url) = body.get("base_url").and_then(Value::as_str) {
        normalize_base_url(base_url)?
    } else {
        let protocol = body
            .get("protocol")
            .and_then(Value::as_str)
            .unwrap_or("http");
        if !matches!(protocol, "http" | "https") {
            return Err(NativeError::InvalidRequest(
                "protocol must be http or https".to_string(),
            ));
        }
        let host = body
            .get("host")
            .and_then(Value::as_str)
            .ok_or_else(|| NativeError::InvalidRequest("host is required".to_string()))?;
        let port = body.get("port").and_then(Value::as_u64).unwrap_or(8080);
        if port == 0 || port > u16::MAX as u64 {
            return Err(NativeError::InvalidRequest("port is invalid".to_string()));
        }
        format!("{protocol}://{}:{port}", host.trim())
    };
    let client = Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .connect_timeout(Duration::from_millis(timeout_ms))
        .user_agent("StreamServerDesktopDiscovery/0.1")
        .build()?;
    let started = Instant::now();
    match probe_base_url(&client, &base_url).await {
        Ok(Some(mut value)) => {
            insert_latency(&mut value, started.elapsed());
            Ok(json!({ "found": true, "item": value }))
        }
        Ok(None) => Ok(json!({ "found": false, "base_url": base_url })),
        Err(error) => Ok(json!({
            "found": false,
            "base_url": base_url,
            "error": error.to_string(),
        })),
    }
}

async fn probe_candidate(client: &Client, candidate: Candidate, _timeout_ms: u64) -> Option<Value> {
    let base_url = format!("http://{}:{}", candidate.host, candidate.port);
    let started = Instant::now();
    let mut value = probe_base_url(client, &base_url).await.ok()??;
    insert_latency(&mut value, started.elapsed());
    if let Some(map) = value.as_object_mut() {
        map.insert(
            "interface_ip".to_string(),
            Value::String(candidate.interface_ip.to_string()),
        );
    }
    Some(value)
}

async fn probe_direct_base_url(client: &Client, base_url: String) -> Option<Value> {
    let started = Instant::now();
    let mut value = probe_base_url(client, &base_url).await.ok()??;
    insert_latency(&mut value, started.elapsed());
    if let Some(map) = value.as_object_mut() {
        map.insert("source".to_string(), Value::String("direct".to_string()));
    }
    Some(value)
}

async fn probe_base_url(client: &Client, base_url: &str) -> Result<Option<Value>, reqwest::Error> {
    let health_url = format!("{base_url}/health/live");
    let health_response = client.get(health_url).send().await?;
    let health_status = health_response.status().as_u16();
    if !health_response.status().is_success() {
        return Ok(None);
    }
    let health = health_response.json::<Value>().await?;
    if !health.is_object() {
        return Ok(None);
    }

    let me_url = format!("{base_url}/api/v1/me");
    let me_response = client.get(me_url).send().await?;
    let me_status = me_response.status().as_u16();
    let me_payload = me_response.json::<Value>().await.unwrap_or(Value::Null);
    if !is_streamserver_me_response(me_status, &me_payload) {
        return Ok(None);
    }

    Ok(Some(json!({
        "base_url": base_url,
        "health_status": health_status,
        "me_status": me_status,
        "environment": health.get("environment").cloned().unwrap_or(Value::Null),
        "started_at": health.get("started_at").cloned().unwrap_or(Value::Null),
        "auth_required": me_status == 403,
        "health": health,
        "me": me_payload,
    })))
}

fn insert_latency(value: &mut Value, elapsed: Duration) {
    if let Some(map) = value.as_object_mut() {
        map.insert(
            "latency_ms".to_string(),
            Value::Number(serde_json::Number::from(elapsed.as_millis() as u64)),
        );
    }
}

fn ports_from_body(body: Option<&Value>) -> Vec<u16> {
    let Some(values) = body
        .and_then(|value| value.get("ports"))
        .and_then(Value::as_array)
    else {
        return DEFAULT_PORTS.to_vec();
    };
    let mut ports = values
        .iter()
        .filter_map(Value::as_u64)
        .filter(|port| *port > 0 && *port <= u16::MAX as u64)
        .map(|port| port as u16)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if ports.is_empty() {
        ports = DEFAULT_PORTS.to_vec();
    }
    ports
}

fn scan_candidates(body: Option<&Value>, ports: &[u16]) -> Result<Vec<Candidate>, NativeError> {
    let mut candidates = BTreeSet::new();
    for iface in get_if_addrs().map_err(|error| NativeError::Network(error.to_string()))? {
        let IfAddr::V4(ipv4) = iface.addr else {
            continue;
        };
        let ip = ipv4.ip;
        if !is_private_ipv4(ip) || ip.is_loopback() {
            continue;
        }
        for host in subnet_24_hosts(ip) {
            if host == ip {
                continue;
            }
            for port in ports {
                candidates.insert(Candidate {
                    interface_ip: ip,
                    host,
                    port: *port,
                });
            }
        }
    }
    for seed in seed_hosts_from_body(body) {
        for host in subnet_24_hosts(seed) {
            for port in ports {
                candidates.insert(Candidate {
                    interface_ip: seed,
                    host,
                    port: *port,
                });
            }
        }
    }
    Ok(candidates.into_iter().collect())
}

fn direct_base_urls_from_body(body: Option<&Value>, ports: &[u16]) -> Vec<String> {
    let mut urls = BTreeSet::new();
    if let Some(values) = body
        .and_then(|value| value.get("base_urls"))
        .and_then(Value::as_array)
    {
        for value in values.iter().filter_map(Value::as_str) {
            if let Ok(url) = normalize_base_url(value) {
                urls.insert(url);
            }
        }
    }
    if let Some(values) = body
        .and_then(|value| value.get("seed_hosts"))
        .and_then(Value::as_array)
    {
        for host in values.iter().filter_map(Value::as_str).map(str::trim) {
            if host.is_empty() || host.contains("://") {
                continue;
            }
            for port in ports {
                urls.insert(format!("http://{host}:{port}"));
            }
        }
    }
    urls.into_iter().collect()
}

fn seed_hosts_from_body(body: Option<&Value>) -> Vec<Ipv4Addr> {
    let mut hosts = BTreeSet::new();
    if let Some(values) = body
        .and_then(|value| value.get("seed_hosts"))
        .and_then(Value::as_array)
    {
        for value in values.iter().filter_map(Value::as_str) {
            if let Ok(ip) = value.trim().parse::<Ipv4Addr>() {
                if is_private_ipv4(ip) && !ip.is_loopback() {
                    hosts.insert(ip);
                }
            }
        }
    }
    if let Some(values) = body
        .and_then(|value| value.get("base_urls"))
        .and_then(Value::as_array)
    {
        for value in values.iter().filter_map(Value::as_str) {
            if let Some(ip) = ipv4_from_base_url(value) {
                if is_private_ipv4(ip) && !ip.is_loopback() {
                    hosts.insert(ip);
                }
            }
        }
    }
    hosts.into_iter().collect()
}

fn ipv4_from_base_url(value: &str) -> Option<Ipv4Addr> {
    let url = Url::parse(value.trim()).ok()?;
    url.host_str()?.parse::<Ipv4Addr>().ok()
}

fn subnet_24_hosts(ip: Ipv4Addr) -> Vec<Ipv4Addr> {
    let [a, b, c, _] = ip.octets();
    (1..=254).map(|last| Ipv4Addr::new(a, b, c, last)).collect()
}

fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168)
}

fn is_streamserver_me_response(status: u16, payload: &Value) -> bool {
    if status == 200 && payload.is_object() {
        return true;
    }
    if status != 403 {
        return false;
    }
    matches!(
        payload.get("code").and_then(Value::as_str),
        Some("ACCESS_FORBIDDEN")
    ) && payload
        .get("message")
        .and_then(Value::as_str)
        .map(|message| message.contains("Authorization"))
        .unwrap_or(false)
}

fn normalize_base_url(value: &str) -> Result<String, NativeError> {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Ok(trimmed.to_string())
    } else {
        Err(NativeError::InvalidRequest(
            "base_url must start with http:// or https://".to_string(),
        ))
    }
}

fn base_url_of(value: &Value) -> &str {
    value
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[allow(dead_code)]
fn _assert_map_send_sync(_: Map<String, Value>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_private_ipv4_ranges() {
        assert!(is_private_ipv4(Ipv4Addr::new(10, 1, 2, 3)));
        assert!(is_private_ipv4(Ipv4Addr::new(172, 17, 1, 2)));
        assert!(is_private_ipv4(Ipv4Addr::new(192, 168, 1, 2)));
        assert!(!is_private_ipv4(Ipv4Addr::new(198, 18, 0, 1)));
    }

    #[test]
    fn builds_24_hosts() {
        let hosts = subnet_24_hosts(Ipv4Addr::new(172, 17, 18, 228));
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts.first().copied(), Some(Ipv4Addr::new(172, 17, 18, 1)));
        assert_eq!(hosts.last().copied(), Some(Ipv4Addr::new(172, 17, 18, 254)));
    }

    #[test]
    fn seed_hosts_add_private_subnet_candidates() {
        let body = json!({"seed_hosts": ["172.17.13.196"]});
        let candidates = scan_candidates(Some(&body), &[8080]).unwrap();
        assert!(candidates.iter().any(|candidate| candidate.host
            == Ipv4Addr::new(172, 17, 13, 196)
            && candidate.port == 8080));
    }

    #[test]
    fn direct_urls_include_base_urls_and_seed_hosts() {
        let body = json!({
            "base_urls": ["http://172.17.13.196:8080/"],
            "seed_hosts": ["172.17.13.196"]
        });
        let urls = direct_base_urls_from_body(Some(&body), &[8080]);
        assert!(urls.contains(&"http://172.17.13.196:8080".to_string()));
    }

    #[test]
    fn recognizes_streamserver_auth_response() {
        let payload = json!({
            "code": "ACCESS_FORBIDDEN",
            "message": "missing Authorization header"
        });
        assert!(is_streamserver_me_response(403, &payload));
        assert!(!is_streamserver_me_response(404, &payload));
    }
}
