use std::{
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState, TaskType, WorkerKind};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex as TokioMutex, mpsc, oneshot},
    time::{Duration as TokioDuration, sleep, timeout},
};
use tonic::async_trait;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime_events::{RuntimeEventSink, RuntimeNotification, RuntimeTaskProgress},
    runtime_executor::LocalExecutor,
    runtime_manager::{ProcessExitedEvent, RecordDurationReachedEvent, RuntimeInternalEvent},
    runtime_metadata::live_relay_recording_from_handle,
    runtime_recording::{LiveRelayRecording, ZlmRecordKind},
    runtime_registry::{AdoptFilter, AdoptRuntimeFilter, LocalRuntimeRegistry, RuntimeReadModel},
    runtime_types::{
        ExecutorError, RecordingControlAction, StartTaskRequest, StopTaskRequest,
        TaskRecordingControlRequest,
    },
};

use super::internal_event::{ProgressObservedEvent, RuntimeGeneration};
use super::state::RuntimeManagerState;
use super::{
    RuntimeManager, RuntimeManagerHandle, RuntimeManagerLimits, RuntimeManagerOptions,
    RuntimeManagerRequestOutcome,
};
use crate::{
    runtime_executor::ManagedProcessExecutor,
    runtime_process::{ManagedRuntime, ProcessIdentity, RuntimeSlotPermit},
};

#[derive(Default)]
struct RecordingExecutor {
    start_requests: Mutex<Vec<StartTaskRequest>>,
    stop_requests: Mutex<Vec<StopTaskRequest>>,
    recording_requests: Mutex<Vec<TaskRecordingControlRequest>>,
    adopt_filters: Mutex<Vec<AdoptFilter>>,
    adopt_result: Mutex<Vec<RuntimeHandle>>,
    zlm_server_ids: Mutex<Vec<String>>,
    zlm_rtmp_hints: Mutex<Vec<Option<bool>>>,
    start_gate: TokioMutex<Option<oneshot::Receiver<()>>>,
    adopt_gate: TokioMutex<Option<oneshot::Receiver<()>>>,
    start_active: AtomicUsize,
    max_start_active: AtomicUsize,
    adopt_active: AtomicUsize,
    max_adopt_active: AtomicUsize,
    registry: Option<LocalRuntimeRegistry>,
    stop_removes_runtime: AtomicBool,
}

impl RecordingExecutor {
    fn with_registry(registry: LocalRuntimeRegistry) -> Self {
        Self {
            registry: Some(registry),
            ..Self::default()
        }
    }

    async fn block_start(&self) -> oneshot::Sender<()> {
        install_gate(&self.start_gate).await
    }

    async fn block_adopt(&self) -> oneshot::Sender<()> {
        install_gate(&self.adopt_gate).await
    }

