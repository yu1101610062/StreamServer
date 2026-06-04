use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{
    runtime::RECORD_DURATION_FORCE_KILL_DELAY,
    runtime_adoption::RuntimeAdoptionOutcome,
    runtime_controls::{
        RuntimeRecordingCommandIdStatus, RuntimeRecordingOutcome, RuntimeRecordingPreparation,
        RuntimeRecordingWorkerRequest, recording_command_id_status, recording_control_action_name,
        recording_control_request_fingerprint,
    },
    runtime_events::{RuntimeNotification, runtime_session_epoch},
    runtime_executor::{
        ManagedProcessExecutor, RuntimeProcessExitOutcome, RuntimeStartWorkerResult,
    },
    runtime_metadata::runtime_lease_token,
    runtime_registry::{AdoptFilter, AdoptRuntimeFilter, RuntimeReadHandle},
    runtime_stop::{RuntimeStopOutcome, RuntimeStopPreparation, RuntimeStopWorkerRequest},
    runtime_types::{
        ExecutorError, StartTaskRequest, StopTaskRequest, TaskRecordingControlRequest,
    },
};

use super::{
    command::{
        RUNTIME_MANAGER_COMMAND_BUFFER, RuntimeAdoptReply, RuntimeCommand, RuntimeManagerLimits,
        RuntimeManagerRequestOutcome, RuntimeRecordingReply, RuntimeStartReply, RuntimeStopReply,
    },
    handle::{RuntimeManagerHandle, RuntimeMonitorHandle},
    internal_event::{
        CompanionProcessExitedEvent, ProgressObservedEvent, RecordDurationReachedEvent,
        RuntimeGeneration, RuntimeInternalEvent, RuntimeMonitorCommit, RuntimeMonitorSnapshot,
    },
    state::{RuntimeEntry, RuntimeManagerState, RuntimeOperationId},
};

pub struct RuntimeManager {
    executor: Arc<ManagedProcessExecutor>,
    tx: mpsc::Sender<RuntimeCommand>,
    rx: mpsc::Receiver<RuntimeCommand>,
    // 当前 Core 控制流的会话号。带 session 的控制命令必须匹配它，cleanup/internal 等
    // sessionless 命令不受连接重建影响。
    active_session_epoch: Option<u64>,
    limits: RuntimeManagerLimits,
    read_handle: RuntimeReadHandle,
    // actor 内的权威 runtime 状态；外部同步读由 read_handle 投影提供。
    state: RuntimeManagerState,
    next_operation_id: u64,
    next_generation: u64,
    // active 计数只在 actor 线程内增减，worker 完成后通过 Finished command 回来释放名额。
    active: RuntimeManagerActiveCounts,
    // 四类控制命令各自 FIFO 排队，互不抢占对方的并发上限。
    queues: RuntimeManagerQueues,
    // 录制控制用 command_id 做幂等和 pending 复用，避免同一请求重复调用 ZLM。
    pending_recording: HashMap<RuntimeRecordingCommandKey, PendingRecordingControl>,
    pending_recording_by_runtime: HashMap<uuid::Uuid, RuntimeRecordingCommandKey>,
}

#[derive(Clone, Default)]
pub(crate) struct RuntimeManagerOptions {
    pub(crate) limits: RuntimeManagerLimits,
}

#[derive(Debug, Default)]
struct RuntimeManagerActiveCounts {
    // 这些计数表示已经派发但尚未回 actor 收尾的 worker 数。
    start: usize,
    stop: usize,
    recording: usize,
    adopt: usize,
}

#[derive(Default)]
struct RuntimeManagerQueues {
    // 队列只保存还没有派发给 worker 的请求；session 过期时会在 actor 内统一清理。
    start: VecDeque<QueuedStart>,
    stop: VecDeque<QueuedStop>,
    recording: VecDeque<QueuedRecording>,
    adopt: VecDeque<QueuedAdopt>,
}

struct QueuedStart {
    operation_id: RuntimeOperationId,
    session_epoch: Option<u64>,
    request: StartTaskRequest,
    reply: RuntimeStartReply,
}

struct QueuedStop {
    operation_id: RuntimeOperationId,
    session_epoch: Option<u64>,
    request: StopTaskRequest,
    reply: Option<RuntimeStopReply>,
}

struct PreparedStopWorker {
    operation_id: RuntimeOperationId,
    session_epoch: Option<u64>,
    request: StopTaskRequest,
    reply: Option<RuntimeStopReply>,
    generation: RuntimeGeneration,
    worker: RuntimeStopWorkerRequest,
}

struct QueuedRecording {
    operation_id: RuntimeOperationId,
    session_epoch: Option<u64>,
    request: TaskRecordingControlRequest,
    reply: RuntimeRecordingReply,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RuntimeRecordingCommandKey {
    runtime_id: uuid::Uuid,
    command_id: String,
}

struct PendingRecordingControl {
    operation_id: RuntimeOperationId,
    generation: RuntimeGeneration,
    task_id: uuid::Uuid,
    attempt_no: i32,
    action: &'static str,
    request_fingerprint: String,
    replies: Vec<PendingRecordingReply>,
}

struct PendingRecordingReply {
    session_epoch: Option<u64>,
    reply: RuntimeRecordingReply,
}

struct QueuedAdopt {
    operation_id: RuntimeOperationId,
    session_epoch: Option<u64>,
    filter: AdoptFilter,
    reply: RuntimeAdoptReply,
}

impl RuntimeManager {
    pub(crate) fn spawn_managed_with_options(
        executor: Arc<ManagedProcessExecutor>,
        options: RuntimeManagerOptions,
    ) -> RuntimeManagerHandle {
        let (tx, rx) = mpsc::channel(RUNTIME_MANAGER_COMMAND_BUFFER);
        let read_handle = RuntimeReadHandle::new();
        tokio::spawn(
            Self {
                executor,
                tx: tx.clone(),
                rx,
                active_session_epoch: None,
                limits: options.limits,
                read_handle: read_handle.clone(),
                state: RuntimeManagerState::default(),
                next_operation_id: 0,
                next_generation: 0,
                active: RuntimeManagerActiveCounts::default(),
                queues: RuntimeManagerQueues::default(),
                pending_recording: HashMap::new(),
                pending_recording_by_runtime: HashMap::new(),
            }
            .run(),
        );
        RuntimeManagerHandle::new(tx, read_handle)
    }

