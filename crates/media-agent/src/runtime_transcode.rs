//! 转码与内部推流策略：集中维护 FFmpeg copy/transcode 选择、音频探测补偿和内部 ZLM 入流协议。

use media_domain::{InputKind, PublishTargetKind, SourceMode, TaskSpec, TaskType};
use reqwest::Url;

use crate::{
    capability::gpu_acceleration_enabled,
    config::AgentSettings,
    ffmpeg_args::insert_ffmpeg_input_args,
    ffmpeg_plan::PublishOutput,
    ffmpeg_probe::{
        infer_input_source_family, probe_input_media_profile, probe_primary_video_codec_family,
    },
    media_policy::{
        AudioCopyDecoration, AudioOutputPolicy, InputMediaProfile, InputSourceFamily,
        TranscodeSelection, VideoCodecFamily, VideoOutputPolicy, audio_copy_decoration_for_stream,
        audio_stream_can_be_copied, format_supports_video_codec_copy, output_video_family,
        resolve_audio_copy_selection, select_audio_stream_for_output,
        selected_audio_stream_parameters_available, should_force_h264_nvenc_to_yuv420p,
    },
    runtime::{ExecutorError, RuntimeCapabilityHints, StartupProbe, SuccessCheck},
};

const STREAM_INGEST_TS_AAC_COPY_PROBE_SIZE: u64 = 8_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InternalIngressProtocol {
    Rtmp,
    EnhancedRtmp,
    Rtsp,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProcessArgsContext<'a> {
    pub(crate) default_mode: &'a str,
    pub(crate) input_url: &'a str,
    pub(crate) output_format: &'a str,
    pub(crate) video_policy: VideoOutputPolicy,
    pub(crate) audio_policy: AudioOutputPolicy,
    pub(crate) input_profile: Option<&'a InputMediaProfile>,
}

impl InternalIngressProtocol {
    pub(crate) fn schema(self) -> &'static str {
        match self {
            Self::Rtmp | Self::EnhancedRtmp => "rtmp",
            Self::Rtsp => "rtsp",
        }
    }

    pub(crate) fn muxer_format(self) -> &'static str {
        match self {
            Self::Rtmp | Self::EnhancedRtmp => "flv",
            Self::Rtsp => "rtsp",
        }
    }

    pub(crate) fn compatibility_output_format(self) -> &'static str {
        match self {
            Self::Rtmp => "internal_flv",
            Self::EnhancedRtmp => "internal_enhanced_flv",
            Self::Rtsp => "internal_rtsp",
        }
    }

    pub(crate) fn metadata_value(self) -> &'static str {
        match self {
            Self::Rtmp => "rtmp",
            Self::EnhancedRtmp => "enhanced_rtmp",
            Self::Rtsp => "rtsp",
        }
    }
}

pub(crate) fn stream_ingest_probe_input_args(spec: &TaskSpec, input_url: &str) -> Vec<String> {
    if matches!(
        infer_input_source_family(spec, input_url, None),
        InputSourceFamily::MpegTs | InputSourceFamily::Hls
    ) {
        stream_ingest_ts_aac_copy_probe_input_args()
    } else {
        Vec::new()
    }
}

fn stream_ingest_ts_aac_copy_probe_input_args() -> Vec<String> {
    vec![
        "-probesize".to_string(),
        STREAM_INGEST_TS_AAC_COPY_PROBE_SIZE.to_string(),
    ]
}

pub(crate) fn should_loop_file_to_live_input(spec: &TaskSpec) -> bool {
    spec.task_type == TaskType::StreamIngest
        && spec.input.loop_enabled.unwrap_or(false)
        && spec.input.source_mode == Some(SourceMode::Vod)
        && matches!(
            spec.input.kind,
            Some(InputKind::File | InputKind::HttpMp4 | InputKind::Hls | InputKind::HttpTs)
        )
}

