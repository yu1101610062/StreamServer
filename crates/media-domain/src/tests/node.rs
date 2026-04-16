use super::*;

#[test]
fn network_mode_roundtrips() {
    let mode = NetworkMode::from_str("host").expect("mode should parse");
    assert_eq!(mode, NetworkMode::Host);
    assert_eq!(mode.to_string(), "host");
}

#[test]
fn runtime_state_roundtrips() {
    let state = RuntimeState::from_str("running").expect("state should parse");
    assert_eq!(state, RuntimeState::Running);
    assert_eq!(state.to_string(), "running");
}

#[test]
fn normalize_output_mount_relative_prefix_cleans_current_dir_segments() {
    assert_eq!(
        normalize_output_mount_relative_prefix("./output/mp4/./node-a")
            .expect("prefix should normalize"),
        "output/mp4/node-a"
    );
    assert_eq!(
        normalize_output_mount_relative_prefix("  output/hls  ").expect("prefix should trim"),
        "output/hls"
    );
}

#[test]
fn normalize_output_mount_relative_prefix_rejects_unsafe_paths() {
    assert!(
        normalize_output_mount_relative_prefix("../output/mp4")
            .expect_err("parent path should fail")
            .contains("parent")
    );
    assert!(
        normalize_output_mount_relative_prefix("/output/mp4")
            .expect_err("absolute path should fail")
            .contains("relative")
    );
}
