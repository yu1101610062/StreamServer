create index if not exists idx_stream_bindings_attempt_id
  on stream_bindings(attempt_id);

create index if not exists idx_record_files_attempt_id
  on record_files(attempt_id);

create index if not exists idx_transcode_artifacts_attempt_id
  on transcode_artifacts(attempt_id);

create index if not exists idx_task_events_attempt_id
  on task_events(attempt_id);

create index if not exists idx_task_checkpoints_task_id
  on task_checkpoints(task_id);

create index if not exists idx_task_checkpoints_attempt_id
  on task_checkpoints(attempt_id);

create index if not exists idx_task_callback_outbox_attempt_id
  on task_callback_outbox(attempt_id);
