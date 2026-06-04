use media_domain::RuntimeHandle;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;
use uuid::Uuid;

use crate::{
    runtime_registry::{AdoptFilter, RuntimeReadHandle},
    runtime_types::{
        ExecutorError, StartTaskRequest, StopTaskRequest, TaskRecordingControlRequest,
    },
};

use super::{
    command::{RuntimeCommand, RuntimeManagerRequestOutcome},
    internal_event::{RuntimeGeneration, RuntimeInternalEvent, RuntimeMonitorSnapshot},
};

#[derive(Clone)]
pub struct RuntimeManagerHandle {
    tx: mpsc::Sender<RuntimeCommand>,
    read_handle: RuntimeReadHandle,
}

impl RuntimeManagerHandle {
    pub(crate) fn new(tx: mpsc::Sender<RuntimeCommand>, read_handle: RuntimeReadHandle) -> Self {
        Self { tx, read_handle }
    }

    pub fn read_handle(&self) -> RuntimeReadHandle {
        self.read_handle.clone()
    }

    pub async fn begin_session(&self, session_epoch: u64) -> Result<(), ExecutorError> {
        self.tx
            .send(RuntimeCommand::BeginSession { session_epoch })
            .await
            .map_err(|_| {
                ExecutorError::InvalidRequest("runtime manager command channel closed".to_string())
            })
    }

    pub async fn end_session(&self, session_epoch: u64) -> Result<(), ExecutorError> {
        self.tx
            .send(RuntimeCommand::EndSession { session_epoch })
            .await
            .map_err(|_| {
                ExecutorError::InvalidRequest("runtime manager command channel closed".to_string())
            })
    }

    pub async fn check_session(
        &self,
        session_epoch: u64,
    ) -> Result<RuntimeManagerRequestOutcome<()>, ExecutorError> {
        self.send_request(|reply| RuntimeCommand::CheckSession {
            session_epoch,
            reply,
        })
        .await
    }

    pub async fn start_task_in_session(
        &self,
        session_epoch: u64,
        request: StartTaskRequest,
    ) -> Result<RuntimeManagerRequestOutcome<Result<RuntimeHandle, ExecutorError>>, ExecutorError>
    {
        self.send_request(|reply| RuntimeCommand::StartTaskInSession {
            session_epoch,
            request,
            reply,
        })
        .await
    }

    pub async fn stop_task_in_session(
        &self,
        session_epoch: u64,
        request: StopTaskRequest,
    ) -> Result<RuntimeManagerRequestOutcome<Result<(), ExecutorError>>, ExecutorError> {
        self.send_request(|reply| RuntimeCommand::StopTaskInSession {
            session_epoch,
            request,
            reply,
        })
        .await
    }

    pub async fn set_task_recording_in_session(
        &self,
        session_epoch: u64,
        request: TaskRecordingControlRequest,
    ) -> Result<RuntimeManagerRequestOutcome<Result<RuntimeHandle, ExecutorError>>, ExecutorError>
    {
        self.send_request(|reply| RuntimeCommand::SetTaskRecordingInSession {
            session_epoch,
            request,
            reply,
        })
        .await
    }

    pub async fn adopt_orphans_in_session(
        &self,
        session_epoch: u64,
        filter: AdoptFilter,
    ) -> Result<RuntimeManagerRequestOutcome<Vec<RuntimeHandle>>, ExecutorError> {
        self.send_request(|reply| RuntimeCommand::AdoptOrphansInSession {
            session_epoch,
            filter,
            reply,
        })
        .await
    }

    pub fn observe_runtime_snapshot(&self, handle: RuntimeHandle) {
        if let Err(error) = self
            .tx
            .try_send(RuntimeCommand::ObserveRuntimeSnapshot { handle })
        {
            warn!(error = %error, "runtime manager snapshot observer command dropped");
        }
    }

    pub async fn stop_task(&self, request: StopTaskRequest) -> Result<(), ExecutorError> {
        self.send_request(|reply| RuntimeCommand::StopTask { request, reply })
            .await?
    }

    pub fn set_zlm_server_id(&self, server_id: String) {
        if let Err(error) = self
            .tx
            .try_send(RuntimeCommand::SetZlmServerId { server_id })
        {
            warn!(error = %error, "runtime manager hint command dropped");
        }
    }

    pub fn set_zlm_rtmp_enhanced_enabled(&self, enabled: Option<bool>) {
        if let Err(error) = self
            .tx
            .try_send(RuntimeCommand::SetZlmRtmpEnhancedEnabled { enabled })
        {
            warn!(error = %error, "runtime manager hint command dropped");
        }
    }
}

impl RuntimeMonitorHandle {
    pub(crate) fn new(
        tx: mpsc::Sender<RuntimeCommand>,
        runtime_id: Uuid,
        generation: RuntimeGeneration,
    ) -> Self {
        RuntimeMonitorHandle {
            tx,
            runtime_id,
            generation,
        }
    }
}

impl RuntimeManagerHandle {
    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        if let Err(error) = self.tx.send(RuntimeCommand::Shutdown).await {
            warn!(error = %error, "runtime manager shutdown command was not delivered");
        }
        self.tx.closed().await;
    }

    async fn send_request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> RuntimeCommand,
    ) -> Result<T, ExecutorError> {
        let (reply, response) = oneshot::channel();
        self.tx.send(command(reply)).await.map_err(|_| {
            ExecutorError::InvalidRequest("runtime manager command channel closed".to_string())
        })?;
        response.await.map_err(|_| {
            ExecutorError::InvalidRequest("runtime manager reply channel closed".to_string())
        })
    }
}

#[derive(Clone)]
pub(crate) struct RuntimeMonitorHandle {
    tx: mpsc::Sender<RuntimeCommand>,
    runtime_id: Uuid,
    generation: RuntimeGeneration,
}

impl RuntimeMonitorHandle {
    pub(crate) fn runtime_id(&self) -> Uuid {
        self.runtime_id
    }

    pub(crate) fn generation(&self) -> RuntimeGeneration {
        self.generation
    }

    pub(crate) async fn snapshot(&self) -> Option<RuntimeMonitorSnapshot> {
        let (reply, response) = oneshot::channel();
        if let Err(error) = self
            .tx
            .send(RuntimeCommand::MonitorSnapshot {
                runtime_id: self.runtime_id,
                generation: self.generation,
                reply,
            })
            .await
        {
            warn!(error = %error, runtime_id = %self.runtime_id, "runtime manager monitor snapshot command dropped");
            return None;
        }
        match response.await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!(error = %error, runtime_id = %self.runtime_id, "runtime manager monitor snapshot reply dropped");
                None
            }
        }
    }

    pub(crate) async fn send_event(&self, event: RuntimeInternalEvent) {
        if let Err(error) = self
            .tx
            .send(RuntimeCommand::RuntimeInternalEvent { event })
            .await
        {
            warn!(error = %error, runtime_id = %self.runtime_id, "runtime manager internal event dropped");
        }
    }
}