pub(crate) fn append_process_args(
    args: &mut Vec<String>,
    settings: &AgentSettings,
    spec: &TaskSpec,
    context: ProcessArgsContext<'_>,
) -> Result<Option<AudioCopyDecoration>, ExecutorError> {
    let mode = normalized_process_mode(spec, context.default_mode);
    match mode {
        "passthrough" => {
            // passthrough 明确要求全复制，只在必要时追加封装格式需要的音频 bitstream filter。
            let audio_copy_decoration = resolve_passthrough_audio_copy_decoration(
                settings,
                spec,
                context.input_url,
                context.output_format,
                context.input_profile,
            );
            args.extend([
                "-c:v".to_string(),
                "copy".to_string(),
                "-c:a".to_string(),
                "copy".to_string(),
            ]);
            Ok(audio_copy_decoration)
        }
        "copy_or_transcode" | "force_transcode" => {
            // copy_or_transcode 先基于探测结果尝试复制，不满足封装/策略时只转码必要的轨道。
            let probed_profile;
            let selection_profile = match context.input_profile {
                Some(profile) => Some(profile),
                None => {
                    probed_profile = probe_input_media_profile(settings, spec, context.input_url);
                    Some(&probed_profile)
                }
            };
            let selection = resolve_process_selection(
                settings,
                spec,
                mode,
                ProcessArgsContext {
                    input_profile: selection_profile,
                    ..context
                },
            );
            if !selection.input_args.is_empty() {
                insert_ffmpeg_input_args(args, selection.input_args);
            }
            args.extend([
                "-c:v".to_string(),
                selection.video_encoder,
                "-c:a".to_string(),
                selection.audio_encoder,
            ]);
            if selection_profile
                .is_some_and(|profile| should_force_h264_nvenc_to_yuv420p(profile, args))
            {
                args.extend([
                    "-vf".to_string(),
                    "format=yuv420p".to_string(),
                    "-pix_fmt".to_string(),
                    "yuv420p".to_string(),
                ]);
            }
            if let Some(bitrate) = spec.process.bitrate {
                args.extend(["-b:v".to_string(), format!("{bitrate}k")]);
            }
            if let Some(fps) = spec.process.fps {
                args.extend(["-r".to_string(), fps.to_string()]);
            }
            if let Some(gop) = spec.process.gop {
                args.extend(["-g".to_string(), gop.to_string()]);
            }
            Ok(selection.audio_copy_decoration)
        }
        other => Err(ExecutorError::InvalidRequest(format!(
            "unsupported process.mode: {other}"
        ))),
    }
}

fn normalized_process_mode<'a>(spec: &'a TaskSpec, default_mode: &'a str) -> &'a str {
    match spec.process.mode.as_deref().unwrap_or(default_mode) {
        "transcode" => "force_transcode",
        value => value,
    }
}

fn resolve_process_selection(
    settings: &AgentSettings,
    spec: &TaskSpec,
    mode: &str,
    context: ProcessArgsContext<'_>,
) -> TranscodeSelection {
    if mode == "force_transcode" {
        // 强制转码仍使用探测到的输入视频族来选择 H.264/HEVC 输出编码器。
        if let Some(profile) = context.input_profile {
            return resolve_transcode_selection_for_input_family(
                settings,
                profile.video_family,
                context.video_policy,
                context.audio_policy,
            );
        }
        return resolve_transcode_selection(
            settings,
            spec,
            context.input_url,
            context.video_policy,
            context.audio_policy,
        );
    }

    let probed_profile;
    let profile = match context.input_profile {
        Some(profile) => profile,
        None => {
            probed_profile = probe_input_media_profile(settings, spec, context.input_url);
            &probed_profile
        }
    };
    let video_copy =
        should_copy_video_stream(spec, context.output_format, profile, context.video_policy);
    let audio_copy = resolve_audio_copy_selection(
        context.output_format,
        profile,
        context.audio_policy,
        process_requires_audio_transcode(spec),
    );
    // 视频和音频都可复制时不插入额外输入参数，最大限度保留原始码流。
    if video_copy && audio_copy.copy {
        return TranscodeSelection {
            input_args: Vec::new(),
            video_encoder: "copy".to_string(),
            audio_encoder: "copy".to_string(),
            audio_copy_decoration: audio_copy.decoration,
        };
    }

    let transcode = resolve_transcode_selection_for_input_family(
        settings,
        profile.video_family,
        context.video_policy,
        context.audio_policy,
    );

    TranscodeSelection {
        input_args: if video_copy {
            Vec::new()
        } else {
            transcode.input_args
        },
        video_encoder: if video_copy {
            "copy".to_string()
        } else {
            transcode.video_encoder
        },
        audio_encoder: if audio_copy.copy {
            "copy".to_string()
        } else {
            transcode.audio_encoder
        },
        audio_copy_decoration: if audio_copy.copy {
            audio_copy.decoration
        } else {
            None
        },
    }
}

