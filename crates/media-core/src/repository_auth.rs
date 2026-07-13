//! 认证仓储：维护本地用户、刷新会话、机器访问白名单和安全审计事件。

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgRow};
use uuid::Uuid;

use super::{RepoError, TaskRepository};

impl TaskRepository {
    pub async fn has_enabled_admin_user(&self) -> Result<bool, RepoError> {
        Ok(sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from auth_users
               where enabled = true
                 and role = 'admin'
            )
            "#,
        )
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn bootstrap_admin_password_state(
        &self,
        username: &str,
        handoff_id: Uuid,
    ) -> Result<BootstrapAdminPasswordProbe, RepoError> {
        let auth_users_exists: bool =
            sqlx::query_scalar("select to_regclass('auth_users') is not null")
                .fetch_one(&self.pool)
                .await?;
        if !auth_users_exists {
            return Ok(BootstrapAdminPasswordProbe::missing());
        }
        let schema = sqlx::query(
            r#"
            select
              exists (
                select 1 from pg_attribute
                 where attrelid = to_regclass('auth_users')
                   and attname = 'bootstrap_handoff_id' and not attisdropped
              ) as has_handoff_id,
              exists (
                select 1 from pg_attribute
                 where attrelid = to_regclass('auth_users')
                   and attname = 'bootstrap_handoff_version' and not attisdropped
              ) as has_handoff_version,
              exists (
                select 1 from pg_attribute
                 where attrelid = to_regclass('auth_users')
                   and attname = 'bootstrap_handoff_completed_at' and not attisdropped
              ) as has_handoff_completed_at,
              exists (
                select 1 from pg_attribute
                 where attrelid = to_regclass('auth_users')
                   and attname = 'credential_version' and not attisdropped
              ) as has_credential_version
            "#,
        )
        .fetch_one(&self.pool)
        .await?;
        let schema_columns = [
            schema.try_get::<bool, _>("has_handoff_id")?,
            schema.try_get::<bool, _>("has_handoff_version")?,
            schema.try_get::<bool, _>("has_handoff_completed_at")?,
            schema.try_get::<bool, _>("has_credential_version")?,
        ];
        if schema_columns.iter().all(|present| !present) {
            let legacy_conflict: bool = sqlx::query_scalar(
                r#"
                select exists (
                  select 1 from auth_users
                   where username = $1
                      or (enabled = true and role = 'admin')
                )
                "#,
            )
            .bind(username)
            .fetch_one(&self.pool)
            .await?;
            return Ok(if legacy_conflict {
                BootstrapAdminPasswordProbe::state(BootstrapAdminPasswordState::Conflict)
            } else {
                BootstrapAdminPasswordProbe::missing()
            });
        }
        if schema_columns.iter().any(|present| !present) {
            return Err(RepoError::AuthHandoffSchemaIncomplete);
        }
        let user = sqlx::query(
            r#"
            select role, enabled, must_change_password, bootstrap_handoff_id,
                   bootstrap_handoff_version, bootstrap_handoff_completed_at
              from auth_users
             where username = $1
            "#,
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        let Some(user) = user else {
            return if self.has_enabled_admin_user().await? {
                Ok(BootstrapAdminPasswordProbe::state(
                    BootstrapAdminPasswordState::Conflict,
                ))
            } else {
                Ok(BootstrapAdminPasswordProbe::missing())
            };
        };
        let role: String = user.try_get("role")?;
        let enabled: bool = user.try_get("enabled")?;
        let must_change_password: bool = user.try_get("must_change_password")?;
        let stored_handoff_id: Option<Uuid> = user.try_get("bootstrap_handoff_id")?;
        let handoff_version: i64 = user.try_get("bootstrap_handoff_version")?;
        let completed_at: Option<DateTime<Utc>> = user.try_get("bootstrap_handoff_completed_at")?;
        if !enabled || role != "admin" || stored_handoff_id != Some(handoff_id) {
            Ok(BootstrapAdminPasswordProbe::state(
                BootstrapAdminPasswordState::Conflict,
            ))
        } else if must_change_password && completed_at.is_none() && handoff_version > 0 {
            Ok(BootstrapAdminPasswordProbe {
                state: BootstrapAdminPasswordState::PendingPasswordChange,
                expected_version: Some(handoff_version),
            })
        } else if !must_change_password && completed_at.is_some() && handoff_version > 0 {
            Ok(BootstrapAdminPasswordProbe::state(
                BootstrapAdminPasswordState::Complete,
            ))
        } else {
            Ok(BootstrapAdminPasswordProbe::state(
                BootstrapAdminPasswordState::Conflict,
            ))
        }
    }

    pub async fn create_bootstrap_admin(
        &self,
        username: &str,
        password_hash: &str,
        must_change_password: bool,
    ) -> Result<(), RepoError> {
        let now = Utc::now();
        sqlx::query(
            r#"
            insert into auth_users (
              id, username, password_hash, role, enabled, must_change_password,
              password_changed_at, created_at, updated_at
            ) values (
              $1, $2, $3, 'admin', true, $4, $5, $5, $5
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(username)
        .bind(password_hash)
        .bind(must_change_password)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.insert_security_audit_event(SecurityAuditEventRecord {
            event_type: "admin_bootstrapped".to_string(),
            actor: username.to_string(),
            subject: Some(username.to_string()),
            remote_ip: None,
            user_agent: None,
            payload: json!({}),
        })
        .await?;
        Ok(())
    }

    pub async fn reconcile_bootstrap_admin_password(
        &self,
        username: &str,
        handoff_id: Uuid,
        expected_version: i64,
        password_hash: &str,
    ) -> Result<BootstrapAdminReconcileOutcome, RepoError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("lock table auth_users in share row exclusive mode")
            .execute(&mut *tx)
            .await?;
        let target = sqlx::query(
            r#"
            select id, role, enabled, must_change_password, bootstrap_handoff_id,
                   bootstrap_handoff_version, bootstrap_handoff_completed_at
              from auth_users
             where username = $1
             for update
            "#,
        )
        .bind(username)
        .fetch_optional(&mut *tx)
        .await?;

        let now = Utc::now();
        let outcome = if let Some(target) = target {
            let user_id: Uuid = target.try_get("id")?;
            let role: String = target.try_get("role")?;
            let enabled: bool = target.try_get("enabled")?;
            let must_change_password: bool = target.try_get("must_change_password")?;
            let stored_handoff_id: Option<Uuid> = target.try_get("bootstrap_handoff_id")?;
            let handoff_version: i64 = target.try_get("bootstrap_handoff_version")?;
            let completed_at: Option<DateTime<Utc>> =
                target.try_get("bootstrap_handoff_completed_at")?;
            if !enabled || role != "admin" || stored_handoff_id != Some(handoff_id) {
                BootstrapAdminReconcileOutcome::Conflict
            } else if !must_change_password && completed_at.is_some() {
                BootstrapAdminReconcileOutcome::AlreadyComplete
            } else if !must_change_password
                || completed_at.is_some()
                || handoff_version != expected_version
            {
                BootstrapAdminReconcileOutcome::Stale
            } else {
                let updated_user_id = sqlx::query_scalar::<_, Uuid>(
                    r#"
                    update auth_users
                       set password_hash = $1,
                           must_change_password = true,
                           bootstrap_handoff_version = bootstrap_handoff_version + 1,
                           bootstrap_handoff_completed_at = null,
                           credential_version = credential_version + 1,
                           password_changed_at = $2,
                           updated_at = $2
                     where id = $3
                       and enabled = true
                       and role = 'admin'
                       and must_change_password = true
                       and bootstrap_handoff_id = $4
                       and bootstrap_handoff_version = $5
                       and bootstrap_handoff_completed_at is null
                 returning id
                    "#,
                )
                .bind(password_hash)
                .bind(now)
                .bind(user_id)
                .bind(handoff_id)
                .bind(expected_version)
                .fetch_optional(&mut *tx)
                .await?;
                if updated_user_id.is_none() {
                    BootstrapAdminReconcileOutcome::Stale
                } else {
                    revoke_refresh_sessions_in_transaction(&mut tx, user_id, now).await?;
                    insert_bootstrap_reconcile_audit_in_transaction(
                        &mut tx,
                        username,
                        "admin_bootstrap_password_recovered",
                        now,
                    )
                    .await?;
                    BootstrapAdminReconcileOutcome::Recovered
                }
            }
        } else {
            let enabled_admin_exists: bool = sqlx::query_scalar(
                "select exists (select 1 from auth_users where enabled = true and role = 'admin')",
            )
            .fetch_one(&mut *tx)
            .await?;
            if enabled_admin_exists || expected_version != 0 {
                BootstrapAdminReconcileOutcome::Conflict
            } else {
                sqlx::query(
                    r#"
                    insert into auth_users (
                      id, username, password_hash, role, enabled, must_change_password,
                      password_changed_at, bootstrap_handoff_id,
                      bootstrap_handoff_version, bootstrap_handoff_completed_at,
                      credential_version, created_at, updated_at
                    ) values (
                      $1, $2, $3, 'admin', true, true,
                      $4, $5, 1, null, 1, $4, $4
                    )
                    "#,
                )
                .bind(Uuid::now_v7())
                .bind(username)
                .bind(password_hash)
                .bind(now)
                .bind(handoff_id)
                .execute(&mut *tx)
                .await?;
                insert_bootstrap_reconcile_audit_in_transaction(
                    &mut tx,
                    username,
                    "admin_bootstrapped",
                    now,
                )
                .await?;
                BootstrapAdminReconcileOutcome::Created
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments)] // Keeps the credential CAS and its audit context explicit.
    pub async fn reset_user_password(
        &self,
        username: &str,
        password_hash: &str,
        must_change_password: bool,
        actor: &str,
        event_type: &str,
        remote_ip: Option<IpAddr>,
        user_agent: Option<&str>,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        let now = Utc::now();
        let row = sqlx::query(
            r#"
            update auth_users
               set password_hash = $1,
                   must_change_password = $2,
                   bootstrap_handoff_id = null,
                   bootstrap_handoff_version = 0,
                   bootstrap_handoff_completed_at = null,
                   credential_version = credential_version + 1,
                   password_changed_at = $3,
                   updated_at = $3
             where username = $4
         returning id
            "#,
        )
        .bind(password_hash)
        .bind(must_change_password)
        .bind(now)
        .bind(username)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| RepoError::AuthUserNotFound(username.to_string()))?;
        let user_id: Uuid = row.try_get("id")?;
        revoke_refresh_sessions_in_transaction(&mut tx, user_id, now).await?;
        insert_security_audit_in_transaction(
            &mut tx,
            event_type,
            actor,
            Some(username),
            remote_ip,
            user_agent,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn change_user_password(
        &self,
        username: &str,
        expected_password_hash: &str,
        expected_handoff_id: Option<Uuid>,
        expected_handoff_version: i64,
        password_hash: &str,
        actor: &str,
        remote_ip: Option<IpAddr>,
        user_agent: Option<&str>,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await?;
        let now = Utc::now();
        let row = sqlx::query(
            r#"
            update auth_users
               set password_hash = $1,
                   must_change_password = false,
                   bootstrap_handoff_id = case
                     when must_change_password = true
                       and bootstrap_handoff_id is not null
                       then bootstrap_handoff_id
                     else null
                   end,
                   bootstrap_handoff_version = case
                     when must_change_password = true
                       and bootstrap_handoff_id is not null
                       then bootstrap_handoff_version + 1
                     else 0
                   end,
                   bootstrap_handoff_completed_at = case
                     when must_change_password = true
                       and bootstrap_handoff_id is not null
                       then $2
                     else null
                   end,
                   credential_version = credential_version + 1,
                   password_changed_at = $2,
                   updated_at = $2
             where username = $3
               and password_hash = $4
               and bootstrap_handoff_version = $5
               and bootstrap_handoff_id is not distinct from $6
         returning id
            "#,
        )
        .bind(password_hash)
        .bind(now)
        .bind(username)
        .bind(expected_password_hash)
        .bind(expected_handoff_version)
        .bind(expected_handoff_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Ok(false);
        };
        let user_id: Uuid = row.try_get("id")?;
        revoke_refresh_sessions_in_transaction(&mut tx, user_id, now).await?;
        insert_security_audit_in_transaction(
            &mut tx,
            "password_changed",
            actor,
            Some(username),
            remote_ip,
            user_agent,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn find_auth_user_by_username(
        &self,
        username: &str,
    ) -> Result<Option<AuthUser>, RepoError> {
        sqlx::query(
            r#"
            select
              id,
              username,
              password_hash,
              role,
              enabled,
              must_change_password,
              bootstrap_handoff_id,
              bootstrap_handoff_version,
              credential_version,
              last_login_at,
              password_changed_at,
              created_at,
              updated_at
            from auth_users
            where username = $1
            "#,
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| AuthUser::from_row(&row))
        .transpose()
    }

    pub async fn local_access_token_is_current(
        &self,
        username: &str,
        credential_version: i64,
        must_change_password: bool,
    ) -> Result<bool, RepoError> {
        Ok(sqlx::query_scalar(
            r#"
            select exists (
              select 1 from auth_users
               where username = $1
                 and enabled = true
                 and role = 'admin'
                 and credential_version = $2
                 and must_change_password = $3
            )
            "#,
        )
        .bind(username)
        .bind(credential_version)
        .bind(must_change_password)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn touch_auth_user_login(
        &self,
        user_id: Uuid,
        logged_in_at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            update auth_users
               set last_login_at = $1,
                   updated_at = $1
             where id = $2
            "#,
        )
        .bind(logged_in_at)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_login_refresh_session(
        &self,
        record: NewRefreshSession,
        expected_password_hash: &str,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await?;
        let locked_user = sqlx::query_scalar::<_, Uuid>(
            r#"
            select id
              from auth_users
             where id = $1
               and enabled = true
               and password_hash = $2
             for share
            "#,
        )
        .bind(record.user_id)
        .bind(expected_password_hash)
        .fetch_optional(&mut *tx)
        .await?;
        if locked_user.is_none() {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query(
            r#"
            insert into auth_refresh_sessions (
              id, user_id, token_hash, expires_at, revoked_at, created_at,
              updated_at, last_used_at, client_ip, user_agent
            ) values (
              $1, $2, $3, $4, null, $5,
              $5, null, $6::inet, $7
            )
            "#,
        )
        .bind(record.id)
        .bind(record.user_id)
        .bind(record.token_hash)
        .bind(record.expires_at)
        .bind(record.created_at)
        .bind(record.client_ip.map(|value| value.to_string()))
        .bind(record.user_agent.as_deref())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn find_refresh_session(
        &self,
        token_hash: &str,
    ) -> Result<Option<RefreshSession>, RepoError> {
        sqlx::query(
            r#"
            select
              rs.id,
              rs.user_id,
              rs.token_hash,
              rs.expires_at,
              rs.revoked_at,
              rs.created_at as session_created_at,
              rs.updated_at as session_updated_at,
              rs.last_used_at,
              rs.client_ip::text as client_ip,
              rs.user_agent,
              u.username,
              u.password_hash,
              u.role,
              u.enabled,
              u.must_change_password,
              u.bootstrap_handoff_id,
              u.bootstrap_handoff_version,
              u.credential_version,
              u.last_login_at,
              u.password_changed_at,
              u.created_at as user_created_at,
              u.updated_at as user_updated_at
            from auth_refresh_sessions rs
            join auth_users u on u.id = rs.user_id
            where rs.token_hash = $1
            "#,
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| RefreshSession::from_row(&row))
        .transpose()
    }

    #[allow(clippy::too_many_arguments)] // R2 replaces this legacy row rotation with a token family CAS.
    pub async fn rotate_refresh_session(
        &self,
        session_id: Uuid,
        user_id: Uuid,
        expected_password_hash: &str,
        token_hash: &str,
        expires_at: DateTime<Utc>,
        used_at: DateTime<Utc>,
        client_ip: Option<IpAddr>,
        user_agent: Option<&str>,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await?;
        let locked_user = sqlx::query_scalar::<_, Uuid>(
            r#"
            select id
              from auth_users
             where id = $1
               and enabled = true
               and password_hash = $2
             for share
            "#,
        )
        .bind(user_id)
        .bind(expected_password_hash)
        .fetch_optional(&mut *tx)
        .await?;
        if locked_user.is_none() {
            tx.rollback().await?;
            return Ok(false);
        }
        let rotated = sqlx::query_scalar::<_, Uuid>(
            r#"
            update auth_refresh_sessions
               set token_hash = $1,
                   expires_at = $2,
                   updated_at = $3,
                   last_used_at = $3,
                   client_ip = $4::inet,
                   user_agent = $5
              where id = $6
               and revoked_at is null
               and expires_at > $3
         returning id
            "#,
        )
        .bind(token_hash)
        .bind(expires_at)
        .bind(used_at)
        .bind(client_ip.map(|value| value.to_string()))
        .bind(user_agent)
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rotated.is_some())
    }

    pub async fn revoke_refresh_session(
        &self,
        token_hash: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<bool, RepoError> {
        let result = sqlx::query(
            r#"
            update auth_refresh_sessions
               set revoked_at = coalesce(revoked_at, $1),
                   updated_at = $1
             where token_hash = $2
            "#,
        )
        .bind(revoked_at)
        .bind(token_hash)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn list_machine_allowlist(&self) -> Result<Vec<MachineAllowlistEntry>, RepoError> {
        sqlx::query(
            r#"
            select
              id,
              cidr::text as cidr,
              description,
              created_at,
              updated_at
            from machine_api_allowlist
            order by cidr asc
            "#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| MachineAllowlistEntry::from_row(&row))
        .collect()
    }

    pub async fn replace_machine_allowlist(
        &self,
        entries: &[MachineAllowlistWrite],
    ) -> Result<(), RepoError> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        sqlx::query("delete from machine_api_allowlist")
            .execute(&mut *tx)
            .await?;
        for entry in entries {
            sqlx::query(
                r#"
                insert into machine_api_allowlist (id, cidr, description, created_at, updated_at)
                values ($1, $2::cidr, $3, $4, $4)
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(&entry.cidr)
            .bind(&entry.description)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn is_machine_ip_allowlisted(&self, ip: IpAddr) -> Result<bool, RepoError> {
        Ok(sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from machine_api_allowlist
               where $1::inet <<= cidr
            )
            "#,
        )
        .bind(ip.to_string())
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn insert_security_audit_event(
        &self,
        record: SecurityAuditEventRecord,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            insert into security_audit_events (
              id, event_type, actor, subject, remote_ip, user_agent, payload, created_at
            ) values (
              $1, $2, $3, $4, $5::inet, $6, $7, $8
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(&record.event_type)
        .bind(&record.actor)
        .bind(record.subject.as_deref())
        .bind(record.remote_ip.map(|value| value.to_string()))
        .bind(record.user_agent.as_deref())
        .bind(&record.payload)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

async fn revoke_refresh_sessions_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    revoked_at: DateTime<Utc>,
) -> Result<(), RepoError> {
    sqlx::query(
        r#"
        update auth_refresh_sessions
           set revoked_at = coalesce(revoked_at, $1),
               updated_at = $1
         where user_id = $2
           and revoked_at is null
        "#,
    )
    .bind(revoked_at)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_bootstrap_reconcile_audit_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    username: &str,
    event_type: &str,
    created_at: DateTime<Utc>,
) -> Result<(), RepoError> {
    sqlx::query(
        r#"
        insert into security_audit_events (
          id, event_type, actor, subject, remote_ip, user_agent, payload, created_at
        ) values (
          $1, $2, $3, $3, null, null, $4, $5
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(event_type)
    .bind(username)
    .bind(json!({}))
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn insert_security_audit_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event_type: &str,
    actor: &str,
    subject: Option<&str>,
    remote_ip: Option<IpAddr>,
    user_agent: Option<&str>,
    created_at: DateTime<Utc>,
) -> Result<(), RepoError> {
    sqlx::query(
        r#"
        insert into security_audit_events (
          id, event_type, actor, subject, remote_ip, user_agent, payload, created_at
        ) values (
          $1, $2, $3, $4, $5::inet, $6, $7, $8
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(event_type)
    .bind(actor)
    .bind(subject)
    .bind(remote_ip.map(|value| value.to_string()))
    .bind(user_agent)
    .bind(json!({}))
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapAdminPasswordProbe {
    pub state: BootstrapAdminPasswordState,
    pub expected_version: Option<i64>,
}

impl BootstrapAdminPasswordProbe {
    fn missing() -> Self {
        Self {
            state: BootstrapAdminPasswordState::Missing,
            expected_version: Some(0),
        }
    }

    fn state(state: BootstrapAdminPasswordState) -> Self {
        Self {
            state,
            expected_version: None,
        }
    }

    pub fn as_cli_value(&self) -> String {
        match &self.expected_version {
            Some(version) => format!("{}:{version}", self.state.as_str()),
            None => self.state.as_str().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapAdminPasswordState {
    Missing,
    PendingPasswordChange,
    Complete,
    Conflict,
}

impl BootstrapAdminPasswordState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::PendingPasswordChange => "pending-password-change",
            Self::Complete => "complete",
            Self::Conflict => "conflict",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapAdminReconcileOutcome {
    Created,
    Recovered,
    AlreadyComplete,
    Conflict,
    Stale,
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: Uuid,
    pub username: String,
    pub password_hash: String,
    pub role: String,
    pub enabled: bool,
    pub must_change_password: bool,
    pub bootstrap_handoff_id: Option<Uuid>,
    pub bootstrap_handoff_version: i64,
    pub credential_version: i64,
}

impl AuthUser {
    pub(super) fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            username: row.try_get("username")?,
            password_hash: row.try_get("password_hash")?,
            role: row.try_get("role")?,
            enabled: row.try_get("enabled")?,
            must_change_password: row.try_get("must_change_password")?,
            bootstrap_handoff_id: row.try_get("bootstrap_handoff_id")?,
            bootstrap_handoff_version: row.try_get("bootstrap_handoff_version")?,
            credential_version: row.try_get("credential_version")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RefreshSession {
    pub id: Uuid,
    pub token_hash: String,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub user: AuthUser,
}

impl RefreshSession {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            token_hash: row.try_get("token_hash")?,
            expires_at: row.try_get("expires_at")?,
            revoked_at: row.try_get("revoked_at")?,
            user: AuthUser::from_row(row)?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct NewRefreshSession {
    pub id: Uuid,
    pub user_id: Uuid,
    pub token_hash: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub client_ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineAllowlistEntry {
    pub id: Uuid,
    pub cidr: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl MachineAllowlistEntry {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            cidr: row.try_get("cidr")?,
            description: row.try_get("description")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MachineAllowlistWrite {
    pub cidr: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct SecurityAuditEventRecord {
    pub event_type: String,
    pub actor: String,
    pub subject: Option<String>,
    pub remote_ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub payload: Value,
}
