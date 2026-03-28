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
  received_at timestamptz not null default now()
);

create index idx_node_heartbeats_node_received_desc
  on node_heartbeats(node_id, received_at desc);