fn should_copy_video_stream(
    spec: &TaskSpec,
    output_format: &str,
    profile: &InputMediaProfile,
    video_policy: VideoOutputPolicy,
) -> bool {
    if !profile.has_video {
        // 没有探测到视频时不强行转码，交给 ffmpeg 对实际输入做容错处理。
        return true;
    }
    if process_requires_video_transcode(spec)
        || requires_live_mpegts_multicast_video_stabilization(spec, output_format)
    {
        return false;
    }

    let format_allows_copy =
        format_supports_video_codec_copy(output_format, profile.video_codec_name.as_deref());
    if !format_allows_copy {
        return false;
    }

    match video_policy {
        VideoOutputPolicy::KeepSourceFamily | VideoOutputPolicy::CopyWhitelistedElseH264 => true,
        VideoOutputPolicy::ForceH264 => profile.video_family == VideoCodecFamily::H264,
    }
}

pub(crate) fn resolve_stream_ingest_audio_copy_probe_input_args(
    spec: &TaskSpec,
    output_format: &str,
    profile: &InputMediaProfile,
    audio_policy: AudioOutputPolicy,
) -> Result<Vec<String>, ExecutorError> {
    let audio_transcode_required = process_requires_audio_transcode(spec);
    let audio_copy = resolve_audio_copy_selection(
        output_format,
        profile,
        audio_policy,
        audio_transcode_required,
    );
    let selected_audio_stream = select_audio_stream_for_output(
        output_format,
        profile,
        audio_policy,
        audio_transcode_required,
    );
    if !audio_copy.copy
        || !matches!(
            profile.source_family,
            InputSourceFamily::MpegTs | InputSourceFamily::Hls
        )
        || selected_audio_stream
            .as_ref()
            .and_then(|stream| stream.codec_name.as_deref())
            != Some("aac")
    {
        return Ok(Vec::new());
    }

    if !selected_audio_stream
        .as_ref()
        .is_some_and(selected_audio_stream_parameters_available)
    {
        // TS 系 AAC 缺少 sample_rate/channels 时，复制到 FLV/RTMP 容易得到不可播放输出。
        return Err(ExecutorError::InvalidRequest(format!(
            "input audio stream is AAC in a TS-family source, but sample_rate/channels remain unavailable after probing; refusing audio copy for {output_format} output"
        )));
    }

    Ok(stream_ingest_ts_aac_copy_probe_input_args())
}

fn resolve_passthrough_audio_copy_decoration(
    settings: &AgentSettings,
    spec: &TaskSpec,
    input_url: &str,
    output_format: &str,
    input_profile: Option<&InputMediaProfile>,
) -> Option<AudioCopyDecoration> {
    let probed_profile;
    let profile = match input_profile {
        Some(profile) => profile,
        None => {
            probed_profile = probe_input_media_profile(settings, spec, input_url);
            &probed_profile
        }
    };
    if !profile.has_audio
        || select_audio_stream_for_output(
            output_format,
            profile,
            AudioOutputPolicy::CopyWhitelistedElseAac,
            process_requires_audio_transcode(spec),
        )
        .as_ref()
        .is_none_or(|stream| {
            !audio_stream_can_be_copied(output_format, profile.source_family, stream)
        })
    {
        return None;
    }

    select_audio_stream_for_output(
        output_format,
        profile,
        AudioOutputPolicy::CopyWhitelistedElseAac,
        process_requires_audio_transcode(spec),
    )
    .and_then(|stream| audio_copy_decoration_for_stream(profile, &stream))
}

