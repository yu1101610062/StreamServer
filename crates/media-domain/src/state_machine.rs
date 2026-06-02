#[cfg(test)]
#[path = "tests/state_machine.rs"]
mod tests;

use thiserror::Error;

use crate::task::TaskStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOperation {
    Start,
    Stop,
    Cancel,
    Retry,
    Clone,
}

impl TaskStatus {
    pub fn can_transition_to(self, next: Self) -> bool {
        use TaskStatus::*;

        // 任务状态只允许走白名单迁移，避免异步事件把终态任务重新写回运行态。
        matches!(
            (self, next),
            (Created, Validating)
                | (Validating, Queued)
                | (Queued, Dispatching)
                | (Queued, Failed)
                | (Dispatching, Queued)
                | (Dispatching, Starting)
                | (Dispatching, Reclaiming)
                | (Starting, Running)
                | (Starting, Stopping)
                | (Starting, Failed)
                | (Starting, Reclaiming)
                | (Running, Stopping)
                | (Running, Failed)
                | (Running, Lost)
                | (Running, Reclaiming)
                | (Stopping, Succeeded)
                | (Stopping, Canceled)
                | (Stopping, Failed)
                | (Stopping, Reclaiming)
                | (Lost, Recovering)
                | (Lost, Queued)
                | (Recovering, Running)
                | (Recovering, Failed)
                | (Recovering, Reclaiming)
                | (Reclaiming, Starting)
                | (Reclaiming, Running)
                | (Reclaiming, Stopping)
                | (Reclaiming, Recovering)
                | (Reclaiming, Lost)
                | (Failed, Queued)
                | (Failed, Validating)
                | (Canceled, Validating)
        )
    }

    pub fn ensure_transition(self, next: Self) -> Result<(), TaskStateError> {
        if self.can_transition_to(next) {
            Ok(())
        } else {
            Err(TaskStateError::InvalidTransition {
                from: self,
                to: next,
            })
        }
    }

    pub fn apply_operation(self, operation: TaskOperation) -> Result<Self, TaskStateError> {
        use TaskOperation::*;
        use TaskStatus::*;

        // 用户操作先映射到目标状态，再复用状态机白名单校验实际迁移是否合法。
        let next = match (operation, self) {
            (Start, Created | Failed | Canceled) => Validating,
            (Stop, Dispatching | Starting | Running | Recovering | Reclaiming) => Stopping,
            (Cancel, Created | Validating | Queued) => Canceled,
            (Cancel, Dispatching | Starting | Running | Recovering | Reclaiming) => Stopping,
            (Retry, Failed | Lost) => Queued,
            (Clone, Succeeded | Failed | Canceled | Lost) => Created,
            _ => {
                return Err(TaskStateError::InvalidOperation {
                    operation,
                    status: self,
                });
            }
        };

        // Clone 创建的是新任务，不是原任务状态迁移；Created/Validating/Queued 可直接取消到终态。
        if operation == Clone || self.can_transition_to(next) || next == Canceled {
            Ok(next)
        } else {
            Err(TaskStateError::InvalidTransition {
                from: self,
                to: next,
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TaskStateError {
    #[error("task cannot perform {operation:?} from {status}")]
    InvalidOperation {
        operation: TaskOperation,
        status: TaskStatus,
    },
    #[error("invalid task transition from {from} to {to}")]
    InvalidTransition { from: TaskStatus, to: TaskStatus },
}
