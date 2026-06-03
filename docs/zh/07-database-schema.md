# 07. 数据库设计与 DDL

## 1. 文档目标

本文件定义 PostgreSQL 真相库的核心表结构、约束、索引与迁移规则。开发和联调都以此为准；SQLite 仅用于本地演示，不要求完全覆盖 PostgreSQL 特性。

## 2. 设计原则

- 所有任务、尝试、租约、事件、录像、能力快照都落 PostgreSQL。
- 主键使用应用层生成的 `UUIDv7`。
- 任务规格使用 `jsonb`，但状态、类型、绑定关系、索引字段必须结构化。
- 时间统一使用 `timestamptz`。

## 3. 枚举类型

```sql
create type task_type as enum (
  'stream_ingest',
  'stream_bridge',
  'file_transcode'
);

create type task_status as enum (
  'CREATED',
  'VALIDATING',
  'QUEUED',
  'DISPATCHING',
  'STARTING',
  'RUNNING',
  'STOPPING',
  'RECOVERING',
  'SUCCEEDED',
  'FAILED',
  'CANCELED',
  'LOST'
);

create type attempt_status as enum (
  'PENDING',
  'STARTING',
  'RUNNING',
  'STOPPING',
  'SUCCEEDED',
  'FAILED',
  'ADOPTED',
  'ORPHANED'
);

create type worker_kind as enum (
  'zlm_proxy',
  'ffmpeg',
  'zlm_rtp_server',
  'hybrid'
);

create type event_source as enum (
  'core',
  'agent',
  'ffmpeg',
  'zlm_api',
  'zlm_hook',
  'scheduler',
  'user'
);
```

## 4. 核心表结构

### 4.1 `media_nodes`

```sql
create table media_nodes (
  id uuid primary key,
  node_name text not null unique,
  hostname text not null,
  labels jsonb not null default '[]'::jsonb,
  zlm_api_base text not null,
  agent_stream_addr text not null,
  network_mode text not null check (network_mode in ('bridge', 'host', 'macvlan')),
  interfaces jsonb not null default '[]'::jsonb,
  healthy boolean not null default false,
  last_seen_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);
```

### 4.2 `node_capabilities`

```sql
create table node_capabilities (
  node_id uuid primary key references media_nodes(id) on delete cascade,
  ffmpeg_protocols jsonb not null default '[]'::jsonb,
  ffmpeg_formats jsonb not null default '[]'::jsonb,
  ffmpeg_encoders jsonb not null default '[]'::jsonb,
  ffmpeg_decoders jsonb not null default '[]'::jsonb,
  zlm_api_list jsonb not null default '[]'::jsonb,
  zlm_version text,
  gpu jsonb not null default '[]'::jsonb,
  captured_at timestamptz not null default now()
);
```

### 4.3 `tasks`

```sql
create table tasks (
  id uuid primary key,
  name text not null,
  type task_type not null,
  status task_status not null,
  idempotency_key text not null,
  priority integer not null default 50 check (priority between 0 and 100),
  requested_spec jsonb not null,
  resolved_spec jsonb,
  created_by text not null,
  assigned_node_id uuid references media_nodes(id),
  current_attempt_no integer not null default 0,
  schedule_start_mode text not null default 'immediate' check (schedule_start_mode in ('immediate', 'manual', 'cron', 'at')),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  started_at timestamptz,
  finished_at timestamptz,
  unique (tenant_id, idempotency_key)
);
```

### 4.5 `task_attempts`

```sql
create table task_attempts (
  id uuid primary key,
  task_id uuid not null references tasks(id) on delete cascade,
  attempt_no integer not null,
  node_id uuid references media_nodes(id),
  worker_kind worker_kind not null,
  status attempt_status not null,
  pid integer,
  zlm_key text,
  zlm_schema text,
  zlm_vhost text,
  zlm_app text,
  zlm_stream text,
  rtp_port integer,
  exit_code integer,
  failure_code text,
  failure_reason text,
  checkpoint_json jsonb,
  started_at timestamptz,
  ended_at timestamptz,
  created_at timestamptz not null default now(),
  unique (task_id, attempt_no)
);
```

### 4.6 `task_leases`

```sql
create table task_leases (
  task_id uuid primary key references tasks(id) on delete cascade,
  holder text not null,
  lease_token text not null,
  node_id uuid references media_nodes(id),
  expires_at timestamptz not null,
  updated_at timestamptz not null default now()
);
```

### 4.7 `stream_bindings`

```sql
create table stream_bindings (
  id uuid primary key,
  task_id uuid not null references tasks(id) on delete cascade,
  attempt_id uuid not null references task_attempts(id) on delete cascade,
  schema text not null,
  vhost text not null,
  app text not null,
  stream text not null,
  zlm_proxy_key text,
  zlm_pusher_key text,
  rtp_stream_id text,
  created_at timestamptz not null default now(),
  unique (schema, vhost, app, stream)
);
```

### 4.8 `record_files`

```sql
create table record_files (
  id uuid primary key,
  task_id uuid not null references tasks(id) on delete cascade,
  attempt_id uuid references task_attempts(id) on delete set null,
  vhost text,
  app text,
  stream text,
  file_path text not null,
  http_url text,
  file_size bigint not null default 0,
  time_len integer,
  start_time timestamptz,
  source text not null,
  created_at timestamptz not null default now(),
  unique (file_path)
);
```

