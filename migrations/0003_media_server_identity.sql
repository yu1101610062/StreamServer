create table if not exists media_servers (
  server_id text primary key,
  node_id uuid not null references media_nodes(id),
  last_seen_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

insert into media_servers (server_id, node_id, last_seen_at, created_at, updated_at)
select distinct
  sb.server_id,
  sb.node_id,
  now(),
  now(),
  now()
from stream_bindings sb
where coalesce(sb.server_id, '') <> ''
  and sb.node_id is not null
on conflict (server_id) do update
   set node_id = excluded.node_id,
       last_seen_at = excluded.last_seen_at,
       updated_at = excluded.updated_at;
