create index if not exists idx_task_events_runtime_latest
  on task_events(task_id, created_at desc, id desc)
  where event_type in ('stream_no_reader', 'stream_publish_requested', 'running');
