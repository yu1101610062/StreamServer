alter table media_nodes
  add column if not exists zlm_api_secret text not null default '';
