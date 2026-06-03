# ADR-0004: Keep FFmpeg and ZLMediaKit Responsibilities Separate

## Status

Accepted

## Context

FFmpeg and ZLMediaKit overlap in some media workflows, but they are stronger at different jobs. ZLMediaKit is a realtime media server with APIs and hooks. FFmpeg is a general-purpose media processing tool.

## Decision

StreamServer uses ZLMediaKit for realtime serving, proxying, distribution, recording hooks, RTP/RTSP-related media-server behavior, and API-managed live runtime.

StreamServer uses FFmpeg for file transcoding, file-to-live, stream processing, muxing, codec conversion, filtering, multi-output, and fallback paths that require explicit command planning.

Agent builds an execution plan instead of passing raw FFmpeg strings through the API.

## Consequences

Pros:

- Cleaner runtime ownership.
- Easier testing of codec/container/protocol decisions.
- Avoids assuming ZLMediaKit OSS provides every possible transcoding/IPTV capability.

Cons:

- Agent planning logic is larger.
- More smoke testing is required across codec, muxer, protocol, and ZLM combinations.
