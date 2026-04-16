#[cfg(test)]
#[path = "tests/capability.rs"]
mod tests;

use std::{process::Command, time::Duration};

use chrono::Utc;
use media_domain::{CapabilitySnapshot, GpuDeviceInfo, GpuRuntimeStats};
use reqwest::{Client, Url};
use serde_json::Value;

use crate::config::AgentSettings;

#[derive(Debug, Clone)]
pub struct CapabilityProbe {
    client: Client,
}

#[derive(Debug, Default, Clone)]
struct ZlmProbeResult {
    version: Option<String>,
    api_list: Vec<String>,
    server_id: Option<String>,
    rtmp_enhanced_enabled: Option<bool>,
}

impl CapabilityProbe {
    pub fn new() -> anyhow::Result<Self> {
        let client = Client::builder().timeout(Duration::from_secs(3)).build()?;
        Ok(Self { client })
    }

    pub async fn snapshot(&self, settings: &AgentSettings) -> CapabilitySnapshot {
        let ffmpeg_protocols = probe_ffmpeg_entries(
            &settings.ffmpeg_bin,
            &["-protocols"],
            parse_ffmpeg_protocols,
        );
        let ffmpeg_formats =
            probe_ffmpeg_entries(&settings.ffmpeg_bin, &["-formats"], parse_ffmpeg_formats);
        let ffmpeg_encoders =
            probe_ffmpeg_entries(&settings.ffmpeg_bin, &["-encoders"], parse_ffmpeg_codecs);
        let ffmpeg_decoders =
            probe_ffmpeg_entries(&settings.ffmpeg_bin, &["-decoders"], parse_ffmpeg_codecs);
        let zlm = self.probe_zlm(settings).await.unwrap_or_default();
        let gpu_devices = probe_gpu_devices(settings);

        CapabilitySnapshot {
            ffmpeg_protocols,
            ffmpeg_formats,
            ffmpeg_encoders,
            ffmpeg_decoders,
            zlm_version: zlm.version,
            zlm_api_list: zlm.api_list,
            gpu: summarize_gpu_devices(&gpu_devices),
            gpu_devices,
            captured_at: Utc::now(),
        }
    }

    pub async fn zlm_alive(&self, settings: &AgentSettings) -> bool {
        self.probe_zlm(settings)
            .await
            .map(|result| result.version.is_some() || !result.api_list.is_empty())
            .unwrap_or(false)
    }

    pub async fn zlm_server_id(&self, settings: &AgentSettings) -> Option<String> {
        self.probe_zlm(settings)
            .await
            .ok()
            .and_then(|result| result.server_id)
            .filter(|value| !value.trim().is_empty())
    }

    pub async fn zlm_rtmp_enhanced_enabled(&self, settings: &AgentSettings) -> Option<bool> {
        self.probe_zlm(settings)
            .await
            .ok()
            .and_then(|result| result.rtmp_enhanced_enabled)
    }

    async fn probe_zlm(&self, settings: &AgentSettings) -> anyhow::Result<ZlmProbeResult> {
        let base = settings.zlm_api_base.trim();
        if base.is_empty() {
            return Ok(ZlmProbeResult::default());
        }

        let version = self
            .fetch_zlm_json(base, "/index/api/version", &settings.zlm_api_secret)
            .await
            .ok()
            .and_then(extract_zlm_version);
        let api_list = self
            .fetch_zlm_json(base, "/index/api/getApiList", &settings.zlm_api_secret)
            .await
            .ok()
            .map(extract_zlm_api_list)
            .unwrap_or_default();
        let server_config = self
            .fetch_zlm_json(base, "/index/api/getServerConfig", &settings.zlm_api_secret)
            .await
            .ok();
        let server_id = server_config.as_ref().and_then(extract_zlm_server_id);
        let rtmp_enhanced_enabled = server_config
            .as_ref()
            .and_then(extract_zlm_rtmp_enhanced_enabled);

        Ok(ZlmProbeResult {
            version,
            api_list,
            server_id,
            rtmp_enhanced_enabled,
        })
    }

    async fn fetch_zlm_json(&self, base: &str, path: &str, secret: &str) -> anyhow::Result<Value> {
        let mut url = Url::parse(base)?.join(path.trim_start_matches('/'))?;
        if !secret.trim().is_empty() {
            url.query_pairs_mut().append_pair("secret", secret.trim());
        }

        let response = self.client.get(url).send().await?.error_for_status()?;
        Ok(response.json().await?)
    }
}

