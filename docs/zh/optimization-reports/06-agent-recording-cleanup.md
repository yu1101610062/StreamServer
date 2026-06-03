这块可以收敛成一个很薄的 agent 本地能力，不用 manifest，不用碰 core 协议，也不用区分录像/转码/快录。

当前代码里产物已经是固定落到这两个根目录：

* `/data/zlm/www/output/mp4`
* `/data/zlm/www/output/hls`

而且目录结构也是固定的：

* `/data/zlm/www/output/mp4/node-<token>-mp4/<task_id>/...`
* `/data/zlm/www/output/hls/node-<token>-hls/<task_id>/...`

所以第一版就只围绕这两个根目录做。

## 最小方案

### 1. 配置项

建议直接挂到 agent 配置里：

```toml
[agent.artifact_cleanup]
enabled = true
threshold_percent = 85
strategy = "delete_oldest_then_reject" # 或 reject_only
check_interval_sec = 30
```

`strategy` 支持两种：

* `delete_oldest_then_reject`：超过阈值先删旧产物，删完还不够再拒绝新任务
* `reject_only`：严格禁止删旧产物，超过阈值就拒绝新任务

这样已经覆盖你说的“有的现场严禁删除已有录像哪怕任务失败”的场景了。

我不建议第一版再开放一堆额外参数。
内部固定两个小常量就够了：

* 回落区间：比如删到 `threshold - 5%` 就停，避免抖动
* 冷静期：比如最近 60 秒还在写的目录不参与删除

### 2. 只看 mp4 / hls 两个根目录所属的挂载

不要再看 `work_root`。

实现上就两步：

* `stat(root).st_dev`：判断 mp4 和 hls 是不是同一个 filesystem
* `statvfs(root)`：取总量/可用量/使用率

规则很简单：

* 如果 mp4 / hls 在不同挂载，就各管各的
* 如果它们其实在同一个挂载，就合并成一个 volume 处理

这个合并一定要做。
不然会出现“mp4 把盘占满了，但 hls 目录自己看起来还不大，结果 hls 任务还继续放行”的问题。

### 3. 清理范围只扫当前节点自己的目录

这一点很重要，不要扫整个 `/output/mp4` 或 `/output/hls`。

只扫：

* `node-<当前节点token>-mp4`
* `node-<当前节点token>-hls`

原因是这个目录结构本来就是按节点隔离的。
尤其如果后面有共享存储，不能误删别的节点的产物。

候选目录就是这些 node 目录下的直接子目录，也就是 `<task_id>` 目录。
再加一个保护：只处理目录名能解析成 UUID 的，其他一律跳过。

### 4. 删除顺序按“任务目录最近写入时间”升序

你说“按照文件更新时间正序删除”，删除单位又是任务文件夹，那排序键不要直接用目录 mtime，建议这样定义：

* 取 `<task_id>` 目录内所有直接子文件的 `mtime`
* 用其中最大的那个，作为这个任务目录的“最后写入时间”
* 按这个时间升序删，最老的先删

原因很简单：

* mp4 文件会持续追加写入
* hls 会不断新增 ts / 更新 m3u8
* 目录本身的 mtime 不可靠
* 用“目录内最新文件 mtime”更接近真实最后活跃时间

当前这套结构下，产物基本都直接平铺在 `<task_id>` 目录里，这么做够用了。

### 5. 活跃任务一定要跳过

虽然不想和业务强耦合，但这个保护必须有，不过可以做得很轻：

* 从 `LocalRuntimeRegistry` 取当前活跃任务的 `task_id`
* 扫描到同名 `<task_id>` 目录时直接跳过

因为现在产物目录本来就是按 `task_id` 命名，不需要查 DB，也不需要读复杂 metadata。

如果 mp4 / hls 在同一个 volume 上，我建议再做一个小优化：

* 同一个 `task_id` 在 mp4/hls 下如果都存在，合并成一个候选
* 真删的时候一起删

这样不会出现“同一个任务只删掉 hls，mp4 还留着一半”的情况。
实现也不复杂，因为目录名就是 task_id。

## 触发方式

### 后台定时清理

每 `check_interval_sec` 跑一次：

