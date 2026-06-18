//! 运行时计划构造：负责把 TaskSpec 转成 FFmpeg/ZLM 执行计划，并准备计划相关目录和参数。

use std::{
    fs,
    path::{Path, PathBuf},
};

use media_domain::{
    ExposeSpec, InputKind, PublishSpec, PublishTargetKind, SourceMode, StreamIngestRecordMode,
    TaskSpec, TaskType,
};
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    ffmpeg_args::{
        append_audio_bitstream_filter_arg, ffmpeg_base_args, ffmpeg_base_args_without_maps,
        hls_segment_template, insert_ffmpeg_input_args,
    },
    ffmpeg_plan::{PublishOutput, append_publish_output_args},
    ffmpeg_probe::{probe_input_media_profile, probe_input_media_profile_with_input_args},
    media_policy::{
        AudioOutputPolicy, VideoOutputPolicy, ffmpeg_muxer_for_format,
        logical_output_format_for_format,
    },
    runtime::{
        ExecutorError, RuntimeCapabilityHints, StartTaskRequest, StartupProbe, SuccessCheck,
    },
    runtime_io::{
        attempt_work_dir, bool_as_flag, build_input_url, build_multicast_url,
        input_timeout_seconds, required_nonempty, resolve_interface_binding_ip,
    },
    runtime_metadata::RtpServerMetadata,
    runtime_metadata::live_relay_auto_close_enabled,
    runtime_outputs::{
        ManagedFileOutputKind, allocate_managed_file_output, allocate_managed_output,
        ensure_output_format_enabled, hls_record_segment_sec, managed_file_output_kind_for_task,
    },
    runtime_process::RuntimeSlotClass,
    runtime_recording::{LiveRelayRecording, build_live_relay_recording},
    runtime_transcode::{
        InternalIngressProtocol, append_live_mpegts_multicast_bridge_args, append_process_args,
        append_process_args_with_profile, append_single_audio_output_maps,
        build_internal_stream_output, resolve_stream_ingest_audio_copy_probe_input_args,
        select_internal_ingress_protocol, should_loop_file_to_live_input,
        should_stabilize_live_mpegts_multicast_bridge, stream_ingest_probe_input_args,
    },
    runtime_zlm::ZLM_RUNTIME_VHOST,
};