    async fn run(mut self) {
        // actor loop 只做短同步决策：入队、限流、session/generation 校验和状态提交。
        // 任何可能长时间等待的 I/O、进程信号、ZLM API 调用都派发到 worker task。
        while let Some(command) = self.rx.recv().await {
            match command {
                RuntimeCommand::BeginSession { session_epoch } => {
                    // 新控制流成为唯一有效会话，旧会话尚未派发的命令会被回 StaleSession。
                    self.active_session_epoch = Some(session_epoch);
                    self.drop_stale_session_work();
                    self.drain_queues();
                }
                RuntimeCommand::EndSession { session_epoch } => {
                    if self.active_session_epoch == Some(session_epoch) {
                        // 只结束当前会话；迟到的 EndSession 不能清掉新连接的 active epoch。
                        self.active_session_epoch = None;
                        self.drop_stale_session_work();
                        self.drain_queues();
                    }
                }
                RuntimeCommand::CheckSession {
                    session_epoch,
                    reply,
                } => {
                    let outcome = if self.is_session_current(session_epoch) {
                        RuntimeManagerRequestOutcome::Completed(())
                    } else {
                        RuntimeManagerRequestOutcome::StaleSession
                    };
                    let _ = reply.send(outcome);
                }
                RuntimeCommand::StartTaskInSession {
                    session_epoch,
                    request,
                    reply,
                } => {
                    if self.is_session_current(session_epoch) {
                        let operation_id = self.begin_operation();
                        self.queues.start.push_back(QueuedStart {
                            operation_id,
                            session_epoch: Some(session_epoch),
                            request,
                            reply: RuntimeStartReply::Session(reply),
                        });
                        self.drain_queues();
                    } else {
                        let _ = reply.send(RuntimeManagerRequestOutcome::StaleSession);
                    }
                }
                RuntimeCommand::StopTask { request, reply } => {
                    let operation_id = self.begin_operation();
                    self.queues.stop.push_back(QueuedStop {
                        operation_id,
                        session_epoch: None,
                        request,
                        reply: Some(RuntimeStopReply::Sessionless(reply)),
                    });
                    self.drain_queues();
                }
                RuntimeCommand::StopTaskInSession {
                    session_epoch,
                    request,
                    reply,
                } => {
                    if self.is_session_current(session_epoch) {
                        let operation_id = self.begin_operation();
                        self.queues.stop.push_back(QueuedStop {
                            operation_id,
                            session_epoch: Some(session_epoch),
                            request,
                            reply: Some(RuntimeStopReply::Session(reply)),
                        });
                        self.drain_queues();
                    } else {
                        let _ = reply.send(RuntimeManagerRequestOutcome::StaleSession);
                    }
                }
                RuntimeCommand::SetTaskRecordingInSession {
                    session_epoch,
                    request,
                    reply,
                } => {
                    if self.is_session_current(session_epoch) {
                        let operation_id = self.begin_operation();
                        self.queues.recording.push_back(QueuedRecording {
                            operation_id,
                            session_epoch: Some(session_epoch),
                            request,
                            reply: RuntimeRecordingReply::Session(reply),
                        });
                        self.drain_queues();
                    } else {
                        let _ = reply.send(RuntimeManagerRequestOutcome::StaleSession);
                    }
                }
                RuntimeCommand::AdoptOrphansInSession {
                    session_epoch,
                    filter,
                    reply,
                } => {
                    if self.is_session_current(session_epoch) {
                        let operation_id = self.begin_operation();
                        self.queues.adopt.push_back(QueuedAdopt {
                            operation_id,
                            session_epoch: Some(session_epoch),
                            filter,
                            reply: RuntimeAdoptReply::Session(reply),
                        });
                        self.drain_queues();
                    } else {
                        let _ = reply.send(RuntimeManagerRequestOutcome::StaleSession);
                    }
                }
                RuntimeCommand::StartTaskFinished {
                    operation_id,
                    session_epoch,
                    request,
                    reply,
                    result,
                } => {
                    self.active.start = self.active.start.saturating_sub(1);
                    self.finish_start(operation_id, session_epoch, request, reply, result)
                        .await;
                    self.drain_queues();
                }
                RuntimeCommand::StopTaskFinished {
                    operation_id,
                    session_epoch,
                    generation,
                    request,
                    reply,
                    result,
                } => {
                    self.active.stop = self.active.stop.saturating_sub(1);
                    self.finish_stop(
                        operation_id,
                        session_epoch,
                        generation,
                        request,
                        reply,
                        result,
                    );
                    self.drain_queues();
                }
                RuntimeCommand::SetTaskRecordingForManagerFinished {
                    runtime_id,
                    command_id,
                    generation,
                    result,
                } => {
                    self.active.recording = self.active.recording.saturating_sub(1);
                    self.finish_recording_for_manager(
                        RuntimeRecordingCommandKey {
                            runtime_id,
                            command_id,
                        },
                        generation,
                        result,
                    );
                    self.drain_queues();
                }
                RuntimeCommand::AdoptOrphansForManagerFinished {
                    operation_id,
                    session_epoch,
                    adopt_session_epoch,
                    reply,
                    existing,
                    outcomes,
                } => {
                    self.active.adopt = self.active.adopt.saturating_sub(1);
                    self.finish_adopt_for_manager(
                        operation_id,
                        session_epoch,
                        adopt_session_epoch,
                        reply,
                        existing,
                        outcomes,
                    )
                    .await;
                    self.drain_queues();
                }
                RuntimeCommand::ObserveRuntimeSnapshot { handle } => {
                    self.apply_state_handle(handle);
                }
                RuntimeCommand::MonitorSnapshot {
                    runtime_id,
                    generation,
                    reply,
                } => {
                    let snapshot = self.monitor_snapshot(runtime_id, generation);
                    let _ = reply.send(snapshot);
                }
                RuntimeCommand::RuntimeInternalEvent { event } => {
                    self.handle_internal_event(event).await;
                }
                RuntimeCommand::ProcessExitFinished {
                    runtime_id,
                    generation,
                    result,
                } => {
                    self.finish_process_exit(runtime_id, generation, result)
                        .await;
                    self.drain_queues();
                }
                RuntimeCommand::SetZlmServerId { server_id } => {
                    self.executor.set_zlm_server_id(server_id);
                }
                RuntimeCommand::SetZlmRtmpEnhancedEnabled { enabled } => {
                    self.executor.set_zlm_rtmp_enhanced_enabled(enabled);
                }
                RuntimeCommand::Shutdown => break,
            }
        }
    }

    fn drain_queues(&mut self) {
        // 每次入队、worker 完成或会话切换后都尝试 drain。顺序固定，容量由各队列自己的
        // active 计数和 limit 控制，不把并发 permit 暴露给 controller。
        self.drain_start_queue();
        self.drain_stop_queue();
        self.drain_recording_queue();
        self.drain_adopt_queue();
    }