fn process_requires_video_transcode(spec: &TaskSpec) -> bool {
    spec.process.bitrate.is_some() || spec.process.fps.is_some() || spec.process.gop.is_some()
}

fn process_requires_audio_transcode(spec: &TaskSpec) -> bool {
    let _ = spec;
    false
}

fn resolve_transcode_selection(
    settings: &AgentSettings,
    spec: &TaskSpec,
    input_url: &str,
    video_policy: VideoOutputPolicy,
    audio_policy: AudioOutputPolicy,
) -> TranscodeSelection {
    let (input_family, _) = resolve_video_families(
        settings,
        input_url,
        spec.input.probe_timeout_ms,
        video_policy,
    );
    resolve_transcode_selection_for_input_family(settings, input_family, video_policy, audio_policy)
}

pub(crate) fn resolve_transcode_selection_for_input_family(
    settings: &AgentSettings,
    input_family: VideoCodecFamily,
    video_policy: VideoOutputPolicy,
    audio_policy: AudioOutputPolicy,
) -> TranscodeSelection {
    let output_family = output_video_family(input_family, video_policy);
    let use_gpu = gpu_acceleration_enabled(settings)
        && matches!(
            output_family,
            VideoCodecFamily::H264 | VideoCodecFamily::Hevc
        );

    let video_encoder = if use_gpu {
        match output_family {
            VideoCodecFamily::Hevc => "hevc_nvenc".to_string(),
            _ => "h264_nvenc".to_string(),
        }
    } else {
        match output_family {
            VideoCodecFamily::Hevc => "libx265".to_string(),
            _ => "libx264".to_string(),
        }
    };

    let audio_encoder = match audio_policy {
        AudioOutputPolicy::Copy => "copy".to_string(),
        AudioOutputPolicy::Aac | AudioOutputPolicy::CopyWhitelistedElseAac => "aac".to_string(),
    };

    TranscodeSelection {
        input_args: Vec::new(),
        video_encoder,
        audio_encoder,
        audio_copy_decoration: None,
    }
}

pub(crate) fn resolve_video_families(
    settings: &AgentSettings,
    input_url: &str,
    probe_timeout_ms: Option<u64>,
    video_policy: VideoOutputPolicy,
) -> (VideoCodecFamily, VideoCodecFamily) {
    let input_family = probe_primary_video_codec_family(settings, input_url, probe_timeout_ms);
    let output_family = output_video_family(input_family, video_policy);
    (input_family, output_family)
}

pub(crate) fn append_single_audio_output_maps(
    args: &mut Vec<String>,
    spec: &TaskSpec,
    output_format: &str,
    profile: &InputMediaProfile,
    audio_policy: AudioOutputPolicy,
) {
    args.extend(["-map".to_string(), "0:v?".to_string()]);
    if let Some(audio_stream) = select_audio_stream_for_output(
        output_format,
        profile,
        audio_policy,
        process_requires_audio_transcode(spec),
    ) {
        args.extend(["-map".to_string(), format!("0:{}", audio_stream.index)]);
    }
}

pub(crate) fn select_internal_ingress_protocol(
    settings: &AgentSettings,
    profile: &InputMediaProfile,
    capability_hints: RuntimeCapabilityHints,
) -> InternalIngressProtocol {
    let audio_codec = profile.audio_codec_name.as_deref();
    let video_codec = profile.video_codec_name.as_deref();
    let enhanced_enabled = settings.allow_enhanced_rtmp_expose
        && capability_hints.zlm_rtmp_enhanced_enabled.unwrap_or(false);

    if matches!(video_codec, Some("vp8")) || matches!(audio_codec, Some("mp2")) {
        return InternalIngressProtocol::Rtsp;
    }

    if matches!(video_codec, Some("hevc" | "h265" | "vp9" | "av1"))
        || matches!(audio_codec, Some("opus"))
    {
        return if enhanced_enabled {
            InternalIngressProtocol::EnhancedRtmp
        } else {
            InternalIngressProtocol::Rtsp
        };
    }

    if matches!(video_codec, Some("h264")) || !profile.has_video || video_codec.is_none() {
        return InternalIngressProtocol::Rtmp;
    }

    InternalIngressProtocol::Rtsp
}

