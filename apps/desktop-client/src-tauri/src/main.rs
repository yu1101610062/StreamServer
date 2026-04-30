#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use url::Url;

const DEFAULT_HOST: &str = "172.17.13.196";
const DEFAULT_PORT: u16 = 8080;

const DESKTOP_BRIDGE_SCRIPT: &str = r#"
(() => {
  if (window.streamServerDesktop) {
    return;
  }
  const invoke = window.__TAURI__?.core?.invoke || window.__TAURI_INTERNALS__?.invoke;
  if (!invoke) {
    return;
  }
  const bridge = {
    openInVlc: (url) => invoke("open_in_vlc", { url })
  };
  const isLocalPage = location.protocol === "tauri:" || location.protocol === "asset:" || location.hostname === "tauri.localhost";
  if (isLocalPage) {
    Object.assign(bridge, {
      getSettings: () => invoke("get_settings"),
      saveSettings: (settings) => invoke("save_settings", { settings }),
      pickVlcPath: () => invoke("pick_vlc_path"),
      testVlc: (settings) => invoke("test_vlc", { settings }),
      openManagementCenter: (settings) => invoke("open_management_center", { settings })
    });
  }
  window.streamServerDesktop = bridge;
})();
"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    server: ServerSettings,
    vlc: VlcSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerSettings {
    protocol: String,
    host: String,
    port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VlcSettings {
    mode: String,
    custom_path: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandResult {
    ok: bool,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PickVlcPathResult {
    path: Option<String>,
}

enum VlcLaunch {
    Program(PathBuf),
    #[cfg(target_os = "macos")]
    MacOpenByName,
    #[cfg(target_os = "macos")]
    MacOpenApp(PathBuf),
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            pick_vlc_path,
            test_vlc,
            open_in_vlc,
            open_management_center
        ])
        .setup(|app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("StreamServer 管理中心")
                .inner_size(1280.0, 820.0)
                .min_inner_size(1024.0, 700.0)
                .initialization_script(DESKTOP_BRIDGE_SCRIPT)
                .build()?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run StreamServer desktop client");
}

#[tauri::command]
fn open_management_center(
    app: AppHandle,
    window: WebviewWindow,
    settings: Option<AppSettings>,
) -> Result<CommandResult, String> {
    ensure_local_window(&window)?;
    let settings = match settings {
        Some(settings) => settings,
        None => read_or_create_settings(&app)?,
    };
    if let Err(error) = validate_settings(&settings) {
        return Ok(CommandResult {
            ok: false,
            error: Some(error),
        });
    }
    write_settings(&app, &settings)?;
    let url = server_url(&settings)?;
    let main_window = app
        .get_webview_window("main")
        .ok_or_else(|| "主窗口不存在".to_string())?;
    main_window
        .navigate(url)
        .map_err(|error| format!("无法打开管理中心: {error}"))?;
    if window.label() == "settings" {
        window
            .close()
            .map_err(|error| format!("无法关闭客户端设置窗口: {error}"))?;
    }
    Ok(CommandResult {
        ok: true,
        error: None,
    })
}

#[tauri::command]
fn get_settings(app: AppHandle, window: WebviewWindow) -> Result<AppSettings, String> {
    ensure_local_window(&window)?;
    read_or_create_settings(&app)
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    window: WebviewWindow,
    settings: AppSettings,
) -> Result<CommandResult, String> {
    ensure_local_window(&window)?;
    if let Err(error) = validate_settings(&settings) {
        return Ok(CommandResult {
            ok: false,
            error: Some(error),
        });
    }
    write_settings(&app, &settings)?;
    Ok(CommandResult {
        ok: true,
        error: None,
    })
}

#[tauri::command]
fn pick_vlc_path(window: WebviewWindow) -> Result<PickVlcPathResult, String> {
    ensure_local_window(&window)?;
    let path = rfd::FileDialog::new()
        .set_title("选择 VLC 可执行文件")
        .pick_file()
        .map(|path| path.to_string_lossy().to_string());

    Ok(PickVlcPathResult { path })
}

#[tauri::command]
fn test_vlc(
    app: AppHandle,
    window: WebviewWindow,
    settings: Option<AppSettings>,
) -> Result<CommandResult, String> {
    ensure_local_window(&window)?;
    let settings = match settings {
        Some(settings) => settings,
        None => read_or_create_settings(&app)?,
    };
    if let Err(error) = validate_settings(&settings) {
        return Ok(CommandResult {
            ok: false,
            error: Some(error),
        });
    }
    match resolve_vlc(&settings).and_then(|launch| spawn_vlc(launch, &[])) {
        Ok(()) => Ok(CommandResult {
            ok: true,
            error: None,
        }),
        Err(error) => Ok(CommandResult {
            ok: false,
            error: Some(error),
        }),
    }
}

#[tauri::command]
fn open_in_vlc(
    app: AppHandle,
    window: WebviewWindow,
    url: String,
) -> Result<CommandResult, String> {
    ensure_window_allowed(&app, &window)?;
    if let Err(error) = validate_media_url(&url) {
        return Ok(CommandResult {
            ok: false,
            error: Some(error),
        });
    }

    let settings = read_or_create_settings(&app)?;
    match resolve_vlc(&settings).and_then(|launch| spawn_vlc(launch, &[url.as_str()])) {
        Ok(()) => Ok(CommandResult {
            ok: true,
            error: None,
        }),
        Err(error) => Ok(CommandResult {
            ok: false,
            error: Some(error),
        }),
    }
}

fn default_settings() -> AppSettings {
    AppSettings {
        server: ServerSettings {
            protocol: "http".to_string(),
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
        },
        vlc: VlcSettings {
            mode: "auto".to_string(),
            custom_path: None,
        },
    }
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("无法定位客户端配置目录: {error}"))?;
    fs::create_dir_all(&dir).map_err(|error| format!("无法创建客户端配置目录: {error}"))?;
    Ok(dir.join("settings.json"))
}

fn read_or_create_settings(app: &AppHandle) -> Result<AppSettings, String> {
    let path = settings_path(app)?;
    if !path.exists() {
        let settings = default_settings();
        write_settings(app, &settings)?;
        return Ok(settings);
    }

    let content = fs::read_to_string(&path).map_err(|error| format!("无法读取客户端配置: {error}"))?;
    let settings: AppSettings =
        serde_json::from_str(&content).map_err(|error| format!("客户端配置格式错误: {error}"))?;
    validate_settings(&settings)?;
    Ok(settings)
}

fn write_settings(app: &AppHandle, settings: &AppSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    let content = serde_json::to_string_pretty(settings)
        .map_err(|error| format!("无法序列化客户端配置: {error}"))?;
    fs::write(path, content).map_err(|error| format!("无法保存客户端配置: {error}"))
}

fn validate_settings(settings: &AppSettings) -> Result<(), String> {
    if !matches!(settings.server.protocol.as_str(), "http" | "https") {
        return Err("管理中心协议只支持 http 或 https".to_string());
    }
    if settings.server.host.trim().is_empty() {
        return Err("管理中心 IP 或域名不能为空".to_string());
    }
    if !matches!(settings.vlc.mode.as_str(), "auto" | "custom") {
        return Err("VLC 路径模式只支持 auto 或 custom".to_string());
    }
    if settings.vlc.mode == "custom" {
        let path = settings
            .vlc
            .custom_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "手动指定 VLC 时必须填写路径".to_string())?;
        if !Path::new(path).exists() {
            return Err("手动指定的 VLC 路径不存在".to_string());
        }
    }
    Ok(())
}

