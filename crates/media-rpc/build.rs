fn main() -> Result<(), Box<dyn std::error::Error>> {
    let descriptor_path =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"))
            .join("streamserver_control_plane_descriptor.bin");
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .file_descriptor_set_path(descriptor_path)
        // Register intentionally carries the complete, bounded capability
        // snapshot while the remaining envelope variants are much smaller.
        // Boxing it would change the generated public API throughout both
        // peers without changing the protobuf wire format, so keep the API
        // stable and localize this representation-only lint exemption.
        // prost-build currently cannot address a generated nested oneof by a
        // stable fully-qualified selector, so apply the lint allowance to the
        // generated protocol types as a group. It has no runtime or wire effect.
        .type_attribute(".", "#[allow(clippy::large_enum_variant)]")
        .compile_protos(&["../../proto/control_plane.proto"], &["../../proto"])?;

    println!("cargo:rerun-if-changed=../../proto/control_plane.proto");
    Ok(())
}
