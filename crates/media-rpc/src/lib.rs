pub mod control_plane {
    tonic::include_proto!("streamserver.controlplane");
}

pub const CONTROL_PLANE_FILE_DESCRIPTOR_SET: &[u8] =
    tonic::include_file_descriptor_set!("streamserver_control_plane_descriptor");
