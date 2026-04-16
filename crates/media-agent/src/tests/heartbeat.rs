use super::*;

#[test]
fn slot_usage_is_capped() {
    let mut sampler = HeartbeatSampler::new(".", 2);
    sampler.previous_cpu = Some(CpuCounters { total: 10, idle: 5 });

    let heartbeat = sampler.sample(10, 0, 0, 0, true, true, Vec::new());
    assert_eq!(heartbeat.slot_usage, 1.0);
}
