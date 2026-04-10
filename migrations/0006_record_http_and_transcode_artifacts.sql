alter table record_files
  add column if not exists http_url text;

create table if not exists transcode_artifacts (
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

create index if not exists idx_transcode_artifacts_task_created_desc
  on transcode_artifacts(task_id, created_at desc);

create index if not exists idx_transcode_artifacts_created_desc
  on transcode_artifacts(created_at desc);