#[derive(Debug, Clone)]
pub(crate) struct ProcessPlan {
    pub(crate) executable: String,
    pub(crate) args: Vec<String>,
    pub(crate) work_dir: PathBuf,
    pub(crate) output_target: String,
    pub(crate) outputs: Vec<String>,
    pub(crate) success_check: SuccessCheck,
    pub(crate) startup_probe: Option<StartupProbe>,
    pub(crate) recording: Option<LiveRelayRecording>,
    pub(crate) managed_file_output_kind: Option<ManagedFileOutputKind>,
    pub(crate) companion_recording: Option<CompanionProcessPlan>,
    pub(crate) internal_ingress_protocol: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct CompanionProcessPlan {
    pub(crate) executable: String,
    pub(crate) args: Vec<String>,
    pub(crate) work_dir: PathBuf,
    pub(crate) output_target: String,
    pub(crate) outputs: Vec<String>,
    pub(crate) success_check: SuccessCheck,
    pub(crate) kind: crate::runtime::CompanionProcessKind,
}

#[derive(Debug, Clone)]
pub(crate) struct LiveRelayPlan {
    pub(crate) work_dir: PathBuf,
    pub(crate) input_url: String,
    pub(crate) command_line: String,
    pub(crate) outputs: Vec<String>,
    pub(crate) startup_probe: StartupProbe,
    pub(crate) recording: Option<LiveRelayRecording>,
}

#[derive(Debug, Clone)]
pub(crate) struct RtpReceivePlan {
    pub(crate) work_dir: PathBuf,
    pub(crate) stream_id: String,
    pub(crate) requested_port: u16,
    pub(crate) tcp_mode: u8,
    pub(crate) reuse_port: Option<bool>,
    pub(crate) ssrc: Option<u32>,
    pub(crate) command_line: String,
    pub(crate) outputs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskRuntimeMode {
    ManagedProcess,
    ZlmProxy,
    ZlmRtpServer,
}

pub(crate) fn build_process_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    capability_hints: RuntimeCapabilityHints,
) -> Result<ProcessPlan, ExecutorError> {
    let spec = parse_task_spec(request)?;

    match request.task_type {
        TaskType::FileTranscode => build_file_transcode_plan(settings, request, &spec),
        TaskType::StreamBridge => build_multicast_bridge_plan(settings, request, &spec),
        TaskType::StreamIngest => {
            if task_runtime_mode(&spec) != TaskRuntimeMode::ManagedProcess {
                return Err(ExecutorError::InvalidRequest(
                    "stream_ingest task should not run in the managed process executor".to_string(),
                ));
            }
            build_stream_ingest_plan_with_capability_hints(
                settings,
                request,
                &spec,
                capability_hints,
            )
        }
    }
}

pub(crate) fn build_stream_ingest_plan_with_capability_hints(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
    capability_hints: RuntimeCapabilityHints,
) -> Result<ProcessPlan, ExecutorError> {
    match spec.stream_ingest_record_mode() {
        Some(StreamIngestRecordMode::Fast) => {
            build_stream_ingest_fast_record_plan(settings, request, spec)
        }
        _ => build_stream_ingest_realtime_plan(settings, request, spec, capability_hints),
    }
}

pub(crate) fn parse_task_spec(request: &StartTaskRequest) -> Result<TaskSpec, ExecutorError> {
    serde_json::from_value(request.resolved_spec.clone()).map_err(|error| {
        ExecutorError::InvalidRequest(format!("invalid resolved_spec for task execution: {error}"))
    })
}

pub(crate) fn runtime_slot_class_for_request(
    request: &StartTaskRequest,
) -> Result<RuntimeSlotClass, ExecutorError> {
    let spec = parse_task_spec(request)?;
    let source_mode = spec.input.source_mode.ok_or_else(|| {
        ExecutorError::InvalidRequest("resolved_spec.input.source_mode is required".to_string())
    })?;
    Ok(RuntimeSlotClass::from_source_mode(source_mode))
}

pub(crate) fn task_runtime_mode(spec: &TaskSpec) -> TaskRuntimeMode {
    match spec.task_type {
        TaskType::FileTranscode | TaskType::StreamBridge => TaskRuntimeMode::ManagedProcess,
        TaskType::StreamIngest => match (spec.input.kind, spec.input.source_mode) {
            (Some(InputKind::GbRtp), _) => TaskRuntimeMode::ZlmRtpServer,
            (Some(InputKind::Rtsp | InputKind::Rtmp | InputKind::HttpFlv), _) => {
                if should_use_managed_process_for_record_only_live_ingest(spec) {
                    TaskRuntimeMode::ManagedProcess
                } else {
                    TaskRuntimeMode::ZlmProxy
                }
            }
            (Some(InputKind::Hls | InputKind::HttpTs), Some(SourceMode::Live)) => {
                if should_use_managed_process_for_record_only_live_ingest(spec) {
                    TaskRuntimeMode::ManagedProcess
                } else {
                    TaskRuntimeMode::ZlmProxy
                }
            }
            _ => TaskRuntimeMode::ManagedProcess,
        },
    }
}

fn should_use_managed_process_for_record_only_live_ingest(spec: &TaskSpec) -> bool {
    spec.task_type == TaskType::StreamIngest
        && spec.input.source_mode == Some(SourceMode::Live)
        && spec.record.enabled.unwrap_or(false)
        && !spec.expose.any_playback_enabled()
}

pub(crate) fn build_file_transcode_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    let input_url = build_input_url(settings, &spec.input)?;

    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let output = match spec.publish.kind {
        Some(PublishTargetKind::File) => {
            allocate_managed_file_output(settings, request.task_id, &spec.publish)?
        }
        Some(_) => {
            return Err(ExecutorError::InvalidRequest(
                "file_transcode requires publish.kind=file".to_string(),
            ));
        }
        None => {
            return Err(ExecutorError::InvalidRequest(
                "file_transcode requires publish.kind".to_string(),
            ));
        }
    };
    let profile = probe_input_media_profile(settings, spec, input_url.as_str());
    let mut args = ffmpeg_base_args_without_maps(input_url.clone(), false);
    let audio_copy_decoration = append_process_args_with_profile(
        &mut args,
        settings,
        spec,
        "copy_or_transcode",
        input_url.as_str(),
        output.format.as_str(),
        VideoOutputPolicy::KeepSourceFamily,
        AudioOutputPolicy::Aac,
        Some(&profile),
    )?;
    append_single_audio_output_maps(
        &mut args,
        spec,
        output.format.as_str(),
        &profile,
        AudioOutputPolicy::Aac,
    );
    if let Some(filter) =
        audio_copy_decoration.and_then(|value| value.filter_for_output(output.format.as_str()))
    {
        append_audio_bitstream_filter_arg(&mut args, filter);
    }