    fn drain_start_queue(&mut self) {
        // start worker 负责准备 runtime outcome；真正的状态/read-handle/backend commit 在
        // finish_start 中回到 actor 线程完成。
        while self.active.start < self.limits.start {
            let Some(queued) = self.queues.start.pop_front() else {
                break;
            };
            if is_stale_session(queued.session_epoch, self.active_session_epoch) {
                self.finish_operation(queued.operation_id);
                send_start_reply(queued.reply, RuntimeManagerRequestOutcome::StaleSession);
                continue;
            }
            self.active.start = self.active.start.saturating_add(1);
            let executor = self.executor.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let request_for_worker = queued.request.clone();
                let result = executor.start_task_for_manager(request_for_worker).await;
                let _ = tx
                    .send(RuntimeCommand::StartTaskFinished {
                        operation_id: queued.operation_id,
                        session_epoch: queued.session_epoch,
                        request: queued.request,
                        reply: queued.reply,
                        result,
                    })
                    .await;
            });
        }
    }

    fn drain_stop_queue(&mut self) {
        // stop 的生产路径先在 actor 内校验 runtime/lease/generation 并提交 Stopping，
        // 再把 signal、ZLM close、RTP close 等慢副作用交给 worker。
        while self.active.stop < self.limits.stop {
            let Some(queued) = self.queues.stop.pop_front() else {
                break;
            };
            if is_stale_session(queued.session_epoch, self.active_session_epoch) {
                self.finish_operation(queued.operation_id);
                if let Some(reply) = queued.reply {
                    send_stop_reply(reply, RuntimeManagerRequestOutcome::StaleSession);
                }
                continue;
            }
            let Some(prepared) = self.prepare_actor_stop(queued) else {
                continue;
            };
            self.active.stop = self.active.stop.saturating_add(1);
            let generation = prepared.generation;
            let executor = self.executor.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let result = executor.run_stop_worker_for_manager(prepared.worker).await;
                let _ = tx
                    .send(RuntimeCommand::StopTaskFinished {
                        operation_id: prepared.operation_id,
                        session_epoch: prepared.session_epoch,
                        generation,
                        request: prepared.request,
                        reply: prepared.reply,
                        result,
                    })
                    .await;
            });
        }
    }

    fn drain_recording_queue(&mut self) {
        // recording 与其他控制命令独立限流；同 command_id 的重复请求可以穿透容量检查，
        // 复用 pending worker 或已完成结果，不重复调用 ZLM。
        loop {
            let Some(queued) = self.queues.recording.pop_front() else {
                break;
            };
            if is_stale_session(queued.session_epoch, self.active_session_epoch) {
                self.finish_operation(queued.operation_id);
                send_recording_reply(queued.reply, RuntimeManagerRequestOutcome::StaleSession);
                continue;
            }
            if self.active.recording >= self.limits.recording
                && !self.recording_can_run_without_capacity(&queued.request)
            {
                self.queues.recording.push_front(queued);
                break;
            }
            let Some((key, generation, worker)) = self.prepare_actor_recording(queued) else {
                continue;
            };
            self.active.recording = self.active.recording.saturating_add(1);
            let executor = self.executor.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let result = executor.run_recording_worker_for_manager(worker).await;
                let _ = tx
                    .send(RuntimeCommand::SetTaskRecordingForManagerFinished {
                        runtime_id: key.runtime_id,
                        command_id: key.command_id,
                        generation,
                        result,
                    })
                    .await;
            });
        }
    }

    fn recording_can_run_without_capacity(
        &mut self,
        request: &TaskRecordingControlRequest,
    ) -> bool {
        // 容量已满时仍允许纯 actor 判定继续执行：重复 command_id、冲突请求或已有
        // pending 的 runtime 都不需要新增 worker，不能被队头阻塞。
        let Some(entry) = self.resolve_recording_entry(request) else {
            return true;
        };
        let request_fingerprint = recording_control_request_fingerprint(request);
        if recording_command_id_status(&entry.handle, request, &request_fingerprint)
            != RuntimeRecordingCommandIdStatus::New
        {
            return true;
        }
        let key = RuntimeRecordingCommandKey {
            runtime_id: entry.handle.runtime_id,
            command_id: request.command_id.clone(),
        };
        self.pending_recording.contains_key(&key)
            || self
                .pending_recording_by_runtime
                .contains_key(&entry.handle.runtime_id)
    }

    fn drain_adopt_queue(&mut self) {
        // adopt 默认单并发。已经在 manager state 中的 runtime 直接 reattach；真正需要扫描、
        // 探测或重启的剩余项才进入 adopt worker。
        while self.active.adopt < self.limits.adopt {
            let Some(queued) = self.queues.adopt.pop_front() else {
                break;
            };
            if is_stale_session(queued.session_epoch, self.active_session_epoch) {
                self.finish_operation(queued.operation_id);
                send_adopt_reply(queued.reply, RuntimeManagerRequestOutcome::StaleSession);
                continue;
            }
            let (existing, remaining_filter) = self.prepare_existing_adoptions(&queued.filter);
            if remaining_filter.runtimes.is_empty() {
                self.finish_adopt_existing_for_manager(
                    queued.operation_id,
                    queued.session_epoch,
                    queued.filter.session_epoch,
                    queued.reply,
                    existing,
                );
                continue;
            }

            self.active.adopt = self.active.adopt.saturating_add(1);
            let executor = self.executor.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let outcomes = executor
                    .prepare_adopt_orphans_for_manager(remaining_filter)
                    .await;
                let _ = tx
                    .send(RuntimeCommand::AdoptOrphansForManagerFinished {
                        operation_id: queued.operation_id,
                        session_epoch: queued.session_epoch,
                        adopt_session_epoch: queued.filter.session_epoch,
                        reply: queued.reply,
                        existing,
                        outcomes,
                    })
                    .await;
            });
        }
    }

    fn prepare_actor_stop(&mut self, queued: QueuedStop) -> Option<PreparedStopWorker> {
        // manager stop path 的状态提交都发生在 worker 前；worker 只拿到 actor 已验证的
        // backend snapshot/request，不能再写 runtime 状态源。
        let Some(entry) = self.resolve_stop_entry(&queued.request) else {
            self.finish_operation(queued.operation_id);
            if let Some(reply) = queued.reply {
                send_stop_reply(reply, RuntimeManagerRequestOutcome::Completed(Ok(())));
            }
            return None;
        };

        let handle_lease_token = runtime_lease_token(&entry.handle).unwrap_or_default();
        if handle_lease_token != queued.request.lease_token {
            self.finish_operation(queued.operation_id);
            if let Some(reply) = queued.reply {
                send_stop_reply(
                    reply,
                    RuntimeManagerRequestOutcome::Completed(Err(ExecutorError::InvalidRequest(
                        format!(
                            "stale stop for {}/{}: lease_token mismatch",
                            queued.request.task_id, queued.request.attempt_no
                        ),
                    ))),
                );
            }
            return None;
        }

        let monitor_handle =
            RuntimeMonitorHandle::new(self.tx.clone(), entry.handle.runtime_id, entry.generation);
        match self.executor.prepare_stop_for_manager(
            &queued.request,
            &entry.handle,
            entry.generation,
            monitor_handle,
        ) {
            Ok(RuntimeStopPreparation::Worker { commit, worker }) => {
                self.apply_stop_commit(commit, entry.generation);
                Some(PreparedStopWorker {
                    operation_id: queued.operation_id,
                    session_epoch: queued.session_epoch,
                    request: queued.request,
                    reply: queued.reply,
                    generation: entry.generation,
                    worker,
                })
            }
            Ok(RuntimeStopPreparation::AlreadyGone(commit)) => {
                self.apply_stop_outcome(entry.generation, RuntimeStopOutcome::AlreadyGone(commit));
                self.finish_operation(queued.operation_id);
                if let Some(reply) = queued.reply {
                    send_stop_reply(reply, RuntimeManagerRequestOutcome::Completed(Ok(())));
                }
                None
            }
            Err(error) => {
                self.finish_operation(queued.operation_id);
                if let Some(reply) = queued.reply {
                    send_stop_reply(reply, RuntimeManagerRequestOutcome::Completed(Err(error)));
                }
                None
            }
        }
    }

    fn prepare_actor_recording(
        &mut self,
        queued: QueuedRecording,
    ) -> Option<(
        RuntimeRecordingCommandKey,
        RuntimeGeneration,
        RuntimeRecordingWorkerRequest,
    )> {
        // recording command_id 的幂等窗口由 actor 独占维护，保证同 runtime 同请求只产生
        // 一个慢 worker，等待中的重复请求挂到同一批 reply 上。
        let Some(entry) = self.resolve_recording_entry(&queued.request) else {
            self.finish_operation(queued.operation_id);
            send_recording_reply(
                queued.reply,
                RuntimeManagerRequestOutcome::Completed(Err(ExecutorError::RuntimeNotFound {
                    task_id: queued.request.task_id,
                    attempt_no: queued.request.attempt_no,
                })),
            );
            return None;
        };

        let request_fingerprint = recording_control_request_fingerprint(&queued.request);
        let action = recording_control_action_name(queued.request.action);
        match recording_command_id_status(&entry.handle, &queued.request, &request_fingerprint) {
            RuntimeRecordingCommandIdStatus::Duplicate => {
                self.finish_operation(queued.operation_id);
                send_recording_reply(
                    queued.reply,
                    RuntimeManagerRequestOutcome::Completed(Ok(entry.handle.clone())),
                );
                return None;
            }
            RuntimeRecordingCommandIdStatus::Conflict => {
                self.finish_operation(queued.operation_id);
                send_recording_reply(
                    queued.reply,
                    RuntimeManagerRequestOutcome::Completed(Err(ExecutorError::InvalidRequest(
                        "recording control command_id conflicts with existing request".to_string(),
                    ))),
                );
                return None;
            }
            RuntimeRecordingCommandIdStatus::New => {}
        }

        let key = RuntimeRecordingCommandKey {
            runtime_id: entry.handle.runtime_id,
            command_id: queued.request.command_id.clone(),
        };
        if let Some(matches_pending) = self.pending_recording.get(&key).map(|pending| {
            pending.action == action && pending.request_fingerprint == request_fingerprint
        }) {
            self.finish_operation(queued.operation_id);
            if matches_pending {
                if let Some(pending) = self.pending_recording.get_mut(&key) {
                    pending.replies.push(PendingRecordingReply {
                        session_epoch: queued.session_epoch,
                        reply: queued.reply,
                    });
                }
            } else {
                send_recording_reply(
                    queued.reply,
                    RuntimeManagerRequestOutcome::Completed(Err(ExecutorError::InvalidRequest(
                        "recording control command_id conflicts with existing request".to_string(),
                    ))),
                );
            }
            return None;
        }

        if self
            .pending_recording_by_runtime
            .get(&entry.handle.runtime_id)
            .is_some()
        {
            self.finish_operation(queued.operation_id);
            send_recording_reply(
                queued.reply,
                RuntimeManagerRequestOutcome::Completed(Err(ExecutorError::InvalidRequest(
                    "recording control is already in progress for this runtime".to_string(),
                ))),
            );
            return None;
        }

        let monitor_handle =
            RuntimeMonitorHandle::new(self.tx.clone(), entry.handle.runtime_id, entry.generation);
        match self.executor.prepare_recording_for_manager(
            &queued.request,
            &entry.handle,
            entry.generation,
            monitor_handle,
        ) {
            Ok(RuntimeRecordingPreparation::Unchanged(handle)) => {
                self.finish_operation(queued.operation_id);
                send_recording_reply(
                    queued.reply,
                    RuntimeManagerRequestOutcome::Completed(Ok(handle)),
                );
                None
            }
            Ok(RuntimeRecordingPreparation::Immediate(commit)) => {
                let result = self.apply_recording_commit(commit, entry.generation);
                self.finish_operation(queued.operation_id);
                send_recording_reply(
                    queued.reply,
                    RuntimeManagerRequestOutcome::Completed(result),
                );
                None
            }
            Ok(RuntimeRecordingPreparation::Worker {
                initial_commit,
                worker,
            }) => {
                let _ = self.apply_recording_commit(initial_commit, entry.generation);
                self.pending_recording_by_runtime
                    .insert(key.runtime_id, key.clone());
                self.pending_recording.insert(
                    key.clone(),
                    PendingRecordingControl {
                        operation_id: queued.operation_id,
                        generation: entry.generation,
                        task_id: queued.request.task_id,
                        attempt_no: queued.request.attempt_no,
                        action,
                        request_fingerprint,
                        replies: vec![PendingRecordingReply {
                            session_epoch: queued.session_epoch,
                            reply: queued.reply,
                        }],
                    },
                );
                Some((key, entry.generation, worker))
            }
            Err(error) => {
                self.finish_operation(queued.operation_id);
                send_recording_reply(
                    queued.reply,
                    RuntimeManagerRequestOutcome::Completed(Err(error)),
                );
                None
            }
        }
    }

    fn resolve_stop_entry(&mut self, request: &StopTaskRequest) -> Option<RuntimeEntry> {
        self.state
            .entry_by_task_attempt(request.task_id, request.attempt_no)
            .cloned()
    }

    fn resolve_recording_entry(
        &mut self,
        request: &TaskRecordingControlRequest,
    ) -> Option<RuntimeEntry> {
        self.state
            .entry_by_task_attempt(request.task_id, request.attempt_no)
            .cloned()
    }

    fn prepare_existing_adoptions(
        &mut self,
        filter: &AdoptFilter,
    ) -> (Vec<(RuntimeHandle, RuntimeGeneration)>, AdoptFilter) {
        let mut existing = Vec::new();
        let mut remaining = Vec::new();
        let mut seen = HashSet::new();

        for runtime_filter in &filter.runtimes {
            let key = (runtime_filter.task_id, runtime_filter.attempt_no);
            if seen.contains(&key) {
                continue;
            }
            if let Some(entry) = self.resolve_existing_adoption_entry(runtime_filter) {
                existing.push((entry.handle, entry.generation));
                seen.insert(key);
            } else {
                remaining.push(runtime_filter.clone());
            }
        }

        (
            existing,
            AdoptFilter {
                session_epoch: filter.session_epoch,
                runtimes: remaining,
            },
        )
    }

    fn resolve_existing_adoption_entry(
        &mut self,
        filter: &AdoptRuntimeFilter,
    ) -> Option<RuntimeEntry> {
        self.state
            .entry_by_task_attempt(filter.task_id, filter.attempt_no)
            .filter(|entry| adopt_filter_matches_handle(filter, &entry.handle))
            .cloned()
    }

    async fn finish_start(
        &mut self,
        operation_id: RuntimeOperationId,
        session_epoch: Option<u64>,
        request: StartTaskRequest,
        reply: RuntimeStartReply,
        result: Result<RuntimeStartWorkerResult, crate::runtime_types::ExecutorError>,
    ) {
        self.finish_operation(operation_id);
        let generation = self.next_runtime_generation();
        let result = match result {
            Ok(result) => match result.runtime_id() {
                Some(runtime_id) => {
                    let monitor_handle =
                        RuntimeMonitorHandle::new(self.tx.clone(), runtime_id, generation);
                    result.commit(monitor_handle).await
                }
                None => Err(ExecutorError::InvalidRequest(
                    "runtime start result did not include runtime_id".to_string(),
                )),
            },
            Err(error) => Err(error),
        };
        let start_succeeded = result.is_ok();
        if let Ok(handle) = &result {
            self.apply_start_result_to_state(handle, generation);
        }
        if let Some(session_epoch) = session_epoch {
            if self.is_session_current(session_epoch) {
                send_start_reply(reply, RuntimeManagerRequestOutcome::Completed(result));
            } else {
                if start_succeeded {
                    // start 已成功但所属 control session 过期时，actor 插入 sessionless stop，
                    // 避免旧连接启动出的 runtime 泄漏到新会话。
                    let operation_id = self.begin_operation();
                    self.queues.stop.push_front(QueuedStop {
                        operation_id,
                        session_epoch: None,
                        request: StopTaskRequest {
                            task_id: request.task_id,
                            attempt_no: request.attempt_no,
                            lease_token: request.lease_token,
                            reason: "stale_session_replaced".to_string(),
                            grace_period_sec: 0,
                            force_after_sec: 1,
                        },
                        reply: None,
                    });
                }
                send_start_reply(reply, RuntimeManagerRequestOutcome::StaleSession);
            }
        } else {
            send_start_reply(reply, RuntimeManagerRequestOutcome::Completed(result));
        }
    }

    fn finish_stop(
        &mut self,
        operation_id: RuntimeOperationId,
        session_epoch: Option<u64>,
        generation: RuntimeGeneration,
        _request: StopTaskRequest,
        reply: Option<RuntimeStopReply>,
        result: Result<RuntimeStopOutcome, crate::runtime_types::ExecutorError>,
    ) {
        self.finish_operation(operation_id);
        let result = match result {
            Ok(outcome) => {
                self.apply_stop_outcome(generation, outcome);
                Ok(())
            }
            Err(error) => Err(error),
        };
        let Some(reply) = reply else {
            return;
        };
        if let Some(session_epoch) = session_epoch {
            if self.is_session_current(session_epoch) {
                send_stop_reply(reply, RuntimeManagerRequestOutcome::Completed(result));
            } else {
                send_stop_reply(reply, RuntimeManagerRequestOutcome::StaleSession);
            }
        } else {
            send_stop_reply(reply, RuntimeManagerRequestOutcome::Completed(result));
        }
    }

    fn apply_stop_outcome(&mut self, generation: RuntimeGeneration, outcome: RuntimeStopOutcome) {
        match outcome {
            RuntimeStopOutcome::ManagedProcessStopAccepted => {}
            RuntimeStopOutcome::Terminal(commit) | RuntimeStopOutcome::AlreadyGone(commit) => {
                self.apply_stop_commit(commit, generation);
            }
        }
    }

    fn apply_stop_commit(&mut self, commit: RuntimeMonitorCommit, generation: RuntimeGeneration) {
        if commit.generation != generation {
            return;
        }
        let Some(entry) = self.state.entry(commit.runtime_id) else {
            return;
        };
        if entry.generation != generation {
            return;
        }
        let runtime_id = commit.runtime_id;
        let remove = commit.remove_runtime_entry || commit.handle.state == RuntimeState::Exited;
        let handle = commit.handle.clone();
        let commit_generation = commit.generation;
        self.executor.apply_monitor_commit(commit);
        if remove {
            self.state.remove_runtime_id(runtime_id);
            self.read_handle.remove_runtime_id(runtime_id);
            self.assert_state_consistency();
        } else {
            self.apply_state_handle_with_generation(handle, commit_generation);
        }
    }

    fn finish_recording_for_manager(
        &mut self,
        key: RuntimeRecordingCommandKey,
        generation: RuntimeGeneration,
        result: Result<RuntimeRecordingOutcome, crate::runtime_types::ExecutorError>,
    ) {
        let Some(pending) = self.pending_recording.remove(&key) else {
            return;
        };
        self.pending_recording_by_runtime.remove(&key.runtime_id);
        self.finish_operation(pending.operation_id);

        if pending.generation != generation {
            let result = self
                .state
                .entry(key.runtime_id)
                .map(|entry| Ok(entry.handle.clone()))
                .unwrap_or(Err(ExecutorError::RuntimeNotFound {
                    task_id: pending.task_id,
                    attempt_no: pending.attempt_no,
                }));
            for reply in pending.replies {
                send_recording_reply(
                    reply.reply,
                    self.recording_reply_outcome(
                        reply.session_epoch,
                        clone_recording_result(&result),
                    ),
                );
            }
            return;
        }

        let result = match result {
            Ok(outcome) => self.apply_recording_outcome(generation, outcome),
            Err(error) => Err(error),
        };
        for reply in pending.replies {
            send_recording_reply(
                reply.reply,
                self.recording_reply_outcome(reply.session_epoch, clone_recording_result(&result)),
            );
        }
    }

    fn apply_recording_outcome(
        &mut self,
        generation: RuntimeGeneration,
        outcome: RuntimeRecordingOutcome,
    ) -> Result<RuntimeHandle, crate::runtime_types::ExecutorError> {
        match outcome {
            RuntimeRecordingOutcome::Updated(commit) => {
                self.apply_recording_commit(commit, generation)
            }
            RuntimeRecordingOutcome::Unchanged(handle) => Ok(handle),
        }
    }

    fn apply_recording_commit(
        &mut self,
        commit: RuntimeMonitorCommit,
        generation: RuntimeGeneration,
    ) -> Result<RuntimeHandle, crate::runtime_types::ExecutorError> {
        if commit.generation != generation {
            return Ok(commit.handle);
        }
        let Some(entry) = self.state.entry(commit.runtime_id) else {
            return Ok(commit.handle);
        };
        if entry.generation != generation {
            return Ok(entry.handle.clone());
        }
        if entry.handle.state != RuntimeState::Running
            && entry.handle.state != RuntimeState::Starting
        {
            return Ok(entry.handle.clone());
        }

        let handle = commit.handle.clone();
        let commit_generation = commit.generation;
        self.executor.apply_monitor_commit(commit);
        self.apply_state_handle_with_generation(handle.clone(), commit_generation);
        Ok(handle)
    }

    fn recording_reply_outcome(
        &self,
        session_epoch: Option<u64>,
        result: Result<RuntimeHandle, crate::runtime_types::ExecutorError>,
    ) -> RuntimeManagerRequestOutcome<Result<RuntimeHandle, crate::runtime_types::ExecutorError>>
    {
        if let Some(session_epoch) = session_epoch {
            if self.is_session_current(session_epoch) {
                RuntimeManagerRequestOutcome::Completed(result)
            } else {
                RuntimeManagerRequestOutcome::StaleSession
            }
        } else {
            RuntimeManagerRequestOutcome::Completed(result)
        }
    }

    fn finish_adopt_existing_for_manager(
        &mut self,
        operation_id: RuntimeOperationId,
        session_epoch: Option<u64>,
        adopt_session_epoch: u64,
        reply: RuntimeAdoptReply,
        existing: Vec<(RuntimeHandle, RuntimeGeneration)>,
    ) {
        self.finish_operation(operation_id);
        if let Some(session_epoch) = session_epoch {
            if !self.is_session_current(session_epoch) {
                send_adopt_reply(reply, RuntimeManagerRequestOutcome::StaleSession);
                return;
            }
        }
        let handles = self.commit_existing_adoptions(adopt_session_epoch, existing);
        send_adopt_reply(reply, RuntimeManagerRequestOutcome::Completed(handles));
    }

    async fn finish_adopt_for_manager(
        &mut self,
        operation_id: RuntimeOperationId,
        session_epoch: Option<u64>,
        adopt_session_epoch: u64,
        reply: RuntimeAdoptReply,
        existing: Vec<(RuntimeHandle, RuntimeGeneration)>,
        outcomes: Vec<RuntimeAdoptionOutcome<RuntimeStartWorkerResult>>,
    ) {
        self.finish_operation(operation_id);
        if let Some(session_epoch) = session_epoch {
            if !self.is_session_current(session_epoch) {
                send_adopt_reply(reply, RuntimeManagerRequestOutcome::StaleSession);
                return;
            }
        }

        let mut handles = self.commit_existing_adoptions(adopt_session_epoch, existing);
        for outcome in outcomes {
            match outcome {
                RuntimeAdoptionOutcome::Adopted(commit) => {
                    let generation = self.next_runtime_generation();
                    let monitor_handle = RuntimeMonitorHandle::new(
                        self.tx.clone(),
                        commit.handle.runtime_id,
                        generation,
                    );
                    let handle =
                        self.executor
                            .apply_adoption_commit(commit, generation, monitor_handle);
                    self.apply_state_handle_with_generation(handle.clone(), generation);
                    handles.push(handle);
                }
                RuntimeAdoptionOutcome::Restart(result) => {
                    let generation = self.next_runtime_generation();
                    let Some(runtime_id) = result.runtime_id() else {
                        warn!("failed to commit restarted adopted runtime: missing runtime_id");
                        continue;
                    };
                    let monitor_handle =
                        RuntimeMonitorHandle::new(self.tx.clone(), runtime_id, generation);
                    match result.commit(monitor_handle).await {
                        Ok(handle) => {
                            self.apply_start_result_to_state(&handle, generation);
                            handles.push(handle);
                        }
                        Err(error) => {
                            warn!(error = %error, "failed to commit restarted adopted runtime");
                        }
                    }
                }
            }
        }
        send_adopt_reply(reply, RuntimeManagerRequestOutcome::Completed(handles));
    }

    fn commit_existing_adoptions(
        &mut self,
        adopt_session_epoch: u64,
        existing: Vec<(RuntimeHandle, RuntimeGeneration)>,
    ) -> Vec<RuntimeHandle> {
        let mut handles = Vec::new();
        for (handle, generation) in existing {
            let commit = self.executor.prepare_existing_adoption_commit(
                &handle,
                adopt_session_epoch,
                generation,
            );
            let updated = commit.handle.clone();
            self.apply_stop_commit(commit, generation);
            handles.push(updated);
        }
        handles
    }

    fn begin_operation(&mut self) -> RuntimeOperationId {
        self.next_operation_id = self.next_operation_id.saturating_add(1);
        let operation_id = RuntimeOperationId::new(self.next_operation_id);
        self.state.track_operation(operation_id);
        operation_id
    }

    fn next_runtime_generation(&mut self) -> RuntimeGeneration {
        self.next_generation = self.next_generation.saturating_add(1);
        RuntimeGeneration::new(self.next_generation)
    }

    fn finish_operation(&mut self, operation_id: RuntimeOperationId) {
        self.state.finish_operation(operation_id);
    }

    fn apply_state_handle(&mut self, handle: RuntimeHandle) {
        self.read_handle.apply_handle(handle.clone());
        self.state.apply_handle(handle);
        self.assert_state_consistency();
    }

    fn apply_state_handle_with_generation(
        &mut self,
        handle: RuntimeHandle,
        generation: RuntimeGeneration,
    ) {
        self.read_handle.apply_handle(handle.clone());
        self.state.apply_handle_with_generation(handle, generation);
        self.assert_state_consistency();
    }

    fn apply_start_result_to_state(
        &mut self,
        handle: &RuntimeHandle,
        generation: RuntimeGeneration,
    ) {
        self.apply_state_handle_with_generation(handle.clone(), generation);
    }

    #[cfg(test)]
    fn assert_state_consistency(&self) {
        self.state
            .assert_consistent_with_read_model(&self.read_handle);
    }

    #[cfg(not(test))]
    fn assert_state_consistency(&self) {}

    fn drop_stale_session_work(&mut self) {
        // 这里只清理尚未派发的队列项。已经派发的 worker 完成后仍会回到 actor，
        // 再通过 session/generation 校验决定是否提交或只返回 StaleSession。
        let active_session_epoch = self.active_session_epoch;
        let mut start = VecDeque::new();
        while let Some(queued) = self.queues.start.pop_front() {
            if is_stale_session(queued.session_epoch, active_session_epoch) {
                self.finish_operation(queued.operation_id);
                send_start_reply(queued.reply, RuntimeManagerRequestOutcome::StaleSession);
            } else {
                start.push_back(queued);
            }
        }
        self.queues.start = start;

        let mut stop = VecDeque::new();
        while let Some(queued) = self.queues.stop.pop_front() {
            if is_stale_session(queued.session_epoch, active_session_epoch) {
                self.finish_operation(queued.operation_id);
                if let Some(reply) = queued.reply {
                    send_stop_reply(reply, RuntimeManagerRequestOutcome::StaleSession);
                }
            } else {
                stop.push_back(queued);
            }
        }
        self.queues.stop = stop;

        let mut recording = VecDeque::new();
        while let Some(queued) = self.queues.recording.pop_front() {
            if is_stale_session(queued.session_epoch, active_session_epoch) {
                self.finish_operation(queued.operation_id);
                send_recording_reply(queued.reply, RuntimeManagerRequestOutcome::StaleSession);
            } else {
                recording.push_back(queued);
            }
        }
        self.queues.recording = recording;

        let mut adopt = VecDeque::new();
        while let Some(queued) = self.queues.adopt.pop_front() {
            if is_stale_session(queued.session_epoch, active_session_epoch) {
                self.finish_operation(queued.operation_id);
                send_adopt_reply(queued.reply, RuntimeManagerRequestOutcome::StaleSession);
            } else {
                adopt.push_back(queued);
            }
        }
        self.queues.adopt = adopt;
    }

    fn is_session_current(&self, session_epoch: u64) -> bool {
        self.active_session_epoch == Some(session_epoch)
    }

    fn monitor_snapshot(
        &self,
        runtime_id: uuid::Uuid,
        generation: RuntimeGeneration,
    ) -> Option<RuntimeMonitorSnapshot> {
        let entry = self.state.entry(runtime_id)?;
        if entry.generation != generation {
            return None;
        }
        let backend = self.executor.monitor_snapshot(runtime_id)?;
        Some(RuntimeMonitorSnapshot {
            handle: entry.handle.clone(),
            stop_requested: backend.stop_requested,
            companion_processes: backend.companion_processes,
        })
    }

    async fn handle_internal_event(&mut self, event: RuntimeInternalEvent) {
        let runtime_id = event.runtime_id();
        let generation = event.generation();
        let Some(entry) = self.state.entry(runtime_id).cloned() else {
            return;
        };
        if entry.generation != generation {
            return;
        }

        match event {
            RuntimeInternalEvent::ProgressObserved(event) => {
                self.handle_progress_observed(event, entry.generation);
            }
            RuntimeInternalEvent::CompanionProcessExited(event) => {
                self.handle_companion_process_exited(event, entry.generation);
            }
            RuntimeInternalEvent::RecordDurationReached(event) => {
                self.handle_record_duration_reached(event, entry);
            }
            RuntimeInternalEvent::ProcessExited(event) => {
                let executor = self.executor.clone();
                let tx = self.tx.clone();
                let current_handle = entry.handle.clone();
                tokio::spawn(async move {
                    let result = executor.handle_process_exited(event, current_handle).await;
                    let _ = tx
                        .send(RuntimeCommand::ProcessExitFinished {
                            runtime_id,
                            generation,
                            result,
                        })
                        .await;
                });
            }
            RuntimeInternalEvent::PersistenceFailed(event) => {
                warn!(
                    runtime_id = %event.runtime_id,
                    generation = event.generation.value(),
                    error = %event.error,
                    "runtime monitor persistence failed"
                );
            }
            RuntimeInternalEvent::StartupProbeSucceeded(commit)
            | RuntimeInternalEvent::StartupProbeFailed(commit)
            | RuntimeInternalEvent::LiveRelayOffline(commit)
            | RuntimeInternalEvent::RtpServerMissing(commit)
            | RuntimeInternalEvent::ApplyMonitorCommit(commit) => {
                let remove =
                    commit.remove_runtime_entry || commit.handle.state == RuntimeState::Exited;
                let handle = commit.handle.clone();
                self.executor.apply_monitor_commit(commit);
                if remove {
                    self.state.remove_runtime_id(runtime_id);
                    self.read_handle.remove_runtime_id(runtime_id);
                    self.assert_state_consistency();
                } else {
                    self.apply_state_handle_with_generation(handle, generation);
                }
            }
        }
    }

    async fn finish_process_exit(
        &mut self,
        runtime_id: uuid::Uuid,
        generation: RuntimeGeneration,
        result: RuntimeProcessExitOutcome,
    ) {
        let Some(entry) = self.state.entry(runtime_id) else {
            return;
        };
        if entry.generation != generation {
            return;
        }

        match result {
            RuntimeProcessExitOutcome::Terminal(commit) => {
                self.apply_stop_commit(commit, generation);
            }
            RuntimeProcessExitOutcome::Restarted {
                exit_commit,
                mut restart,
                emit_starting_event,
            } => {
                let exited_handle = exit_commit.handle.clone();
                self.apply_stop_commit(exit_commit, generation);

                let restart_generation = self.next_runtime_generation();
                restart.carry_reconnect_metadata_from(&exited_handle);
                let Some(runtime_id) = restart.runtime_id() else {
                    warn!("failed to commit recovered runtime: missing runtime_id");
                    return;
                };
                let monitor_handle =
                    RuntimeMonitorHandle::new(self.tx.clone(), runtime_id, restart_generation);
                match restart.commit(monitor_handle).await {
                    Ok(handle) => {
                        self.apply_start_result_to_state(&handle, restart_generation);
                        let recovered = self
                            .state
                            .entry(handle.runtime_id)
                            .map(|entry| entry.handle.clone())
                            .unwrap_or(handle);
                        let mut notifications = Vec::new();
                        if emit_starting_event {
                            notifications.push(RuntimeNotification::TaskEvent(
                                crate::runtime_events::RuntimeTaskEvent {
                                    task_id: recovered.task_id,
                                    attempt_no: recovered.attempt_no,
                                    lease_token: runtime_lease_token(&recovered)
                                        .unwrap_or_default(),
                                    session_epoch: runtime_session_epoch(&recovered),
                                    event_type: "starting".to_string(),
                                    event_level: "info".to_string(),
                                    message: "runtime handle recreated after local recovery"
                                        .to_string(),
                                    payload: serde_json::json!({
                                        "runtime_id": recovered.runtime_id,
                                        "worker_kind": recovered.worker_kind,
                                        "recovered": true,
                                    }),
                                },
                            ));
                        }
                        notifications.push(RuntimeNotification::TaskSnapshot(recovered.clone()));
                        let commit =
                            RuntimeMonitorCommit::new(recovered.clone(), restart_generation)
                                .with_notifications(notifications);
                        self.executor.apply_monitor_commit(commit);
                        self.apply_state_handle_with_generation(recovered, restart_generation);
                    }
                    Err(error) => {
                        warn!(error = %error, "failed to commit recovered runtime");
                    }
                }
            }
        }
    }

    fn handle_progress_observed(
        &mut self,
        event: ProgressObservedEvent,
        generation: RuntimeGeneration,
    ) {
        let Some(entry) = self.state.entry(event.runtime_id) else {
            return;
        };
        let mut handle = entry.handle.clone();
        handle.last_progress_at = Some(Utc::now());
        handle.state = RuntimeState::Running;
        let commit = RuntimeMonitorCommit::new(handle.clone(), generation)
            .with_notifications(vec![RuntimeNotification::TaskProgress(event.progress)]);
        self.executor.apply_monitor_commit(commit);
        self.apply_state_handle_with_generation(handle, generation);
    }

    fn handle_record_duration_reached(
        &mut self,
        event: RecordDurationReachedEvent,
        entry: RuntimeEntry,
    ) {
        // 录制时长到达是 monitor 观察到的事实，真正的停止请求必须回到 actor 排队。
        // 这样 session/generation、并发限流、Stopping projection 和 force-kill gate
        // 都继续复用 stop_task actor 化后的统一路径。
        let operation_id = self.begin_operation();
        self.queues.stop.push_front(QueuedStop {
            operation_id,
            session_epoch: None,
            request: StopTaskRequest {
                task_id: entry.handle.task_id,
                attempt_no: entry.handle.attempt_no,
                lease_token: runtime_lease_token(&entry.handle).unwrap_or_default(),
                reason: "record_duration_reached".to_string(),
                grace_period_sec: 0,
                force_after_sec: record_duration_force_after_sec(),
            },
            reply: None,
        });
        self.drain_stop_queue();
        info!(
            runtime_id = %event.runtime_id,
            generation = event.generation.value(),
            task_id = %entry.handle.task_id,
            attempt_no = entry.handle.attempt_no,
            "live relay recording duration reached; stop queued through runtime manager"
        );
    }

    fn handle_companion_process_exited(
        &mut self,
        event: CompanionProcessExitedEvent,
        generation: RuntimeGeneration,
    ) {
        let Some(entry) = self.state.entry(event.runtime_id) else {
            return;
        };
        let mut handle = entry.handle.clone();
        crate::runtime_metadata::update_companion_recording_metadata(&mut handle, |companion| {
            companion.pid = None;
            companion.state = if event.succeeded {
                crate::runtime_metadata::CompanionProcessState::Succeeded
            } else {
                crate::runtime_metadata::CompanionProcessState::Failed
            };
            companion.error = event.error.clone();
        });
        let stop_or_suppressed = self
            .executor
            .monitor_snapshot(event.runtime_id)
            .map(|snapshot| snapshot.stop_requested || snapshot.suppress_companion_events)
            .unwrap_or(false);
        let mut notifications = Vec::new();
        if !event.succeeded && !stop_or_suppressed {
            notifications.push(RuntimeNotification::TaskSnapshot(handle.clone()));
            notifications.push(RuntimeNotification::TaskEvent(
                crate::runtime_events::RuntimeTaskEvent {
                    task_id: event.task_id,
                    attempt_no: event.attempt_no,
                    lease_token: runtime_lease_token(&handle).unwrap_or_default(),
                    session_epoch: runtime_session_epoch(&handle),
                    event_type: "recording_degraded".to_string(),
                    event_level: "warn".to_string(),
                    message: "mp4 recording sidecar stopped; continuing without recording"
                        .to_string(),
                    payload: event.exit_payload,
                },
            ));
        }
        let mut commit = RuntimeMonitorCommit::new(handle.clone(), generation)
            .with_persist(event.work_dir, event.success_check)
            .with_notifications(notifications);
        commit.remove_companion_pid = Some(event.companion_pid);
        self.executor.apply_monitor_commit(commit);
        self.apply_state_handle_with_generation(handle, generation);
    }
}

