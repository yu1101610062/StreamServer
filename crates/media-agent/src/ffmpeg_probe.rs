use std::{
    io::Read,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

#[cfg(test)]
use std::{
    collections::HashMap,
    fs,
    os::unix::process::ExitStatusExt,
    path::PathBuf,
    sync::{Mutex as StdMutex, OnceLock},
};

use media_domain::{InputKind, TaskSpec};
use serde::Deserialize;
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    media_policy::{InputAudioStream, InputMediaProfile, InputSourceFamily, VideoCodecFamily},
};

pub(crate) const DEFAULT_INPUT_PROBE_TIMEOUT_MS: u64 = 7000;
const FFPROBE_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Deserialize)]
struct FfprobeMediaResponse {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    format_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    index: Option<u32>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    pix_fmt: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    extradata_size: Option<u64>,
}

#[derive(Debug)]
struct TimedProcessOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct MockFfprobeAudioStream {
    pub(crate) index: Option<u32>,
    pub(crate) codec_name: String,
    pub(crate) sample_rate: Option<u32>,
    pub(crate) channels: Option<u32>,
    pub(crate) extradata_size: Option<u64>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct MockFfprobeBinary {
    pub(crate) format_name: String,
    pub(crate) video_codec_name: String,
    pub(crate) video_pix_fmt: Option<String>,
    pub(crate) video_extradata_size: Option<u64>,
    pub(crate) audio_streams: Vec<MockFfprobeAudioStream>,
    pub(crate) recorded_args_path: Option<PathBuf>,
    pub(crate) sleep_ms: u64,
}

#[cfg(test)]
static MOCK_FFPROBE_BINARIES: OnceLock<StdMutex<HashMap<String, MockFfprobeBinary>>> =
    OnceLock::new();

pub(crate) fn probe_input_media_profile(
    settings: &AgentSettings,
    spec: &TaskSpec,
    input_url: &str,
) -> InputMediaProfile {
    probe_input_media_profile_with_input_args(settings, spec, input_url, &[])
}

pub(crate) fn probe_input_media_profile_with_input_args(
    settings: &AgentSettings,
    spec: &TaskSpec,
    input_url: &str,
    extra_input_args: &[String],
) -> InputMediaProfile {
    // ffprobe failure returns a profile with source_family, so output policy stays conservative.
    let default_profile = InputMediaProfile {
        source_family: infer_input_source_family(spec, input_url, None),
        ..InputMediaProfile::default()
    };
    let mut args = vec!["-v".to_string(), "error".to_string()];
    args.extend(extra_input_args.iter().cloned());
    args.extend([
        "-show_entries".to_string(),
        "stream=index,codec_type,codec_name,pix_fmt,sample_rate,channels,extradata_size:format=format_name"
            .to_string(),
        "-of".to_string(),
        "json".to_string(),
        input_url.to_string(),
    ]);
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = run_ffprobe_with_timeout(
        &settings.ffprobe_bin,
        &arg_refs,
        input_probe_timeout_duration(spec.input.probe_timeout_ms),
    );

    let Some(output) = output else {
        return default_profile;
    };
    if !output.status.success() {
        return default_profile;
    }

    let Ok(parsed) = serde_json::from_slice::<FfprobeMediaResponse>(&output.stdout) else {
        return default_profile;
    };

    let mut profile = InputMediaProfile {
        source_family: infer_input_source_family(
            spec,
            input_url,
            parsed
                .format
                .as_ref()
                .and_then(|format| format.format_name.as_deref()),
        ),
        ..InputMediaProfile::default()
    };
    for (stream_position, stream) in parsed.streams.into_iter().enumerate() {
        match stream.codec_type.as_deref() {
            Some("video") if !profile.has_video => {
                profile.has_video = true;
                profile.video_codec_name = stream
                    .codec_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_ascii_lowercase);
                profile.video_pixel_format = stream
                    .pix_fmt
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_ascii_lowercase);
                profile.video_family = match profile.video_codec_name.as_deref() {
                    Some("h264") => VideoCodecFamily::H264,
                    Some("hevc") | Some("h265") => VideoCodecFamily::Hevc,
                    Some("vp8") => VideoCodecFamily::Vp8,
                    Some("vp9") => VideoCodecFamily::Vp9,
                    Some("av1") => VideoCodecFamily::Av1,
                    _ => VideoCodecFamily::Unknown,
                };
                profile.video_extradata_present = stream.extradata_size.unwrap_or_default() > 0;
            }
            Some("audio") => {
                let audio_stream = InputAudioStream {
                    index: stream.index.unwrap_or(stream_position as u32),
                    codec_name: stream
                        .codec_name
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_ascii_lowercase),
                    sample_rate: stream
                        .sample_rate
                        .as_deref()
                        .and_then(|value| value.trim().parse::<u32>().ok()),
                    channels: stream.channels,
                    extradata_present: stream.extradata_size.unwrap_or_default() > 0,
                };
                if !profile.has_audio {
                    profile.has_audio = true;
                    profile.audio_codec_name = audio_stream.codec_name.clone();
                    profile.audio_sample_rate = audio_stream.sample_rate;
                    profile.audio_channels = audio_stream.channels;
                    profile.audio_extradata_present = audio_stream.extradata_present;
                }
                profile.audio_streams.push(audio_stream);
            }
            _ => {}
        }
    }

    profile
}

