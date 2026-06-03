//! 仓储核心：定义 TaskRepository 的共享状态、构造方法和基础健康检查。

use sqlx::PgPool;

use super::RepoError;

#[derive(Debug, Clone)]
pub struct TaskRepository {
    pub(super) pool: PgPool,
    pub(super) callback_settle_delay: chrono::Duration,
    pub(super) artifact_callback_wait_timeout: chrono::Duration,
}

impl TaskRepository {
    pub fn new(pool: PgPool) -> Self {
        // 默认延迟给 Agent 留出上报终态产物的时间，避免终态回调先于文件产物到达。
        Self::with_callback_delays(
            pool,
            chrono::Duration::milliseconds(8_000),
            chrono::Duration::milliseconds(30_000),
        )
    }

    pub fn with_callback_settle_delay(
        pool: PgPool,
        callback_settle_delay: chrono::Duration,
    ) -> Self {
        Self::with_callback_delays(pool, callback_settle_delay, callback_settle_delay)
    }

    pub fn with_callback_delays(
        pool: PgPool,
        callback_settle_delay: chrono::Duration,
        artifact_callback_wait_timeout: chrono::Duration,
    ) -> Self {
        Self {
            pool,
            callback_settle_delay,
            artifact_callback_wait_timeout,
        }
    }

    pub async fn health_check(&self) -> Result<(), RepoError> {
        sqlx::query("select 1").execute(&self.pool).await?;
        Ok(())
    }
}