1. 先采样 mp4/hls 所属 volume 的使用率
2. 没超阈值就直接返回，不扫目录
3. 超阈值且策略是 `delete_oldest_then_reject`，才开始枚举候选并删除
4. 每删一个候选后重新 `statvfs`
5. 降到 `threshold - 5%` 就停；删光了还不够也停

这样平时开销很小，只有真的超阈值才会扫目录。

### 启动前准入检查

这个也要有，而且要放在 `accepted` 之前。

因为现在 `crates/media-agent/src/control_plane.rs` 的 `handle_start_task()` 是先发 `accepted`，再去 `executor.start_task()`。
如果拒绝逻辑放到后面，就会出现：

* 先 `accepted`
* 然后又 `start_rejected`

这个体验很差。

但这里不建议在热路径里重新 `statvfs` 或枚举目录。
启动前只做一件事：

1. 解析 `resolved_spec`
2. 算出这个任务会用到哪个 bucket
3. 映射到对应 volume
4. 读取后台线程维护的 volume 准入缓存状态
5. 通过了再发 `accepted`

也就是说，真正的磁盘采样和目录清理只由后台线程负责；
`handle_start_task()` 只做“按任务规格查缓存并拒绝/放行”。

另外需要一个保守约束：

* agent 启动后，在首个有效采样完成之前，相关产物任务默认拒绝
* bucket root 不可访问时，也直接把对应 bucket 标成不可接单

## 任务需要哪个 bucket，直接按现有规则推断

这一块不需要新概念，直接复用现有产物分配规则。

### stream_ingest

如果 `record.enabled = true`：

* `record.format = mp4` -> 需要 mp4
* `record.format = hls` -> 需要 hls
* `record.format = both` -> 两边都需要

### file_transcode / stream_bridge

只有 `publish.kind = file` 时才占产物盘。

然后 bucket 规则要和现在代码保持一致：

* `publish.format == "hls"` -> hls
* 其他格式一律都算 mp4 bucket

这个点要特别注意。
当前代码里的 “mp4 bucket” 实际意思是“非 hls 的文件输出 bucket”，不只是 `.mp4` 文件。
比如 mkv/flv/webm 也会落到 mp4 根目录里，所以清理和准入都应该按 bucket 走，不按扩展名走。

## 超阈值时的行为

### `reject_only`

最简单：

* 不删任何已有产物
* 相关 volume 超阈值就直接拒绝新任务
* 正在跑的任务不主动停

这就满足“现场严格禁止删除已有录像”的要求。

### `delete_oldest_then_reject`

流程是：

* 先删最老的任务目录
* 每删一个就重新检查 volume 使用率
* 还在阈值之上就继续删
* 没有可删的了，或者还是降不下来，就拒绝新任务

## 当前代码怎么落

我会只改 agent 这几处：

### `crates/media-agent/src/config.rs`

加一个很小的配置结构，比如：

* `enabled`
* `threshold_percent`
* `strategy`
* `check_interval_sec`

### 新增 `crates/media-agent/src/artifact_cleanup.rs`

这个模块做三件事：

* 识别 mp4/hls 所属 volume
* 周期采样和清理
* 维护 bucket/volume 的准入缓存状态，供启动前快速判断

建议内部做成同步逻辑，外层通过 `spawn_blocking` 调用，避免卡住 control-plane loop。

### `crates/media-agent/src/runtime.rs`

补几个可复用 helper 就够了：

* bucket root 获取
* 当前节点的 mp4/hls node 目录获取
* `required_buckets(spec)` 计算任务需要哪个 bucket

这样清理模块和运行时路径规则保持一致，不会写两套逻辑。

### `crates/media-agent/src/control_plane.rs`

加两处：

* controller 启动时先做一次同步 refresh，再起定时清理任务
* `handle_start_task()` 里在 `accepted` 之前读取缓存状态做 precheck，而不是现查磁盘

### `crates/media-agent/src/heartbeat.rs`

现在是采样 `work_root`。
改成采样 mp4/hls 对应 volume，并把 `disk_percent` 设成“最危险那个 volume 的使用率”。

这样不用改 proto，也不用改 core。

## 一个很值得顺手修的小点

当前输出目录的创建逻辑会 `create_dir_all(parent)`。
如果某个网络挂载掉了，而根路径又不存在，理论上有机会把目录创建到宿主本地盘上，这个很危险。