fn server_url(settings: &AppSettings) -> Result<Url, String> {
    Url::parse(&format!(
        "{}://{}:{}/",
        settings.server.protocol,
        settings.server.host.trim(),
        settings.server.port
    ))
    .map_err(|error| format!("管理中心地址无效: {error}"))
}

fn ensure_window_allowed(app: &AppHandle, window: &WebviewWindow) -> Result<(), String> {
    let current_url = window
        .url()
        .map_err(|error| format!("无法读取当前窗口地址: {error}"))?;
    if is_local_url(&current_url) {
        return Ok(());
    }

    let settings = read_or_create_settings(app)?;
    let allowed_url = server_url(&settings)?;
    if current_url.origin().ascii_serialization() == allowed_url.origin().ascii_serialization() {
        return Ok(());
    }

    Err("当前页面不允许调用桌面客户端能力".to_string())
}

fn ensure_local_window(window: &WebviewWindow) -> Result<(), String> {
    let current_url = window
        .url()
        .map_err(|error| format!("无法读取当前窗口地址: {error}"))?;
    if is_local_url(&current_url) {
        return Ok(());
    }

    Err("配置能力只允许客户端本地页面调用".to_string())
}

fn is_local_url(url: &Url) -> bool {
    url.scheme() == "tauri" || url.scheme() == "asset" || url.host_str() == Some("tauri.localhost")
}

