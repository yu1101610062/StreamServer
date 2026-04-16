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
