alter table media_nodes
  add column if not exists agent_http_base_url text not null default '';
