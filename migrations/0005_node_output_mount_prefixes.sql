alter table media_nodes
  add column output_mount_relative_prefix_mp4 text not null default '',
  add column output_mount_relative_prefix_hls text not null default '';
