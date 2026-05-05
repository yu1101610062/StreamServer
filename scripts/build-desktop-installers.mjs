#!/usr/bin/env node
import {
  chmodSync,
  copyFileSync,
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const appDir = join(rootDir, "apps", "desktop-client");
const tauriDir = join(appDir, "src-tauri");
const macAppName = "StreamServer 管理中心.app";
let outputDir = join(appDir, "releases");
let skipInstall = false;
let skipBuild = false;
let requireDualSystem = false;

function usage() {
  console.log(`用法:
  node scripts/build-desktop-installers.mjs [options]

说明:
  构建当前系统的 StreamServer 桌面客户端安装包，并复制到 apps/desktop-client/releases/。
  Windows 和 macOS 通常需要分别在对应系统运行本脚本；两个系统产物齐全后，前端打包脚本会把它们内置进去。

参数:
  --output-dir DIR       输出目录，默认 apps/desktop-client/releases
  --skip-install         跳过 npm ci / npm install 检查
  --skip-build           不执行 tauri build，只整理已有 bundle 产物
  --require-dual-system  打包后校验 releases 中同时存在 Windows 和 macOS 安装包
  -h, --help             显示帮助
`);
}

function fail(message) {
  console.error(`[desktop-installer-build] ERROR: ${message}`);
  process.exit(1);
}

function run(command, commandArgs, options = {}) {
  const result = spawnSync(command, commandArgs, {
    stdio: "inherit",
    shell: process.platform === "win32",
    ...options,
  });
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}

for (let index = 0; index < process.argv.slice(2).length; index += 1) {
  const args = process.argv.slice(2);
  const arg = args[index];
  if (arg === "--output-dir") {
    outputDir = resolve(args[++index] ?? "");
  } else if (arg === "--skip-install") {
    skipInstall = true;
  } else if (arg === "--skip-build") {
    skipBuild = true;
  } else if (arg === "--require-dual-system") {
    requireDualSystem = true;
  } else if (arg === "-h" || arg === "--help") {
    usage();
    process.exit(0);
  } else {
    fail(`未知参数: ${arg}`);
  }
}

function copyIfExists(sourcePath, targetName) {
  if (!existsSync(sourcePath) || !statSync(sourcePath).isFile()) {
    return false;
  }
  mkdirSync(outputDir, { recursive: true });
  const targetPath = join(outputDir, targetName);
  copyFileSync(sourcePath, targetPath);
  console.log(`[desktop-installer-build] ${sourcePath} -> ${targetPath}`);
  return true;
}

function latestFile(dir, predicate) {
  if (!existsSync(dir)) {
    return null;
  }
  const candidates = readdirSync(dir)
    .map((fileName) => join(dir, fileName))
    .filter((filePath) => statSync(filePath).isFile() && predicate(filePath))
    .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs);
  return candidates[0] ?? null;
}

function findMacApp() {
  const macosDir = join(tauriDir, "target", "release", "bundle", "macos");
  const exactPath = join(macosDir, macAppName);
  if (existsSync(exactPath) && statSync(exactPath).isDirectory()) {
    return exactPath;
  }

  if (!existsSync(macosDir)) {
    return null;
  }

  return (
    readdirSync(macosDir)
      .map((fileName) => join(macosDir, fileName))
      .filter((filePath) => statSync(filePath).isDirectory() && filePath.endsWith(".app"))
      .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0] ?? null
  );
}

function writeMacInstallCommand(commandPath) {
  writeFileSync(
    commandPath,
    `#!/bin/bash
set -euo pipefail

APP_NAME="StreamServer 管理中心.app"
SRC_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC_APP="$SRC_DIR/$APP_NAME"
DST_APP="/Applications/$APP_NAME"

pause() {
  echo ""
  echo "按任意键关闭此窗口..."
  read -r -n 1 _ || true
}
trap pause EXIT

copy_app() {
  local source_app="$1"
  local target_app="$2"

  if rm -rf "$target_app" 2>/dev/null && cp -R "$source_app" "$target_app" 2>/dev/null; then
    return 0
  fi

  echo "需要管理员权限写入 /Applications，请输入本机登录密码。"
  sudo rm -rf "$target_app"
  sudo cp -R "$source_app" "$target_app"
  sudo chown -R "$(id -un)":staff "$target_app" 2>/dev/null || true
}

clear_quarantine() {
  local app_path="$1"

  if ! xattr -lr "$app_path" 2>/dev/null | grep -q "com.apple.quarantine"; then
    return 0
  fi

  if xattr -rd com.apple.quarantine "$app_path" 2>/dev/null; then
    return 0
  fi

  echo "需要管理员权限移除 macOS 隔离标记。"
  sudo xattr -rd com.apple.quarantine "$app_path" 2>/dev/null || true
}

if [ ! -d "$SRC_APP" ]; then
  echo "未找到安装包内的 $APP_NAME。"
  exit 1
fi

echo "正在退出旧版本..."
osascript -e 'quit app "StreamServer 管理中心"' >/dev/null 2>&1 || true
sleep 1

echo "正在安装到 /Applications..."
copy_app "$SRC_APP" "$DST_APP"

echo "正在移除 macOS 隔离标记..."
clear_quarantine "$DST_APP"

echo "安装完成，正在打开 StreamServer 管理中心..."
open "$DST_APP"
`,
    "utf8",
  );
  chmodSync(commandPath, 0o755);
}

