//! 节点仓储：维护 Agent 节点注册、心跳、能力快照、调试目标和节点标识解析。

use chrono::{DateTime, Utc};
use media_domain::{
    AgentRegistration, CapabilitySnapshot, GpuDeviceInfo, GpuRuntimeStats, HeartbeatSnapshot,
};
use serde::Serialize;
use serde_json::Value;
use sqlx::{Row, postgres::PgRow};
use uuid::Uuid;

use super::{RepoError, TaskRepository, validation_error};

impl TaskRepository {
    pub async fn resolve_node_id_by_server_id(
        &self,
        server_id: &str,
    ) -> Result<Option<Uuid>, RepoError> {
        let row = sqlx::query("select node_id from media_servers where server_id = $1")
            .bind(server_id.trim())
            .fetch_optional(&self.pool)
            .await?;

        row.map(|row| row.try_get("node_id"))
            .transpose()
            .map_err(RepoError::Sqlx)
    }

    pub async fn list_nodes(&self) -> Result<Vec<NodeSummary>, RepoError> {
        sqlx::query(
            r#"
            select
              n.id,
              n.node_name,
              n.hostname,
              n.labels,
              n.zlm_api_base,
              n.agent_stream_addr,
              n.agent_http_base_url,
              n.zlm_rtmp_port,
              n.zlm_rtsp_port,
              n.network_mode,
              n.interfaces,
              n.healthy,
              n.control_connected,
              n.last_seen_at,
              n.control_last_seen_at,
              n.media_last_seen_at,
              n.created_at,
              n.updated_at,
              c.ffmpeg_protocols,
              c.ffmpeg_formats,
              c.ffmpeg_encoders,
              c.ffmpeg_decoders,
              c.zlm_api_list,
              c.zlm_version,
              c.gpu,
              c.gpu_devices,
              c.captured_at
            from media_nodes n
            left join node_capabilities c on c.node_id = n.id
            order by n.updated_at desc, n.node_name asc
            "#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| NodeSummary::from_row(&row))
        .collect()
    }

    pub async fn list_node_heartbeats(
        &self,
        node_id: Uuid,
        limit: u32,
    ) -> Result<Vec<NodeHeartbeatSummary>, RepoError> {
        let limit = limit.clamp(1, 200);
        Ok(sqlx::query(
            r#"
            select
              node_id,
              cpu_percent,
              mem_percent,
              disk_percent,
              upload_disk_total_bytes,
              upload_disk_available_bytes,
              upload_disk_used_percent,
              running_tasks,
              starting_tasks,
              stopping_tasks,
              orphaned_tasks,
              slot_usage,
              zlm_alive,
              ffmpeg_alive,
              gpu_runtime,
              node_time,
              received_at
            from node_heartbeats
            where node_id = $1
            order by received_at desc, node_time desc
            limit $2
            "#,
        )
        .bind(node_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| NodeHeartbeatSummary::from_row(&row))
        .collect::<Result<Vec<_>, _>>()?)
    }

    pub async fn get_node_debug_target(&self, node_id: Uuid) -> Result<NodeDebugTarget, RepoError> {
        sqlx::query(
            r#"
            select id, zlm_api_base, zlm_api_secret
              from media_nodes
             where id = $1
            "#,
        )
        .bind(node_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(RepoError::NodeNotFound(node_id))
        .and_then(|row| {
            Ok(NodeDebugTarget {
                zlm_api_base: row.try_get("zlm_api_base")?,
                zlm_api_secret: row.try_get("zlm_api_secret")?,
            })
        })
    }

    pub async fn upsert_node_registration(
        &self,
        registration: &AgentRegistration,
        seen_at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            insert into media_nodes (
              id, node_name, hostname, labels, zlm_api_base, zlm_api_secret, agent_stream_addr,
              agent_http_base_url, zlm_rtmp_port, zlm_rtsp_port,
              output_mount_relative_prefix_mp4, output_mount_relative_prefix_hls,
              network_mode, interfaces, healthy, control_connected, last_seen_at,
              control_last_seen_at, created_at, updated_at
            ) values (
              $1, $2, $3, $4, $5, $6, $7,
              $8, $9, $10, $11, $12, $13, $14, true, true, $15, $15, $16, $16
            )
            on conflict (id) do update
               set node_name = excluded.node_name,
                   hostname = excluded.hostname,
                   labels = excluded.labels,
                   zlm_api_base = excluded.zlm_api_base,
                   zlm_api_secret = excluded.zlm_api_secret,
                   agent_stream_addr = excluded.agent_stream_addr,
                   agent_http_base_url = excluded.agent_http_base_url,
                   zlm_rtmp_port = excluded.zlm_rtmp_port,
                   zlm_rtsp_port = excluded.zlm_rtsp_port,
                   output_mount_relative_prefix_mp4 =
                     excluded.output_mount_relative_prefix_mp4,
                   output_mount_relative_prefix_hls =
                     excluded.output_mount_relative_prefix_hls,
                   network_mode = excluded.network_mode,
                   interfaces = excluded.interfaces,
                   healthy = true,
                   control_connected = true,
                   last_seen_at = excluded.last_seen_at,
                   control_last_seen_at = excluded.control_last_seen_at,
                   updated_at = excluded.updated_at
            "#,
        )
        .bind(registration.node_id)
        .bind(&registration.node_name)
        .bind(&registration.hostname)
        .bind(serde_json::to_value(&registration.labels)?)
        .bind(&registration.zlm_api_base)
        .bind(&registration.zlm_api_secret)
        .bind(&registration.agent_stream_addr)
        .bind(&registration.agent_http_base_url)
        .bind(i32::from(registration.zlm_rtmp_port))
        .bind(i32::from(registration.zlm_rtsp_port))
        .bind(&registration.output_mount_relative_prefix_mp4)
        .bind(&registration.output_mount_relative_prefix_hls)
        .bind(registration.network_mode.as_str())
        .bind(serde_json::to_value(&registration.interfaces)?)
        .bind(seen_at)
        .bind(Utc::now())
        .execute(&mut *tx)
        .await?;

        let zlm_server_id = registration.zlm_server_id.trim();
        if !zlm_server_id.is_empty() {
            sqlx::query(
                r#"
                insert into media_servers (server_id, node_id, last_seen_at, created_at, updated_at)
                values ($1, $2, $3, $4, $4)
                on conflict (server_id) do update
                   set node_id = excluded.node_id,
                       last_seen_at = excluded.last_seen_at,
                       updated_at = excluded.updated_at
                "#,
            )
            .bind(zlm_server_id)
            .bind(registration.node_id)
            .bind(seen_at)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        Ok(())
    }

    pub async fn record_node_heartbeat(
        &self,
        node_id: Uuid,
        heartbeat: &HeartbeatSnapshot,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            r#"
            update media_nodes
               set healthy = not $4,
                   control_connected = true,
                   last_seen_at = $1,
                   control_last_seen_at = $1,
                   updated_at = $2
             where id = $3
            "#,
        )
        .bind(heartbeat.node_time)
        .bind(Utc::now())
        .bind(node_id)
        .bind(heartbeat.artifact_cleanup_blocked)
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            return Err(RepoError::NodeNotFound(node_id));
        }

        sqlx::query(
            r#"
            insert into node_heartbeats (
              id, node_id, cpu_percent, mem_percent, disk_percent, running_tasks,
              upload_disk_total_bytes, upload_disk_available_bytes, upload_disk_used_percent,
              starting_tasks, stopping_tasks, orphaned_tasks,
              slot_usage, zlm_alive, ffmpeg_alive, gpu_runtime, node_time, received_at
            ) values (
              $1, $2, $3, $4, $5, $6,
              $7, $8, $9,
              $10, $11, $12,
              $13, $14, $15, $16, $17, $18
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(node_id)
        .bind(heartbeat.cpu_percent)
        .bind(heartbeat.mem_percent)
        .bind(heartbeat.disk_percent)
        .bind(i32::try_from(heartbeat.running_tasks).unwrap_or(i32::MAX))
        .bind(i64::try_from(heartbeat.upload_disk_total_bytes).unwrap_or(i64::MAX))
        .bind(i64::try_from(heartbeat.upload_disk_available_bytes).unwrap_or(i64::MAX))
        .bind(heartbeat.upload_disk_used_percent)
        .bind(i32::try_from(heartbeat.starting_tasks).unwrap_or(i32::MAX))
        .bind(i32::try_from(heartbeat.stopping_tasks).unwrap_or(i32::MAX))
        .bind(i32::try_from(heartbeat.orphaned_tasks).unwrap_or(i32::MAX))
        .bind(heartbeat.slot_usage)
        .bind(heartbeat.zlm_alive)
        .bind(heartbeat.ffmpeg_alive)
        .bind(serde_json::to_value(&heartbeat.gpu_runtime)?)
        .bind(heartbeat.node_time)
        .bind(Utc::now())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(())
    }

    pub async fn update_node_health(
        &self,
        node_id: Uuid,
        healthy: bool,
        last_seen_at: Option<DateTime<Utc>>,
    ) -> Result<(), RepoError> {
        let result = sqlx::query(
            r#"
            update media_nodes
               set healthy = $1,
                   control_connected = $1,
                   last_seen_at = coalesce($2, last_seen_at),
                   control_last_seen_at = case when $1 then coalesce($2, control_last_seen_at) else control_last_seen_at end,
                   updated_at = $3
             where id = $4
            "#,
        )
        .bind(healthy)
        .bind(last_seen_at)
        .bind(Utc::now())
        .bind(node_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(RepoError::NodeNotFound(node_id));
        }

        Ok(())
    }

    pub async fn record_media_server_seen(
        &self,
        node_id: Uuid,
        server_id: &str,
        seen_at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            insert into media_servers (server_id, node_id, last_seen_at, created_at, updated_at)
            values ($1, $2, $3, $4, $4)
            on conflict (server_id) do update
               set node_id = excluded.node_id,
                   last_seen_at = excluded.last_seen_at,
                   updated_at = excluded.updated_at
            "#,
        )
        .bind(server_id.trim())
        .bind(node_id)
        .bind(seen_at)
        .bind(Utc::now())
        .execute(&mut *tx)
        .await?;

        let result = sqlx::query(
            r#"
            update media_nodes
               set healthy = control_connected,
                   last_seen_at = $1,
                   media_last_seen_at = $1,
                   updated_at = $2
             where id = $3
            "#,
        )
        .bind(seen_at)
        .bind(Utc::now())
        .bind(node_id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(RepoError::NodeNotFound(node_id));
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn upsert_node_capabilities(
        &self,
        node_id: Uuid,
        snapshot: &CapabilitySnapshot,
    ) -> Result<(), RepoError> {
        let result = sqlx::query(
            r#"
            insert into node_capabilities (
              node_id, ffmpeg_protocols, ffmpeg_formats, ffmpeg_encoders,
              ffmpeg_decoders, zlm_api_list, zlm_version, gpu, gpu_devices, captured_at
            ) values (
              $1, $2, $3, $4,
              $5, $6, $7, $8, $9, $10
            )
            on conflict (node_id) do update
               set ffmpeg_protocols = excluded.ffmpeg_protocols,
                   ffmpeg_formats = excluded.ffmpeg_formats,
                   ffmpeg_encoders = excluded.ffmpeg_encoders,
                   ffmpeg_decoders = excluded.ffmpeg_decoders,
                   zlm_api_list = excluded.zlm_api_list,
                   zlm_version = excluded.zlm_version,
                   gpu = excluded.gpu,
                   gpu_devices = excluded.gpu_devices,
                   captured_at = excluded.captured_at
            "#,
        )
        .bind(node_id)
        .bind(serde_json::to_value(&snapshot.ffmpeg_protocols)?)
        .bind(serde_json::to_value(&snapshot.ffmpeg_formats)?)
        .bind(serde_json::to_value(&snapshot.ffmpeg_encoders)?)
        .bind(serde_json::to_value(&snapshot.ffmpeg_decoders)?)
        .bind(serde_json::to_value(&snapshot.zlm_api_list)?)
        .bind(snapshot.zlm_version.as_deref())
        .bind(serde_json::to_value(&snapshot.gpu)?)
        .bind(serde_json::to_value(&snapshot.gpu_devices)?)
        .bind(snapshot.captured_at)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(RepoError::NodeNotFound(node_id));
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeSummary {
    pub id: Uuid,
    pub node_name: String,
    pub hostname: String,
    pub labels: Vec<String>,
    pub zlm_api_base: String,
    pub agent_stream_addr: String,
    pub agent_http_base_url: String,
    pub zlm_rtmp_port: u16,
    pub zlm_rtsp_port: u16,
    pub network_mode: String,
    pub interfaces: Vec<String>,
    pub healthy: bool,
    pub control_connected: bool,
    pub media_alive: bool,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub control_last_seen_at: Option<DateTime<Utc>>,
    pub media_last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub ffmpeg_protocols: Vec<String>,
    pub ffmpeg_formats: Vec<String>,
    pub ffmpeg_encoders: Vec<String>,
    pub ffmpeg_decoders: Vec<String>,
    pub zlm_api_list: Vec<String>,
    pub zlm_version: Option<String>,
    pub gpu: Vec<String>,
    pub gpu_devices: Vec<GpuDeviceInfo>,
    pub capability_captured_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_usage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_tasks: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starting_tasks: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopping_tasks: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orphaned_tasks: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_disk_total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_disk_available_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_disk_used_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zlm_alive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ffmpeg_alive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_runtime: Option<Vec<GpuRuntimeStats>>,
}

impl NodeSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        let media_last_seen_at: Option<DateTime<Utc>> = row.try_get("media_last_seen_at")?;
        let zlm_rtmp_port = u16::try_from(row.try_get::<i32, _>("zlm_rtmp_port")?)
            .map_err(|_| validation_error("zlm_rtmp_port", "stored value is out of range"))?;
        let zlm_rtsp_port = u16::try_from(row.try_get::<i32, _>("zlm_rtsp_port")?)
            .map_err(|_| validation_error("zlm_rtsp_port", "stored value is out of range"))?;
        let media_alive = media_last_seen_at
            .map(|seen_at| seen_at >= Utc::now() - chrono::Duration::seconds(30))
            .unwrap_or(false);
        Ok(Self {
            id: row.try_get("id")?,
            node_name: row.try_get("node_name")?,
            hostname: row.try_get("hostname")?,
            labels: serde_json::from_value(row.try_get("labels")?)?,
            zlm_api_base: row.try_get("zlm_api_base")?,
            agent_stream_addr: row.try_get("agent_stream_addr")?,
            agent_http_base_url: row.try_get("agent_http_base_url")?,
            zlm_rtmp_port,
            zlm_rtsp_port,
            network_mode: row.try_get("network_mode")?,
            interfaces: serde_json::from_value(row.try_get("interfaces")?)?,
            healthy: row.try_get("healthy")?,
            control_connected: row.try_get("control_connected")?,
            media_alive,
            last_seen_at: row.try_get("last_seen_at")?,
            control_last_seen_at: row.try_get("control_last_seen_at")?,
            media_last_seen_at,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            ffmpeg_protocols: row
                .try_get::<Option<Value>, _>("ffmpeg_protocols")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            ffmpeg_formats: row
                .try_get::<Option<Value>, _>("ffmpeg_formats")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            ffmpeg_encoders: row
                .try_get::<Option<Value>, _>("ffmpeg_encoders")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            ffmpeg_decoders: row
                .try_get::<Option<Value>, _>("ffmpeg_decoders")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            zlm_api_list: row
                .try_get::<Option<Value>, _>("zlm_api_list")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            zlm_version: row.try_get("zlm_version")?,
            gpu: row
                .try_get::<Option<Value>, _>("gpu")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            gpu_devices: row
                .try_get::<Option<Value>, _>("gpu_devices")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            capability_captured_at: row.try_get("captured_at")?,
            slot_usage: None,
            running_tasks: None,
            starting_tasks: None,
            stopping_tasks: None,
            orphaned_tasks: None,
            connected: None,
            cpu_percent: None,
            mem_percent: None,
            disk_percent: None,
            upload_disk_total_bytes: None,
            upload_disk_available_bytes: None,
            upload_disk_used_percent: None,
            zlm_alive: None,
            ffmpeg_alive: None,
            gpu_runtime: None,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeHeartbeatSummary {
    pub node_id: Uuid,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub disk_percent: f64,
    pub upload_disk_total_bytes: u64,
    pub upload_disk_available_bytes: u64,
    pub upload_disk_used_percent: f64,
    pub running_tasks: u32,
    pub starting_tasks: u32,
    pub stopping_tasks: u32,
    pub orphaned_tasks: u32,
    pub slot_usage: f64,
    pub zlm_alive: bool,
    pub ffmpeg_alive: bool,
    pub gpu_runtime: Vec<GpuRuntimeStats>,
    pub node_time: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
}

impl NodeHeartbeatSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        let running_tasks = row.try_get::<i32, _>("running_tasks")?;
        Ok(Self {
            node_id: row.try_get("node_id")?,
            cpu_percent: row.try_get("cpu_percent")?,
            mem_percent: row.try_get("mem_percent")?,
            disk_percent: row.try_get("disk_percent")?,
            upload_disk_total_bytes: u64::try_from(
                row.try_get::<i64, _>("upload_disk_total_bytes")?,
            )
            .unwrap_or_default(),
            upload_disk_available_bytes: u64::try_from(
                row.try_get::<i64, _>("upload_disk_available_bytes")?,
            )
            .unwrap_or_default(),
            upload_disk_used_percent: row.try_get("upload_disk_used_percent")?,
            running_tasks: u32::try_from(running_tasks).unwrap_or_default(),
            starting_tasks: u32::try_from(row.try_get::<i32, _>("starting_tasks")?)
                .unwrap_or_default(),
            stopping_tasks: u32::try_from(row.try_get::<i32, _>("stopping_tasks")?)
                .unwrap_or_default(),
            orphaned_tasks: u32::try_from(row.try_get::<i32, _>("orphaned_tasks")?)
                .unwrap_or_default(),
            slot_usage: row.try_get("slot_usage")?,
            zlm_alive: row.try_get("zlm_alive")?,
            ffmpeg_alive: row.try_get("ffmpeg_alive")?,
            gpu_runtime: row
                .try_get::<Option<Value>, _>("gpu_runtime")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            node_time: row.try_get("node_time")?,
            received_at: row.try_get("received_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct NodeDebugTarget {
    pub zlm_api_base: String,
    pub zlm_api_secret: String,
}
