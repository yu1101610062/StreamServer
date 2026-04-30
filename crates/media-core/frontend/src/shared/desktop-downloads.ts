export type DesktopClientPlatform = "windows" | "macos" | "linux" | "unknown";

export interface DesktopClientDownload {
  platform: DesktopClientPlatform;
  arch: string;
  label: string;
  fileName: string;
  url: string;
  sizeBytes: number;
  updatedAt: string;
}

export interface DesktopClientManifest {
  generatedAt: string;
  downloads: DesktopClientDownload[];
}

export function detectDesktopPlatform(userAgent = navigator.userAgent, platform = navigator.platform): DesktopClientPlatform {
  const value = `${platform} ${userAgent}`.toLowerCase();
  if (value.includes("win")) {
    return "windows";
  }
  if (value.includes("mac")) {
    return "macos";
  }
  if (value.includes("linux") || value.includes("x11")) {
    return "linux";
  }
  return "unknown";
}

export function platformLabel(platform: DesktopClientPlatform) {
  switch (platform) {
    case "windows":
      return "Windows";
    case "macos":
      return "macOS";
    case "linux":
      return "Linux";
    default:
      return "未知系统";
  }
}

export async function loadDesktopClientManifest(): Promise<DesktopClientManifest> {
  const response = await fetch("/assets/downloads/desktop/manifest.json", { cache: "no-store" });
  if (!response.ok) {
    return { generatedAt: "", downloads: [] };
  }
  return (await response.json()) as DesktopClientManifest;
}

export function recommendedDownload(
  downloads: DesktopClientDownload[],
  platform: DesktopClientPlatform,
) {
  if (platform === "unknown") {
    return null;
  }
  return downloads.find((item) => item.platform === platform) ?? null;
}

export function formatDownloadSize(sizeBytes: number) {
  if (!Number.isFinite(sizeBytes) || sizeBytes <= 0) {
    return "";
  }
  const mib = sizeBytes / 1024 / 1024;
  return `${mib.toFixed(mib >= 10 ? 0 : 1)} MB`;
}