    args.extend(["-threads".to_string(), "0".to_string()]);
    append_publish_output_args(&mut args, &output);

    Ok(ProcessPlan {
        executable: settings.ffmpeg_bin.clone(),
        args,
        work_dir,
        output_target: output.target.clone(),
        outputs: vec![output.target.clone()],
        success_check: output.success_check,
        startup_probe: None,
        recording: None,
        managed_file_output_kind: Some(ManagedFileOutputKind::Transcode),
        companion_recording: None,
        internal_ingress_protocol: None,
    })
}

pub(crate) fn build_stream_ingest_realtime_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
    capability_hints: RuntimeCapabilityHints,
) -> Result<ProcessPlan, ExecutorError> {
    // 实时接入会先推到本机 ZLM，再由 ZLM 负责 RTSP/RTMP/HTTP 等对外播放。
    let input_url = build_input_url(settings, &spec.input)?;
    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let probe_input_args = stream_ingest_probe_input_args(spec, input_url.as_str());
    let profile = probe_input_media_profile_with_input_args(
        settings,
        spec,
        input_url.as_str(),
        &probe_input_args,
    );
    let ingress_protocol = select_internal_ingress_protocol(settings, &profile, capability_hints);
    let startup_probe =
        build_managed_stream_ingest_startup_probe(request.task_id, spec, ingress_protocol)?;
    let publish_output = build_internal_stream_output(settings, &startup_probe, ingress_protocol);
    let mut outputs = vec![publish_output.target.clone()];
    let success_check = publish_output.success_check.clone();
    let mut recording = None;
    let managed_file_output_kind = None;
    let process_output_format = ingress_protocol.compatibility_output_format();

    let mut args = ffmpeg_base_args_without_maps(
        input_url.clone(),
        spec.stream_ingest_requires_realtime_pacing(),
    );
    let stream_ingest_audio_copy_probe_args = resolve_stream_ingest_audio_copy_probe_input_args(
        spec,
        process_output_format,
        &profile,
        AudioOutputPolicy::CopyWhitelistedElseAac,
    )?;
    // TS/HLS 中的 AAC 复制到 FLV/MP4 前需要确认参数完整，否则宁可拒绝也不生成坏流。
    if !stream_ingest_audio_copy_probe_args.is_empty() {
        insert_ffmpeg_input_args(&mut args, stream_ingest_audio_copy_probe_args);
    }
    if should_loop_file_to_live_input(spec) {
        insert_ffmpeg_input_args(
            &mut args,
            vec!["-stream_loop".to_string(), "-1".to_string()],
        );
    }
    if spec.input.source_mode != Some(SourceMode::Vod) {
        let mut input_args = vec![
            "-thread_queue_size".to_string(),
            "1024".to_string(),
            "-use_wallclock_as_timestamps".to_string(),
            "1".to_string(),
            "-fflags".to_string(),
            "+genpts+discardcorrupt".to_string(),
            "-err_detect".to_string(),
            "ignore_err".to_string(),
        ];
        if matches!(spec.input.kind, Some(InputKind::UdpMpegtsMulticast)) {
            input_args.extend(["-max_delay".to_string(), "500000".to_string()]);
        }
        insert_ffmpeg_input_args(&mut args, input_args);
    }
    let audio_copy_decoration = append_process_args_with_profile(
        &mut args,
        settings,
        spec,
        "copy_or_transcode",
        input_url.as_str(),
        process_output_format,
        VideoOutputPolicy::CopyWhitelistedElseH264,
        AudioOutputPolicy::CopyWhitelistedElseAac,
        Some(&profile),
    )?;
    if !matches!(ingress_protocol, InternalIngressProtocol::Rtmp) {
        args.extend(["-threads".to_string(), "0".to_string()]);
    }
    if !spec.stream_ingest_uses_wall_clock_record_duration() {
        if let Some(duration_sec) = spec.record.duration_sec {
            args.extend(["-t".to_string(), duration_sec.to_string()]);
        }
    }

    if let Some(filter) = audio_copy_decoration
        .and_then(|value| value.filter_for_output(publish_output.format.as_str()))
    {
        append_audio_bitstream_filter_arg(&mut args, filter);
    }
    append_single_audio_output_maps(
        &mut args,
        spec,
        process_output_format,
        &profile,
        AudioOutputPolicy::CopyWhitelistedElseAac,
    );
    append_publish_output_args(&mut args, &publish_output);

    if spec.record.enabled.unwrap_or(false) {
        recording = build_live_relay_recording(settings, request.task_id, spec)?;
        if let Some(recording_plan) = &recording {
            outputs.extend(recording_plan.all_root_paths());
        }
    }

    Ok(ProcessPlan {
        executable: settings.ffmpeg_bin.clone(),
        args,
        work_dir,
        output_target: publish_output.target,
        outputs,
        success_check,
        startup_probe: Some(startup_probe),
        recording,
        managed_file_output_kind,
        companion_recording: None,
        internal_ingress_protocol: Some(ingress_protocol.metadata_value().to_string()),
    })
}