pub(crate) fn probe_primary_video_codec_family(
    settings: &AgentSettings,
    input_url: &str,
    probe_timeout_ms: Option<u64>,
) -> VideoCodecFamily {
    let args = [
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=codec_name",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
        input_url,
    ];
    let output = run_ffprobe_with_timeout(
        &settings.ffprobe_bin,
        &args,
        input_probe_timeout_duration(probe_timeout_ms),
    );

    let Some(output) = output else {
        return VideoCodecFamily::Unknown;
    };
    if !output.status.success() {
        return VideoCodecFamily::Unknown;
    }

    match String::from_utf8_lossy(&output.stdout).trim() {
        "h264" => VideoCodecFamily::H264,
        "hevc" | "h265" => VideoCodecFamily::Hevc,
        "vp8" => VideoCodecFamily::Vp8,
        "vp9" => VideoCodecFamily::Vp9,
        "av1" => VideoCodecFamily::Av1,
        _ => VideoCodecFamily::Unknown,
    }
}

#[cfg(test)]
pub(crate) fn register_mock_ffprobe_binary(name: &str, mock: MockFfprobeBinary) -> String {
    let key = format!("mock-ffprobe://{name}/{}", Uuid::now_v7());
    MOCK_FFPROBE_BINARIES
        .get_or_init(|| StdMutex::new(HashMap::new()))
        .lock()
        .expect("mock ffprobe registry lock poisoned")
        .insert(key.clone(), mock);
    key
}

