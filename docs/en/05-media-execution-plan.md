# 05. Media Execution Plan

The Agent does not execute raw user-provided FFmpeg command strings. Every media task is rendered into an execution plan first.

## Plan Inputs

Execution planning uses:

- task type and resolved task spec;
- ffprobe input profile;
- target container and output type;
- publishing protocol;
- FFmpeg protocol/format/encoder/decoder capabilities;
- ZLMediaKit capability and configuration;
- node runtime settings such as CPU/GPU mode and output roots.

## Common Decisions

| Scenario | Decision |
| --- | --- |
| H.264 + AAC -> RTMP | copy video/audio and use FLV/RTMP |
| HEVC / AV1 / VP9 -> Live | prefer Enhanced RTMP and fall back to RTSP |
| MPEGTS/HLS AAC -> MP4/FLV | add `aac_adtstoasc` automatically |
| Multi-audio input | choose the best copy-safe audio stream |
| Incompatible audio | transcode to AAC/Opus or reject depending on target |
| HLS output | generate m3u8 and segment templates |
| Recording | support MP4, HLS, or dual-output recording |
| WebM | accepted as upload input, not exposed as an output target |

## Safety Rules

- Do not concatenate user-provided raw FFmpeg arguments.
- Validate input protocols, output muxers, encoders, and GPU requirements before starting a task.
- Keep rendered command lines and runtime metadata for recovery and diagnosis.
- Prefer stable fallback over optimistic codec paths that are known to fail on common runtime/device combinations.

## Related Files

- `crates/media-agent/src/ffmpeg_plan.rs`
- `crates/media-agent/src/media_policy.rs`
- `crates/media-agent/src/runtime_plan.rs`
- `crates/media-agent/src/tests/runtime.rs`