pub(crate) fn build_stream_ingest_fast_record_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    // 快速录像不经过 ZLM 内部播放链路，直接把 VOD 输入写成托管文件产物。
    let input_url = build_input_url(settings, &spec.input)?;
    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let mut args = ffmpeg_base_args_without_maps(input_url.clone(), false);
    let preferred_output_format = match spec
        .record
        .format
        .unwrap_or(media_domain::RecordFormat::Mp4)
    {
        media_domain::RecordFormat::Mp4 | media_domain::RecordFormat::Both => "mp4",
        media_domain::RecordFormat::Hls => "hls",
    };
    let probe_input_args = stream_ingest_probe_input_args(spec, input_url.as_str());
    let profile = probe_input_media_profile_with_input_args(
        settings,
        spec,
        input_url.as_str(),
        &probe_input_args,
    );
    let stream_ingest_audio_copy_probe_args = resolve_stream_ingest_audio_copy_probe_input_args(
        spec,
        preferred_output_format,
        &profile,
        AudioOutputPolicy::CopyWhitelistedElseAac,
    )?;
    if !stream_ingest_audio_copy_probe_args.is_empty() {
        insert_ffmpeg_input_args(&mut args, stream_ingest_audio_copy_probe_args);
    }
    if should_loop_file_to_live_input(spec) {
        insert_ffmpeg_input_args(
            &mut args,
            vec!["-stream_loop".to_string(), "-1".to_string()],
        );
    }
    let audio_copy_decoration = append_process_args_with_profile(
        &mut args,
        settings,
        spec,
        "copy_or_transcode",
        input_url.as_str(),
        preferred_output_format,
        VideoOutputPolicy::CopyWhitelistedElseH264,
        AudioOutputPolicy::CopyWhitelistedElseAac,
        Some(&profile),
    )?;
    args.extend(["-threads".to_string(), "0".to_string()]);
    if let Some(duration_sec) = spec.record.duration_sec {
        args.extend(["-t".to_string(), duration_sec.to_string()]);
    }

    let mut outputs = Vec::new();
    let (primary_output, success_check) = match spec
        .record
        .format
        .unwrap_or(media_domain::RecordFormat::Mp4)
    {
        media_domain::RecordFormat::Mp4 => {
            let output = allocate_managed_output(settings, request.task_id, Some("mp4"))?;
            append_single_audio_output_maps(
                &mut args,
                spec,
                output.format.as_str(),
                &profile,
                AudioOutputPolicy::CopyWhitelistedElseAac,
            );
            if let Some(filter) = audio_copy_decoration
                .and_then(|value| value.filter_for_output(output.format.as_str()))
            {
                append_audio_bitstream_filter_arg(&mut args, filter);
            }
            args.extend([
                "-f".to_string(),
                output.format.clone(),
                output.target.clone(),
            ]);
            outputs.push(output.target.clone());
            (output.clone(), output.success_check)
        }
        media_domain::RecordFormat::Hls => {
            let output = allocate_managed_output(settings, request.task_id, Some("hls"))?;
            let segment_template = hls_segment_template(output.target.as_str());
            append_single_audio_output_maps(
                &mut args,
                spec,
                output.format.as_str(),
                &profile,
                AudioOutputPolicy::CopyWhitelistedElseAac,
            );
            args.extend([
                "-f".to_string(),
                "hls".to_string(),
                "-hls_time".to_string(),
                hls_record_segment_sec(settings, spec).to_string(),
                "-hls_list_size".to_string(),
                "0".to_string(),
                "-hls_segment_filename".to_string(),
                segment_template,
                output.target.clone(),
            ]);
            outputs.push(output.target.clone());
            (output.clone(), output.success_check)
        }
        media_domain::RecordFormat::Both => {
            let mp4_output = allocate_managed_output(settings, request.task_id, Some("mp4"))?;
            let hls_output = allocate_managed_output(settings, request.task_id, Some("hls"))?;
            let segment_template = hls_segment_template(hls_output.target.as_str());
            append_single_audio_output_maps(
                &mut args,
                spec,
                mp4_output.format.as_str(),
                &profile,
                AudioOutputPolicy::CopyWhitelistedElseAac,
            );
            if let Some(filter) = audio_copy_decoration
                .and_then(|value| value.filter_for_output(mp4_output.format.as_str()))
            {
                append_audio_bitstream_filter_arg(&mut args, filter);
            }
            args.extend([
                "-f".to_string(),
                "mp4".to_string(),
                mp4_output.target.clone(),
            ]);
            let hls_audio_copy_decoration = append_process_args_with_profile(
                &mut args,
                settings,
                spec,
                "copy_or_transcode",
                input_url.as_str(),
                hls_output.format.as_str(),
                VideoOutputPolicy::CopyWhitelistedElseH264,
                AudioOutputPolicy::CopyWhitelistedElseAac,
                Some(&profile),
            )?;
            args.extend(["-threads".to_string(), "0".to_string()]);
            if let Some(duration_sec) = spec.record.duration_sec {
                args.extend(["-t".to_string(), duration_sec.to_string()]);
            }
            append_single_audio_output_maps(
                &mut args,
                spec,
                hls_output.format.as_str(),
                &profile,
                AudioOutputPolicy::CopyWhitelistedElseAac,
            );
            if let Some(filter) = hls_audio_copy_decoration
                .and_then(|value| value.filter_for_output(hls_output.format.as_str()))
            {
                append_audio_bitstream_filter_arg(&mut args, filter);
            }
            args.extend([
                "-f".to_string(),
                "hls".to_string(),
                "-hls_time".to_string(),
                hls_record_segment_sec(settings, spec).to_string(),
                "-hls_list_size".to_string(),
                "0".to_string(),
                "-hls_segment_filename".to_string(),
                segment_template,
                hls_output.target.clone(),
            ]);
            outputs.push(mp4_output.target.clone());
            outputs.push(hls_output.target.clone());
            (
                mp4_output,
                SuccessCheck::FilesExist(vec![
                    PathBuf::from(&outputs[0]),
                    PathBuf::from(&outputs[1]),
                ]),
            )
        }
    };

    Ok(ProcessPlan {
        executable: settings.ffmpeg_bin.clone(),
        args,
        work_dir,
        output_target: primary_output.target.clone(),
        outputs,
        success_check,
        startup_probe: None,
        recording: None,
        managed_file_output_kind: Some(ManagedFileOutputKind::StreamIngestRecord),
        companion_recording: None,
        internal_ingress_protocol: None,
    })
}

