import { describe, expect, it } from "vitest";

import { buildDraftPayload, createDefaultDraft, normalizeDraftForTaskType } from "@/shared/task-create";

describe("buildDraftPayload", () => {
  it("omits ingest-only sections for stream_bridge tasks", () => {
    const draft = createDefaultDraft();
    draft.name = "bridge";
    draft.common.created_by = "alice";
    normalizeDraftForTaskType(draft, "stream_bridge");
    draft.publish.kind = "file";

    const payload = buildDraftPayload(draft) as Record<string, unknown>;

    expect(payload).not.toHaveProperty("stream");
    expect(payload).not.toHaveProperty("expose");
    expect(payload).not.toHaveProperty("record");
  });

  it("normalizes managed file input paths for file_transcode tasks", () => {
    const draft = createDefaultDraft();
    draft.name = "transcode";
    draft.common.created_by = "alice";
    normalizeDraftForTaskType(draft, "file_transcode");
    draft.input.kind = "file";
    draft.input.source_mode = "vod";
    draft.input.url = "/vod/demo.ts";
    draft.publish.kind = "file";

    const payload = buildDraftPayload(draft) as Record<string, unknown>;

    expect(payload.input).toMatchObject({ kind: "file", source_mode: "vod", url: "vod/demo.ts" });
    expect(payload).not.toHaveProperty("stream");
    expect(payload).not.toHaveProperty("expose");
    expect(payload).not.toHaveProperty("record");
  });

  it("keeps record extras out of stream_ingest payloads when recording is disabled", () => {
    const draft = createDefaultDraft();
    draft.name = "ingest";
    draft.common.created_by = "alice";
    draft.record.enabled = false;
    draft.record.duration_sec = "300";
    draft.record.as_player = true;

    const payload = buildDraftPayload(draft) as Record<string, unknown>;

    expect(payload.record).toEqual({ enabled: false });
  });

  it("does not turn empty numeric fields into zero-valued publish settings", () => {
    const draft = createDefaultDraft();
    draft.name = "ingest-http-ts";
    draft.common.created_by = "admin";
    draft.input.kind = "http_ts";
    draft.input.source_mode = "vod";
    draft.input.url = "http://172.17.28.109:28081/source.ts";
    draft.stream.name = "tdsy";

    const payload = buildDraftPayload(draft) as Record<string, unknown>;

    expect(payload).not.toHaveProperty("publish");
    expect(payload.input).toMatchObject({
      kind: "http_ts",
      source_mode: "vod",
      url: "http://172.17.28.109:28081/source.ts",
      probe_timeout_ms: 7000,
      reuse: false,
    });
    expect(payload.input).not.toMatchObject({
      port: 0,
      ttl: 0,
      tcp_mode: 0,
      ssrc: 0,
    });
    expect(payload.process).toEqual({ mode: "copy_or_transcode" });
    expect(payload.recovery).toEqual({ policy: "auto" });
  });
});