    fn remove_runtime_on_stop(&self) {
        self.stop_removes_runtime.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl LocalExecutor for RecordingExecutor {
    async fn start_task(&self, request: StartTaskRequest) -> Result<RuntimeHandle, ExecutorError> {
        self.start_requests
            .lock()
            .expect("start requests lock")
            .push(request.clone());
        let active = self
            .start_active
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        record_max(&self.max_start_active, active);
        wait_gate(&self.start_gate).await;
        self.start_active.fetch_sub(1, Ordering::SeqCst);
        let handle = test_handle(
            request.task_id,
            request.attempt_no,
            request.lease_token,
            request.task_type.default_worker_kind(),
            request.session_epoch,
        );
        if let Some(registry) = &self.registry {
            registry.track(handle.clone());
        }
        Ok(handle)
    }

    async fn stop_task(&self, request: StopTaskRequest) -> Result<(), ExecutorError> {
        self.stop_requests
            .lock()
            .expect("stop requests lock")
            .push(request.clone());
        if let Some(registry) = &self.registry {
            if self.stop_removes_runtime.load(Ordering::SeqCst) {
                if let Some(handle) =
                    registry.find_by_task_attempt(request.task_id, request.attempt_no)
                {
                    registry.remove(handle.runtime_id);
                }
            } else if let Some(handle) =
                registry.find_by_task_attempt(request.task_id, request.attempt_no)
            {
                registry.update(handle.runtime_id, |runtime| {
                    runtime.state = RuntimeState::Stopping;
                    runtime.last_progress_at = Some(Utc::now());
                });
            }
        }
        Ok(())
    }

    async fn set_task_recording(
        &self,
        request: TaskRecordingControlRequest,
    ) -> Result<RuntimeHandle, ExecutorError> {
        self.recording_requests
            .lock()
            .expect("recording requests lock")
            .push(request.clone());
        let handle = test_handle(
            request.task_id,
            request.attempt_no,
            request.lease_token,
            WorkerKind::ZlmProxy,
            1,
        );
        if let Some(registry) = &self.registry {
            registry.track(handle.clone());
        }
        Ok(handle)
    }

    async fn adopt_orphans(&self, filter: AdoptFilter) -> Vec<RuntimeHandle> {
        self.adopt_filters
            .lock()
            .expect("adopt filters lock")
            .push(filter);
        let active = self
            .adopt_active
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        record_max(&self.max_adopt_active, active);
        wait_gate(&self.adopt_gate).await;
        self.adopt_active.fetch_sub(1, Ordering::SeqCst);
        let handles = self.adopt_result.lock().expect("adopt result lock").clone();
        if let Some(registry) = &self.registry {
            for handle in &handles {
                registry.track(handle.clone());
            }
        }
        handles
    }

    fn set_zlm_server_id(&self, server_id: String) {
        self.zlm_server_ids
            .lock()
            .expect("zlm server ids lock")
            .push(server_id);
    }

    fn set_zlm_rtmp_enhanced_enabled(&self, enabled: Option<bool>) {
        self.zlm_rtmp_hints
            .lock()
            .expect("zlm rtmp hints lock")
            .push(enabled);
    }
}

#[tokio::test]
async fn runtime_manager_handle_forwards_executor_commands() {
    let executor = Arc::new(RecordingExecutor::default());
    let handle = RuntimeManager::spawn(executor.clone());
    let start = start_request();
    let stop = stop_request(start.task_id);
    let recording = recording_request(start.task_id);
    let adopt_handle = test_handle(
        Uuid::now_v7(),
        2,
        "adopt-lease".to_string(),
        WorkerKind::Ffmpeg,
        7,
    );
    *executor.adopt_result.lock().expect("adopt result lock") = vec![adopt_handle.clone()];

    let started = handle
        .start_task(start.clone())
        .await
        .expect("start should be forwarded");
    assert_eq!(started.task_id, start.task_id);
    handle
        .stop_task(stop.clone())
        .await
        .expect("stop should be forwarded");
    let recorded = handle
        .set_task_recording(recording.clone())
        .await
        .expect("recording should be forwarded");
    assert_eq!(recorded.task_id, recording.task_id);
    let adopted = handle
        .adopt_orphans(adopt_filter(adopt_handle.task_id))
        .await;
    assert_eq!(adopted, vec![adopt_handle]);

    let start_requests = executor.start_requests.lock().expect("start requests lock");
    assert_eq!(start_requests.len(), 1);
    assert_eq!(start_requests[0].task_id, start.task_id);
    assert_eq!(start_requests[0].attempt_no, start.attempt_no);
    assert_eq!(start_requests[0].lease_token, start.lease_token);
    drop(start_requests);

    let stop_requests = executor.stop_requests.lock().expect("stop requests lock");
    assert_eq!(stop_requests.len(), 1);
    assert_eq!(stop_requests[0].task_id, stop.task_id);
    assert_eq!(stop_requests[0].attempt_no, stop.attempt_no);
    assert_eq!(stop_requests[0].lease_token, stop.lease_token);
    drop(stop_requests);

    let recording_requests = executor
        .recording_requests
        .lock()
        .expect("recording requests lock");
    assert_eq!(recording_requests.len(), 1);
    assert_eq!(recording_requests[0].task_id, recording.task_id);
    assert_eq!(recording_requests[0].attempt_no, recording.attempt_no);
    assert_eq!(recording_requests[0].lease_token, recording.lease_token);
    assert_eq!(recording_requests[0].command_id, recording.command_id);
    drop(recording_requests);
    assert_eq!(
        executor
            .adopt_filters
            .lock()
            .expect("adopt filters lock")
            .len(),
        1
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_hints_forward_before_later_command() {
    let executor = Arc::new(RecordingExecutor::default());
    let handle = RuntimeManager::spawn(executor.clone());
    let task_id = Uuid::now_v7();

    handle.set_zlm_server_id("zlm-a".to_string());
    handle.set_zlm_rtmp_enhanced_enabled(Some(true));
    handle
        .stop_task(stop_request(task_id))
        .await
        .expect("stop should act as a FIFO barrier");

    assert_eq!(
        executor
            .zlm_server_ids
            .lock()
            .expect("zlm server ids lock")
            .as_slice(),
        &["zlm-a".to_string()]
    );
    assert_eq!(
        executor
            .zlm_rtmp_hints
            .lock()
            .expect("zlm rtmp hints lock")
            .as_slice(),
        &[Some(true)]
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_handle_returns_error_after_shutdown() {
    let executor = Arc::new(RecordingExecutor::default());
    let handle = RuntimeManager::spawn(executor);
    handle.shutdown().await;

    assert_manager_closed(handle.start_task(start_request()).await);
    assert_manager_closed(handle.stop_task(stop_request(Uuid::now_v7())).await);
    assert_manager_closed(
        handle
            .set_task_recording(recording_request(Uuid::now_v7()))
            .await,
    );
}

#[tokio::test]
async fn runtime_manager_session_commands_respect_begin_and_end() {
    let executor = Arc::new(RecordingExecutor::default());
    let handle = RuntimeManager::spawn(executor.clone());
    let request = start_request();

    assert!(matches!(
        handle
            .start_task_in_session(1, request.clone())
            .await
            .expect("manager should reply"),
        RuntimeManagerRequestOutcome::StaleSession
    ));

    handle.begin_session(1).await.expect("session should begin");
    assert!(matches!(
        handle
            .start_task_in_session(1, request.clone())
            .await
            .expect("manager should reply"),
        RuntimeManagerRequestOutcome::Completed(Ok(handle)) if handle.task_id == request.task_id
    ));
    handle.end_session(1).await.expect("session should end");
    assert!(matches!(
        handle
            .stop_task_in_session(1, stop_request(request.task_id))
            .await
            .expect("manager should reply"),
        RuntimeManagerRequestOutcome::StaleSession
    ));

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_session_stale_start_schedules_internal_stop() {
    let executor = Arc::new(RecordingExecutor::default());
    let release_start = executor.block_start().await;
    let handle = RuntimeManager::spawn(executor.clone());
    handle.begin_session(1).await.expect("session should begin");
    let request = start_request();
    let task_id = request.task_id;
    let start_handle = handle.clone();
    let start_job =
        tokio::spawn(async move { start_handle.start_task_in_session(1, request).await });
    wait_for_counter(&executor.start_requests, 1).await;

    handle.end_session(1).await.expect("session should end");
    let _ = release_start.send(());
    assert!(matches!(
        start_job.await.expect("start task should join"),
        Ok(RuntimeManagerRequestOutcome::StaleSession)
    ));
    wait_for_counter(&executor.stop_requests, 1).await;
    let stop_requests = executor.stop_requests.lock().expect("stop requests lock");
    assert_eq!(stop_requests[0].task_id, task_id);
    assert_eq!(stop_requests[0].reason, "stale_session_replaced");
    drop(stop_requests);

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_start_limit_queues_fifo() {
    let executor = Arc::new(RecordingExecutor::default());
    let release_start = executor.block_start().await;
    let handle = RuntimeManager::spawn_with_limits(
        executor.clone(),
        RuntimeManagerLimits {
            start: 1,
            ..RuntimeManagerLimits::default()
        },
    );
    handle.begin_session(1).await.expect("session should begin");
    let first = start_request();
    let second = start_request();
    let first_handle = handle.clone();
    let first_job = tokio::spawn(async move { first_handle.start_task_in_session(1, first).await });
    let second_handle = handle.clone();
    let second_job =
        tokio::spawn(async move { second_handle.start_task_in_session(1, second).await });

    wait_for_counter(&executor.start_requests, 1).await;
    sleep(TokioDuration::from_millis(50)).await;
    assert_eq!(
        executor
            .start_requests
            .lock()
            .expect("start requests lock")
            .len(),
        1
    );
    assert_eq!(executor.max_start_active.load(Ordering::SeqCst), 1);

    let _ = release_start.send(());
    wait_for_counter(&executor.start_requests, 2).await;
    assert!(matches!(
        first_job.await.expect("first start should join"),
        Ok(RuntimeManagerRequestOutcome::Completed(Ok(_)))
    ));
    assert!(matches!(
        second_job.await.expect("second start should join"),
        Ok(RuntimeManagerRequestOutcome::Completed(Ok(_)))
    ));
    assert_eq!(executor.max_start_active.load(Ordering::SeqCst), 1);

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_adopt_limit_allows_one_active_call() {
    let executor = Arc::new(RecordingExecutor::default());
    let release_adopt = executor.block_adopt().await;
    let handle = RuntimeManager::spawn(executor.clone());
    handle.begin_session(1).await.expect("session should begin");
    let first_handle = handle.clone();
    let first_job = tokio::spawn(async move {
        first_handle
            .adopt_orphans_in_session(1, adopt_filter(Uuid::now_v7()))
            .await
    });
    let second_handle = handle.clone();
    let second_job = tokio::spawn(async move {
        second_handle
            .adopt_orphans_in_session(1, adopt_filter(Uuid::now_v7()))
            .await
    });

    wait_for_counter(&executor.adopt_filters, 1).await;
    sleep(TokioDuration::from_millis(50)).await;
    assert_eq!(
        executor
            .adopt_filters
            .lock()
            .expect("adopt filters lock")
            .len(),
        1
    );
    assert_eq!(executor.max_adopt_active.load(Ordering::SeqCst), 1);

    let _ = release_adopt.send(());
    wait_for_counter(&executor.adopt_filters, 2).await;
    assert!(matches!(
        first_job.await.expect("first adopt should join"),
        Ok(RuntimeManagerRequestOutcome::Completed(_))
    ));
    assert!(matches!(
        second_job.await.expect("second adopt should join"),
        Ok(RuntimeManagerRequestOutcome::Completed(_))
    ));
    assert_eq!(executor.max_adopt_active.load(Ordering::SeqCst), 1);

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_sessionless_stop_runs_without_active_session() {
    let executor = Arc::new(RecordingExecutor::default());
    let handle = RuntimeManager::spawn(executor.clone());
    let request = stop_request(Uuid::now_v7());

    handle
        .stop_task(request.clone())
        .await
        .expect("sessionless stop should run without active session");

    let stop_requests = executor.stop_requests.lock().expect("stop requests lock");
    assert_eq!(stop_requests.len(), 1);
    assert_eq!(stop_requests[0].task_id, request.task_id);
    drop(stop_requests);

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_stop_actor_commits_stopping_before_rtp_worker_finishes() {
    let (settings, release_close, calls) = rtp_close_mock_settings("stop-actor").await;
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let (priority_tx, mut priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        settings,
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle = RuntimeManager::spawn_managed_with_options(
        executor.clone(),
        runtime_manager_options(Some(legacy_read_model)),
    );
    let runtime = rtp_runtime_handle();
    registry.track(runtime.clone());
    insert_runtime_backend(&executor, runtime.runtime_id, None);
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");

    let stop_handle = handle.clone();
    let stop =
        tokio::spawn(async move { stop_handle.stop_task(stop_request(runtime.task_id)).await });
    wait_for_manager_state(&handle, runtime.runtime_id, RuntimeState::Stopping).await;
    wait_for_call_count(&calls, 1).await;
    assert!(
        !stop.is_finished(),
        "stop worker should still be waiting on mock closeRtpServer"
    );
    assert_eq!(
        calls.lock().await.as_slice(),
        &["closeRtpServer".to_string()]
    );

    let _ = release_close.send(());
    stop.await
        .expect("stop task should join")
        .expect("stop should succeed");
    assert!(
        handle
            .read_handle()
            .find_by_task_attempt(runtime.task_id, runtime.attempt_no)
            .is_none(),
        "RTP terminal stop should remove active read projection"
    );
    assert!(matches!(
        timeout(TokioDuration::from_secs(1), priority_rx.recv())
            .await
            .expect("terminal event should be delivered"),
        Some(RuntimeNotification::TaskEvent(event)) if event.event_type == "canceled"
    ));
    assert!(matches!(
        timeout(TokioDuration::from_secs(1), priority_rx.recv())
            .await
            .expect("terminal snapshot should be delivered"),
        Some(RuntimeNotification::TaskSnapshot(snapshot)) if snapshot.state == RuntimeState::Exited
    ));

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_stop_actor_rejects_lease_mismatch_without_state_change() {
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        AgentSettings::default(),
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle = RuntimeManager::spawn_managed_with_options(
        executor.clone(),
        runtime_manager_options(Some(legacy_read_model)),
    );
    let runtime = rtp_runtime_handle();
    registry.track(runtime.clone());
    insert_runtime_backend(&executor, runtime.runtime_id, None);
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");

    let mut stop = stop_request(runtime.task_id);
    stop.lease_token = "wrong-lease".to_string();
    let error = handle
        .stop_task(stop)
        .await
        .expect_err("lease mismatch should reject stop");
    assert!(matches!(
        error,
        ExecutorError::InvalidRequest(message) if message.contains("lease_token mismatch")
    ));
    assert_eq!(
        manager_read_handle(&handle, &runtime).state,
        RuntimeState::Running
    );
    assert!(
        !executor
            .monitor_snapshot(runtime.runtime_id)
            .expect("backend snapshot should exist")
            .stop_requested
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_stop_actor_missing_runtime_is_idempotent() {
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        AgentSettings::default(),
        registry,
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle = RuntimeManager::spawn_managed_with_options(
        executor,
        runtime_manager_options(Some(legacy_read_model)),
    );

    handle
        .stop_task(stop_request(Uuid::now_v7()))
        .await
        .expect("missing runtime stop should remain idempotent");

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_adopt_actor_reattaches_existing_runtime_without_worker() {
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let (priority_tx, mut priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        AgentSettings::default(),
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle = RuntimeManager::spawn_managed_with_options(
        executor.clone(),
        runtime_manager_options(Some(legacy_read_model)),
    );
    handle.set_zlm_server_id("zlm-adopt".to_string());
    let runtime = test_handle(
        Uuid::now_v7(),
        2,
        "adopt-lease".to_string(),
        WorkerKind::Ffmpeg,
        1,
    );
    registry.track(runtime.clone());
    insert_runtime_backend(
        &executor,
        runtime.runtime_id,
        Some(ProcessIdentity::spawned_process_group(1)),
    );
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");

    let adopted = handle.adopt_orphans(adopt_filter(runtime.task_id)).await;

    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].runtime_id, runtime.runtime_id);
    let stored = handle
        .read_handle()
        .find_by_task_attempt(runtime.task_id, runtime.attempt_no)
        .expect("runtime should remain active");
    assert_eq!(stored.metadata["session_epoch"].as_u64(), Some(7));
    assert_eq!(stored.metadata["zlm_server_id"].as_str(), Some("zlm-adopt"));
    assert!(executor.monitor_snapshot(runtime.runtime_id).is_some());
    assert!(matches!(
        timeout(TokioDuration::from_secs(1), priority_rx.recv())
            .await
            .expect("adopted event should be delivered"),
        Some(RuntimeNotification::TaskEvent(event))
            if event.event_type == "adopted"
                && event.payload["orphaned"].as_bool() == Some(false)
    ));

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_recovery_actor_commits_process_exit_terminal() {
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let (priority_tx, mut priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        AgentSettings::default(),
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle = RuntimeManager::spawn_managed_with_options(
        executor.clone(),
        runtime_manager_options(Some(legacy_read_model)),
    );
    let runtime = test_handle(
        Uuid::now_v7(),
        1,
        "lease".to_string(),
        WorkerKind::Ffmpeg,
        1,
    );
    registry.track(runtime.clone());
    insert_runtime_backend(
        &executor,
        runtime.runtime_id,
        Some(ProcessIdentity::spawned_process_group(1)),
    );
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");
    let work_dir = std::env::temp_dir().join(format!(
        "streamserver-runtime-manager-recovery-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&work_dir).expect("work dir should be created");

    handle
        .monitor_handle(runtime.runtime_id, RuntimeGeneration::new(0))
        .send_event(RuntimeInternalEvent::ProcessExited(ProcessExitedEvent {
            runtime_id: runtime.runtime_id,
            generation: RuntimeGeneration::new(0),
            work_dir,
            output_target: "runtime-manager-recovery-test".to_string(),
            success_check: crate::runtime::SuccessCheck::ProcessExit,
            status: Err("injected wait failure".to_string()),
            was_stopped: false,
        }))
        .await;

    timeout(TokioDuration::from_secs(1), async {
        loop {
            if handle
                .read_handle()
                .find_by_task_attempt(runtime.task_id, runtime.attempt_no)
                .is_none()
            {
                break;
            }
            sleep(TokioDuration::from_millis(10)).await;
        }
    })
    .await
    .expect("process exit should remove read projection");
    assert!(
        executor.monitor_snapshot(runtime.runtime_id).is_none(),
        "process exit should remove backend"
    );
    let state = handle
        .manager_state()
        .await
        .expect("manager state should be available");
    assert!(state.get(runtime.runtime_id).is_none());
    assert!(matches!(
        timeout(TokioDuration::from_secs(1), priority_rx.recv())
            .await
            .expect("terminal event should be delivered"),
        Some(RuntimeNotification::TaskEvent(event)) if event.event_type == "failed"
    ));
    assert!(matches!(
        timeout(TokioDuration::from_secs(1), priority_rx.recv())
            .await
            .expect("terminal snapshot should be delivered"),
        Some(RuntimeNotification::TaskSnapshot(snapshot)) if snapshot.state == RuntimeState::Exited
    ));

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_recording_actor_start_online_updates_metadata_and_notifies() {
    let (settings, _release_start, calls) =
        recording_mock_settings("recording-start-online", None, 0, 0).await;
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let (priority_tx, mut priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        settings,
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle = RuntimeManager::spawn_managed_with_options(
        executor,
        runtime_manager_options(Some(legacy_read_model)),
    );
    let runtime = recording_runtime_handle(true, None);
    registry.track(runtime.clone());
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");

    let request = recording_request(runtime.task_id);
    let updated = handle
        .set_task_recording(request.clone())
        .await
        .expect("recording start should succeed");

    wait_for_call_count(&calls, 1).await;
    assert_eq!(calls.lock().await.as_slice(), &["startRecord".to_string()]);
    let stored = manager_read_handle(&handle, &runtime);
    let recording =
        live_relay_recording_from_handle(&stored).expect("recording metadata should exist");
    assert!(recording.started);
    assert!(recording.recording_started_at.is_some());
    assert_eq!(
        stored.metadata["recording_control"]["last_command_id"].as_str(),
        Some(request.command_id.as_str())
    );
    assert_eq!(
        updated.metadata["recording_control"]["last_action"].as_str(),
        Some("start")
    );
    assert_eq!(
        collect_task_event_types(&mut priority_rx, 2)
            .await
            .as_slice(),
        &[
            "recording_start_requested".to_string(),
            "recording_started".to_string()
        ]
    );
    assert!(matches!(
        timeout(TokioDuration::from_secs(1), priority_rx.recv())
            .await
            .expect("snapshot should be delivered"),
        Some(RuntimeNotification::TaskSnapshot(snapshot)) if snapshot.runtime_id == runtime.runtime_id
    ));

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_recording_actor_start_offline_commits_pending_without_zlm() {
    let (settings, _release_start, calls) =
        recording_mock_settings("recording-start-offline", None, 0, 0).await;
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let (priority_tx, mut priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        settings,
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle = RuntimeManager::spawn_managed_with_options(
        executor,
        runtime_manager_options(Some(legacy_read_model)),
    );
    let runtime = recording_runtime_handle(false, None);
    registry.track(runtime.clone());
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");

    let request = recording_request(runtime.task_id);
    handle
        .set_task_recording(request.clone())
        .await
        .expect("offline recording start should commit pending");

    assert!(calls.lock().await.is_empty());
    let stored = manager_read_handle(&handle, &runtime);
    let recording =
        live_relay_recording_from_handle(&stored).expect("recording metadata should exist");
    assert!(recording.manual_control);
    assert!(recording.desired_enabled);
    assert!(!recording.started);
    assert_eq!(
        stored.metadata["recording_control"]["last_command_id"].as_str(),
        Some(request.command_id.as_str())
    );
    assert_eq!(
        collect_task_event_types(&mut priority_rx, 2)
            .await
            .as_slice(),
        &[
            "recording_start_requested".to_string(),
            "recording_start_pending".to_string()
        ]
    );
    assert!(matches!(
        timeout(TokioDuration::from_secs(1), priority_rx.recv())
            .await
            .expect("snapshot should be delivered"),
        Some(RuntimeNotification::TaskSnapshot(snapshot)) if snapshot.runtime_id == runtime.runtime_id
    ));

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_recording_actor_stop_online_updates_metadata() {
    let (settings, _release_start, calls) =
        recording_mock_settings("recording-stop-online", None, 0, 0).await;
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let (priority_tx, mut priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        settings,
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle = RuntimeManager::spawn_managed_with_options(
        executor,
        runtime_manager_options(Some(legacy_read_model)),
    );
    let runtime = recording_runtime_handle(true, Some(started_recording("cmd-start")));
    registry.track(runtime.clone());
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");

    let mut request = recording_request(runtime.task_id);
    request.action = RecordingControlAction::Stop;
    request.reason = "manual_stop".to_string();
    request.command_id = "cmd-stop".to_string();
    handle
        .set_task_recording(request.clone())
        .await
        .expect("recording stop should succeed");

    wait_for_call_count(&calls, 1).await;
    assert_eq!(calls.lock().await.as_slice(), &["stopRecord".to_string()]);
    let stored = manager_read_handle(&handle, &runtime);
    let recording =
        live_relay_recording_from_handle(&stored).expect("recording metadata should exist");
    assert!(!recording.started);
    assert_eq!(recording.completion_reason.as_deref(), Some("manual_stop"));
    assert_eq!(
        stored.metadata["recording_control"]["last_action"].as_str(),
        Some("stop")
    );
    assert_eq!(
        collect_task_event_types(&mut priority_rx, 2)
            .await
            .as_slice(),
        &[
            "recording_stop_requested".to_string(),
            "recording_stopped".to_string()
        ]
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_recording_actor_zlm_failure_preserves_metadata() {
    let (settings, _release_start, calls) =
        recording_mock_settings("recording-zlm-failure", None, -1, 0).await;
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let (priority_tx, mut priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        settings,
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle = RuntimeManager::spawn_managed_with_options(
        executor,
        runtime_manager_options(Some(legacy_read_model)),
    );
    let runtime = recording_runtime_handle(true, None);
    registry.track(runtime.clone());
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");

    let error = handle
        .set_task_recording(recording_request(runtime.task_id))
        .await
        .expect_err("ZLM failure should be returned");
    assert!(matches!(error, ExecutorError::ApiCall(_)));
    wait_for_call_count(&calls, 1).await;
    let stored = registry
        .get(runtime.runtime_id)
        .expect("runtime should remain active");
    assert!(
        live_relay_recording_from_handle(&stored).is_none(),
        "failed worker should not write recording metadata"
    );
    assert!(stored.metadata.get("recording_control").is_none());
    assert_eq!(
        collect_task_event_types(&mut priority_rx, 1)
            .await
            .as_slice(),
        &["recording_start_requested".to_string()]
    );
    assert!(priority_rx.try_recv().is_err());

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_recording_actor_duplicate_pending_reuses_worker_result() {
    let (settings, release_start, calls) =
        recording_mock_settings("recording-duplicate-pending", Some("startRecord"), 0, 0).await;
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        settings,
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let mut options = runtime_manager_options(Some(legacy_read_model));
    options.limits.recording = 1;
    let handle = RuntimeManager::spawn_managed_with_options(executor, options);
    let runtime = recording_runtime_handle(true, None);
    registry.track(runtime.clone());
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");
    let request = recording_request(runtime.task_id);

    let first_handle = handle.clone();
    let first_request = request.clone();
    let first = tokio::spawn(async move { first_handle.set_task_recording(first_request).await });
    wait_for_call_count(&calls, 1).await;
    let second_handle = handle.clone();
    let second_request = request.clone();
    let second =
        tokio::spawn(async move { second_handle.set_task_recording(second_request).await });
    sleep(TokioDuration::from_millis(50)).await;
    assert_eq!(calls.lock().await.len(), 1);

    release_start
        .expect("start release should exist")
        .send(())
        .expect("release should be consumed");
    let first = first
        .await
        .expect("first recording task should join")
        .expect("first recording should succeed");
    let second = second
        .await
        .expect("second recording task should join")
        .expect("second recording should succeed");
    assert_eq!(first.runtime_id, second.runtime_id);
    assert_eq!(calls.lock().await.len(), 1);

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_recording_actor_completed_duplicate_is_idempotent() {
    let (settings, _release_start, calls) =
        recording_mock_settings("recording-duplicate-completed", None, 0, 0).await;
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        settings,
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle = RuntimeManager::spawn_managed_with_options(
        executor,
        runtime_manager_options(Some(legacy_read_model)),
    );
    let runtime = recording_runtime_handle(true, None);
    registry.track(runtime.clone());
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");
    let request = recording_request(runtime.task_id);

    handle
        .set_task_recording(request.clone())
        .await
        .expect("first recording should succeed");
    handle
        .set_task_recording(request.clone())
        .await
        .expect("duplicate recording should be idempotent");

    assert_eq!(calls.lock().await.len(), 1);

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_recording_actor_rejects_lease_and_stopping_runtime() {
    let (settings, _release_start, _calls) =
        recording_mock_settings("recording-rejects", None, 0, 0).await;
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        settings,
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle = RuntimeManager::spawn_managed_with_options(
        executor,
        runtime_manager_options(Some(legacy_read_model)),
    );
    let runtime = recording_runtime_handle(true, None);
    registry.track(runtime.clone());
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");

    let mut wrong_lease = recording_request(runtime.task_id);
    wrong_lease.lease_token = "wrong".to_string();
    let error = handle
        .set_task_recording(wrong_lease)
        .await
        .expect_err("lease mismatch should reject");
    assert!(matches!(
        error,
        ExecutorError::InvalidRequest(message) if message.contains("lease_token mismatch")
    ));

    registry.update(runtime.runtime_id, |handle| {
        handle.state = RuntimeState::Stopping;
    });
    handle.observe_runtime_snapshot(
        registry
            .get(runtime.runtime_id)
            .expect("updated runtime should exist"),
    );
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");
    let error = handle
        .set_task_recording(recording_request(runtime.task_id))
        .await
        .expect_err("stopping runtime should reject");
    assert!(matches!(
        error,
        ExecutorError::InvalidRequest(message)
            if message.contains("recording control requires an active runtime")
    ));

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_state_start_tracks_legacy_executor_handle() {
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let executor = Arc::new(RecordingExecutor::with_registry(registry));
    let handle = RuntimeManager::spawn_with_options(
        executor,
        runtime_manager_options(Some(legacy_read_model)),
    );
    let request = start_request();

    let started = handle
        .start_task(request.clone())
        .await
        .expect("start should complete");
    let state = handle
        .manager_state()
        .await
        .expect("manager state should be available");

    assert_eq!(
        state
            .get(started.runtime_id)
            .expect("runtime should be tracked")
            .handle,
        started
    );
    assert_eq!(
        state
            .find_by_task_attempt(request.task_id, request.attempt_no)
            .expect("task attempt should be indexed")
            .handle
            .runtime_id,
        started.runtime_id
    );
    assert_eq!(state.state_counts().running, 1);
    assert_eq!(state.pending_operation_count(), 0);

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_state_stop_uses_legacy_read_model_snapshot() {
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let executor = Arc::new(RecordingExecutor::with_registry(registry));
    let handle = RuntimeManager::spawn_with_options(
        executor,
        runtime_manager_options(Some(legacy_read_model)),
    );
    let request = start_request();
    handle
        .start_task(request.clone())
        .await
        .expect("start should complete");

    handle
        .stop_task(stop_request(request.task_id))
        .await
        .expect("stop should complete");
    let state = handle
        .manager_state()
        .await
        .expect("manager state should be available");
    let entry = state
        .find_by_task_attempt(request.task_id, request.attempt_no)
        .expect("stopping runtime should remain tracked");

    assert_eq!(entry.handle.state, RuntimeState::Stopping);
    assert_eq!(state.state_counts().stopping, 1);
    assert_eq!(state.pending_operation_count(), 0);

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_state_stop_removes_when_legacy_read_model_has_no_snapshot() {
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let executor = Arc::new(RecordingExecutor::with_registry(registry));
    executor.remove_runtime_on_stop();
    let handle = RuntimeManager::spawn_with_options(
        executor,
        runtime_manager_options(Some(legacy_read_model)),
    );
    let request = start_request();
    let started = handle
        .start_task(request.clone())
        .await
        .expect("start should complete");

    handle
        .stop_task(stop_request(request.task_id))
        .await
        .expect("stop should complete");
    let state = handle
        .manager_state()
        .await
        .expect("manager state should be available");

    assert!(state.get(started.runtime_id).is_none());
    assert!(
        state
            .find_by_task_attempt(request.task_id, request.attempt_no)
            .is_none()
    );
    assert_eq!(state.state_counts(), Default::default());
    assert_eq!(state.pending_operation_count(), 0);

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_state_record_and_adopt_apply_handles() {
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let executor = Arc::new(RecordingExecutor::with_registry(registry));
    let adopted = test_handle(
        Uuid::now_v7(),
        2,
        "adopt-lease".to_string(),
        WorkerKind::Ffmpeg,
        7,
    );
    *executor.adopt_result.lock().expect("adopt result lock") = vec![adopted.clone()];
    let handle = RuntimeManager::spawn_with_options(
        executor,
        runtime_manager_options(Some(legacy_read_model)),
    );
    let task_id = Uuid::now_v7();

    let recorded = handle
        .set_task_recording(recording_request(task_id))
        .await
        .expect("recording control should complete");
    let adopted_handles = handle.adopt_orphans(adopt_filter(adopted.task_id)).await;
    assert_eq!(adopted_handles, vec![adopted.clone()]);

    let state = handle
        .manager_state()
        .await
        .expect("manager state should be available");
    assert_eq!(
        state
            .find_by_task_attempt(recorded.task_id, recorded.attempt_no)
            .expect("recorded runtime should be tracked")
            .handle,
        recorded
    );
    assert_eq!(
        state
            .find_by_task_attempt(adopted.task_id, adopted.attempt_no)
            .expect("adopted runtime should be tracked")
            .handle,
        adopted
    );
    assert_eq!(state.state_counts().running, 2);
    assert_eq!(state.pending_operation_count(), 0);

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_state_observes_task_snapshots() {
    let executor = Arc::new(RecordingExecutor::default());
    let handle = RuntimeManager::spawn(executor);
    let mut snapshot = test_handle(
        Uuid::now_v7(),
        1,
        "snapshot-lease".to_string(),
        WorkerKind::Ffmpeg,
        1,
    );

    handle.observe_runtime_snapshot(snapshot.clone());
    let state = handle
        .manager_state()
        .await
        .expect("manager state should be available");
    assert_eq!(
        state
            .get(snapshot.runtime_id)
            .expect("snapshot should be tracked")
            .handle
            .state,
        RuntimeState::Running
    );

    snapshot.state = RuntimeState::Stopping;
    handle.observe_runtime_snapshot(snapshot.clone());
    let state = handle
        .manager_state()
        .await
        .expect("manager state should be available");
    assert_eq!(
        state
            .get(snapshot.runtime_id)
            .expect("snapshot should update state")
            .handle
            .state,
        RuntimeState::Stopping
    );

    snapshot.state = RuntimeState::Exited;
    handle.observe_runtime_snapshot(snapshot.clone());
    let state = handle
        .manager_state()
        .await
        .expect("manager state should be available");
    assert!(state.get(snapshot.runtime_id).is_none());
    assert_eq!(state.state_counts(), Default::default());
    assert_eq!(state.pending_operation_count(), 0);

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_monitor_snapshot_respects_generation() {
    let registry = LocalRuntimeRegistry::new();
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        AgentSettings::default(),
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle =
        RuntimeManager::spawn_managed_with_options(executor.clone(), runtime_manager_options(None));
    let runtime = test_handle(
        Uuid::now_v7(),
        1,
        "snapshot-generation".to_string(),
        WorkerKind::Ffmpeg,
        1,
    );
    insert_runtime_backend(&executor, runtime.runtime_id, None);
    registry.track(runtime.clone());
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");

    let current_monitor = handle.monitor_handle(runtime.runtime_id, RuntimeGeneration::new(0));
    let snapshot = current_monitor
        .snapshot()
        .await
        .expect("current generation should snapshot");
    assert_eq!(snapshot.handle.runtime_id, runtime.runtime_id);

    let stale_monitor = handle.monitor_handle(runtime.runtime_id, RuntimeGeneration::new(99));
    assert!(stale_monitor.snapshot().await.is_none());

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_progress_observed_is_generation_gated() {
    let registry = LocalRuntimeRegistry::new();
    let (priority_tx, mut priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        AgentSettings::default(),
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle =
        RuntimeManager::spawn_managed_with_options(executor, runtime_manager_options(None));
    let mut runtime = test_handle(
        Uuid::now_v7(),
        1,
        "progress-generation".to_string(),
        WorkerKind::Ffmpeg,
        1,
    );
    runtime.state = RuntimeState::Starting;
    registry.track(runtime.clone());
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");

    let current_monitor = handle.monitor_handle(runtime.runtime_id, RuntimeGeneration::new(0));
    current_monitor
        .send_event(RuntimeInternalEvent::ProgressObserved(
            ProgressObservedEvent {
                runtime_id: runtime.runtime_id,
                generation: current_monitor.generation(),
                progress: progress_for(&runtime),
            },
        ))
        .await;
    let _ = handle
        .manager_state()
        .await
        .expect("progress barrier should complete");
    assert_eq!(
        manager_read_handle(&handle, &runtime).state,
        RuntimeState::Running
    );
    assert!(matches!(
        timeout(TokioDuration::from_secs(1), priority_rx.recv())
            .await
            .expect("progress should be delivered"),
        Some(RuntimeNotification::TaskProgress(progress)) if progress.task_id == runtime.task_id
    ));

    let stale_monitor = handle.monitor_handle(runtime.runtime_id, RuntimeGeneration::new(99));
    stale_monitor
        .send_event(RuntimeInternalEvent::ProgressObserved(
            ProgressObservedEvent {
                runtime_id: runtime.runtime_id,
                generation: stale_monitor.generation(),
                progress: progress_for(&runtime),
            },
        ))
        .await;
    let _ = handle
        .manager_state()
        .await
        .expect("stale progress barrier should complete");
    assert!(
        timeout(TokioDuration::from_millis(50), priority_rx.recv())
            .await
            .is_err(),
        "stale generation should not emit progress"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_record_duration_reached_is_generation_gated_and_marks_completion() {
    let registry = LocalRuntimeRegistry::new();
    let (priority_tx, mut priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = Arc::new(ManagedProcessExecutor::new(
        AgentSettings::default(),
        registry,
        RuntimeEventSink::new(priority_tx, log_tx),
    ));
    let handle =
        RuntimeManager::spawn_managed_with_options(executor, runtime_manager_options(None));
    let mut recording = started_recording("duration-reached");
    recording.duration_sec = Some(8);
    recording.manual_control = false;
    recording.stop_task_on_duration = true;
    let runtime = recording_runtime_handle(true, Some(recording));
    handle.observe_runtime_snapshot(runtime.clone());
    let _ = handle
        .manager_state()
        .await
        .expect("state barrier should complete");

    let stale_monitor = handle.monitor_handle(runtime.runtime_id, RuntimeGeneration::new(99));
    stale_monitor
        .send_event(RuntimeInternalEvent::RecordDurationReached(
            RecordDurationReachedEvent {
                runtime_id: runtime.runtime_id,
                generation: stale_monitor.generation(),
            },
        ))
        .await;
    let _ = handle
        .manager_state()
        .await
        .expect("stale duration barrier should complete");
    assert_eq!(
        manager_read_handle(&handle, &runtime).state,
        RuntimeState::Running
    );
    assert!(
        timeout(TokioDuration::from_millis(50), priority_rx.recv())
            .await
            .is_err(),
        "stale generation should not emit duration stop snapshot"
    );

    let current_monitor = handle.monitor_handle(runtime.runtime_id, RuntimeGeneration::new(0));
    current_monitor
        .send_event(RuntimeInternalEvent::RecordDurationReached(
            RecordDurationReachedEvent {
                runtime_id: runtime.runtime_id,
                generation: current_monitor.generation(),
            },
        ))
        .await;
    let snapshot = timeout(TokioDuration::from_secs(1), async {
        loop {
            match priority_rx
                .recv()
                .await
                .expect("notification stream should stay open")
            {
                RuntimeNotification::TaskSnapshot(snapshot)
                    if snapshot.runtime_id == runtime.runtime_id =>
                {
                    break snapshot;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("duration stop snapshot should be delivered");

    assert_eq!(snapshot.state, RuntimeState::Exited);
    assert_eq!(
        snapshot.metadata["completion_reason"].as_str(),
        Some("record_duration_reached")
    );
    assert_eq!(
        snapshot.metadata["stop"]["reason"].as_str(),
        Some("record_duration_reached")
    );
    let recording =
        live_relay_recording_from_handle(&snapshot).expect("recording metadata should remain");
    assert!(!recording.started);
    assert!(recording.auto_stop_requested);
    assert_eq!(
        recording.completion_reason.as_deref(),
        Some("record_duration_reached")
    );
    assert!(
        handle
            .read_handle()
            .find_by_task_attempt(runtime.task_id, runtime.attempt_no)
            .is_none(),
        "duration stop should remove terminal read projection"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn runtime_manager_state_stale_start_cleanup_removes_runtime() {
    let registry = LocalRuntimeRegistry::new();
    let legacy_read_model: Arc<dyn RuntimeReadModel> = Arc::new(registry.clone());
    let executor = Arc::new(RecordingExecutor::with_registry(registry));
    executor.remove_runtime_on_stop();
    let release_start = executor.block_start().await;
    let handle = RuntimeManager::spawn_with_options(
        executor.clone(),
        runtime_manager_options(Some(legacy_read_model)),
    );
    handle.begin_session(1).await.expect("session should begin");
    let request = start_request();
    let start_handle = handle.clone();
    let start_job =
        tokio::spawn(async move { start_handle.start_task_in_session(1, request).await });
    wait_for_counter(&executor.start_requests, 1).await;

    handle.end_session(1).await.expect("session should end");
    let _ = release_start.send(());
    assert!(matches!(
        start_job.await.expect("start task should join"),
        Ok(RuntimeManagerRequestOutcome::StaleSession)
    ));
    wait_for_counter(&executor.stop_requests, 1).await;
    let state = wait_for_state_active_count(&handle, 0).await;

    assert_eq!(state.state_counts(), Default::default());
    assert_eq!(state.pending_operation_count(), 0);

    handle.shutdown().await;
}

#[test]
fn runtime_manager_state_consistency_diagnostics_include_identity_and_state() {
    let registry = LocalRuntimeRegistry::new();
    let read_handle = test_handle(
        Uuid::now_v7(),
        1,
        "diagnostic-lease".to_string(),
        WorkerKind::Ffmpeg,
        1,
    );
    let mut state_handle = read_handle.clone();
    state_handle.state = RuntimeState::Starting;
    registry.track(read_handle.clone());
    let mut state = RuntimeManagerState::default();
    state.apply_handle(state_handle);

    let diagnostics = state.consistency_errors(&registry).join("\n");
    assert!(diagnostics.contains(&read_handle.runtime_id.to_string()));
    assert!(diagnostics.contains(&read_handle.task_id.to_string()));
    assert!(diagnostics.contains("attempt_no=1"));
    assert!(diagnostics.contains("manager state=starting"));
    assert!(diagnostics.contains("read model state=running"));
}

fn assert_manager_closed<T>(result: Result<T, ExecutorError>) {
    assert!(matches!(
        result,
        Err(ExecutorError::InvalidRequest(message))
            if message.contains("runtime manager command channel closed")
    ));
}

async fn install_gate(gate: &TokioMutex<Option<oneshot::Receiver<()>>>) -> oneshot::Sender<()> {
    let (tx, rx) = oneshot::channel();
    *gate.lock().await = Some(rx);
    tx
}

async fn wait_gate(gate: &TokioMutex<Option<oneshot::Receiver<()>>>) {
    let gate = { gate.lock().await.take() };
    if let Some(gate) = gate {
        let _ = gate.await;
    }
}

async fn wait_for_counter<T>(items: &Mutex<Vec<T>>, expected: usize) {
    timeout(TokioDuration::from_secs(1), async {
        loop {
            if items.lock().expect("items lock").len() >= expected {
                break;
            }
            sleep(TokioDuration::from_millis(10)).await;
        }
    })
    .await
    .expect("counter should reach expected value");
}

async fn wait_for_state_active_count(
    handle: &RuntimeManagerHandle,
    expected: usize,
) -> RuntimeManagerState {
    timeout(TokioDuration::from_secs(1), async {
        loop {
            let state = handle
                .manager_state()
                .await
                .expect("manager state should be available");
            if state.active_handles().len() == expected {
                break state;
            }
            sleep(TokioDuration::from_millis(10)).await;
        }
    })
    .await
    .expect("state active count should reach expected value")
}

fn manager_read_handle(manager: &RuntimeManagerHandle, runtime: &RuntimeHandle) -> RuntimeHandle {
    manager
        .read_handle()
        .find_by_task_attempt(runtime.task_id, runtime.attempt_no)
        .expect("runtime should remain active")
}

async fn wait_for_manager_state(
    handle: &RuntimeManagerHandle,
    runtime_id: Uuid,
    expected: RuntimeState,
) {
    timeout(TokioDuration::from_secs(1), async {
        loop {
            if handle
                .manager_state()
                .await
                .expect("manager state should be available")
                .get(runtime_id)
                .map(|entry| entry.handle.state == expected)
                .unwrap_or(false)
            {
                break;
            }
            sleep(TokioDuration::from_millis(10)).await;
        }
    })
    .await
    .expect("manager state should reach expected value");
}

async fn wait_for_call_count(calls: &Arc<TokioMutex<Vec<String>>>, expected: usize) {
    timeout(TokioDuration::from_secs(1), async {
        loop {
            if calls.lock().await.len() == expected {
                break;
            }
            sleep(TokioDuration::from_millis(10)).await;
        }
    })
    .await
    .expect("mock call count should reach expected value");
}

fn insert_runtime_backend(
    executor: &ManagedProcessExecutor,
    runtime_id: Uuid,
    process: Option<ProcessIdentity>,
) {
    executor
        .runtimes
        .write()
        .expect("runtime map lock poisoned")
        .insert(
            runtime_id,
            ManagedRuntime {
                process,
                companion_processes: Vec::new(),
                _slot_permit: RuntimeSlotPermit::unbounded(),
                stop_requested: Arc::new(AtomicBool::new(false)),
                suppress_companion_events: Arc::new(AtomicBool::new(false)),
            },
        );
}

async fn rtp_close_mock_settings(
    name: &'static str,
) -> (
    AgentSettings,
    oneshot::Sender<()>,
    Arc<TokioMutex<Vec<String>>>,
) {
    use axum::{
        Json, Router,
        extract::{Query, State},
        routing::get,
    };
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    #[derive(Clone)]
    struct StubState {
        calls: Arc<TokioMutex<Vec<String>>>,
        release: Arc<TokioMutex<Option<oneshot::Receiver<()>>>>,
    }

    async fn close_rtp_server(
        State(state): State<StubState>,
        Query(_params): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        state.calls.lock().await.push("closeRtpServer".to_string());
        if let Some(release) = state.release.lock().await.take() {
            let _ = release.await;
        }
        Json(json!({"code": 0}))
    }

    let calls = Arc::new(TokioMutex::new(Vec::new()));
    let (release_tx, release_rx) = oneshot::channel();
    let app = Router::new()
        .route("/index/api/closeRtpServer", get(close_rtp_server))
        .with_state(StubState {
            calls: calls.clone(),
            release: Arc::new(TokioMutex::new(Some(release_rx))),
        });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr should exist");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("stub server should run");
    });

    let mut settings = AgentSettings::default();
    settings.zlm_api_base = format!("http://{addr}");
    settings.zlm_api_secret = "secret".to_string();
    settings.work_root = std::env::temp_dir()
        .join(format!(
            "streamserver-runtime-manager-{name}-{}",
            Uuid::now_v7()
        ))
        .display()
        .to_string();
    (settings, release_tx, calls)
}

async fn recording_mock_settings(
    name: &'static str,
    blocked_endpoint: Option<&'static str>,
    start_code: i64,
    stop_code: i64,
) -> (
    AgentSettings,
    Option<oneshot::Sender<()>>,
    Arc<TokioMutex<Vec<String>>>,
) {
    use axum::{
        Json, Router,
        extract::{Query, State},
        routing::get,
    };
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    #[derive(Clone)]
    struct StubState {
        calls: Arc<TokioMutex<Vec<String>>>,
        release: Arc<TokioMutex<Option<oneshot::Receiver<()>>>>,
        blocked_endpoint: Option<&'static str>,
        start_code: i64,
        stop_code: i64,
    }

    async fn start_record(
        State(state): State<StubState>,
        Query(_params): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        state.calls.lock().await.push("startRecord".to_string());
        if state.blocked_endpoint == Some("startRecord") {
            if let Some(release) = state.release.lock().await.take() {
                let _ = release.await;
            }
        }
        Json(json!({"code": state.start_code, "msg": "injected start result"}))
    }

    async fn stop_record(
        State(state): State<StubState>,
        Query(_params): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        state.calls.lock().await.push("stopRecord".to_string());
        if state.blocked_endpoint == Some("stopRecord") {
            if let Some(release) = state.release.lock().await.take() {
                let _ = release.await;
            }
        }
        Json(json!({"code": state.stop_code, "msg": "injected stop result"}))
    }

    let calls = Arc::new(TokioMutex::new(Vec::new()));
    let (release_tx, release_rx) = oneshot::channel();
    let app = Router::new()
        .route("/index/api/startRecord", get(start_record))
        .route("/index/api/stopRecord", get(stop_record))
        .with_state(StubState {
            calls: calls.clone(),
            release: Arc::new(TokioMutex::new(Some(release_rx))),
            blocked_endpoint,
            start_code,
            stop_code,
        });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr should exist");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("stub server should run");
    });

    let mut settings = AgentSettings::default();
    settings.zlm_api_base = format!("http://{addr}");
    settings.zlm_api_secret = "secret".to_string();
    settings.work_root = std::env::temp_dir()
        .join(format!(
            "streamserver-runtime-manager-{name}-{}",
            Uuid::now_v7()
        ))
        .display()
        .to_string();
    (settings, blocked_endpoint.map(|_| release_tx), calls)
}

async fn collect_task_event_types(
    rx: &mut mpsc::UnboundedReceiver<RuntimeNotification>,
    expected: usize,
) -> Vec<String> {
    timeout(TokioDuration::from_secs(1), async {
        let mut event_types = Vec::new();
        while event_types.len() < expected {
            match rx
                .recv()
                .await
                .expect("notification stream should stay open")
            {
                RuntimeNotification::TaskEvent(event) => event_types.push(event.event_type),
                RuntimeNotification::TaskSnapshot(_) => {}
                RuntimeNotification::TaskProgress(_) => {}
                RuntimeNotification::TaskLogBatch(_) => {}
            }
        }
        event_types
    })
    .await
    .expect("task events should be delivered")
}

fn runtime_manager_options(
    legacy_read_model: Option<Arc<dyn RuntimeReadModel>>,
) -> RuntimeManagerOptions {
    RuntimeManagerOptions {
        limits: RuntimeManagerLimits::default(),
        legacy_read_model,
    }
}

fn record_max(max: &AtomicUsize, value: usize) {
    let mut observed = max.load(Ordering::SeqCst);
    while value > observed {
        match max.compare_exchange(observed, value, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(next) => observed = next,
        }
    }
}

fn progress_for(handle: &RuntimeHandle) -> RuntimeTaskProgress {
    RuntimeTaskProgress {
        task_id: handle.task_id,
        attempt_no: handle.attempt_no,
        lease_token: "lease".to_string(),
        session_epoch: 1,
        frame: 42,
        fps: 25.0,
        bitrate_kbps: 1200.0,
        speed: 1.0,
        out_time_ms: 1000,
        dup_frames: 0,
        drop_frames: 0,
    }
}

fn start_request() -> StartTaskRequest {
    StartTaskRequest {
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "runtime-manager-test",
            "input": {"kind": "rtsp", "url": "rtsp://127.0.0.1/live"},
            "stream": {"app": "live", "name": "test"},
            "record": {"enabled": false},
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    }
}

fn stop_request(task_id: Uuid) -> StopTaskRequest {
    StopTaskRequest {
        task_id,
        attempt_no: 1,
        lease_token: "lease".to_string(),
        reason: "test".to_string(),
        grace_period_sec: 0,
        force_after_sec: 0,
    }
}

fn rtp_runtime_handle() -> RuntimeHandle {
    let task_id = Uuid::now_v7();
    let mut handle = test_handle(task_id, 1, "lease".to_string(), WorkerKind::ZlmRtpServer, 1);
    handle.pid = None;
    handle.metadata = json!({
        "lease_token": "lease",
        "session_epoch": 1,
        "rtp_stream_id": format!("{}-1", task_id),
        "resolved_spec": {
            "type": "stream_ingest",
            "name": "runtime-manager-rtp-stop",
            "input": {
                "kind": "gb_rtp",
                "source_mode": "live",
                "port": 0,
                "tcp_mode": 0
            },
            "record": {"enabled": false},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        },
    });
    handle
}

fn recording_runtime_handle(
    stream_online: bool,
    recording: Option<LiveRelayRecording>,
) -> RuntimeHandle {
    let task_id = Uuid::now_v7();
    let mut handle = test_handle(task_id, 1, "lease".to_string(), WorkerKind::ZlmProxy, 1);
    let mut metadata = json!({
        "task_type": "stream_ingest",
        "execution_mode": "zlm_proxy",
        "lease_token": "lease",
        "session_epoch": 1,
        "stream_online": stream_online,
        "stream_binding": {
            "schema": "rtmp",
            "vhost": "__defaultVhost__",
            "app": "live",
            "stream": format!("recording-{}", task_id),
        },
        "resolved_spec": {
            "type": "stream_ingest",
            "name": "runtime-manager-recording",
            "input": {
                "kind": "rtsp",
                "source_mode": "live",
                "url": "rtsp://127.0.0.1/live"
            },
            "stream": {
                "app": "live",
                "name": format!("recording-{}", task_id)
            },
            "record": {"enabled": false},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        },
    });
    if let Some(recording) = recording {
        metadata["recording"] = json!(recording);
    }
    handle.metadata = metadata;
    handle
}

fn started_recording(command_id: &str) -> LiveRelayRecording {
    LiveRelayRecording {
        formats: vec![ZlmRecordKind::Mp4],
        root_path_mp4: Some("/tmp/streamserver-recording/mp4".to_string()),
        root_path_hls: None,
        duration_sec: None,
        segment_sec: None,
        as_player: false,
        desired_enabled: true,
        manual_control: true,
        stop_task_on_duration: false,
        control_command_id: Some(command_id.to_string()),
        recording_started_at: Some(Utc::now()),
        auto_stop_requested: false,
        completion_reason: None,
        started: true,
        failed: false,
    }
}

fn recording_request(task_id: Uuid) -> TaskRecordingControlRequest {
    TaskRecordingControlRequest {
        task_id,
        attempt_no: 1,
        lease_token: "lease".to_string(),
        action: RecordingControlAction::Start,
        record: None,
        reason: "test".to_string(),
        command_id: Uuid::now_v7().to_string(),
    }
}

fn adopt_filter(task_id: Uuid) -> AdoptFilter {
    AdoptFilter {
        session_epoch: 7,
        runtimes: vec![AdoptRuntimeFilter {
            task_id,
            attempt_no: 2,
            lease_token: "adopt-lease".to_string(),
            worker_kind: WorkerKind::Ffmpeg,
        }],
    }
}

fn test_handle(
    task_id: Uuid,
    attempt_no: i32,
    lease_token: String,
    worker_kind: WorkerKind,
    session_epoch: u64,
) -> RuntimeHandle {
    RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id,
        attempt_no,
        worker_kind,
        pid: Some(1),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Running,
        command_line: None,
        outputs: Vec::new(),
        metadata: json!({
            "lease_token": lease_token,
            "session_epoch": session_epoch,
        }),
    }
}