pub fn binary_available(binary: &str) -> bool {
    Command::new(binary)
        .arg("-version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub fn gpu_acceleration_enabled(settings: &AgentSettings) -> bool {
    settings.acceleration_mode.trim() == "gpu"
}

pub fn probe_gpu_devices(settings: &AgentSettings) -> Vec<GpuDeviceInfo> {
    if !gpu_acceleration_enabled(settings) {
        return Vec::new();
    }

    probe_nvidia_smi_csv(&[
        "--query-gpu=index,uuid,name,memory.total",
        "--format=csv,noheader,nounits",
    ])
    .into_iter()
    .filter_map(|row| {
        let index = row.first()?.trim().parse::<u32>().ok()?;
        let uuid = row.get(1)?.trim().to_string();
        let name = row.get(2)?.trim().to_string();
        let memory_total_mb = row.get(3)?.trim().parse::<u64>().ok()?;
        if uuid.is_empty() || name.is_empty() {
            return None;
        }
        Some(GpuDeviceInfo {
            index,
            uuid,
            name,
            memory_total_mb,
        })
    })
    .collect()
}

pub fn probe_gpu_runtime(settings: &AgentSettings) -> Vec<GpuRuntimeStats> {
    if !gpu_acceleration_enabled(settings) {
        return Vec::new();
    }

    probe_nvidia_smi_csv(&[
        "--query-gpu=index,utilization.gpu,memory.used,memory.total,utilization.encoder,utilization.decoder",
        "--format=csv,noheader,nounits",
    ])
    .into_iter()
    .filter_map(|row| {
        let index = row.first()?.trim().parse::<u32>().ok()?;
        let gpu_util_percent = parse_metric_percent(row.get(1)?)?;
        let memory_used_mb = row.get(2)?.trim().parse::<u64>().ok()?;
        let memory_total_mb = row.get(3)?.trim().parse::<u64>().ok()?;
        let encoder_util_percent = parse_metric_percent(row.get(4)?)?;
        let decoder_util_percent = parse_metric_percent(row.get(5)?)?;
        Some(GpuRuntimeStats {
            index,
            gpu_util_percent,
            memory_used_mb,
            memory_total_mb,
            encoder_util_percent,
            decoder_util_percent,
        })
    })
    .collect()
}

pub fn summarize_gpu_devices(devices: &[GpuDeviceInfo]) -> Vec<String> {
    devices
        .iter()
        .map(|device| {
            format!(
                "{}#{} ({} MB)",
                device.name, device.index, device.memory_total_mb
            )
        })
        .collect()
}

fn probe_ffmpeg_entries(
    binary: &str,
    args: &[&str],
    parser: fn(&str) -> Vec<String>,
) -> Vec<String> {
    let output = Command::new(binary).args(args).output();
    match output {
        Ok(output) if output.status.success() => parser(&combine_output(&output)),
        Ok(output) => parser(&combine_output(&output)),
        Err(_) => Vec::new(),
    }
}

fn combine_output(output: &std::process::Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    text
}

fn parse_ffmpeg_protocols(text: &str) -> Vec<String> {
    normalize(
        text.lines()
            .map(str::trim)
            .filter(|line| {
                !line.is_empty()
                    && !line.starts_with("Supported file protocols")
                    && !line.ends_with(':')
            })
            .flat_map(|line| line.split_whitespace().map(str::to_string))
            .collect(),
    )
}

fn parse_ffmpeg_formats(text: &str) -> Vec<String> {
    normalize(
        text.lines()
            .filter_map(|line| {
                let line = line.trim_start();
                let mut parts = line.split_whitespace();
                let flags = parts.next()?;
                let names = parts.next()?;
                if !is_format_flag_block(flags) {
                    return None;
                }
                Some(
                    names
                        .split(',')
                        .map(str::to_string)
                        .collect::<Vec<String>>(),
                )
            })
            .flatten()
            .collect(),
    )
}

fn parse_ffmpeg_codecs(text: &str) -> Vec<String> {
    normalize(
        text.lines()
            .filter_map(|line| {
                let line = line.trim_start();
                let mut parts = line.split_whitespace();
                let flags = parts.next()?;
                let codec = parts.next()?;
                if !is_codec_flag_block(flags) {
                    return None;
                }
                Some(codec.to_string())
            })
            .collect(),
    )
}

fn is_format_flag_block(value: &str) -> bool {
    value.len() <= 2 && value.chars().all(|ch| ch == '.' || ch.is_ascii_uppercase())
}

fn is_codec_flag_block(value: &str) -> bool {
    value.len() >= 6 && value.chars().all(|ch| ch == '.' || ch.is_ascii_uppercase())
}

fn normalize(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

fn probe_nvidia_smi_csv(args: &[&str]) -> Vec<Vec<String>> {
    let output = Command::new("nvidia-smi").args(args).output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(parse_csv_row)
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_csv_row(line: &str) -> Vec<String> {
    line.split(',')
        .map(|value| value.trim().to_string())
        .collect()
}

fn parse_metric_percent(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("n/a") || trimmed.eq_ignore_ascii_case("[not supported]") {
        return Some(0.0);
    }
    trimmed.parse::<f64>().ok()
}

fn extract_zlm_version(value: Value) -> Option<String> {
    match value.get("data") {
        Some(Value::String(version)) if !version.trim().is_empty() => {
            Some(version.trim().to_string())
        }
        Some(Value::Object(map)) => map
            .get("version")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

fn extract_zlm_api_list(value: Value) -> Vec<String> {
    let Some(data) = value.get("data") else {
        return Vec::new();
    };

    let values = match data {
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Value::Object(map) => map
            .values()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };

    normalize(values)
}

fn extract_zlm_server_id(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(server_id) = map.get("mediaServerId").and_then(Value::as_str) {
                let server_id = server_id.trim();
                if !server_id.is_empty() {
                    return Some(server_id.to_string());
                }
            }
            map.values().find_map(extract_zlm_server_id)
        }
        Value::Array(items) => items.iter().find_map(extract_zlm_server_id),
        _ => None,
    }
}

fn extract_zlm_rtmp_enhanced_enabled(value: &Value) -> Option<bool> {
    match value {
        Value::Object(map) => {
            if map.get("key").and_then(Value::as_str) == Some("rtmp.enhanced") {
                return map.get("value").and_then(parse_bool_like_value);
            }
            if let Some(enabled) = map.get("rtmp.enhanced").and_then(parse_bool_like_value) {
                return Some(enabled);
            }
            map.values().find_map(extract_zlm_rtmp_enhanced_enabled)
        }
        Value::Array(items) => items.iter().find_map(extract_zlm_rtmp_enhanced_enabled),
        _ => None,
    }
}

fn parse_bool_like_value(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::Number(value) => value.as_i64().map(|value| value != 0),
        Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}
