#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputFormatDescriptor {
    pub(crate) logical_format: &'static str,
    pub(crate) ffmpeg_muxer: &'static str,
    pub(crate) extension: &'static str,
}

pub(crate) const WEBM_OUTPUT_DISABLED_MESSAGE: &str =
    "publish.format=webm is temporarily disabled for output; webm upload inputs remain supported";

pub(crate) fn normalized_output_format_label(output_format: &str) -> String {
    output_format.trim().to_ascii_lowercase()
}

pub(crate) fn output_format_descriptor(format: &str) -> Option<OutputFormatDescriptor> {
    match normalized_output_format_label(format).as_str() {
        "hls" => Some(OutputFormatDescriptor {
            logical_format: "hls",
            ffmpeg_muxer: "hls",
            extension: "m3u8",
        }),
        "mp4" => Some(OutputFormatDescriptor {
            logical_format: "mp4",
            ffmpeg_muxer: "mp4",
            extension: "mp4",
        }),
        "flv" => Some(OutputFormatDescriptor {
            logical_format: "flv",
            ffmpeg_muxer: "flv",
            extension: "flv",
        }),
        "mpegts" => Some(OutputFormatDescriptor {
            logical_format: "mpegts",
            ffmpeg_muxer: "mpegts",
            extension: "ts",
        }),
        "rtp_mpegts" => Some(OutputFormatDescriptor {
            logical_format: "rtp_mpegts",
            ffmpeg_muxer: "rtp_mpegts",
            extension: "ts",
        }),
        "matroska" | "mkv" => Some(OutputFormatDescriptor {
            logical_format: "matroska",
            ffmpeg_muxer: "matroska",
            extension: "mkv",
        }),
        "mov" => Some(OutputFormatDescriptor {
            logical_format: "mov",
            ffmpeg_muxer: "mov",
            extension: "mov",
        }),
        _ => None,
    }
}

pub(crate) fn disabled_output_format_message(format: &str) -> Option<&'static str> {
    (normalized_output_format_label(format) == "webm").then_some(WEBM_OUTPUT_DISABLED_MESSAGE)
}

pub(crate) fn ffmpeg_muxer_for_format(format: &str) -> String {
    output_format_descriptor(format)
        .map(|descriptor| descriptor.ffmpeg_muxer.to_string())
        .unwrap_or_else(|| format.to_string())
}

pub(crate) fn logical_output_format_for_format(format: &str) -> String {
    output_format_descriptor(format)
        .map(|descriptor| descriptor.logical_format.to_string())
        .unwrap_or_else(|| format.to_string())
}

pub(crate) fn canonical_output_muxer(output_format: &str) -> &'static str {
    match normalized_output_format_label(output_format).as_str() {
        "internal_flv" | "internal_enhanced_flv" => "flv",
        "internal_rtsp" => "rtsp",
        "flv" => "flv",
        "rtsp" => "rtsp",
        "mp4" => "mp4",
        "mov" => "mov",
        "matroska" | "mkv" => "matroska",
        "mpegts" | "rtp_mpegts" | "hls" => "mpegts",
        _ => "",
    }
}

fn sanitized_fallback_extension(other: &str) -> String {
    let sanitized: String = other
        .chars()
        .filter(|value| value.is_ascii_alphanumeric() || matches!(value, '.' | '_' | '+' | '-'))
        .collect();
    if sanitized.is_empty() {
        "bin".to_string()
    } else {
        sanitized
    }
}

