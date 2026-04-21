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

create table media_nodes (
  id uuid primary key,
  node_name text not null,
  hostname text not null,
  labels jsonb not null default '[]'::jsonb,
  zlm_api_base text not null,
  zlm_api_secret text not null default '',
  agent_stream_addr text not null,
  zlm_rtmp_port integer not null default 1935
    check (zlm_rtmp_port between 1 and 65535),
  zlm_rtsp_port integer not null default 554
    check (zlm_rtsp_port between 1 and 65535),
  network_mode text not null check (network_mode in ('bridge', 'host', 'macvlan')),
  interfaces jsonb not null default '[]'::jsonb,
  healthy boolean not null default false,
  last_seen_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table node_capabilities (
  node_id uuid primary key references media_nodes(id) on delete cascade,
  ffmpeg_protocols jsonb not null default '[]'::jsonb,
  ffmpeg_formats jsonb not null default '[]'::jsonb,
  ffmpeg_encoders jsonb not null default '[]'::jsonb,
  ffmpeg_decoders jsonb not null default '[]'::jsonb,
  zlm_api_list jsonb not null default '[]'::jsonb,
  zlm_version text,
  gpu jsonb not null default '[]'::jsonb,
  gpu_devices jsonb not null default '[]'::jsonb,
  captured_at timestamptz not null default now()
);

create table node_heartbeats (
  id uuid primary key,
  node_id uuid not null references media_nodes(id) on delete cascade,
  cpu_percent double precision not null,
  mem_percent double precision not null,
  disk_percent double precision not null,
  running_tasks integer not null,
  slot_usage double precision not null,
  zlm_alive boolean not null,
  ffmpeg_alive boolean not null,
  node_time timestamptz not null,
  received_at timestamptz not null default now(),
  gpu_runtime jsonb not null default '[]'::jsonb
);

create table tasks (
  id uuid primary key,
  name text not null,
  type task_type not null,
  status task_status not null,
  idempotency_key text not null unique,
  priority integer not null default 50 check (priority between 0 and 100),
  requested_spec jsonb not null,
  resolved_spec jsonb,
  created_by text not null,
  assigned_node_id uuid references media_nodes(id),
  current_attempt_no integer not null default 0,
  schedule_start_mode text not null default 'immediate'
    check (schedule_start_mode in ('immediate', 'manual', 'cron', 'at')),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  started_at timestamptz,
  finished_at timestamptz
);

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

create table task_leases (
  task_id uuid primary key references tasks(id) on delete cascade,
  holder text not null,
  lease_token text not null,
  node_id uuid references media_nodes(id),
  expires_at timestamptz not null,
  updated_at timestamptz not null default now()
);

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

create table transcode_artifacts (
  id uuid primary key,
  task_id uuid not null references tasks(id) on delete cascade,
  attempt_id uuid references task_attempts(id) on delete set null,
  node_id uuid not null references media_nodes(id),
  file_name text not null,
  file_path text not null,
  http_url text not null,
  file_size bigint not null default 0,
  created_at timestamptz not null default now(),
  unique (file_path)
);

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

create table task_checkpoints (
  id uuid primary key,
  task_id uuid not null references tasks(id) on delete cascade,
  attempt_id uuid not null references task_attempts(id) on delete cascade,
  offset_ms bigint,
  segment_no integer,
  extra_json jsonb not null default '{}'::jsonb,
  created_at timestamptz not null default now()
);

create table operation_requests (
  id uuid primary key,
  operation_key text not null,
  method text not null,
  path text not null,
  request_hash text not null,
  resource_type text,
  resource_id uuid,
  response_status integer,
  response_body jsonb,
  created_at timestamptz not null default now(),
  unique (operation_key, method, path)
);

create table hook_events (
  id uuid primary key,
  server_id text not null,
  hook_name text not null,
  dedup_key text not null unique,
  payload jsonb not null,
  received_at timestamptz not null default now(),
  processed_at timestamptz
);

create table auth_users (
  id uuid primary key,
  username text not null unique,
  password_hash text not null,
  role text not null check (role in ('admin')),
  enabled boolean not null default true,
  must_change_password boolean not null default false,
  last_login_at timestamptz,
  password_changed_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table auth_refresh_sessions (
  id uuid primary key,
  user_id uuid not null references auth_users(id) on delete cascade,
  token_hash text not null unique,
  expires_at timestamptz not null,
  revoked_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  last_used_at timestamptz,
  client_ip inet,
  user_agent text
);

create table machine_api_allowlist (
  id uuid primary key,
  cidr cidr not null unique,
  description text not null default '',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table security_audit_events (
  id uuid primary key,
  event_type text not null,
  actor text not null,
  subject text,
  remote_ip inet,
  user_agent text,
  payload jsonb not null default '{}'::jsonb,
  created_at timestamptz not null default now()
);

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

create index idx_tasks_status_priority_created_at
  on tasks(status, priority desc, created_at asc);

create index idx_tasks_assigned_node_status
  on tasks(assigned_node_id, status);

create index idx_task_attempts_task_attempt_no_desc
  on task_attempts(task_id, attempt_no desc);

create index idx_stream_bindings_task_id
  on stream_bindings(task_id);

create index idx_record_files_task_start_time_desc
  on record_files(task_id, start_time desc nulls last);

create index idx_transcode_artifacts_task_created_desc
  on transcode_artifacts(task_id, created_at desc);

create index idx_transcode_artifacts_created_desc
  on transcode_artifacts(created_at desc);

create index idx_task_events_task_created_desc
  on task_events(task_id, created_at desc);

create index idx_node_heartbeats_node_received_desc
  on node_heartbeats(node_id, received_at desc);

create index idx_auth_refresh_sessions_user_id
  on auth_refresh_sessions(user_id);

create index idx_auth_refresh_sessions_expires_at
  on auth_refresh_sessions(expires_at);

create index idx_security_audit_events_created_at
  on security_audit_events(created_at desc);

create index idx_task_callback_outbox_due
  on task_callback_outbox(status, deliver_after asc, created_at asc);

create index idx_task_callback_outbox_task_created_desc
  on task_callback_outbox(task_id, created_at desc);

create unique index idx_task_callback_outbox_pending_unique
  on task_callback_outbox(task_id, attempt_no, event_type, reason)
  where status in ('pending', 'retrying');
