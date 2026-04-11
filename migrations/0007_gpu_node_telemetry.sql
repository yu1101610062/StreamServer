alter table node_capabilities
  add column if not exists gpu_devices jsonb not null default '[]'::jsonb;

alter table node_heartbeats
  add column if not exists gpu_runtime jsonb not null default '[]'::jsonb;
