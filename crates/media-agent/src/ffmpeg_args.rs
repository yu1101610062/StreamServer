use std::path::Path;

use crate::media_policy::AudioBitstreamFilter;

pub(crate) fn ffmpeg_base_args_without_maps(input_url: String, realtime: bool) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-nostdin".to_string(),
        "-y".to_string(),
        "-loglevel".to_string(),
        "info".to_string(),
        "-stats_period".to_string(),
        "1".to_string(),
        "-progress".to_string(),
        "pipe:1".to_string(),
    ];
    if realtime {
        args.push("-re".to_string());
    }
    args.extend(["-i".to_string(), input_url]);
    args
}

pub(crate) fn ffmpeg_base_args(input_url: String, realtime: bool) -> Vec<String> {
    let mut args = ffmpeg_base_args_without_maps(input_url, realtime);
    args.extend([
        "-map".to_string(),
        "0:v?".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
    ]);
    args
}

pub(crate) fn insert_ffmpeg_input_args(args: &mut Vec<String>, extra_args: Vec<String>) {
    if extra_args.is_empty() {
        return;
    }
    let input_index = args
        .iter()
        .position(|arg| arg == "-i")
        .expect("ffmpeg args should always include an input marker");
    if args[..input_index]
        .windows(extra_args.len())
        .any(|window| window == extra_args.as_slice())
    {
        return;
    }
    args.splice(input_index..input_index, extra_args);
}

pub(crate) fn append_audio_bitstream_filter_arg(
    args: &mut Vec<String>,
    filter: AudioBitstreamFilter,
) {
    args.extend(["-bsf:a".to_string(), filter.as_ffmpeg_name().to_string()]);
}

pub(crate) fn append_output_target(
    args: &mut Vec<String>,
    output_args: &[String],
    muxer: &str,
    target: &str,
) {
    args.extend(output_args.iter().cloned());
    args.extend(["-f".to_string(), muxer.to_string(), target.to_string()]);
}

pub(crate) fn hls_segment_template(playlist_path: &str) -> String {
    let path = Path::new(playlist_path);
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("segment");
    parent
        .join(format!("{stem}-%05d.ts"))
        .to_string_lossy()
        .to_string()
}

pub(crate) fn hls_output_args(playlist_path: &str, segment_sec: u32) -> Vec<String> {
    vec![
        "-hls_time".to_string(),
        segment_sec.to_string(),
        "-hls_list_size".to_string(),
        "0".to_string(),
        "-hls_segment_filename".to_string(),
        hls_segment_template(playlist_path),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hls_segment_template_uses_playlist_stem_and_parent() {
        assert_eq!(
            hls_segment_template("/data/out/live.m3u8"),
            "/data/out/live-%05d.ts"
        );
    }

    #[test]
    fn insert_ffmpeg_input_args_dedupes_same_prefix_before_input() {
        let mut args = ffmpeg_base_args_without_maps("input.ts".to_string(), false);
        let extra = vec!["-probesize".to_string(), "10485760".to_string()];

        insert_ffmpeg_input_args(&mut args, extra.clone());
        insert_ffmpeg_input_args(&mut args, extra);

        assert_eq!(
            args.windows(2)
                .filter(|window| *window == ["-probesize", "10485760"])
                .count(),
            1
        );
    }

    #[test]
    fn https_input_does_not_enable_ffmpeg_certificate_verification_flags() {
        let args = ffmpeg_base_args(
            "https://172.21.26.25/bohui/media/relay/test".to_string(),
            true,
        );
        for forbidden in ["-tls_verify", "-ca_file", "-verifyhost"] {
            assert!(
                !args.iter().any(|arg| arg == forbidden),
                "unexpected FFmpeg TLS verification flag {forbidden}"
            );
        }
    }
}
