use super::*;
use media_domain::{RuntimeSlotLoad, SourceMode};

#[test]
fn runtime_slot_loads_are_reported() {
    let mut sampler = HeartbeatSampler::new(".", None);
    sampler.previous_cpu = Some(CpuCounters { total: 10, idle: 5 });

    let heartbeat = sampler.sample(HeartbeatSampleInput {
        running_tasks: 2,
        starting_tasks: 1,
        stopping_tasks: 0,
        orphaned_tasks: 0,
        runtime_slot_loads: vec![RuntimeSlotLoad {
            source_mode: SourceMode::Vod,
            max_runtime_slots: 4,
            running_tasks: 2,
            starting_tasks: 1,
            stopping_tasks: 0,
            orphaned_tasks: 0,
            slot_usage: 0.75,
        }],
        zlm_alive: true,
        ffmpeg_alive: true,
        gpu_runtime: Vec::new(),
    });
    assert_eq!(heartbeat.runtime_slot_loads.len(), 1);
    assert_eq!(heartbeat.runtime_slot_loads[0].source_mode, SourceMode::Vod);
    assert_eq!(heartbeat.runtime_slot_loads[0].slot_usage, 0.75);
}

#[test]
fn heartbeat_reports_upload_disk_for_work_root() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-heartbeat-upload-disk-{}",
        uuid::Uuid::now_v7()
    ));
    std::fs::create_dir_all(&temp_root).expect("temp root should be created");
    let mut sampler = HeartbeatSampler::new(temp_root.to_string_lossy(), None);
    sampler.previous_cpu = Some(CpuCounters { total: 10, idle: 5 });

    let heartbeat = sampler.sample(HeartbeatSampleInput {
        running_tasks: 0,
        starting_tasks: 0,
        stopping_tasks: 0,
        orphaned_tasks: 0,
        runtime_slot_loads: Vec::new(),
        zlm_alive: true,
        ffmpeg_alive: true,
        gpu_runtime: Vec::new(),
    });

    assert!(heartbeat.upload_disk_total_bytes > 0);
    assert!(heartbeat.upload_disk_available_bytes > 0);
    assert!(heartbeat.upload_disk_available_bytes <= heartbeat.upload_disk_total_bytes);
    assert!(heartbeat.upload_disk_used_percent >= 0.0);

    let _ = std::fs::remove_dir_all(&temp_root);
}