### 4.9 `transcode_artifacts`

```sql
create table transcode_artifacts (
  id uuid primary key,
  task_id uuid not null references tasks(id) on delete cascade,
  attempt_id uuid references task_attempts(id) on delete set null,
  node_id uuid not null references media_nodes(id),
  file_name text not null,
  file_path text not null unique,
  http_url text not null,
  file_size bigint not null default 0,
  created_at timestamptz not null default now()
);
```

### 4.10 `task_events`

```sql
create table task_events (
  id uuid primary key,
  task_id uuid not null references tasks(id) on delete cascade,
  attempt_id uuid references task_attempts(id) on delete set null,
  attempt_no integer,
  source event_source not null,
  event_type text not null,
  event_level text not null check (event_level in ('debug', 'info', 'warn', 'error')),
  dedup_key text,
  payload jsonb not null default '{}'::jsonb,
  created_at timestamptz not null default now()
);
```

### 4.11 `task_callback_outbox`

```sql
create table task_callback_outbox (
  id uuid primary key,
  task_id uuid not null references tasks(id) on delete cascade,
  attempt_id uuid references task_attempts(id) on delete set null,
  attempt_no integer not null,
  callback_url text not null,
  event_type text not null,
  reason text not null,
  status text not null check (status in ('pending', 'retrying', 'delivered', 'dead')),
  delivery_attempts integer not null default 0,
  deliver_after timestamptz not null,
  last_error text,
  last_http_status integer,
  last_response_body text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  delivered_at timestamptz
);
```

### 4.12 `task_checkpoints`

```sql
create table task_checkpoints (
  id uuid primary key,
  task_id uuid not null references tasks(id) on delete cascade,
  attempt_id uuid not null references task_attempts(id) on delete cascade,
  offset_ms bigint,
  segment_no integer,
  extra_json jsonb not null default '{}'::jsonb,
  created_at timestamptz not null default now()
);
```

### 4.13 `operation_requests`

用于所有写接口的幂等记录。

```sql
create table operation_requests (
  id uuid primary key,
  tenant_id text not null,
  operation_key text not null,
  method text not null,
  path text not null,
  request_hash text not null,
  resource_type text,
  resource_id uuid,
  response_status integer,
  response_body jsonb,
  created_at timestamptz not null default now(),
  unique (tenant_id, operation_key, method, path)
);
```

### 4.14 `hook_events`

保存原始 Hook 负载，便于幂等和审计。

```sql
create table hook_events (
  id uuid primary key,
  server_id text not null,
  hook_name text not null,
  dedup_key text not null unique,
  payload jsonb not null,
  received_at timestamptz not null default now(),
  processed_at timestamptz
);
```

## 5. 索引

```sql
create index idx_tasks_status_priority_created_at
  on tasks(status, priority desc, created_at asc);

create index idx_tasks_assigned_node_status
  on tasks(assigned_node_id, status);

create index idx_task_attempts_task_attempt_no_desc
  on task_attempts(task_id, attempt_no desc);

create index idx_task_events_task_created_desc
  on task_events(task_id, created_at desc);

create index idx_task_callback_outbox_due
  on task_callback_outbox(status, deliver_after asc, created_at asc);

create index idx_task_callback_outbox_task_created_desc
  on task_callback_outbox(task_id, created_at desc);

create index idx_record_files_task_start_time_desc
  on record_files(task_id, start_time desc nulls last);

create index idx_transcode_artifacts_task_created_desc
  on transcode_artifacts(task_id, created_at desc);

create index idx_transcode_artifacts_created_desc
  on transcode_artifacts(created_at desc);

create index idx_stream_bindings_task_id
  on stream_bindings(task_id);
```

## 6. 数据约束

- `tasks.resolved_spec` 在 `CREATED` 状态可为空，其余状态必须非空。
- `tasks.idempotency_key` 保存 `POST /tasks` 的请求幂等键；其他写操作的幂等记录写入 `operation_requests`。
- `task_attempts.attempt_no` 从 1 开始自增，不回收。
- `stream_bindings` 必须关联到具体 Attempt。
- `record_files.file_path` 全局唯一。
- `record_files.http_url` 允许为空，兼容历史录像和未携带 URL 的 Hook。
- `transcode_artifacts` 记录平台托管的文件产物；当前统一落在 `/data/zlm/www/output/mp4/...` 或 `/data/zlm/www/output/hls/...`。
- `task_callback_outbox` 用于异步任务回调（如 `task.status`、`task.completed`）；同一任务同一 Attempt 的同类待发送回调只保留一条未完成记录。
- `task_events.dedup_key` 允许为空；仅 Hook 或外部重复事件写入时使用。

## 7. 迁移策略

- 使用 `sqlx migrate` 管理 PostgreSQL 迁移。
- 当前仓库将控制面 schema 折叠为单一基线迁移：[migrations/0001_init.sql](../../migrations/0001_init.sql)。
- 重建环境时应直接从该基线初始化；旧数据库若要切换到新基线，建议先完成数据导出，再清库重建。
- 每次表结构变更仍必须伴随迁移脚本和文档更新；若继续维持单文件策略，就直接更新基线迁移内容。

## 8. SQLite 开发约束

SQLite 仅用于开发演示：

- 不使用 enum 类型，统一降级为 `text`。
- 不要求 100% 兼容 PostgreSQL JSONB 查询。
- 禁止把 SQLite 行为当作生产基线。
