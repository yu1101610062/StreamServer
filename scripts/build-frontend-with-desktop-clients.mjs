#!/usr/bin/env node
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const frontendDir = join(rootDir, "crates", "media-core", "frontend");

const args = process.argv.slice(2);
let allowMissingClients = false;
let skipInstall = false;
const passthroughSyncArgs = ["--require-dual-system"];

function usage() {
  console.log(`用法:
  node scripts/build-frontend-with-desktop-clients.mjs [options]

说明:
  单独构建 media-core 前端，并在打包前内置 Windows/macOS 桌面客户端安装包。

参数:
  --allow-missing-clients   允许缺少某个平台客户端，用于本地调试
  --skip-install            跳过 npm ci / npm install 检查
  --source-dir DIR          额外客户端安装包目录，传给 sync-desktop-clients
  -h, --help                显示帮助
`);
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

for (let index = 0; index < args.length; index += 1) {
  const arg = args[index];
  if (arg === "--allow-missing-clients") {
    allowMissingClients = true;
  } else if (arg === "--skip-install") {
    skipInstall = true;
  } else if (arg === "--source-dir") {
    passthroughSyncArgs.push("--source-dir", args[++index] ?? "");
  } else if (arg === "-h" || arg === "--help") {
    usage();
    process.exit(0);
  } else {
    console.error(`[frontend-build] ERROR: 未知参数: ${arg}`);
    process.exit(1);
  }
}

if (allowMissingClients) {
  passthroughSyncArgs.push("--allow-missing");
}

if (!skipInstall && !existsSync(join(frontendDir, "node_modules"))) {
  run("npm", ["ci"], { cwd: frontendDir });
}

run("node", [join(rootDir, "scripts", "sync-desktop-clients.mjs"), ...passthroughSyncArgs], { cwd: rootDir });
run("npm", ["run", "build:web"], { cwd: frontendDir });
