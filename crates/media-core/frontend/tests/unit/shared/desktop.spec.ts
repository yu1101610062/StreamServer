import { describe, expect, it } from "vitest";

import { buildServerUrl, defaultSettings, isMediaUrl } from "@/shared/desktop";

describe("desktop helpers", () => {
  it("accepts media URLs supported by VLC launching", () => {
    expect(isMediaUrl("http://example.test/video.mp4")).toBe(true);
    expect(isMediaUrl("https://example.test/video.mp4")).toBe(true);
    expect(isMediaUrl("rtsp://example.test/live/camera01")).toBe(true);
    expect(isMediaUrl("rtmp://example.test/live/camera01")).toBe(true);
    expect(isMediaUrl("rtmps://example.test/live/camera01")).toBe(true);
  });

  it("rejects non-media or malformed URLs", () => {
    expect(isMediaUrl("file:///etc/passwd")).toBe(false);
    expect(isMediaUrl("javascript:alert(1)")).toBe(false);
    expect(isMediaUrl("not a url")).toBe(false);
  });

  it("builds the configured management center URL", () => {
    const settings = defaultSettings();
    settings.server.protocol = "https";
    settings.server.host = "stream.example.test";
    settings.server.port = 8443;

    expect(buildServerUrl(settings)).toBe("https://stream.example.test:8443/");
  });
});
