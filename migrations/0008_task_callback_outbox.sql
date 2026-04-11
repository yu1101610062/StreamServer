create table if not exists task_callback_outbox (
  id uuid primary key,
  task_id uuid not null references tasks(id) on delete cascade,
  attempt_id uuid references task_attempts(id) on delete set null,
  attempt_no integer not null,
  callback_url text not null,
  event_type text not null,
  reason text not null,
  status text not null check (status in ('pending', 'retrying', 'delivered', 'dead')),
  delivery_attempts integer not null default 0,
  deliver_after timestamptz not null,
  last_error text,
  last_http_status integer,
  last_response_body text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  delivered_at timestamptz
);

create index if not exists idx_task_callback_outbox_due
  on task_callback_outbox(status, deliver_after asc, created_at asc);

create index if not exists idx_task_callback_outbox_task_created_desc
  on task_callback_outbox(task_id, created_at desc);

create unique index if not exists idx_task_callback_outbox_pending_unique
  on task_callback_outbox(task_id, attempt_no, event_type, reason)
  where status in ('pending', 'retrying');
