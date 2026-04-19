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