#[cfg(test)]
fn run_registered_mock_ffprobe_with_timeout(
    ffprobe_bin: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<Option<TimedProcessOutput>> {
    let mock = MOCK_FFPROBE_BINARIES
        .get_or_init(|| StdMutex::new(HashMap::new()))
        .lock()
        .expect("mock ffprobe registry lock poisoned")
        .get(ffprobe_bin)
        .cloned()?;

    if mock.sleep_ms > 0 && Duration::from_millis(mock.sleep_ms) >= timeout {
        return Some(None);
    }

    if let Some(path) = &mock.recorded_args_path {
        let mut recorded = args.join("\n");
        recorded.push('\n');
        let _ = fs::write(path, recorded);
    }

    let want_json = args.windows(2).any(|window| window == ["-of", "json"]);
    let stdout = if want_json {
        let mut streams = Vec::new();
        let mut video = json!({
            "codec_type": "video",
            "codec_name": mock.video_codec_name,
        });
        if !mock.audio_streams.is_empty() {
            video["index"] = json!(0u32);
        }
        if let Some(pix_fmt) = mock.video_pix_fmt {
            video["pix_fmt"] = json!(pix_fmt);
        }
        if let Some(extradata_size) = mock.video_extradata_size {
            video["extradata_size"] = json!(extradata_size);
        }
        streams.push(video);

        for (position, stream) in mock.audio_streams.into_iter().enumerate() {
            let mut audio = json!({
                "codec_type": "audio",
                "codec_name": stream.codec_name,
            });
            audio["index"] = json!(stream.index.unwrap_or((position + 1) as u32));
            if let Some(sample_rate) = stream.sample_rate {
                audio["sample_rate"] = json!(sample_rate.to_string());
            }
            if let Some(channels) = stream.channels {
                audio["channels"] = json!(channels);
            }
            if let Some(extradata_size) = stream.extradata_size {
                audio["extradata_size"] = json!(extradata_size);
            }
            streams.push(audio);
        }

        serde_json::to_vec(&json!({
            "streams": streams,
            "format": {"format_name": mock.format_name},
        }))
        .ok()?
    } else {
        format!("{}\n", mock.video_codec_name).into_bytes()
    };

    Some(Some(TimedProcessOutput {
        status: std::process::ExitStatus::from_raw(0),
        stdout,
    }))
}

fn input_probe_timeout_duration(timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(
        timeout_ms
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_INPUT_PROBE_TIMEOUT_MS),
    )
}

fn run_ffprobe_with_timeout(
    ffprobe_bin: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<TimedProcessOutput> {
    #[cfg(test)]
    if let Some(output) = run_registered_mock_ffprobe_with_timeout(ffprobe_bin, args, timeout) {
        return output;
    }

    // ffprobe can hang on broken streams or unreachable network sources, so poll and kill it.
    let mut child = std::process::Command::new(ffprobe_bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = pipe.read_to_end(&mut stdout);
                }
                return Some(TimedProcessOutput { status, stdout });
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(FFPROBE_POLL_INTERVAL),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

pub(crate) fn infer_input_source_family(
    spec: &TaskSpec,
    input_url: &str,
    probed_format_name: Option<&str>,
) -> InputSourceFamily {
    match spec.input.kind {
        Some(InputKind::Hls) => InputSourceFamily::Hls,
        Some(InputKind::HttpTs | InputKind::UdpMpegtsMulticast) => InputSourceFamily::MpegTs,
        Some(InputKind::HttpMp4) => InputSourceFamily::Mp4Mov,
        Some(InputKind::Rtsp | InputKind::Rtmp | InputKind::HttpFlv) => InputSourceFamily::RtspRtmp,
        _ => classify_input_source_family_from_format_name(probed_format_name)
            .unwrap_or_else(|| classify_input_source_family_from_path(input_url)),
    }
}

fn classify_input_source_family_from_format_name(
    format_name: Option<&str>,
) -> Option<InputSourceFamily> {
    let names = format_name?
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty());

    for name in names {
        match name.to_ascii_lowercase().as_str() {
            "mpegts" => return Some(InputSourceFamily::MpegTs),
            "hls" | "applehttp" => return Some(InputSourceFamily::Hls),
            "mov" | "mp4" | "m4a" | "3gp" | "3g2" | "mj2" => {
                return Some(InputSourceFamily::Mp4Mov);
            }
            "matroska" | "webm" => return Some(InputSourceFamily::Matroska),
            "rtsp" | "rtmp" | "flv" | "live_flv" => return Some(InputSourceFamily::RtspRtmp),
            _ => {}
        }
    }

    None
}

fn classify_input_source_family_from_path(input_url: &str) -> InputSourceFamily {
    let extension = Path::new(input_url)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);

    match extension.as_deref() {
        Some("ts" | "m2ts" | "mts") => InputSourceFamily::MpegTs,
        Some("m3u8") => InputSourceFamily::Hls,
        Some("mp4" | "mov" | "m4v" | "m4a" | "3gp" | "3g2") => InputSourceFamily::Mp4Mov,
        Some("mkv" | "webm") => InputSourceFamily::Matroska,
        _ => InputSourceFamily::Unknown,
    }
}
