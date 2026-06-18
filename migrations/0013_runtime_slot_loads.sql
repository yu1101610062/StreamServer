alter table node_heartbeats
  add column if not exists runtime_slot_loads jsonb not null default '[]'::jsonb;

alter table node_heartbeats
  drop column if exists slot_usage;
