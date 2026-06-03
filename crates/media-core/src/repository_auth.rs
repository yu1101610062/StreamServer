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
        let row = sqlx::query(
            r#"
            update auth_users
               set password_hash = $1,
                   must_change_password = $2,
                   password_changed_at = $3,
                   updated_at = $3
             where username = $4
         returning id
            "#,
        )
        .bind(password_hash)
        .bind(must_change_password)
        .bind(Utc::now())
        .bind(username)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| RepoError::AuthUserNotFound(username.to_string()))?;
        let user_id: Uuid = row.try_get("id")?;
        self.revoke_user_refresh_sessions(user_id, Utc::now())
            .await?;
        self.insert_security_audit_event(SecurityAuditEventRecord {
            event_type: event_type.to_string(),
            actor: actor.to_string(),
            subject: Some(username.to_string()),
            remote_ip,
            user_agent: user_agent.map(str::to_string),
            payload: json!({}),
        })
        .await?;
        Ok(())
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

    pub async fn insert_refresh_session(&self, record: NewRefreshSession) -> Result<(), RepoError> {
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
        .execute(&self.pool)
        .await?;
        Ok(())
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

    pub async fn rotate_refresh_session(
        &self,
        session_id: Uuid,
        token_hash: &str,
        expires_at: DateTime<Utc>,
        used_at: DateTime<Utc>,
        client_ip: Option<IpAddr>,
        user_agent: Option<&str>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            update auth_refresh_sessions
               set token_hash = $1,
                   expires_at = $2,
                   updated_at = $3,
                   last_used_at = $3,
                   client_ip = $4::inet,
                   user_agent = $5
             where id = $6
            "#,
        )
        .bind(token_hash)
        .bind(expires_at)
        .bind(used_at)
        .bind(client_ip.map(|value| value.to_string()))
        .bind(user_agent)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
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

    pub async fn revoke_user_refresh_sessions(
        &self,
        user_id: Uuid,
        revoked_at: DateTime<Utc>,
    ) -> Result<u64, RepoError> {
        let result = sqlx::query(
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
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
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

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: Uuid,
    pub username: String,
    pub password_hash: String,
    pub role: String,
    pub enabled: bool,
    pub must_change_password: bool,
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