fn is_stale_session(session_epoch: Option<u64>, active_session_epoch: Option<u64>) -> bool {
    // None 表示 sessionless 命令，主要来自 cleanup/internal 路径，不能因为 Core 控制流
    // 重连而被丢弃；只有显式绑定旧 session 的命令才算 stale。
    matches!(session_epoch, Some(session_epoch) if active_session_epoch != Some(session_epoch))
}

fn record_duration_force_after_sec() -> u32 {
    let millis = RECORD_DURATION_FORCE_KILL_DELAY.as_millis();
    if millis == 0 {
        return 0;
    }
    ((millis + 999) / 1000).min(u32::MAX as u128) as u32
}

fn adopt_filter_matches_handle(filter: &AdoptRuntimeFilter, handle: &RuntimeHandle) -> bool {
    filter.task_id == handle.task_id
        && filter.attempt_no == handle.attempt_no
        && filter.worker_kind == handle.worker_kind
        && filter.lease_token == runtime_lease_token(handle).unwrap_or_default()
}

fn send_start_reply(
    reply: RuntimeStartReply,
    outcome: RuntimeManagerRequestOutcome<
        Result<media_domain::RuntimeHandle, crate::runtime_types::ExecutorError>,
    >,
) {
    match reply {
        RuntimeStartReply::Session(reply) => {
            let _ = reply.send(outcome);
        }
    }
}