pub(crate) fn build_multicast_bridge_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    let input_url = build_input_url(settings, &spec.input)?;
    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let output = build_publish_output(settings, request.task_id, spec.task_type, &spec.publish)?;
    let startup_probe = None;
    let realtime = spec.input.source_mode == Some(SourceMode::Vod)
        && matches!(
            spec.publish.kind,
            Some(
                PublishTargetKind::UdpMpegtsMulticast
                    | PublishTargetKind::RtpMulticast
                    | PublishTargetKind::RtmpPush
            )
        );
    let mut args = ffmpeg_base_args(input_url.clone(), realtime);
    if spec.input.source_mode != Some(SourceMode::Vod) {
        insert_ffmpeg_input_args(
            &mut args,
            vec![
                "-use_wallclock_as_timestamps".to_string(),
                "1".to_string(),
                "-fflags".to_string(),
                "+genpts".to_string(),
            ],
        );
    }
    if should_stabilize_live_mpegts_multicast_bridge(spec, &output) {
        // ZLM 发布的实时源直接复制到 MPEG-TS 时可能出现 DTS 缺失或不单调。
        // 这里只重编码视频以重建时间戳，音频仍尽量复制，保持接近透传的成本。
        append_live_mpegts_multicast_bridge_args(&mut args, settings, spec, input_url.as_str());
    } else {
        let audio_copy_decoration = append_process_args(
            &mut args,
            settings,
            spec,
            "passthrough",
            input_url.as_str(),
            output.format.as_str(),
            VideoOutputPolicy::ForceH264,
            AudioOutputPolicy::Aac,
        )?;
        if let Some(filter) =
            audio_copy_decoration.and_then(|value| value.filter_for_output(output.format.as_str()))
        {
            append_audio_bitstream_filter_arg(&mut args, filter);
        }
    }
    args.extend(["-threads".to_string(), "0".to_string()]);
    append_publish_output_args(&mut args, &output);

    Ok(ProcessPlan {
        executable: settings.ffmpeg_bin.clone(),
        args,
        work_dir,
        output_target: output.target.clone(),
        outputs: vec![output.target],
        success_check: output.success_check,
        startup_probe,
        recording: None,
        managed_file_output_kind: Some(ManagedFileOutputKind::Bridge),
        companion_recording: None,
        internal_ingress_protocol: None,
    })
}

