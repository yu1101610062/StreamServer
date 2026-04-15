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

        matches!(
            (self, next),
            (Created, Validating)
                | (Validating, Queued)
                | (Queued, Dispatching)
                | (Queued, Failed)
                | (Dispatching, Queued)
                | (Dispatching, Starting)
                | (Starting, Running)
                | (Starting, Failed)
                | (Running, Stopping)
                | (Running, Failed)
                | (Running, Lost)
                | (Stopping, Succeeded)
                | (Stopping, Canceled)
                | (Stopping, Failed)
                | (Lost, Recovering)
                | (Lost, Queued)
                | (Recovering, Running)
                | (Recovering, Failed)
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

        let next = match (operation, self) {
            (Start, Created | Failed | Canceled) => Validating,
            (Stop, Dispatching | Starting | Running | Recovering) => Stopping,
            (Cancel, Created | Validating | Queued) => Canceled,
            (Cancel, Dispatching | Starting | Running | Recovering) => Stopping,
            (Retry, Failed | Lost) => Queued,
            (Clone, Succeeded | Failed | Canceled | Lost) => Created,
            _ => {
                return Err(TaskStateError::InvalidOperation {
                    operation,
                    status: self,
                });
            }
        };

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_running_transition_is_allowed() {
        assert!(TaskStatus::Running.can_transition_to(TaskStatus::Stopping));
        assert!(TaskStatus::Stopping.can_transition_to(TaskStatus::Succeeded));
    }

    #[test]
    fn impossible_transition_is_rejected() {
        let error = TaskStatus::Created
            .ensure_transition(TaskStatus::Running)
            .expect_err("transition should fail");

        assert_eq!(
            error,
            TaskStateError::InvalidTransition {
                from: TaskStatus::Created,
                to: TaskStatus::Running,
            }
        );
    }

    #[test]
    fn start_operation_can_restart_failed_task() {
        let next = TaskStatus::Failed
            .apply_operation(TaskOperation::Start)
            .expect("start should be allowed");

        assert_eq!(next, TaskStatus::Validating);
    }

    #[test]
    fn stop_operation_rejects_created_task() {
        let error = TaskStatus::Created
            .apply_operation(TaskOperation::Stop)
            .expect_err("stop should not be allowed");

        assert_eq!(
            error,
            TaskStateError::InvalidOperation {
                operation: TaskOperation::Stop,
                status: TaskStatus::Created,
            }
        );
    }

    #[test]
    fn stop_operation_rejects_lost_task() {
        let error = TaskStatus::Lost
            .apply_operation(TaskOperation::Stop)
            .expect_err("stop should not be allowed from lost");

        assert_eq!(
            error,
            TaskStateError::InvalidOperation {
                operation: TaskOperation::Stop,
                status: TaskStatus::Lost,
            }
        );
    }

    #[test]
    fn cancel_operation_moves_running_task_to_stopping() {
        let next = TaskStatus::Running
            .apply_operation(TaskOperation::Cancel)
            .expect("cancel should be allowed");

        assert_eq!(next, TaskStatus::Stopping);
    }

    #[test]
    fn retry_operation_moves_lost_task_back_to_queued() {
        let next = TaskStatus::Lost
            .apply_operation(TaskOperation::Retry)
            .expect("retry should be allowed");

        assert_eq!(next, TaskStatus::Queued);
    }

    #[test]
    fn clone_operation_creates_new_created_task() {
        let next = TaskStatus::Canceled
            .apply_operation(TaskOperation::Clone)
            .expect("clone should be allowed");

        assert_eq!(next, TaskStatus::Created);
    }
}
