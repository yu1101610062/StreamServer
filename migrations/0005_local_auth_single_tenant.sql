do $$
begin
  if exists (
    select idempotency_key
      from tasks
     group by idempotency_key
    having count(*) > 1
  ) then
    raise exception 'cannot migrate to single-tenant mode: duplicate tasks.idempotency_key values exist across tenants';
  end if;

  if exists (
    select operation_key, method, path
      from operation_requests
     group by operation_key, method, path
    having count(*) > 1
  ) then
    raise exception 'cannot migrate to single-tenant mode: duplicate operation_requests(operation_key, method, path) values exist across tenants';
  end if;
end
$$;

create table if not exists auth_users (
  id uuid primary key,
  username text not null unique,
  password_hash text not null,
  role text not null check (role in ('admin')),
  enabled boolean not null default true,
  must_change_password boolean not null default false,
  last_login_at timestamptz,
  password_changed_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table if not exists auth_refresh_sessions (
  id uuid primary key,
  user_id uuid not null references auth_users(id) on delete cascade,
  token_hash text not null unique,
  expires_at timestamptz not null,
  revoked_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  last_used_at timestamptz,
  client_ip inet,
  user_agent text
);

create table if not exists machine_api_allowlist (
  id uuid primary key,
  cidr cidr not null unique,
  description text not null default '',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table if not exists security_audit_events (
  id uuid primary key,
  event_type text not null,
  actor text not null,
  subject text,
  remote_ip inet,
  user_agent text,
  payload jsonb not null default '{}'::jsonb,
  created_at timestamptz not null default now()
);

alter table tasks
  drop constraint if exists tasks_tenant_id_idempotency_key_key;

alter table operation_requests
  drop constraint if exists operation_requests_tenant_id_operation_key_method_path_key;

alter table tasks
  drop column if exists tenant_id;

alter table operation_requests
  drop column if exists tenant_id;

alter table tasks
  add constraint tasks_idempotency_key_key unique (idempotency_key);

alter table operation_requests
  add constraint operation_requests_operation_key_method_path_key unique (operation_key, method, path);

create index if not exists idx_auth_refresh_sessions_user_id
  on auth_refresh_sessions(user_id);

create index if not exists idx_auth_refresh_sessions_expires_at
  on auth_refresh_sessions(expires_at);

create index if not exists idx_security_audit_events_created_at
  on security_audit_events(created_at desc);
