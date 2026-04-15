alter table task_attempts
  add column if not exists lease_token text not null default '',
  add column if not exists stop_requested_at timestamptz,
  add column if not exists stop_reason text,
  add column if not exists desired_terminal_status task_status;

update task_attempts ta
   set lease_token = tl.lease_token
  from tasks t
  join task_leases tl
    on tl.task_id = t.id
 where ta.task_id = t.id
   and ta.attempt_no = t.current_attempt_no
   and coalesce(ta.lease_token, '') = '';

alter table stream_bindings
  add column if not exists server_id text not null default '',
  add column if not exists node_id uuid references media_nodes(id);

update stream_bindings sb
   set node_id = coalesce(sb.node_id, ta.node_id, t.assigned_node_id),
       server_id = coalesce(nullif(sb.server_id, ''), ta.node_id::text, t.assigned_node_id::text, '')
  from task_attempts ta
  join tasks t
    on t.id = ta.task_id
 where ta.id = sb.attempt_id
   and t.id = sb.task_id;

alter table stream_bindings
  drop constraint if exists stream_bindings_schema_vhost_app_stream_key;

create unique index if not exists idx_stream_bindings_server_stream_unique
  on stream_bindings(server_id, schema, vhost, app, stream);

alter table media_nodes
  add column if not exists control_connected boolean not null default false,
  add column if not exists control_last_seen_at timestamptz,
  add column if not exists media_last_seen_at timestamptz;

update media_nodes
   set control_connected = healthy,
       control_last_seen_at = coalesce(control_last_seen_at, last_seen_at),
       media_last_seen_at = coalesce(media_last_seen_at, last_seen_at);

alter table node_heartbeats
  add column if not exists starting_tasks integer not null default 0,
  add column if not exists stopping_tasks integer not null default 0,
  add column if not exists orphaned_tasks integer not null default 0;
