export type ServerProtocol = "http" | "https";
export type VlcMode = "auto" | "custom";

export interface AppSettings {
  server: {
    protocol: ServerProtocol;
    host: string;
    port: number;
  };
  vlc: {
    mode: VlcMode;
    customPath: string | null;
  };
}

export interface DesktopResult {
  ok: boolean;
  error?: string | null;
}

export interface DesktopBridge {
  openInVlc(url: string): Promise<DesktopResult>;
  getSettings?: () => Promise<AppSettings>;
  saveSettings?: (settings: AppSettings) => Promise<DesktopResult>;
  pickVlcPath?: () => Promise<{ path?: string | null }>;
  testVlc?: (settings?: AppSettings) => Promise<DesktopResult>;
  openManagementCenter?: (settings?: AppSettings) => Promise<DesktopResult>;
}

declare global {
  interface Window {
    streamServerDesktop?: DesktopBridge;
  }
}

const MEDIA_PROTOCOLS = new Set(["http:", "https:", "rtsp:", "rtmp:", "rtmps:"]);

export function isDesktopClient() {
  return typeof window !== "undefined" && Boolean(window.streamServerDesktop);
}

export function isMediaUrl(value: string) {
  try {
    const url = new URL(value);
    return MEDIA_PROTOCOLS.has(url.protocol);
  } catch {
    return false;
  }
}

export function buildServerUrl(settings: AppSettings) {
  return `${settings.server.protocol}://${settings.server.host}:${settings.server.port}/`;
}

export function defaultSettings(): AppSettings {
  return {
    server: {
      protocol: "http",
      host: "172.17.13.196",
      port: 8080,
    },
    vlc: {
      mode: "auto",
      customPath: null,
    },
  };
}

export async function openMediaUrl(url: string) {
  if (!isMediaUrl(url)) {
    throw new Error("只支持 http、https、rtsp、rtmp、rtmps 媒体地址");
  }

  if (window.streamServerDesktop) {
    let result: DesktopResult;
    try {
      result = await window.streamServerDesktop.openInVlc(url);
    } catch (error) {
      throw new Error(error instanceof Error ? error.message : String(error || "VLC 打开失败"));
    }
    if (!result.ok) {
      throw new Error(result.error || "VLC 打开失败");
    }
    return "desktop" as const;
  }

  window.open(url, "_blank", "noopener,noreferrer");
  return "browser" as const;
}
