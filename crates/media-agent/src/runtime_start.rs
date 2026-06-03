//! Runtime 启动入口决策：封装启动请求的前置校验、幂等判断和 runtime mode 分派准备。
//!
//! 这里不真正创建 ZLM proxy、RTP server 或本地进程，只负责确认本次启动是否应该复用
//! 已有 runtime，或在占用 slot 后交给具体启动模块继续执行。

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use media_domain::RuntimeHandle;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{ExecutorError, StartTaskRequest, StopTaskRequest},
    runtime_metadata::runtime_lease_token,
    runtime_plan::{TaskRuntimeMode, parse_task_spec, task_runtime_mode},
    runtime_process::{ManagedRuntime, RuntimeSlotLimiter, RuntimeSlotPermit},
    runtime_registry::LocalRuntimeRegistry,
    runtime_stop::{StaleAttemptCleanupContext, cleanup_stale_attempt_runtimes},
};

pub(crate) struct RuntimeStartContext<'a> {
    pub(crate) settings: &'a AgentSettings,
    pub(crate) registry: &'a LocalRuntimeRegistry,
    pub(crate) runtimes: &'a Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) stop_intents: &'a Arc<RwLock<HashMap<(Uuid, i32), StopTaskRequest>>>,
    pub(crate) slot_limiter: &'a Arc<RuntimeSlotLimiter>,
}

pub(crate) enum RuntimeStartDecision {
    Existing(RuntimeHandle),
    Start {
        mode: TaskRuntimeMode,
        slot_permit: Arc<RuntimeSlotPermit>,
    },
}

pub(crate) fn prepare_start_task(
    ctx: RuntimeStartContext<'_>,
    request: &StartTaskRequest,
) -> Result<RuntimeStartDecision, ExecutorError> {
    if request.lease_token.trim().is_empty() {
        return Err(ExecutorError::InvalidRequest(
            "lease_token must not be empty".to_string(),
        ));
    }

    if let Some(existing) = ctx
        .registry
        .find_by_task_attempt(request.task_id, request.attempt_no)
    {
        let existing_lease = runtime_lease_token(&existing).unwrap_or_default();
        if existing_lease == request.lease_token {
            return Ok(RuntimeStartDecision::Existing(existing));
        }
        return Err(ExecutorError::InvalidRequest(format!(
            "stale dispatch for {}/{}: lease_token mismatch",
            request.task_id, request.attempt_no
        )));
    }

    let key = (request.task_id, request.attempt_no);
    let stop_already_requested = {
        let stop_intents = ctx.stop_intents.read().expect("stop intents lock poisoned");
        stop_intents
            .get(&key)
            .is_some_and(|intent| intent.lease_token == request.lease_token)
    };
    if stop_already_requested {
        return Err(ExecutorError::InvalidRequest(format!(
            "stop already requested for {}/{}",
            request.task_id, request.attempt_no
        )));
    }

    cleanup_stale_attempt_runtimes(
        StaleAttemptCleanupContext {
            settings: ctx.settings,
            registry: ctx.registry,
            runtimes: ctx.runtimes,
        },
        request,
    );

    let slot_permit = ctx.slot_limiter.try_acquire()?;
    let spec = parse_task_spec(request)?;
    Ok(RuntimeStartDecision::Start {
        mode: task_runtime_mode(&spec),
        slot_permit,
    })
}