pub(crate) fn build_live_relay_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<LiveRelayPlan, ExecutorError> {
    let input_url = required_nonempty("input.url", spec.input.url.as_deref())?;
    let startup_probe = build_startup_probe(request.task_id, spec)?;
    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let recording = build_live_relay_recording(settings, request.task_id, spec)?;
    let command_line = format!(
        "zlm addStreamProxy --url {} --vhost {} --app {} --stream {}",
        input_url, startup_probe.vhost, startup_probe.app, startup_probe.stream
    );
    let mut outputs = vec![format!(
        "zlm://{}/{}/{}",
        startup_probe.vhost, startup_probe.app, startup_probe.stream
    )];
    if let Some(recording) = &recording {
        outputs.extend(recording.all_root_paths());
    }

    Ok(LiveRelayPlan {
        work_dir,
        input_url,
        command_line,
        outputs,
        startup_probe,
        recording,
    })
}

pub(crate) fn build_rtp_receive_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<RtpReceivePlan, ExecutorError> {
    if spec.task_type != TaskType::StreamIngest || spec.input.kind != Some(InputKind::GbRtp) {
        return Err(ExecutorError::InvalidRequest(
            "stream_ingest rtp mode requires input.kind=gb_rtp".to_string(),
        ));
    }
    let requested_port = spec
        .input
        .port
        .ok_or_else(|| ExecutorError::InvalidRequest("input.port must be provided".to_string()))?;
    let tcp_mode = spec.input.tcp_mode.unwrap_or(0);
    if tcp_mode > 2 {
        return Err(ExecutorError::InvalidRequest(
            "input.tcp_mode must be one of 0 (udp), 1 (tcp_passive), 2 (tcp_active)".to_string(),
        ));
    }
    let reuse_port = spec.input.reuse;
    let ssrc = spec.input.ssrc;

    let stream_id = build_rtp_stream_id(request.task_id, request.attempt_no);
    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let mut command_line = format!(
        "zlm openRtpServer --port {} --tcp_mode {} --stream_id {}",
        requested_port, tcp_mode, stream_id
    );
    if let Some(reuse_port) = reuse_port {
        command_line.push_str(&format!(
            " --re_use_port {}",
            if reuse_port { 1 } else { 0 }
        ));
    }
    if let Some(ssrc) = ssrc {
        command_line.push_str(&format!(" --ssrc {ssrc}"));
    }
    Ok(RtpReceivePlan {
        work_dir,
        stream_id: stream_id.clone(),
        requested_port,
        tcp_mode,
        reuse_port,
        ssrc,
        command_line,
        outputs: vec![format!("rtp_receive://{stream_id}")],
    })
}

pub(crate) fn prepare_work_dir(work_dir: &Path) -> Result<(), ExecutorError> {
    fs::create_dir_all(work_dir).map_err(|error| {
        ExecutorError::ProcessSpawn(format!(
            "failed to prepare work dir {}: {error}",
            work_dir.display()
        ))
    })
}

