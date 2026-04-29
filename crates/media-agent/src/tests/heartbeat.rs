use super::*;

#[test]
fn slot_usage_is_capped() {
    let mut sampler = HeartbeatSampler::new(".", 2, None);
    sampler.previous_cpu = Some(CpuCounters { total: 10, idle: 5 });

    let heartbeat = sampler.sample(10, 0, 0, 0, true, true, Vec::new());
    assert_eq!(heartbeat.slot_usage, 1.0);
}

#[test]
fn slot_usage_counts_starting_stopping_and_orphaned_runtimes() {
    let mut sampler = HeartbeatSampler::new(".", 4, None);
    sampler.previous_cpu = Some(CpuCounters { total: 10, idle: 5 });

    let heartbeat = sampler.sample(1, 1, 1, 1, true, true, Vec::new());
    assert_eq!(heartbeat.slot_usage, 1.0);

    let partial = sampler.sample(1, 1, 0, 0, true, true, Vec::new());
    assert_eq!(partial.slot_usage, 0.5);
}

#[test]
fn heartbeat_reports_upload_disk_for_work_root() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-heartbeat-upload-disk-{}",
        uuid::Uuid::now_v7()
    ));
    std::fs::create_dir_all(&temp_root).expect("temp root should be created");
    let mut sampler = HeartbeatSampler::new(temp_root.to_string_lossy(), 2, None);
    sampler.previous_cpu = Some(CpuCounters { total: 10, idle: 5 });

    let heartbeat = sampler.sample(0, 0, 0, 0, true, true, Vec::new());

    assert!(heartbeat.upload_disk_total_bytes > 0);
    assert!(heartbeat.upload_disk_available_bytes > 0);
    assert!(heartbeat.upload_disk_available_bytes <= heartbeat.upload_disk_total_bytes);
    assert!(heartbeat.upload_disk_used_percent >= 0.0);

    let _ = std::fs::remove_dir_all(&temp_root);
}
