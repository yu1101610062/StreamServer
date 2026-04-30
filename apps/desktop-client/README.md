# StreamServer Desktop Client

This Tauri client loads the configured StreamServer management center URL and exposes a small desktop bridge for opening media URLs in the local VLC client.

## Development

```bash
npm install
npm run dev
```

## Build

```bash
npm run build
```

The runtime settings are stored in the OS app config directory as `settings.json`.

## Embed installers in the web console

Build the desktop installer on each target platform, then place the installers in `apps/desktop-client/releases/` using these names:

- `streamserver-desktop-windows-x64.exe`
- `streamserver-desktop-windows-x64.msi`
- `streamserver-desktop-macos-aarch64.dmg`
- `streamserver-desktop-macos-x64.dmg`

When the web console runs `npm run build`, it copies available installers into `crates/media-core/ui/downloads/desktop/` and generates the download manifest used by the login page.
