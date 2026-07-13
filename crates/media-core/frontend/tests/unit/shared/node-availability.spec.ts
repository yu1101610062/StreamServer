import { describe, expect, it } from "vitest";

import { isNodeUploadReady } from "@/shared/node-availability";

const readyNode = {
  healthy: true,
  control_connected: true,
  connected: true,
  ffmpeg_alive: true,
};

describe("Agent upload availability", () => {
  it("depends on the authenticated control session rather than a reported management URL", () => {
    expect(isNodeUploadReady(readyNode)).toBe(true);
    expect(isNodeUploadReady({ ...readyNode, connected: undefined })).toBe(true);
  });

  it.each([
    ["unhealthy", { healthy: false }],
    ["control disconnected", { control_connected: false }],
    ["session disconnected", { connected: false }],
    ["ffmpeg unavailable", { ffmpeg_alive: false }],
  ])("rejects %s nodes", (_name, override) => {
    expect(isNodeUploadReady({ ...readyNode, ...override })).toBe(false);
  });
});
