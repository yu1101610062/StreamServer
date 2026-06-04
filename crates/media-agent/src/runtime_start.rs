//! Runtime 启动入口决策：封装启动请求的前置校验、幂等判断和 runtime mode 分派准备。
//!
//! 这里不真正创建 ZLM proxy、RTP server 或本地进程，只负责启动请求校验和 runtime mode
//! 解析。已有 runtime/lease 判断与 slot 占用由 RuntimeManager actor 统一处理。

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{ExecutorError, StartTaskRequest, StopTaskRequest},
    runtime_plan::{TaskRuntimeMode, parse_task_spec, task_runtime_mode},
};

pub(crate) struct RuntimeStartContext<'a> {
    pub(crate) _settings: &'a AgentSettings,
    pub(crate) stop_intents: &'a Arc<RwLock<HashMap<(Uuid, i32), StopTaskRequest>>>,
}

pub(crate) enum RuntimeStartDecision {
    Start { mode: TaskRuntimeMode },
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

    let spec = parse_task_spec(request)?;
    Ok(RuntimeStartDecision::Start {
        mode: task_runtime_mode(&spec),
    })
}
