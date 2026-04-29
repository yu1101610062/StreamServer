alter table node_heartbeats
  add column if not exists upload_disk_total_bytes bigint not null default 0
    check (upload_disk_total_bytes >= 0),
  add column if not exists upload_disk_available_bytes bigint not null default 0
    check (upload_disk_available_bytes >= 0),
  add column if not exists upload_disk_used_percent double precision not null default 0;

create table if not exists media_upload_assets (
  id uuid primary key,
  node_id uuid not null references media_nodes(id),
  file_name text not null,
  source_url text not null unique,
  http_url text not null,
  duration_sec bigint not null default 0 check (duration_sec >= 0),
  file_size bigint not null default 0 check (file_size >= 0),
  sha256 text not null,
  content_type text not null,
  status text not null default 'active' check (status in ('active', 'deleted')),
  file_deleted boolean not null default false,
  created_by text not null default '',
  created_at timestamptz not null default now(),
  deleted_by text,
  deleted_at timestamptz
);

create index if not exists idx_media_upload_assets_node_created_desc
  on media_upload_assets(node_id, created_at desc);

create index if not exists idx_media_upload_assets_status_created_desc
  on media_upload_assets(status, created_at desc);

create index if not exists idx_media_upload_assets_sha256
  on media_upload_assets(sha256);
