use crate::models::{NativeError, json_object};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
};
use uuid::Uuid;

static SESSIONS: OnceLock<Mutex<HashMap<String, Child>>> = OnceLock::new();

const MEDIA_PROTOCOLS: &[&str] = &["http:", "https:", "rtsp:", "rtmp:", "rtmps:"];

pub fn validate_url(url: &str) -> Result<Value, NativeError> {
    let protocol = protocol_for(url).ok_or_else(|| {
        NativeError::MediaPlayer("media url must include a supported protocol".to_string())
    })?;
    if !MEDIA_PROTOCOLS.contains(&protocol.as_str()) {
        return Err(NativeError::MediaPlayer(format!(
            "unsupported media protocol: {protocol}"
        )));
    }
    Ok(json!({
        "url": url,
        "protocol": protocol.trim_end_matches(':'),
        "supported": true,
        "supported_protocols": MEDIA_PROTOCOLS,
    }))
}

pub fn open(url: &str, requested_player: Option<&str>) -> Result<Value, NativeError> {
    validate_url(url)?;
    if let Some(player) = choose_command_player(requested_player) {
        let child = Command::new(&player)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                NativeError::MediaPlayer(format!("failed to start {player}: {error}"))
            })?;
        let pid = child.id();
        let session_id = Uuid::now_v7().to_string();
        sessions().lock().unwrap().insert(session_id.clone(), child);
        return Ok(json!({
            "session_id": session_id,
            "backend": player,
            "mode": "external_process",
            "pid": pid,
            "url": url,
        }));
    }

    open_with_system_handler(url)?;
    Ok(json!({
        "session_id": Uuid::now_v7().to_string(),
        "backend": "system",
        "mode": "external_handler",
        "url": url,
    }))
}

pub fn stop(session_id: &str) -> Result<Value, NativeError> {
    let mut sessions = sessions().lock().unwrap();
    let Some(mut child) = sessions.remove(session_id) else {
        return Ok(
            json!({ "session_id": session_id, "stopped": false, "message": "session not tracked" }),
        );
    };
    let _ = child.kill();
    Ok(json!({ "session_id": session_id, "stopped": true }))
}

pub fn snapshot(_session_id: &str, _output_path: Option<&str>) -> Result<Value, NativeError> {
    Err(NativeError::MediaPlayer(
        "embedded snapshots require the libmpv/libVLC backend; current backend is external"
            .to_string(),
    ))
}

pub fn probe() -> Value {
    json_object([
        ("mpv_available", Value::Bool(command_exists("mpv"))),
        ("vlc_available", Value::Bool(command_exists("vlc"))),
        (
            "backend",
            Value::String(
                if command_exists("mpv") {
                    "mpv"
                } else if command_exists("vlc") {
                    "vlc"
                } else {
                    "system"
                }
                .to_string(),
            ),
        ),
        (
            "supported_protocols",
            Value::Array(
                MEDIA_PROTOCOLS
                    .iter()
                    .map(|value| Value::String(value.trim_end_matches(':').to_string()))
                    .collect(),
            ),
        ),
    ])
}

fn protocol_for(url: &str) -> Option<String> {
    let index = url.find(':')?;
    Some(url[..=index].to_ascii_lowercase())
}

fn choose_command_player(requested_player: Option<&str>) -> Option<String> {
    if let Some(player) = requested_player.filter(|value| !value.trim().is_empty()) {
        return command_exists(player).then(|| player.to_string());
    }
    if command_exists("mpv") {
        return Some("mpv".to_string());
    }
    if command_exists("vlc") {
        return Some("vlc".to_string());
    }
    None
}

fn open_with_system_handler(url: &str) -> Result<(), NativeError> {
    let status = if cfg!(target_os = "macos") {
        Command::new("open").arg(url).status()
    } else if cfg!(target_os = "windows") {
        Command::new("cmd").args(["/C", "start", "", url]).status()
    } else {
        Command::new("xdg-open").arg(url).status()
    };
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(NativeError::MediaPlayer(format!(
            "system media handler exited with {status}"
        ))),
        Err(error) => Err(NativeError::MediaPlayer(error.to_string())),
    }
}

fn command_exists(command: &str) -> bool {
    let status = if cfg!(target_os = "windows") {
        Command::new("where")
            .arg(command)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    } else {
        Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {}", shell_quote(command)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    status.map(|status| status.success()).unwrap_or(false)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn sessions() -> &'static Mutex<HashMap<String, Child>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_supported_protocol() {
        assert!(validate_url("rtsp://127.0.0.1/live/camera").is_ok());
        assert!(validate_url("rtmp://127.0.0.1/live/camera").is_ok());
        assert!(validate_url("http://127.0.0.1/live/camera.live.flv").is_ok());
        assert!(validate_url("https://127.0.0.1/live/camera/hls.m3u8").is_ok());
        assert!(validate_url("http://127.0.0.1/output/demo.mp4").is_ok());
    }

    #[test]
    fn rejects_unsupported_protocol() {
        assert!(validate_url("file:///tmp/demo.mp4").is_err());
    }
}
