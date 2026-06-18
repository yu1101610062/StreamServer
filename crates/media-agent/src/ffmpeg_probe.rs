use std::{
    io::Read,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use media_domain::{InputKind, TaskSpec};
use serde::Deserialize;

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
    duration: Option<String>,
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
        "stream=index,codec_type,codec_name,pix_fmt,sample_rate,channels,extradata_size:format=format_name,duration"
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
        duration_sec: parsed
            .format
            .as_ref()
            .and_then(|format| parse_format_duration_sec(format.duration.as_deref())),
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

fn parse_format_duration_sec(value: Option<&str>) -> Option<u64> {
    let duration = value?.trim().parse::<f64>().ok()?;
    if !duration.is_finite() || duration <= 0.0 {
        return None;
    }
    Some(duration.ceil() as u64)
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
