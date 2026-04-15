use std::{str::FromStr, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use cron::Schedule;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval},
};
use tracing::warn;

use crate::{
    control_plane::{ControlPlaneError, ControlPlaneService},
    repository::{CronScheduleEntry, RepoError, TaskRepository},
};

const SCHEDULER_TICK: Duration = Duration::from_secs(5);
const CRON_CATCH_UP_LIMIT: usize = 16;
const STOPPING_RECONCILE_TIMEOUT: Duration = Duration::from_secs(65);

pub fn spawn(
    repository: Arc<TaskRepository>,
    control_plane: ControlPlaneService,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(SCHEDULER_TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(error) = run_once(&repository, &control_plane).await {
                        warn!(error = %error, "scheduler tick failed");
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

async fn run_once(
    repository: &TaskRepository,
    control_plane: &ControlPlaneService,
) -> anyhow::Result<()> {
    let now = Utc::now();

    for task_id in repository.list_due_at_tasks(now).await? {
        dispatch_ready_task(control_plane, task_id).await?;
    }

    for schedule in repository.list_cron_schedules().await? {
        trigger_due_cron_tasks(repository, control_plane, now, schedule).await?;
    }

    reconcile_stopping_tasks(repository, control_plane, now).await?;

    Ok(())
}

async fn trigger_due_cron_tasks(
    repository: &TaskRepository,
    control_plane: &ControlPlaneService,
    now: DateTime<Utc>,
    schedule: CronScheduleEntry,
) -> anyhow::Result<()> {
    let spec: media_domain::TaskSpec = serde_json::from_value(schedule.requested_spec.clone())?;
    let Some(expression) = spec
        .schedule
        .cron
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };

    let schedule_expr = Schedule::from_str(expression)?;
    let mut reference = schedule
        .last_scheduled_for
        .unwrap_or_else(|| schedule.created_at - chrono::Duration::seconds(1));

    for _ in 0..CRON_CATCH_UP_LIMIT {
        let Some(next_fire) = schedule_expr.after(&reference).next() else {
            break;
        };
        if next_fire > now {
            break;
        }

        if let Some(task) = repository
            .trigger_cron_task(schedule.task_id, next_fire)
            .await?
        {
            dispatch_ready_task(control_plane, task.id).await?;
        }
        reference = next_fire;
    }

    Ok(())
}

async fn dispatch_ready_task(
    control_plane: &ControlPlaneService,
    task_id: uuid::Uuid,
) -> anyhow::Result<()> {
    match control_plane.dispatch_task(task_id).await {
        Ok(()) => Ok(()),
        Err(ControlPlaneError::NoConnectedNode | ControlPlaneError::NodeDisconnected(_)) => Ok(()),
        Err(ControlPlaneError::Repository(RepoError::TaskNotDispatchable(_))) => Ok(()),
        Err(error) => Err(anyhow::Error::new(error)),
    }
}

async fn reconcile_stopping_tasks(
    repository: &TaskRepository,
    control_plane: &ControlPlaneService,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    for candidate in repository.list_stopping_reconcile_tasks().await? {
        if candidate.attempt_status == media_domain::AttemptStatus::Orphaned {
            let _ = repository.complete_stopping_task(&candidate).await?;
            continue;
        }
        let deadline = candidate.stop_requested_at
            + chrono::Duration::from_std(STOPPING_RECONCILE_TIMEOUT).unwrap();
        if now >= deadline {
            let _ = repository.mark_stopping_timeout(&candidate).await?;
            continue;
        }

        if candidate.attempt_status == media_domain::AttemptStatus::Adopted {
            match control_plane
                .request_stop(candidate.task_id, "reclaim_stop", 30, 5)
                .await
            {
                Ok(()) => {}
                Err(
                    ControlPlaneError::NoConnectedNode | ControlPlaneError::NodeDisconnected(_),
                ) => {}
                Err(error) => return Err(anyhow::Error::new(error)),
            }
        }
    }

    Ok(())
}