所以这里最好加一个保护：

* `/data/zlm/www/output/mp4`
* `/data/zlm/www/output/hls`

这两个 bucket root 必须先存在且可访问。
如果 bucket root 不存在，不要帮它自动创建，直接视为该 bucket 不可用，拒绝相关任务。

这样更符合“这是挂载点”的语义。

## 结论

这版完全可以做得很简单：

* 只管两个固定根目录
* 只按真实挂载分组
* 只删当前节点自己的 `<task_id>` 目录
* 只按目录内文件最后更新时间升序删
* 节点配置只保留“阈值 + 超阈值策略”
* 策略支持“删旧后继续”或“严格不删直接拒绝”

第一版没必要引入 manifest、tombstone、core 联动这些东西。
先把 agent 本地的“清理 + 准入”闭环做对，已经能解决绝大多数现场问题。

## 现场技术人员说明

下面这部分面向现场运维和实施人员，重点说明“系统会自动做什么”和“现场需要手工做什么”。

### 1. 管理范围

当前磁盘清理策略只管理两类固定产物目录：

* `mp4 bucket`：`/data/zlm/www/output/mp4`
* `hls bucket`：`/data/zlm/www/output/hls`

说明：

* `mp4 bucket` 不只是 `.mp4` 文件，所有“非 hls 的文件产物”都按这个 bucket 管理
* `hls bucket` 管理 HLS 相关产物

### 2. 节点级行为

每个 `streamserver-agent` 只管理自己节点名下的任务目录，不会清理其他节点的任务目录。

目录结构如下：

* `/data/zlm/www/output/mp4/node-<节点token>-mp4/<task_id>/...`
* `/data/zlm/www/output/hls/node-<节点token>-hls/<task_id>/...`

自动清理范围只限这两类 `<task_id>` 目录，且目录名必须能解析成 UUID。

不会被自动清理的内容包括：

* 不在 `node-<token>-*` 下面的目录
* 非 UUID 命名目录
* 其他节点的任务目录
* 现场手工放入的样片、测试文件、历史素材目录

也就是说，这不是“全盘清理器”，它只会处理“本节点自己产生的任务产物目录”。

### 3. 磁盘判断规则

系统不会以 `work_root` 作为录像清理依据，而是以实际产物挂载点为准。

判断逻辑：

1. 检查 `mp4` 和 `hls` bucket 所在文件系统
2. 如果在同一个挂载上，则按一个 volume 合并判断
3. 如果在不同挂载上，则分别判断

这意味着：

* `mp4` 满了，只会阻止需要写 `mp4 bucket` 的任务
* `hls` 满了，只会阻止需要写 `hls bucket` 的任务
* 两者在同一挂载时，会一起受限

### 4. 后台清理机制

后台线程是唯一的磁盘检查与清理执行者。

默认行为：

* agent 启动时立即检查一次
* 之后每 `30` 秒检查一次
* 未超阈值时，只更新状态，不扫目录
* 超阈值后，才开始枚举候选目录并清理

默认检查间隔对应配置：

```toml
[agent.artifact_cleanup]
check_interval_sec = 30
```

或环境变量：

```bash
AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC=30
```

### 5. 启动前准入机制

任务下发时，agent 不会实时重新扫盘，而是只读取后台维护的缓存状态。

行为如下：

* 相关 bucket 可接单：正常 `accepted`
* 相关 bucket 不可接单：直接 `start_rejected`
* 首次采样未完成前：保守拒绝相关产物任务
* bucket root 不可访问时：直接拒绝相关产物任务

这样做的目的是避免在任务热路径上频繁做磁盘扫描。

### 6. 支持的两种策略

配置项：

```toml
[agent.artifact_cleanup]
strategy = "delete_oldest_then_reject"
```

支持两个值：

`delete_oldest_then_reject`

* 超阈值后，先删最老的任务目录
* 每删一个就重新检查使用率
* 降到 `threshold - 5%` 就停止
* 如果删完还不够，则拒绝新任务

`reject_only`

* 超阈值后不删任何文件
* 直接拒绝新任务
* 适合严禁自动删除录像的现场

### 7. 默认配置

配置文件写法：

```toml
[agent.artifact_cleanup]
enabled = true
threshold_percent = 85
strategy = "delete_oldest_then_reject"
check_interval_sec = 30
```