fn prepare_success_check_paths(success_check: &SuccessCheck) -> Result<(), ExecutorError> {
    let paths: Vec<&PathBuf> = match success_check {
        SuccessCheck::FileExists(path) => vec![path],
        SuccessCheck::FilesExist(paths) => paths.iter().collect(),
        SuccessCheck::ProcessExit => Vec::new(),
    };

    for path in paths {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|error| {
                ExecutorError::ProcessSpawn(format!(
                    "failed to prepare output dir {}: {error}",
                    parent.display()
                ))
            })?;
        }
    }

    Ok(())
}

pub(crate) fn prepare_plan_paths(plan: &ProcessPlan) -> Result<(), ExecutorError> {
    prepare_work_dir(&plan.work_dir)?;
    prepare_success_check_paths(&plan.success_check)?;

    if let Some(recording) = &plan.recording {
        for root_path in recording.all_root_paths() {
            fs::create_dir_all(&root_path).map_err(|error| {
                ExecutorError::ProcessSpawn(format!(
                    "failed to prepare recording root {}: {error}",
                    root_path
                ))
            })?;
        }
    }

    if let Some(companion) = &plan.companion_recording {
        prepare_success_check_paths(&companion.success_check)?;
    }

    Ok(())
}

fn build_publish_output(
    settings: &AgentSettings,
    task_id: Uuid,
    task_type: TaskType,
    publish: &PublishSpec,
) -> Result<PublishOutput, ExecutorError> {
    match publish.kind {
        Some(PublishTargetKind::File) => managed_file_output_kind_for_task(task_type)
            .ok_or_else(|| {
                ExecutorError::InvalidRequest(
                    "publish.kind=file is only supported for managed file output tasks".to_string(),
                )
            })
            .and_then(|_kind| allocate_managed_file_output(settings, task_id, publish)),
        Some(PublishTargetKind::UdpMpegtsMulticast | PublishTargetKind::RtpMulticast) => {
            let target = build_multicast_url(
                match publish.kind.expect("kind checked") {
                    PublishTargetKind::UdpMpegtsMulticast => InputKind::UdpMpegtsMulticast,
                    PublishTargetKind::RtpMulticast => InputKind::RtpMulticast,
                    _ => unreachable!(),
                },
                publish.group.as_deref(),
                publish.port,
                resolve_interface_binding_ip(
                    publish.interface_name.as_deref(),
                    publish.interface_ip.as_deref(),
                    Some(settings.multicast_interface_name.as_str()),
                    Some(settings.multicast_interface_ip.as_str()),
                    "publish",
                    false,
                )?
                .as_deref(),
                publish.ttl,
                publish.reuse,
                publish.pkt_size,
                publish.dscp,
                publish.buffer_size,
                publish.fifo_size,
                false,
                "publish",
            )?;
            let format = publish
                .format
                .clone()
                .unwrap_or_else(|| match publish.kind {
                    Some(PublishTargetKind::RtpMulticast) => "rtp_mpegts".to_string(),
                    _ => "mpegts".to_string(),
                });
            ensure_output_format_enabled(&format)?;
            let format = logical_output_format_for_format(&format);
            let muxer = ffmpeg_muxer_for_format(&format);
            Ok(PublishOutput {
                success_check: SuccessCheck::ProcessExit,
                target,
                format,
                muxer,
                output_args: Vec::new(),
            })
        }
        Some(PublishTargetKind::RtmpPush) => Ok(PublishOutput {
            success_check: SuccessCheck::ProcessExit,
            target: required_nonempty("publish.url", publish.url.as_deref())?,
            format: "flv".to_string(),
            muxer: "flv".to_string(),
            output_args: Vec::new(),
        }),
        None => Err(ExecutorError::InvalidRequest(
            "publish.kind must be provided".to_string(),
        )),
    }
}

