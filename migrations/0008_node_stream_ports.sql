alter table media_nodes
  add column if not exists zlm_rtmp_port integer not null default 1935,
  add column if not exists zlm_rtsp_port integer not null default 554;

alter table media_nodes
  drop constraint if exists media_nodes_zlm_rtmp_port_check,
  drop constraint if exists media_nodes_zlm_rtsp_port_check;

alter table media_nodes
  add constraint media_nodes_zlm_rtmp_port_check
    check (zlm_rtmp_port between 1 and 65535),
  add constraint media_nodes_zlm_rtsp_port_check
    check (zlm_rtsp_port between 1 and 65535);