环境变量写法：

```bash
AGENT_ARTIFACT_CLEANUP_ENABLED=true
AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT=85
AGENT_ARTIFACT_CLEANUP_STRATEGY=delete_oldest_then_reject
AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC=30
```

默认值说明：

* `enabled=true`
* `threshold_percent=85`
* `strategy=delete_oldest_then_reject`
* `check_interval_sec=30`

### 8. 存储布局

在线 HLS 使用 `/data/zlm/www/<app>/<stream>/...`，默认落在本地 `ZLM_WWW_HOST_DIR`。

录制、转码和桥接文件产物统一使用 `/data/zlm/www/output`，默认挂载自 `ZLM_OUTPUT_HOST_DIR`：

* MP4：`/data/zlm/www/output/mp4/node-<token>-mp4/<task_id>/...`
* HLS：`/data/zlm/www/output/hls/node-<token>-hls/<task_id>/...`

Core 返回业务 `file_path` 时剥掉 `output`，例如 `/mp4/node-.../...`；HTTP URL 仍保留 `/output/...`。

### 9. 任务与 bucket 的对应关系

`stream_ingest`

* `record.enabled=true`
* `record.format=mp4` -> 使用 `mp4`
* `record.format=hls` -> 使用 `hls`
* `record.format=both` -> 同时使用 `mp4` 和 `hls`

`file_transcode / stream_bridge`

* 仅当 `publish.kind=file` 时占产物盘
* `publish.format=hls` -> 使用 `hls`
* 其他格式 -> 使用 `mp4`

### 10. 删除优先级

自动清理时，删除顺序按“任务目录最后活跃时间”升序执行，最老的先删。

保护规则：

* 活跃任务目录不删
* 最近 `60` 秒内仍在写入的目录不删
* 同一 `task_id` 在 `mp4/hls` 下如果都存在，会作为一组一起删

### 11. 现场人员需要特别注意的限制

自动清理只会清理本节点的任务目录，不会处理以下内容：

* 现场手工放入的素材
* 测试文件
* 样片目录
* 非任务目录
* 其他节点目录

因此如果磁盘主要被“非任务目录”占满，自动清理不会生效，必须人工处理。

### 12. 建议的现场排查命令

先看挂载使用率：

```bash
df -h /data/zlm/www/output/mp4 /data/zlm/www/output/hls /data/media/work
```

看哪个目录最占：

```bash
du -xhd 3 /data/zlm/www/output/mp4 | sort -hr | head -n 30
du -xhd 3 /data/zlm/www/output/hls | sort -hr | head -n 30
```

看最大文件：

```bash
find /data/zlm/www/output/mp4 -xdev -type f -printf '%s\t%p\n' | sort -nr | head -n 30
find /data/zlm/www/output/hls -xdev -type f -printf '%s\t%p\n' | sort -nr | head -n 30
```

看 agent 日志里的清理结果：

```bash
journalctl -u <media-agent服务名> --since "10m"
```

重点关注这些日志：

* `artifact cleanup deleted old task directories`
* `artifact bucket is rejecting new tasks`
* `artifact bucket is accepting new tasks`

### 13. 现场调整建议

如果现场要求“绝对不能自动删录像”：

```bash
AGENT_ARTIFACT_CLEANUP_STRATEGY=reject_only
```

如果现场希望减少误拒绝，可适当调高阈值，例如：

```bash
AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT=90
```

如果现场磁盘波动大、希望检查更频繁：

```bash
AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC=10
```

不建议把检查间隔调得太低，`10~30` 秒比较合理。

### 14. 配置变更后的重启方式

修改 `.env` 后，统一使用：

```bash
systemctl restart <对应的streamserver服务名>
```

检查状态：

```bash
systemctl is-active <对应的streamserver服务名>
systemctl status <对应的streamserver服务名>
```

### 15. 经验判断

如果出现“某类文件任务被拒绝，但另一类仍正常”的情况，通常表示：

* 某个 bucket 所在挂载已超阈值
* 另一个 bucket 仍有空间，或者不在同一挂载

如果自动清理执行了但仍无法恢复接单，通常说明：

* 没有可删的旧任务目录了
* 真正占空间的是非任务目录
* 需要人工清理这些历史文件
