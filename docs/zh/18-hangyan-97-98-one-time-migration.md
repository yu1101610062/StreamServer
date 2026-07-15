# 杭研院 97/98 一次性迁移升级手册

本文只用于杭研院本次从约 6.24 版本升级到指定 `master` 提交，不作为通用产品安装流程。所有现场命令先在本地一次性脚本中固化并记录输出；脚本和含敏感信息的清单不得提交仓库。

## 1. 最终拓扑和不可变约束

| 主机 | 最终职责 | 关键端口 |
| --- | --- | --- |
| `172.21.26.25` | 客户 HTTPS Nginx 入口 | `443` |
| `172.21.26.42` | 只运行 Media Gateway，不运行 Nginx | `172.21.26.42:18081` |
| `172.31.243.94` | Avqual 和现有 Nginx；新增 Work Gateway | `80/443/12000/8082` 保持，新增 `7000` |
| `172.31.243.97` | PostgreSQL、Core、Agent、ZLM | Core `8080/50051` 加原媒体端口 |
| `172.31.243.98` | Agent、ZLM | 原媒体端口 |
| `172.31.243.99` | 现有 LLM | `25104` |
| `172.31.243.95` | 现有 NFS | 不登录、不改配置 |

固定约束：

- 代码直接落在 `master`，97、98、42 使用同一个 `DEPLOY_SHA` 产物。
- 全部应用停止后逐台升级，不做在线滚动切换。
- Core 只访问 `https://172.21.26.25/bohui/media/`，不得配置域名或直连 `42:18081`。
- 42 的 Nginx 保持 `disabled/inactive`；Gateway API 本次不增加鉴权。
- Avqual 只改部署配置和 CA，不改项目代码；机器接口只允许 `172.31.243.94/32`。
- 不登录或修改 95，只复用三台机器上已经存在的 NFS 挂载。
- FFmpeg/FFprobe 的 TLS 校验保持现状，不增加 `-tls_verify`、`-ca_file` 或 `-verifyhost`。

请求链路固定为：

```text
Core -> HTTPS 25 /bohui/media/ -> HTTP 42:18081 -> media-gateway
客户 -> HTTPS 25 /bohui/work/ -> HTTP 94:7000 -> HTTP 99:25104
Gateway -> existing NFS streamserver-work <- Agent 97/98
```

## 2. 发布物和审计记录

在任何现场写操作前记录：

```text
RUN_ID=<YYYYMMDD-HHMMSS>
DEPLOY_SHA=<40-character-git-sha>
BUNDLE_FILE=<absolute-path>
BUNDLE_SHA256=<sha256>
MEDIA_CORE_SHA256=<sha256>
MEDIA_AGENT_SHA256=<sha256>
MEDIA_GATEWAY_SHA256=<sha256>
FFMPEG_SHA256=<sha256>
FFPROBE_SHA256=<sha256>
```

构建门禁：

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
./scripts/build-native-bundle.sh --without-gpu
./scripts/verify-native-bundle-on-target.sh --bundle <bundle> --host <target>
```

目标机必须先执行 `sha256sum -c SHA256SUMS`。97、98、42 上的业务二进制哈希必须与发布记录一致；任一不一致立即停止。

## 3. 固定配置

### 3.1 97 Core

基础功能验收阶段先保持 `SOURCE_GATEWAY_BASE_URL` 为空。基础功能通过后写入：

```ini
SOURCE_GATEWAY_BASE_URL=https://172.21.26.25/bohui/media/
SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY=true
SOURCE_GATEWAY_PREFETCH_POLL_MS=1000
SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS=600000
```

该开关只跳过 Core 到 Gateway 的证书链、有效期和主机名验证。Core 启动时必须只出现一次：

```text
SOURCE_GATEWAY TLS verification is disabled for 172.21.26.25
```

### 3.2 42 Media Gateway

`/opt/streamserver-gateway/.env`：

```ini
MEDIA_GATEWAY_BIND_ADDR=172.21.26.42:18081
MEDIA_GATEWAY_PUBLIC_BASE_URL=https://172.21.26.25/bohui/media
MEDIA_GATEWAY_WORK_ROOT=/mnt/nfs/streamserver-work
MEDIA_GATEWAY_FFMPEG_BIN=/opt/streamserver-gateway/bin/ffmpeg
```

`/etc/systemd/system/streamserver-gateway.service`：

```ini
[Unit]
Description=StreamServer Media Gateway
Wants=network-online.target
After=network-online.target
RequiresMountsFor=/mnt/nfs

[Service]
Type=simple
User=streamserver-gateway
Group=streamserver-gateway
SupplementaryGroups=nfs-media-rw
EnvironmentFile=/opt/streamserver-gateway/.env
ExecStart=/opt/streamserver-gateway/bin/media-gateway
Restart=on-failure
RestartSec=3s
UMask=0007
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/mnt/nfs/streamserver-work

