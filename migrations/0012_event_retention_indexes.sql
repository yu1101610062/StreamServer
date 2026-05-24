create index if not exists idx_task_events_log_lookup
  on task_events(task_id, attempt_no, created_at desc, id desc)
  where event_type = 'task_log_batch';

create index if not exists idx_task_events_created_brin
  on task_events using brin(created_at);

create index if not exists idx_hook_events_received_desc
  on hook_events(received_at desc, id desc);

create index if not exists idx_hook_events_received_brin
  on hook_events using brin(received_at);
