mod actor;
mod backend;
mod command;
mod handle;
mod internal_event;
mod state;

#[cfg(test)]
mod tests {
    const PRODUCTION_RUNTIME_SOURCES: &[(&str, &str)] = &[
        (
            "runtime_executor.rs",
            include_str!("../runtime_executor.rs"),
        ),
        (
            "runtime_process_start.rs",
            include_str!("../runtime_process_start.rs"),
        ),
        (
            "runtime_zlm_start.rs",
            include_str!("../runtime_zlm_start.rs"),
        ),
        ("runtime_start.rs", include_str!("../runtime_start.rs")),
        ("runtime_stop.rs", include_str!("../runtime_stop.rs")),
        ("runtime_events.rs", include_str!("../runtime_events.rs")),
        ("runtime_process.rs", include_str!("../runtime_process.rs")),
        ("runtime_manager/actor.rs", include_str!("actor.rs")),
    ];

    #[test]
    fn production_runtime_paths_do_not_write_legacy_registry_or_runtime_map() {
        for (path, source) in PRODUCTION_RUNTIME_SOURCES {
            assert!(
                !source.contains("LocalRuntimeRegistry"),
                "{path} must not depend on LocalRuntimeRegistry"
            );
            assert!(
                !source.contains("runtimes.write"),
                "{path} must not write the legacy runtime map"
            );
            assert!(
                !source.contains("remove_managed_runtime"),
                "{path} must not remove from the legacy runtime map"
            );
            assert!(
                !source.contains("slot_limiter.try_acquire")
                    && !source.contains("slot_limiter.attach_existing"),
                "{path} must acquire runtime slots through RuntimeBackendStore"
            );
        }
    }
}

pub use actor::RuntimeManager;
pub(crate) use actor::RuntimeManagerOptions;
pub(crate) use backend::RuntimeBackendStore;
pub(crate) use command::RuntimeManagerLimits;
pub use command::RuntimeManagerRequestOutcome;
pub use handle::RuntimeManagerHandle;
pub(crate) use handle::RuntimeMonitorHandle;
pub(crate) use internal_event::{
    CompanionProcessExitedEvent, ProcessExitedEvent, ProgressObservedEvent,
    RecordDurationReachedEvent, RuntimeGeneration, RuntimeInternalEvent, RuntimeMonitorCommit,
};