fn send_stop_reply(
    reply: RuntimeStopReply,
    outcome: RuntimeManagerRequestOutcome<Result<(), crate::runtime_types::ExecutorError>>,
) {
    match reply {
        RuntimeStopReply::Session(reply) => {
            let _ = reply.send(outcome);
        }
        RuntimeStopReply::Sessionless(reply) => {
            if let RuntimeManagerRequestOutcome::Completed(result) = outcome {
                let _ = reply.send(result);
            }
        }
    }
}

fn send_recording_reply(
    reply: RuntimeRecordingReply,
    outcome: RuntimeManagerRequestOutcome<
        Result<media_domain::RuntimeHandle, crate::runtime_types::ExecutorError>,
    >,
) {
    match reply {
        RuntimeRecordingReply::Session(reply) => {
            let _ = reply.send(outcome);
        }
    }
}

fn send_adopt_reply(
    reply: RuntimeAdoptReply,
    outcome: RuntimeManagerRequestOutcome<Vec<media_domain::RuntimeHandle>>,
) {
    match reply {
        RuntimeAdoptReply::Session(reply) => {
            let _ = reply.send(outcome);
        }
    }
}

fn clone_recording_result(
    result: &Result<RuntimeHandle, crate::runtime_types::ExecutorError>,
) -> Result<RuntimeHandle, crate::runtime_types::ExecutorError> {
    result
        .as_ref()
        .map(Clone::clone)
        .map_err(clone_executor_error)
}

fn clone_executor_error(
    error: &crate::runtime_types::ExecutorError,
) -> crate::runtime_types::ExecutorError {
    match error {
        crate::runtime_types::ExecutorError::RuntimeNotFound {
            task_id,
            attempt_no,
        } => crate::runtime_types::ExecutorError::RuntimeNotFound {
            task_id: *task_id,
            attempt_no: *attempt_no,
        },
        crate::runtime_types::ExecutorError::InvalidRequest(message) => {
            crate::runtime_types::ExecutorError::InvalidRequest(message.clone())
        }
        crate::runtime_types::ExecutorError::ApiCall(message) => {
            crate::runtime_types::ExecutorError::ApiCall(message.clone())
        }
        crate::runtime_types::ExecutorError::ProcessSpawn(message) => {
            crate::runtime_types::ExecutorError::ProcessSpawn(message.clone())
        }
        crate::runtime_types::ExecutorError::ProcessSignal(message) => {
            crate::runtime_types::ExecutorError::ProcessSignal(message.clone())
        }
    }
}