pub(crate) fn build_live_relay_api_params(
    settings: &AgentSettings,
    spec: &TaskSpec,
    startup_probe: &StartupProbe,
    input_url: &str,
) -> Vec<(String, String)> {
    let mut params = vec![
        ("vhost".to_string(), startup_probe.vhost.clone()),
        ("app".to_string(), startup_probe.app.clone()),
        ("stream".to_string(), startup_probe.stream.clone()),
        ("url".to_string(), input_url.to_string()),
        ("retry_count".to_string(), "-1".to_string()),
        (
            "timeout_sec".to_string(),
            input_timeout_seconds(spec.input.probe_timeout_ms).to_string(),
        ),
        ("enable_audio".to_string(), "1".to_string()),
        ("add_mute_audio".to_string(), "0".to_string()),
        ("modify_stamp".to_string(), "2".to_string()),
        (
            "enable_rtsp".to_string(),
            bool_as_flag(spec.expose.enable_rtsp.unwrap_or(true)),
        ),
        (
            "enable_rtmp".to_string(),
            bool_as_flag(spec.expose.enable_rtmp.unwrap_or(true)),
        ),
        (
            "enable_hls".to_string(),
            bool_as_flag(spec.expose.enable_hls.unwrap_or(false)),
        ),
        (
            "enable_ts".to_string(),
            bool_as_flag(spec.expose.enable_http_ts.unwrap_or(true)),
        ),
        (
            "enable_fmp4".to_string(),
            bool_as_flag(spec.expose.enable_http_fmp4.unwrap_or(true)),
        ),
        ("enable_mp4".to_string(), bool_as_flag(false)),
        (
            "auto_close".to_string(),
            bool_as_flag(live_relay_auto_close_enabled(settings, spec)),
        ),
    ];

    if matches!(spec.input.kind, Some(InputKind::Rtsp)) {
        params.push(("rtp_type".to_string(), "0".to_string()));
    }

    params
}

pub(crate) fn build_open_rtp_server_params(plan: &RtpReceivePlan) -> Vec<(String, String)> {
    let mut params = vec![
        ("port".to_string(), plan.requested_port.to_string()),
        ("tcp_mode".to_string(), plan.tcp_mode.to_string()),
        ("stream_id".to_string(), plan.stream_id.clone()),
    ];
    if let Some(reuse_port) = plan.reuse_port {
        params.push((
            "re_use_port".to_string(),
            if reuse_port { "1" } else { "0" }.to_string(),
        ));
    }
    if let Some(ssrc) = plan.ssrc {
        params.push(("ssrc".to_string(), ssrc.to_string()));
    }
    params
}

pub(crate) fn build_open_rtp_server_params_from_metadata(
    rtp_server: &RtpServerMetadata,
) -> Vec<(String, String)> {
    let mut params = vec![
        ("port".to_string(), rtp_server.requested_port.to_string()),
        ("tcp_mode".to_string(), rtp_server.tcp_mode.to_string()),
        ("stream_id".to_string(), rtp_server.stream_id.clone()),
    ];
    if let Some(reuse_port) = rtp_server.reuse_port {
        params.push((
            "re_use_port".to_string(),
            if reuse_port { "1" } else { "0" }.to_string(),
        ));
    }
    if let Some(ssrc) = rtp_server.ssrc {
        params.push(("ssrc".to_string(), ssrc.to_string()));
    }
    params
}

pub(crate) fn build_startup_probe(
    task_id: Uuid,
    spec: &TaskSpec,
) -> Result<StartupProbe, ExecutorError> {
    let app = spec
        .stream
        .app
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("live")
        .to_string();
    let stream = spec
        .stream
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| task_id.to_string());
    Ok(StartupProbe {
        schema: playback_probe_schema(&spec.expose),
        vhost: spec
            .stream
            .vhost
            .clone()
            .unwrap_or_else(|| ZLM_RUNTIME_VHOST.to_string()),
        app,
        stream,
    })
}

fn build_managed_stream_ingest_startup_probe(
    task_id: Uuid,
    spec: &TaskSpec,
    protocol: InternalIngressProtocol,
) -> Result<StartupProbe, ExecutorError> {
    let mut probe = build_startup_probe(task_id, spec)?;
    probe.schema = Some(protocol.schema().to_string());
    Ok(probe)
}

fn playback_probe_schema(expose: &ExposeSpec) -> Option<String> {
    expose
        .any_playback_enabled()
        .then(|| preferred_publish_schema(expose))
}

fn preferred_publish_schema(expose: &ExposeSpec) -> String {
    if expose.enable_rtmp.unwrap_or(true) {
        "rtmp".to_string()
    } else if expose.enable_rtsp.unwrap_or(true) {
        "rtsp".to_string()
    } else if expose.enable_http_ts.unwrap_or(true) {
        "ts".to_string()
    } else if expose.enable_http_fmp4.unwrap_or(true) {
        "fmp4".to_string()
    } else if expose.enable_hls.unwrap_or(false) {
        "hls".to_string()
    } else {
        "rtmp".to_string()
    }
}

fn build_rtp_stream_id(task_id: Uuid, attempt_no: i32) -> String {
    format!("{task_id}-{attempt_no}")
}
