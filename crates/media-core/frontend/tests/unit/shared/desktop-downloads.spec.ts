import { describe, expect, it } from "vitest";

import { detectDesktopPlatform, recommendedDownload } from "@/shared/desktop-downloads";

describe("desktop client downloads", () => {
  it("detects common desktop platforms", () => {
    expect(detectDesktopPlatform("Mozilla/5.0 (Windows NT 10.0; Win64; x64)", "Win32")).toBe("windows");
    expect(detectDesktopPlatform("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)", "MacIntel")).toBe("macos");
    expect(detectDesktopPlatform("Mozilla/5.0 (X11; Linux x86_64)", "Linux x86_64")).toBe("linux");
  });

  it("selects the download for the detected platform", () => {
    const downloads = [
      {
        platform: "windows" as const,
        arch: "x64",
        label: "Windows 客户端",
        fileName: "streamserver-desktop-windows-x64.exe",
        url: "/downloads/desktop/streamserver-desktop-windows-x64.exe",
        sizeBytes: 10,
        updatedAt: "2026-04-30T00:00:00.000Z",
      },
      {
        platform: "macos" as const,
        arch: "aarch64",
        label: "macOS 客户端",
        fileName: "streamserver-desktop-macos-aarch64.dmg",
        url: "/downloads/desktop/streamserver-desktop-macos-aarch64.dmg",
        sizeBytes: 20,
        updatedAt: "2026-04-30T00:00:00.000Z",
      },
    ];

    expect(recommendedDownload(downloads, "macos")?.fileName).toBe("streamserver-desktop-macos-aarch64.dmg");
    expect(recommendedDownload(downloads, "linux")).toBeNull();
  });
});
