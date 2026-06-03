#!/usr/bin/env node
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const frontendDir = join(rootDir, "crates", "media-core", "frontend");

const args = process.argv.slice(2);
let skipInstall = false;

function usage() {
  console.log(`用法:
  node scripts/build-frontend-ui.mjs [options]

说明:
  构建 media-core 前端静态资源。

参数:
  --skip-install             跳过 npm ci / npm install 检查
  -h, --help                 显示帮助
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
  if (arg === "--skip-install") {
    skipInstall = true;
  } else if (arg === "-h" || arg === "--help") {
    usage();
    process.exit(0);
  } else {
    console.error(`[frontend-build] ERROR: 未知参数: ${arg}`);
    process.exit(1);
  }
}

if (!skipInstall && !existsSync(join(frontendDir, "node_modules"))) {
  run("npm", ["ci"], { cwd: frontendDir });
}

run("npm", ["run", "build"], { cwd: frontendDir });