pub(crate) fn build_internal_stream_output(
    settings: &AgentSettings,
    probe: &StartupProbe,
    protocol: InternalIngressProtocol,
) -> PublishOutput {
    PublishOutput {
        success_check: SuccessCheck::ProcessExit,
        target: build_internal_stream_target(settings, probe, protocol),
        format: protocol.muxer_format().to_string(),
        muxer: protocol.muxer_format().to_string(),
        output_args: match protocol {
            InternalIngressProtocol::Rtsp => {
                vec!["-rtsp_transport".to_string(), "tcp".to_string()]
            }
            InternalIngressProtocol::Rtmp | InternalIngressProtocol::EnhancedRtmp => Vec::new(),
        },
    }
}

fn build_internal_stream_target(
    settings: &AgentSettings,
    probe: &StartupProbe,
    protocol: InternalIngressProtocol,
) -> String {
    let host = Url::parse(&settings.zlm_api_base)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "127.0.0.1".to_string());
    match protocol {
        InternalIngressProtocol::Rtmp | InternalIngressProtocol::EnhancedRtmp => format!(
            "rtmp://{}:{}/{}/{}",
            host, settings.zlm_rtmp_port, probe.app, probe.stream
        ),
        InternalIngressProtocol::Rtsp => format!(
            "rtsp://{}:{}/{}/{}",
            host, settings.zlm_rtsp_port, probe.app, probe.stream
        ),
    }
}

pub(crate) fn should_stabilize_live_mpegts_multicast_bridge(
    spec: &TaskSpec,
    output: &PublishOutput,
) -> bool {
    spec.process.mode.as_deref().unwrap_or("passthrough") == "passthrough"
        && requires_live_mpegts_multicast_video_stabilization(spec, output.format.as_str())
}

fn requires_live_mpegts_multicast_video_stabilization(
    spec: &TaskSpec,
    output_format: &str,
) -> bool {
    output_format.eq_ignore_ascii_case("mpegts")
        && matches!(
            spec.input.kind,
            Some(
                InputKind::Rtsp
                    | InputKind::Rtmp
                    | InputKind::Hls
                    | InputKind::HttpFlv
                    | InputKind::HttpTs
            )
        )
        && matches!(
            spec.publish.kind,
            Some(PublishTargetKind::UdpMpegtsMulticast)
        )
}

pub(crate) fn append_live_mpegts_multicast_bridge_args(
    args: &mut Vec<String>,
    settings: &AgentSettings,
    spec: &TaskSpec,
    input_url: &str,
) {
    let selection = resolve_transcode_selection(
        settings,
        spec,
        input_url,
        VideoOutputPolicy::ForceH264,
        AudioOutputPolicy::Copy,
    );
    let video_codec = selection.video_encoder;
    if !selection.input_args.is_empty() {
        insert_ffmpeg_input_args(args, selection.input_args);
    }

    args.extend([
        "-c:v".to_string(),
        video_codec.clone(),
        "-c:a".to_string(),
        selection.audio_encoder,
    ]);

    if let Some(bitrate) = spec.process.bitrate {
        args.extend(["-b:v".to_string(), format!("{bitrate}k")]);
    }
    if let Some(fps) = spec.process.fps {
        args.extend(["-r".to_string(), fps.to_string()]);
    }

    let gop = spec.process.gop.unwrap_or(24);
    args.extend([
        "-g".to_string(),
        gop.to_string(),
        "-sc_threshold".to_string(),
        "0".to_string(),
    ]);

    if video_codec == "libx264" {
        args.extend(["-preset".to_string(), "ultrafast".to_string()]);
    }
}