fn validate_media_url(value: &str) -> Result<(), String> {
    let url = Url::parse(value).map_err(|_| "媒体地址格式无效".to_string())?;
    match url.scheme() {
        "http" | "https" | "rtsp" | "rtmp" | "rtmps" => Ok(()),
        _ => Err("只支持 http、https、rtsp、rtmp、rtmps 媒体地址".to_string()),
    }
}

fn resolve_vlc(settings: &AppSettings) -> Result<VlcLaunch, String> {
    if settings.vlc.mode == "custom" {
        let path = settings
            .vlc
            .custom_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "手动指定 VLC 时必须填写路径".to_string())?;
        return vlc_launch_from_path(PathBuf::from(path));
    }

    auto_detect_vlc()
}

fn vlc_launch_from_path(path: PathBuf) -> Result<VlcLaunch, String> {
    if !path.exists() {
        return Err("VLC 路径不存在".to_string());
    }

    #[cfg(target_os = "macos")]
    {
        if path.extension().is_some_and(|extension| extension == "app") {
            return Ok(VlcLaunch::MacOpenApp(path));
        }
    }

    Ok(VlcLaunch::Program(path))
}

fn auto_detect_vlc() -> Result<VlcLaunch, String> {
    #[cfg(target_os = "windows")]
    {
        for path in [
            r"C:\Program Files\VideoLAN\VLC\vlc.exe",
            r"C:\Program Files (x86)\VideoLAN\VLC\vlc.exe",
        ] {
            let path = PathBuf::from(path);
            if path.exists() {
                return Ok(VlcLaunch::Program(path));
            }
        }
        if let Some(path) = find_in_path("vlc.exe") {
            return Ok(VlcLaunch::Program(path));
        }
    }

    #[cfg(target_os = "macos")]
    {
        let app_binary = PathBuf::from("/Applications/VLC.app/Contents/MacOS/VLC");
        if app_binary.exists() {
            return Ok(VlcLaunch::Program(app_binary));
        }
        if let Some(path) = find_in_path("vlc") {
            return Ok(VlcLaunch::Program(path));
        }
        return Ok(VlcLaunch::MacOpenByName);
    }

    #[cfg(not(target_os = "macos"))]
    {
        #[cfg(target_os = "linux")]
        {
            if let Some(path) = find_in_path("vlc") {
                return Ok(VlcLaunch::Program(path));
            }
        }

        Err("未找到 VLC，请安装 VLC 或在客户端设置里手动指定路径".to_string())
    }
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn spawn_vlc(launch: VlcLaunch, args: &[&str]) -> Result<(), String> {
    let result = match launch {
        VlcLaunch::Program(path) => Command::new(path).args(args).spawn(),
        #[cfg(target_os = "macos")]
        VlcLaunch::MacOpenByName => {
            let mut command = Command::new("open");
            command.args(["-a", "VLC"]).args(args).spawn()
        }
        #[cfg(target_os = "macos")]
        VlcLaunch::MacOpenApp(path) => {
            let mut command = Command::new("open");
            command.arg("-a").arg(path).args(args).spawn()
        }
    };

    result
        .map(|_| ())
        .map_err(|error| format!("启动 VLC 失败: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{default_settings, server_url, validate_media_url, validate_settings};

    #[test]
    fn accepts_supported_media_urls() {
        for value in [
            "http://example.test/video.mp4",
            "https://example.test/video.mp4",
            "rtsp://example.test/live/camera01",
            "rtmp://example.test/live/camera01",
            "rtmps://example.test/live/camera01",
        ] {
            assert!(validate_media_url(value).is_ok(), "{value}");
        }
    }

    #[test]
    fn rejects_unsupported_media_urls() {
        for value in ["file:///tmp/a.mp4", "javascript:alert(1)", "not a url"] {
            assert!(validate_media_url(value).is_err(), "{value}");
        }
    }

    #[test]
    fn builds_default_server_url() {
        let settings = default_settings();
        assert_eq!(
            server_url(&settings).unwrap().as_str(),
            "http://172.17.13.196:8080/"
        );
    }

    #[test]
    fn validates_default_settings() {
        assert!(validate_settings(&default_settings()).is_ok());
    }
}