[Install]
WantedBy=multi-user.target
```

42 上不创建 18082 listener；Nginx 必须保持停用。网络侧只允许 25 访问 42 的 18081，该项由客户网络实施，应用验收时只记录结果。

### 3.3 94 Work Gateway

复用 `/home/avqual/nginx/sbin/nginx` 和 `avqual-nginx.service`。在现有 `http {}` 中只增加一次：

```nginx
include /home/avqual/nginx/conf/conf.d/*.conf;
```

新增 `/home/avqual/nginx/conf/conf.d/work-gateway.conf`：

```nginx
server {
    listen 7000 default_server;
    server_name _;

    access_log /home/avqual/nginx/logs/work-gateway.access.log;
    error_log  /home/avqual/nginx/logs/work-gateway.error.log warn;
    client_max_body_size 1m;

    proxy_http_version 1.1;
    proxy_set_header Host              $http_host;
    proxy_set_header X-Real-IP         $http_x_real_ip;
    proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto https;
    proxy_set_header Connection        "";

    location = /healthz {
        access_log off;
        default_type text/plain;
        return 200 "ok\n";
    }

    location /llm/ {
        proxy_buffering off;
        proxy_request_buffering off;
        proxy_cache off;
        gzip off;
        proxy_connect_timeout 10s;
        proxy_send_timeout 24h;
        proxy_read_timeout 24h;
        send_timeout 24h;
        rewrite ^/llm/(.*)$ /$1 break;
        proxy_pass http://172.31.243.99:25104;
    }

    location / { return 404; }
}
```

先运行配置检查，再 reload；不得启动第二套 Nginx。50051、8080 保持空闲，未来服务只能新增固定前缀到固定 upstream。

### 3.4 客户 25 双路由

由客户在原 HTTPS `server` 内加入：

```nginx
location /bohui/media/ {
    proxy_buffering off;
    proxy_request_buffering off;
    proxy_cache off;
    gzip off;
    proxy_connect_timeout 10s;
    proxy_send_timeout 24h;
    proxy_read_timeout 24h;
    send_timeout 24h;
    rewrite ^/bohui/media/(.*)$ /$1 break;
    proxy_pass http://172.21.26.42:18081;
}

location /bohui/work/ {
    proxy_buffering off;
    proxy_request_buffering off;
    proxy_cache off;
    gzip off;
    proxy_connect_timeout 10s;
    proxy_send_timeout 24h;
    proxy_read_timeout 24h;
    send_timeout 24h;
    rewrite ^/bohui/work/(.*)$ /$1 break;
    proxy_pass http://172.31.243.94:7000;
}
```

客户 Nginx 自身仍必须能加载证书和私钥并完成 TLS 握手；Core 的跳过校验开关不能绕过 Nginx 证书缺失或 TLS 服务未启动。

## 4. 共享存储

沿用：

```text
97/98: 172.31.243.95:/media/media -> /home/streamserver/data/zlm/www/output
42:    172.31.243.95:/media/media -> /mnt/nfs
```

实施规则：

1. 在 97、98、42 核对 GID 1029 未被其他本地组占用后创建 `nfs-media-rw`。
2. 97、98 的服务用户和 42 的 `streamserver-gateway` 加入该组，重新登录或重启服务使补充组生效。
3. 只通过现有挂载创建 `streamserver-work`，目录组设为 1029，权限至少 `2770`；不递归 `chown` 整个 5.8 TB 目录树。
4. 97、98 的 fstab 不变；42 只把现场已经验证的 `/mnt/nfs` source、fstype 和 options 原样持久化。
5. 97、98 的 ZLM unit 增加：

```ini
[Service]
InaccessiblePaths=/home/streamserver/data/zlm/www/output/streamserver-work
```

迁移 97、98 原 `work` 前，生成相对路径、大小和 SHA256 清单。目标已有同名文件时：内容相同可跳过，内容不同必须终止，禁止覆盖。复制完成后对比文件数、总字节数和上传文件 SHA256。本地旧目录不改名、不删除，至少保留到验收后 7 天。

## 5. 停止、备份和数据库安全迁移

停止顺序：94 Avqual，97 Core/Agent/ZLM，98 Agent/ZLM，最后 PostgreSQL。停止后再次确认相关 PID 和 listener 均消失。

备份必须同时包含：

- `pg_dumpall --globals-only`、业务库 `pg_dump -Fc` 和成功的 `pg_restore --list`。
- PostgreSQL 停库后的物理数据目录副本。
- 97、98 的 `/home/streamserver`、环境文件、systemd unit、fstab、mount 和 ACL 清单。
- 94 的 Avqual 配置、系统 CA、完整 Nginx 配置和 unit。
- 42 的 fstab、Nginx状态及后续 Gateway 配置。
- 所有主机升级前的 `ss -lntup`、服务状态、版本和 SHA256 清单。

只启动 PostgreSQL 后执行当前 `DEPLOY_SHA` 的全部 migration，并由安装器生成独立的 JWT Ed25519、Agent 签发 CA、Core 控制面 CA、管理客户端 CA、capability 签名密钥和 Core HTTP/gRPC 证书。Core HTTP/gRPC 证书 SAN 必须包含 `172.31.243.97`、`localhost`、`127.0.0.1`、`::1`。

创建一次性管理员密码并要求首次登录修改。机器接口白名单最终只能有 `172.31.243.94/32`。撤销历史节点 `02172e33-39f0-4cf9-8352-86a4105d3a7f` 的有效凭据，但保留历史数据库记录。

临时 Core 使用 HTTPS 18443、gRPC 15051，依次用 10 分钟一次性 token 注册：

| 主机 | 角色 | 节点 ID |
| --- | --- | --- |
| 97 | `all-in-one-host-cpu` | `93e3d554-6676-4b2d-9388-f5450489f859` |
| 98 | `worker-host-cpu` | `75852b53-f5fb-4228-8cf4-4816e558169e` |

token 只能经标准输入传给 enrollment 命令，不得进入 argv、环境文件、shell history 或日志。注册完成后停止临时 Core，再在全部业务仍停止的状态下先升级 98、后升级 97。

94 只安装 Core 控制面 CA、刷新系统信任库，并把 Avqual `app.toml` 的 `base_url` 改为 `https://172.31.243.97:8080`；其他地址和参数保持原值。

## 6. 启动顺序

1. 核对三台共享目录为同一 NFS inode/文件集，启动 42 Gateway。
2. 核对 42 无 Nginx、只监听 `172.21.26.42:18081`。
3. 检查并 reload 94 现有 Nginx，先本机验证 7000 `/healthz` 和 `/llm/`。
4. 客户应用 25 双路由并回传配置检查、reload 和两个入口的结果。
5. 保持 Core 的 `SOURCE_GATEWAY_BASE_URL` 为空，依次启动 97、98、94，完成数据库、登录、节点、ZLM、Avqual 基础验收。
6. 写入最终四项 `SOURCE_GATEWAY_*`，只重启 97 Core。
7. 完成 Gateway 直播 relay、点播预取、时间切片和 Work LLM 验收。

## 7. 验收清单

- 42：仅 Gateway 监听 18081，Nginx inactive/disabled，18082 未使用。
- 94：原 80/443/12000/8082 保持，新增 7000；50051、8080 未占用；Avqual 原页面和 `/api/` 正常。
- Core 配置无 Gateway 域名和 42 直连地址；启动日志包含一次 TLS 风险警告。
- 同一 25 入口在跳过开关为 `false` 时因当前证书条件失败，为 `true` 时成功。
- `curl -k https://172.21.26.25/bohui/media/api/healthz` 和 `/bohui/work/healthz` 成功。
- `/bohui/work/llm/...` 到达 99、查询参数不丢失；未定义 Work 路径返回 404。
- 直播返回 `https://172.21.26.25/bohui/media/relay/...`，Agent 能拉取并完成任务；停止任务后 relay 删除。
- HTTP 点播、时间切片正常，`imports/{task_id}` 在共享目录生成，97/98 哈希一致。
- 98 原上传文件数量和 SHA256 不变，ZLM 无法通过 HTTP 读取 `streamserver-work`。
- 97/98 只使用新节点身份，历史身份不能连接；Core/Agent mTLS 正常。
- 94 通过可信 HTTPS 访问 Core，非 94 来源的机器接口被拒绝。
- FFmpeg/FFprobe 生成参数中不存在 `-tls_verify 1`、`-ca_file`、`-verifyhost`，二进制帮助仍显示 TLS verify 默认 false。
- 三台服务器重启后 NFS 先挂载、服务后启动；42 Gateway 不会在 NFS 缺失时写入本地同名目录。

## 8. 回滚

Gateway 单项失败：清空 `SOURCE_GATEWAY_BASE_URL`，只重启 Core，停止 42 Gateway；保留基础版本、安全链和 94 Work Gateway。

94 Work Gateway 失败：恢复备份的 Nginx 主配置，删除 `work-gateway.conf`，配置检查通过后 reload，确认 80/443 正常且 7000 不再监听。

整体失败：停止全部新服务，恢复 PostgreSQL 物理备份或已验证的逻辑备份，恢复 97/98 原二进制、配置、systemd 和原本地 `WORK_ROOT`，恢复 94 原 CA/配置/Nginx，停止 42 Gateway并保持无 Nginx。旧本地 work 和共享新目录均不立即删除；由于业务尚未运行，失败后保持全部服务停止，查明原因后从备份检查点重新执行。
