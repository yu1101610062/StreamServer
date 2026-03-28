use std::{process::Command, time::Duration};

use chrono::Utc;
use media_domain::CapabilitySnapshot;
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

        CapabilitySnapshot {
            ffmpeg_protocols,
            ffmpeg_formats,
            ffmpeg_encoders,
            ffmpeg_decoders,
            zlm_version: zlm.version,
            zlm_api_list: zlm.api_list,
            gpu: Vec::new(),
            captured_at: Utc::now(),
        }
    }

    pub async fn zlm_alive(&self, settings: &AgentSettings) -> bool {
        self.probe_zlm(settings)
            .await
            .map(|result| result.version.is_some() || !result.api_list.is_empty())
            .unwrap_or(false)
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

        Ok(ZlmProbeResult { version, api_list })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_protocols() {
        let output = r#"
Supported file protocols:
Input:
  async
  http
Output:
  file
  rtmp
"#;

        assert_eq!(
            parse_ffmpeg_protocols(output),
            vec!["async", "file", "http", "rtmp"]
        );
    }

    #[test]
    fn parses_formats() {
        let output = r#"
File formats:
 D  matroska,webm    Matroska / WebM
  E flv              FLV (Flash Video)
"#;

        assert_eq!(
            parse_ffmpeg_formats(output),
            vec!["flv", "matroska", "webm"]
        );
    }

    #[test]
    fn parses_codecs() {
        let output = r#"
Encoders:
 V....D libx264             libx264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10
 A..... aac                AAC (Advanced Audio Coding)
"#;

        assert_eq!(parse_ffmpeg_codecs(output), vec!["aac", "libx264"]);
    }
}