function writeMacReadme(readmePath) {
  writeFileSync(
    readmePath,
    `StreamServer 管理中心 macOS 安装说明

1. 双击“安装.command”。
2. 如提示需要管理员权限，请输入本机登录密码。
3. 安装脚本会复制应用到 /Applications，移除 com.apple.quarantine 标记，然后打开应用。

如果 macOS 阻止打开脚本，请在“系统设置 > 隐私与安全性”中允许，或右键点击“安装.command”后选择“打开”。
`,
    "utf8",
  );
}

function createMacInstallerDmg(appPath, arch) {
  if (process.platform !== "darwin") {
    return null;
  }

  const stagingDir = mkdtempSync(join(tmpdir(), "streamserver-desktop-dmg-"));
  const dmgDir = join(tauriDir, "target", "release", "bundle", "dmg");
  const dmgPath = join(dmgDir, `StreamServer 管理中心_安装_${arch}.dmg`);

  try {
    cpSync(appPath, join(stagingDir, macAppName), { recursive: true });
    writeMacInstallCommand(join(stagingDir, "安装.command"));
    writeMacReadme(join(stagingDir, "README.txt"));

    mkdirSync(dmgDir, { recursive: true });
    rmSync(dmgPath, { force: true });
    run("hdiutil", [
      "create",
      "-volname",
      "StreamServer 管理中心",
      "-srcfolder",
      stagingDir,
      "-ov",
      "-format",
      "UDZO",
      dmgPath,
    ]);
    return dmgPath;
  } finally {
    rmSync(stagingDir, { recursive: true, force: true });
  }
}

function cleanupMacBundleApp(appPath) {
  rmSync(appPath, { recursive: true, force: true });
  console.log(`[desktop-installer-build] 已清理本地 macOS .app 构建产物: ${appPath}`);
}

function collectBundles() {
  let copied = 0;
  const arch = process.arch === "arm64" ? "aarch64" : "x64";

  const appPath = findMacApp();
  const createdMacInstallerDmg = Boolean(appPath && process.platform === "darwin");
  const dmg =
    createdMacInstallerDmg
      ? createMacInstallerDmg(appPath, arch)
      : latestFile(join(tauriDir, "target", "release", "bundle", "dmg"), (filePath) => filePath.toLowerCase().endsWith(".dmg"));
  if (dmg) {
    copied += copyIfExists(dmg, `streamserver-desktop-macos-${arch}.dmg`) ? 1 : 0;
  }
  if (createdMacInstallerDmg) {
    cleanupMacBundleApp(appPath);
  }

  const nsis = latestFile(join(tauriDir, "target", "release", "bundle", "nsis"), (filePath) => filePath.toLowerCase().endsWith(".exe"));
  if (nsis) {
    copied += copyIfExists(nsis, "streamserver-desktop-windows-x64.exe") ? 1 : 0;
  }

  const msi = latestFile(join(tauriDir, "target", "release", "bundle", "msi"), (filePath) => filePath.toLowerCase().endsWith(".msi"));
  if (msi) {
    copied += copyIfExists(msi, "streamserver-desktop-windows-x64.msi") ? 1 : 0;
  }

  if (copied === 0) {
    fail("未找到可整理的 Tauri 安装包产物");
  }
}

function hasPlatform(platform) {
  if (!existsSync(outputDir)) {
    return false;
  }
  return readdirSync(outputDir).some((fileName) => fileName.toLowerCase().includes(platform));
}

if (!skipInstall && !existsSync(join(appDir, "node_modules"))) {
  run("npm", ["install"], { cwd: appDir });
}

if (!skipBuild) {
  run("npm", ["run", "build"], { cwd: appDir });
}

collectBundles();

if (requireDualSystem) {
  const missing = ["windows", "macos"].filter((platform) => !hasPlatform(platform));
  if (missing.length > 0) {
    fail(`缺少 ${missing.join(", ")} 桌面安装包。请在对应系统运行本脚本后再校验。`);
  }
}
