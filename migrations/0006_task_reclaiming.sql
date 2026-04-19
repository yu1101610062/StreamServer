alter type task_status add value if not exists 'RECLAIMING';

alter table tasks
  add column if not exists reclaim_deadline_at timestamptz;
