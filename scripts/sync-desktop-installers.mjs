#!/usr/bin/env node
import { copyFileSync, existsSync, mkdirSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const defaultOutputDir = join(rootDir, "crates", "media-core", "frontend", "public", "assets", "downloads", "desktop");
const defaultSourceDirs = [
  join(rootDir, "apps", "desktop-client", "releases"),
  join(rootDir, "apps", "desktop-client", "src-tauri", "target", "release", "bundle", "dmg"),
  join(rootDir, "apps", "desktop-client", "src-tauri", "target", "release", "bundle", "nsis"),
  join(rootDir, "apps", "desktop-client", "src-tauri", "target", "release", "bundle", "msi"),
];

const args = process.argv.slice(2);
let outputDir = defaultOutputDir;
let allowMissing = false;
let requiredPlatforms = [];
const sourceDirs = [...defaultSourceDirs];

function usage() {
  console.log(`用法:
  node scripts/sync-desktop-installers.mjs [options]

说明:
  扫描桌面安装包，复制到前端 public/assets/downloads/desktop，并生成 manifest.json。

参数:
  --output-dir DIR              输出目录，默认 crates/media-core/frontend/public/assets/downloads/desktop
  --source-dir DIR              额外扫描目录，可重复
  --require-platform PLATFORM   要求平台存在，可重复；支持 windows, macos
  --require-dual-system         等同于 --require-platform windows --require-platform macos
  --allow-missing               允许没有桌面安装包，用于本地开发构建
  -h, --help                    显示帮助

环境变量:
  REQUIRE_DESKTOP_INSTALLERS=1  默认要求 windows 和 macos 两个平台安装包
  REQUIRED_DESKTOP_INSTALLER_PLATFORMS=windows,macos
  REQUIRE_DESKTOP_CLIENTS=1     旧环境变量别名
  REQUIRED_DESKTOP_CLIENT_PLATFORMS=windows,macos 旧环境变量别名
`);
}

function fail(message) {
  console.error(`[desktop-installer-sync] ERROR: ${message}`);
  process.exit(1);
}

function addRequiredPlatform(platform) {
  if (!["windows", "macos"].includes(platform)) {
    fail(`不支持的平台: ${platform}`);
  }
  if (!requiredPlatforms.includes(platform)) {
    requiredPlatforms.push(platform);
  }
}

for (let index = 0; index < args.length; index += 1) {
  const arg = args[index];
  if (arg === "--output-dir") {
    outputDir = resolve(args[++index] ?? "");
  } else if (arg === "--source-dir") {
    sourceDirs.push(resolve(args[++index] ?? ""));
  } else if (arg === "--require-platform") {
    addRequiredPlatform(args[++index] ?? "");
  } else if (arg === "--require-dual-system") {
    addRequiredPlatform("windows");
    addRequiredPlatform("macos");
  } else if (arg === "--allow-missing") {
    allowMissing = true;
  } else if (arg === "-h" || arg === "--help") {
    usage();
    process.exit(0);
  } else {
    fail(`未知参数: ${arg}`);
  }
}

const requiredPlatformsEnv =
  process.env.REQUIRED_DESKTOP_INSTALLER_PLATFORMS ?? process.env.REQUIRED_DESKTOP_CLIENT_PLATFORMS;
if (requiredPlatformsEnv) {
  for (const platform of requiredPlatformsEnv.split(",").map((item) => item.trim()).filter(Boolean)) {
    addRequiredPlatform(platform);
  }
}
if (
  (process.env.REQUIRE_DESKTOP_INSTALLERS === "1" || process.env.REQUIRE_DESKTOP_CLIENTS === "1") &&
  requiredPlatforms.length === 0
) {
  addRequiredPlatform("windows");
  addRequiredPlatform("macos");
}

const downloads = [];
const seen = new Set();

function cleanOutputDir() {
  mkdirSync(outputDir, { recursive: true });
  for (const fileName of readdirSync(outputDir)) {
    if (fileName === ".gitignore") {
      continue;
    }
    rmSync(join(outputDir, fileName), { recursive: true, force: true });
  }
}

function addDownload(sourcePath, targetName, meta) {
  if (!existsSync(sourcePath) || seen.has(targetName)) {
    return;
  }
  const targetPath = join(outputDir, targetName);
  copyFileSync(sourcePath, targetPath);
  const stat = statSync(targetPath);
  downloads.push({
    ...meta,
    fileName: targetName,
    url: `/assets/downloads/desktop/${targetName}`,
    sizeBytes: stat.size,
    updatedAt: stat.mtime.toISOString(),
  });
  seen.add(targetName);
}

function scanDir(dir, onFile) {
  if (!dir || !existsSync(dir)) {
    return;
  }
  for (const fileName of readdirSync(dir)) {
    const filePath = join(dir, fileName);
    if (statSync(filePath).isFile()) {
      onFile(fileName, filePath);
    }
  }
}

function scanInstaller(fileName, filePath) {
  const normalized = fileName.toLowerCase();
  if (normalized.includes("windows") && normalized.endsWith(".exe")) {
    addDownload(filePath, "streamserver-desktop-windows-x64.exe", {
      platform: "windows",
      arch: "x64",
      label: "Windows 客户端",
    });
  } else if (normalized.includes("windows") && normalized.endsWith(".msi")) {
    addDownload(filePath, "streamserver-desktop-windows-x64.msi", {
      platform: "windows",
      arch: "x64",
      label: "Windows 客户端 MSI",
    });
  } else if (normalized.includes("macos") && normalized.includes("aarch64") && normalized.endsWith(".dmg")) {
    addDownload(filePath, "streamserver-desktop-macos-aarch64.dmg", {
      platform: "macos",
      arch: "aarch64",
      label: "macOS Apple 芯片客户端",
    });
  } else if (
    normalized.includes("macos") &&
    (normalized.includes("x64") || normalized.includes("x86_64")) &&
    normalized.endsWith(".dmg")
  ) {
    addDownload(filePath, "streamserver-desktop-macos-x64.dmg", {
      platform: "macos",
      arch: "x64",
      label: "macOS Intel 客户端",
    });
  } else if (normalized.includes("streamserver") && normalized.endsWith(".dmg")) {
    const arch = process.arch === "arm64" ? "aarch64" : "x64";
    addDownload(filePath, `streamserver-desktop-macos-${arch}.dmg`, {
      platform: "macos",
      arch,
      label: arch === "aarch64" ? "macOS Apple 芯片客户端" : "macOS Intel 客户端",
    });
  } else if (normalized.endsWith(".exe")) {
    addDownload(filePath, "streamserver-desktop-windows-x64.exe", {
      platform: "windows",
      arch: "x64",
      label: "Windows 客户端",
    });
  } else if (normalized.endsWith(".msi")) {
    addDownload(filePath, "streamserver-desktop-windows-x64.msi", {
      platform: "windows",
      arch: "x64",
      label: "Windows 客户端 MSI",
    });
  }
}

cleanOutputDir();
for (const sourceDir of sourceDirs) {
  scanDir(sourceDir, scanInstaller);
}

downloads.sort((a, b) => `${a.platform}-${a.arch}-${a.fileName}`.localeCompare(`${b.platform}-${b.arch}-${b.fileName}`));

const platforms = new Set(downloads.map((item) => item.platform));
const missingPlatforms = requiredPlatforms.filter((platform) => !platforms.has(platform));
if (!allowMissing && missingPlatforms.length > 0) {
  fail(`缺少桌面安装包: ${missingPlatforms.join(", ")}。请先运行 scripts/build-desktop-installers.mjs，或把安装包放入 apps/desktop-client/releases/`);
}

const manifest = {
  generatedAt: new Date().toISOString(),
  downloads,
};

writeFileSync(join(outputDir, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);

if (downloads.length === 0) {
  console.warn("[desktop-installer-sync] 未发现桌面安装包，已生成空 manifest。");
} else {
  console.log(`[desktop-installer-sync] 已内置 ${downloads.length} 个桌面安装包。`);
}