pub(crate) fn default_file_extension_for_format(format: &str) -> String {
    match output_format_descriptor(format) {
        Some(descriptor) => descriptor.extension.to_string(),
        None => sanitized_fallback_extension(format),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoOutputPolicy {
    KeepSourceFamily,
    ForceH264,
    CopyWhitelistedElseH264,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioOutputPolicy {
    Copy,
    Aac,
    CopyWhitelistedElseAac,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioBitstreamFilter {
    AacAdtsToAsc,
}

impl AudioBitstreamFilter {
    pub(crate) fn as_ffmpeg_name(self) -> &'static str {
        match self {
            Self::AacAdtsToAsc => "aac_adtstoasc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioCopyDecoration {
    NeedsAdtsToAscForFlvAndMp4,
}

impl AudioCopyDecoration {
    pub(crate) fn filter_for_output(self, output_format: &str) -> Option<AudioBitstreamFilter> {
        match canonical_output_muxer(output_format) {
            "flv" | "mp4" | "mov" => Some(AudioBitstreamFilter::AacAdtsToAsc),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoCodecFamily {
    H264,
    Hevc,
    Vp8,
    Vp9,
    Av1,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputSourceFamily {
    MpegTs,
    Hls,
    Mp4Mov,
    Matroska,
    RtspRtmp,
    Unknown,
}

#[derive(Debug, Clone)]
pub(crate) struct TranscodeSelection {
    pub(crate) input_args: Vec<String>,
    pub(crate) video_encoder: String,
    pub(crate) audio_encoder: String,
    pub(crate) audio_copy_decoration: Option<AudioCopyDecoration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputMediaProfile {
    pub(crate) has_video: bool,
    pub(crate) video_family: VideoCodecFamily,
    pub(crate) video_codec_name: Option<String>,
    pub(crate) video_pixel_format: Option<String>,
    pub(crate) video_extradata_present: bool,
    pub(crate) has_audio: bool,
    pub(crate) audio_codec_name: Option<String>,
    pub(crate) audio_sample_rate: Option<u32>,
    pub(crate) audio_channels: Option<u32>,
    pub(crate) audio_extradata_present: bool,
    pub(crate) audio_streams: Vec<InputAudioStream>,
    pub(crate) source_family: InputSourceFamily,
}

impl Default for InputMediaProfile {
    fn default() -> Self {
        Self {
            has_video: false,
            video_family: VideoCodecFamily::Unknown,
            video_codec_name: None,
            video_pixel_format: None,
            video_extradata_present: false,
            has_audio: false,
            audio_codec_name: None,
            audio_sample_rate: None,
            audio_channels: None,
            audio_extradata_present: false,
            audio_streams: Vec::new(),
            source_family: InputSourceFamily::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputAudioStream {
    pub(crate) index: u32,
    pub(crate) codec_name: Option<String>,
    pub(crate) sample_rate: Option<u32>,
    pub(crate) channels: Option<u32>,
    pub(crate) extradata_present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioCopySelection {
    pub(crate) copy: bool,
    pub(crate) decoration: Option<AudioCopyDecoration>,
}

pub(crate) fn format_supports_video_codec_copy(
    output_format: &str,
    codec_name: Option<&str>,
) -> bool {
    let Some(codec_name) = codec_name.map(str::trim).map(str::to_ascii_lowercase) else {
        return false;
    };

    match normalized_output_format_label(output_format).as_str() {
        "internal_flv" => matches!(codec_name.as_str(), "h264"),
        "internal_enhanced_flv" => {
            matches!(
                codec_name.as_str(),
                "h264" | "hevc" | "h265" | "av1" | "vp9"
            )
        }
        "internal_rtsp" => matches!(
            codec_name.as_str(),
            "h264" | "hevc" | "h265" | "av1" | "vp8" | "vp9"
        ),
        "flv" => matches!(codec_name.as_str(), "h264" | "hevc" | "h265"),
        "rtsp" => matches!(
            codec_name.as_str(),
            "h264" | "hevc" | "h265" | "av1" | "vp8" | "vp9"
        ),
        "mp4" => matches!(
            codec_name.as_str(),
            "h264" | "hevc" | "h265" | "av1" | "vp9" | "mpeg4" | "mjpeg"
        ),
        "mov" => matches!(
            codec_name.as_str(),
            "h264" | "hevc" | "h265" | "av1" | "vp9" | "mpeg4" | "mjpeg" | "prores" | "dnxhd"
        ),
        "matroska" | "mkv" => matches!(
            codec_name.as_str(),
            "h264"
                | "hevc"
                | "h265"
                | "av1"
                | "vp8"
                | "vp9"
                | "mpeg4"
                | "mpeg2video"
                | "mjpeg"
                | "prores"
                | "dnxhd"
        ),
        "mpegts" | "rtp_mpegts" | "hls" => matches!(
            codec_name.as_str(),
            "h264" | "hevc" | "h265" | "mpeg2video" | "mpeg4"
        ),
        _ => false,
    }
}

pub(crate) fn format_supports_audio_codec_copy(
    output_format: &str,
    codec_name: Option<&str>,
) -> bool {
    let Some(codec_name) = codec_name.map(str::trim).map(str::to_ascii_lowercase) else {
        return false;
    };

    match normalized_output_format_label(output_format).as_str() {
        "internal_flv" => matches!(
            codec_name.as_str(),
            "aac" | "mp3" | "pcm_alaw" | "pcm_mulaw"
        ),
        "internal_enhanced_flv" => matches!(
            codec_name.as_str(),
            "aac" | "mp3" | "opus" | "pcm_alaw" | "pcm_mulaw"
        ),
        "internal_rtsp" => matches!(
            codec_name.as_str(),
            "aac" | "mp2" | "mp3" | "opus" | "pcm_alaw" | "pcm_mulaw" | "pcm_s16be" | "pcm_s16le"
        ),
        "flv" => matches!(codec_name.as_str(), "aac" | "mp3"),
        "rtsp" => matches!(
            codec_name.as_str(),
            "aac" | "mp2" | "mp3" | "opus" | "pcm_alaw" | "pcm_mulaw" | "pcm_s16be" | "pcm_s16le"
        ),
        "mp4" => matches!(codec_name.as_str(), "aac" | "mp3" | "ac3" | "eac3" | "alac"),
        "mov" => matches!(
            codec_name.as_str(),
            "aac"
                | "mp3"
                | "ac3"
                | "eac3"
                | "alac"
                | "pcm_s16le"
                | "pcm_s24le"
                | "pcm_s32le"
                | "pcm_f32le"
                | "pcm_f64le"
        ),
        "matroska" | "mkv" => matches!(
            codec_name.as_str(),
            "aac"
                | "mp2"
                | "mp3"
                | "ac3"
                | "eac3"
                | "opus"
                | "vorbis"
                | "flac"
                | "alac"
                | "pcm_s16le"
                | "pcm_s24le"
                | "pcm_s32le"
                | "pcm_f32le"
                | "pcm_f64le"
                | "pcm_alaw"
                | "pcm_mulaw"
        ),
        "mpegts" | "rtp_mpegts" | "hls" => {
            matches!(codec_name.as_str(), "aac" | "mp2" | "mp3" | "ac3" | "eac3")
        }
        _ => false,
    }
}

pub(crate) fn output_video_family(
    input_family: VideoCodecFamily,
    video_policy: VideoOutputPolicy,
) -> VideoCodecFamily {
    match video_policy {
        VideoOutputPolicy::KeepSourceFamily => match input_family {
            VideoCodecFamily::Hevc => VideoCodecFamily::Hevc,
            _ => VideoCodecFamily::H264,
        },
        VideoOutputPolicy::ForceH264 | VideoOutputPolicy::CopyWhitelistedElseH264 => {
            VideoCodecFamily::H264
        }
    }
}

pub(crate) fn is_flv_output_profile(output_format: &str) -> bool {
    matches!(canonical_output_muxer(output_format), "flv")
}

pub(crate) fn is_rtsp_output_profile(output_format: &str) -> bool {
    matches!(canonical_output_muxer(output_format), "rtsp")
}

pub(crate) fn resolve_audio_copy_selection(
    output_format: &str,
    profile: &InputMediaProfile,
    audio_policy: AudioOutputPolicy,
    audio_transcode_required: bool,
) -> AudioCopySelection {
    if !profile.has_audio {
        return AudioCopySelection {
            copy: true,
            decoration: None,
        };
    }
    if audio_transcode_required {
        return AudioCopySelection {
            copy: false,
            decoration: None,
        };
    }

    let selected_audio_stream = select_audio_stream_for_output(
        output_format,
        profile,
        audio_policy,
        audio_transcode_required,
    );
    let Some(selected_audio_stream) = selected_audio_stream else {
        return AudioCopySelection {
            copy: true,
            decoration: None,
        };
    };

    match audio_policy {
        AudioOutputPolicy::Copy => AudioCopySelection {
            copy: audio_stream_can_be_copied(
                output_format,
                profile.source_family,
                &selected_audio_stream,
            ),
            decoration: None,
        },
        AudioOutputPolicy::Aac | AudioOutputPolicy::CopyWhitelistedElseAac => {
            let copy = audio_stream_can_be_copied(
                output_format,
                profile.source_family,
                &selected_audio_stream,
            );
            AudioCopySelection {
                copy,
                decoration: if copy {
                    audio_copy_decoration_for_stream(profile, &selected_audio_stream)
                } else {
                    None
                },
            }
        }
    }
}

pub(crate) fn resolve_audio_copy_decoration(
    source_family: InputSourceFamily,
    codec_name: Option<&str>,
) -> Option<AudioCopyDecoration> {
    (matches!(
        source_family,
        InputSourceFamily::MpegTs | InputSourceFamily::Hls
    ) && codec_name == Some("aac"))
    .then_some(AudioCopyDecoration::NeedsAdtsToAscForFlvAndMp4)
}

pub(crate) fn audio_copy_decoration_for_stream(
    profile: &InputMediaProfile,
    stream: &InputAudioStream,
) -> Option<AudioCopyDecoration> {
    resolve_audio_copy_decoration(profile.source_family, stream.codec_name.as_deref())
}

fn primary_audio_stream(profile: &InputMediaProfile) -> Option<InputAudioStream> {
    profile.has_audio.then(|| InputAudioStream {
        index: 1,
        codec_name: profile.audio_codec_name.clone(),
        sample_rate: profile.audio_sample_rate,
        channels: profile.audio_channels,
        extradata_present: profile.audio_extradata_present,
    })
}

fn audio_streams_for_selection(profile: &InputMediaProfile) -> Vec<InputAudioStream> {
    if !profile.audio_streams.is_empty() {
        return profile.audio_streams.clone();
    }
    primary_audio_stream(profile).into_iter().collect()
}

pub(crate) fn select_audio_stream_for_output(
    output_format: &str,
    profile: &InputMediaProfile,
    audio_policy: AudioOutputPolicy,
    audio_transcode_required: bool,
) -> Option<InputAudioStream> {
    let audio_streams = audio_streams_for_selection(profile);
    if audio_streams.is_empty() {
        return None;
    }

    if audio_transcode_required {
        return audio_streams.into_iter().next();
    }

    match audio_policy {
        // 多音轨输出只选择一个轨道，优先选目标封装可以安全复制的 codec。
        AudioOutputPolicy::Copy => audio_streams
            .iter()
            .find(|stream| audio_stream_can_be_copied(output_format, profile.source_family, stream))
            .cloned()
            .or_else(|| audio_streams.into_iter().next()),
        AudioOutputPolicy::Aac | AudioOutputPolicy::CopyWhitelistedElseAac => {
            preferred_copy_audio_stream(output_format, profile.source_family, &audio_streams)
                .or_else(|| audio_streams.into_iter().next())
        }
    }
}

pub(crate) fn preferred_copy_audio_stream(
    output_format: &str,
    source_family: InputSourceFamily,
    streams: &[InputAudioStream],
) -> Option<InputAudioStream> {
    for codec in preferred_audio_copy_codec_order(output_format) {
        if let Some(stream) = streams.iter().find(|stream| {
            stream.codec_name.as_deref() == Some(codec)
                && audio_stream_can_be_copied(output_format, source_family, stream)
        }) {
            return Some(stream.clone());
        }
    }

    streams
        .iter()
        .find(|stream| audio_stream_can_be_copied(output_format, source_family, stream))
        .cloned()
}

pub(crate) fn preferred_audio_copy_codec_order(output_format: &str) -> &'static [&'static str] {
    match normalized_output_format_label(output_format).as_str() {
        "mp4" => &["aac", "mp3", "ac3", "eac3", "alac"],
        "internal_flv" | "flv" => &["aac", "mp3", "pcm_alaw", "pcm_mulaw"],
        "internal_enhanced_flv" => &["aac", "mp3", "opus", "pcm_alaw", "pcm_mulaw"],
        "internal_rtsp" | "rtsp" => &[
            "aac",
            "mp3",
            "opus",
            "mp2",
            "pcm_alaw",
            "pcm_mulaw",
            "pcm_s16be",
            "pcm_s16le",
        ],
        "matroska" | "mkv" => &["aac", "mp3", "opus", "vorbis", "flac", "mp2"],
        "mpegts" | "rtp_mpegts" | "hls" => &["aac", "mp3", "mp2", "ac3", "eac3"],
        _ => &["aac"],
    }
}

pub(crate) fn audio_stream_can_be_copied(
    output_format: &str,
    source_family: InputSourceFamily,
    stream: &InputAudioStream,
) -> bool {
    format_supports_audio_codec_copy(output_format, stream.codec_name.as_deref())
        && !requires_audio_reencode_for_stream(output_format, source_family, stream)
}

pub(crate) fn selected_audio_stream_parameters_available(stream: &InputAudioStream) -> bool {
    matches!(stream.sample_rate, Some(value) if value > 0)
        && matches!(stream.channels, Some(value) if value > 0)
}

pub(crate) fn requires_audio_reencode_for_stream(
    output_format: &str,
    source_family: InputSourceFamily,
    stream: &InputAudioStream,
) -> bool {
    let Some(audio_codec_name) = stream.codec_name.as_deref() else {
        return false;
    };

    if is_rtsp_output_profile(output_format)
        && audio_codec_name == "aac"
        && matches!(
            source_family,
            InputSourceFamily::MpegTs | InputSourceFamily::Hls
        )
        && !stream.extradata_present
    {
        return true;
    }

    is_flv_output_profile(output_format)
        && audio_codec_name == "mp3"
        && !matches!(stream.sample_rate, Some(44_100 | 22_050 | 11_025))
}

pub(crate) fn should_force_h264_nvenc_to_yuv420p(
    profile: &InputMediaProfile,
    process_args: &[String],
) -> bool {
    // 老显卡/旧驱动对 h264_nvenc 输出 10bit/高位深格式兼容性差，强制降到 yuv420p。
    if !process_args
        .windows(2)
        .any(|window| window == ["-c:v", "h264_nvenc"])
    {
        return false;
    }

    profile
        .video_pixel_format
        .as_deref()
        .is_some_and(video_pixel_format_requires_h264_nvenc_8bit_compatibility)
}

fn video_pixel_format_requires_h264_nvenc_8bit_compatibility(pix_fmt: &str) -> bool {
    let pix_fmt = pix_fmt.trim().to_ascii_lowercase();
    if pix_fmt.is_empty() {
        return false;
    }

    matches!(
        pix_fmt.as_str(),
        "p010le"
            | "p012le"
            | "p016le"
            | "yuv420p9le"
            | "yuv420p10le"
            | "yuv420p12le"
            | "yuv420p14le"
            | "yuv420p16le"
            | "yuv422p10le"
            | "yuv422p12le"
            | "yuv422p14le"
            | "yuv422p16le"
            | "yuv444p10le"
            | "yuv444p12le"
            | "yuv444p14le"
            | "yuv444p16le"
            | "gbrp10le"
            | "gbrp12le"
            | "gbrp14le"
            | "gbrp16le"
            | "yuva420p10le"
            | "yuva420p12le"
            | "yuva420p16le"
            | "yuva444p10le"
            | "yuva444p12le"
            | "yuva444p16le"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio_stream(index: u32, codec: &str, sample_rate: Option<u32>) -> InputAudioStream {
        InputAudioStream {
            index,
            codec_name: Some(codec.to_string()),
            sample_rate,
            channels: Some(2),
            extradata_present: true,
        }
    }

    #[test]
    fn matroska_descriptor_maps_mkv_alias_to_real_muxer_and_extension() {
        let descriptor = output_format_descriptor("mkv").expect("descriptor should exist");

        assert_eq!(descriptor.logical_format, "matroska");
        assert_eq!(descriptor.ffmpeg_muxer, "matroska");
        assert_eq!(descriptor.extension, "mkv");
    }

    #[test]
    fn webm_output_is_disabled_without_defining_output_descriptor() {
        assert_eq!(
            disabled_output_format_message("webm"),
            Some(WEBM_OUTPUT_DISABLED_MESSAGE)
        );
        assert!(output_format_descriptor("webm").is_none());
    }

    #[test]
    fn multi_audio_selection_prefers_copy_safe_mp4_stream() {
        let profile = InputMediaProfile {
            has_audio: true,
            audio_streams: vec![
                audio_stream(1, "opus", Some(48_000)),
                audio_stream(2, "aac", Some(48_000)),
            ],
            source_family: InputSourceFamily::Matroska,
            ..InputMediaProfile::default()
        };

        let selected = select_audio_stream_for_output(
            "mp4",
            &profile,
            AudioOutputPolicy::CopyWhitelistedElseAac,
            false,
        )
        .expect("audio stream should be selected");

        assert_eq!(selected.index, 2);
        assert_eq!(selected.codec_name.as_deref(), Some("aac"));
    }

    #[test]
    fn flv_rejects_mp3_copy_when_sample_rate_is_not_flv_safe() {
        let stream = audio_stream(1, "mp3", Some(48_000));

        assert!(requires_audio_reencode_for_stream(
            "flv",
            InputSourceFamily::Mp4Mov,
            &stream
        ));
        assert!(!audio_stream_can_be_copied(
            "flv",
            InputSourceFamily::Mp4Mov,
            &stream
        ));
    }

    #[test]
    fn ts_aac_copy_decoration_applies_only_to_flv_mp4_mov_outputs() {
        let decoration = resolve_audio_copy_decoration(InputSourceFamily::MpegTs, Some("aac"))
            .expect("decoration should exist");

        assert_eq!(
            decoration.filter_for_output("mp4"),
            Some(AudioBitstreamFilter::AacAdtsToAsc)
        );
        assert_eq!(decoration.filter_for_output("hls"), None);
        assert_eq!(decoration.filter_for_output("matroska"), None);
    }
}
