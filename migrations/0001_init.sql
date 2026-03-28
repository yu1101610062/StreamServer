create type task_type as enum (
  'live_relay',
  'file_transcode',
  'file_to_live',
  'multicast_bridge',
  'rtp_receive'
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

create table task_templates (
  id uuid primary key,
  name text not null unique,
  type task_type not null,
  profile text,
  default_spec jsonb not null,
  enabled boolean not null default true,
  created_by text not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table tasks (
  id uuid primary key,
  tenant_id text not null default 'default',
  name text not null,
  type task_type not null,
  status task_status not null,
  template_id uuid references task_templates(id),
  profile text,
  idempotency_key text not null,
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
  finished_at timestamptz,
  unique (tenant_id, idempotency_key)
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
  file_size bigint not null default 0,
  time_len integer,
  start_time timestamptz,
  source text not null,
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

create table hook_events (
  id uuid primary key,
  server_id text not null,
  hook_name text not null,
  dedup_key text not null unique,
  payload jsonb not null,
  received_at timestamptz not null default now(),
  processed_at timestamptz
);

create index idx_tasks_status_priority_created_at
  on tasks(status, priority desc, created_at asc);

create index idx_tasks_assigned_node_status
  on tasks(assigned_node_id, status);

create index idx_task_attempts_task_attempt_no_desc
  on task_attempts(task_id, attempt_no desc);

create index idx_task_events_task_created_desc
  on task_events(task_id, created_at desc);

create index idx_record_files_task_start_time_desc
  on record_files(task_id, start_time desc nulls last);

create index idx_stream_bindings_task_id
  on stream_bindings(task_id);
