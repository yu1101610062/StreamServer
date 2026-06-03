//! Runtime 文件产物元数据：把已生成的托管文件信息附加到 runtime handle。
//!
//! 这里只处理运行结束后的 artifact metadata 回填，不负责输出路径分配、FFmpeg 参数或
//! runtime 事件投递，避免这些收尾细节继续堆在 executor 主模块里。

use std::{fs, path::Path};

use media_domain::RuntimeHandle;
use serde_json::{Value, json};

use crate::{
    runtime::SuccessCheck, runtime_metadata::managed_file_output_kind_from_handle,
    runtime_outputs::ManagedFileOutputKind,
};

pub(crate) fn attach_file_artifact_metadata(
    handle: &mut RuntimeHandle,
    success_check: &SuccessCheck,
) {
    let Some(kind) = managed_file_output_kind_from_handle(handle) else {
        return;
    };

    if kind == ManagedFileOutputKind::StreamIngestRecord {
        let mut artifacts = handle
            .outputs
            .iter()
            .filter_map(|output| file_artifact_metadata_from_path(Path::new(output)))
            .collect::<Vec<_>>();
        if artifacts.is_empty() {
            if let SuccessCheck::FileExists(path) = success_check {
                if let Some(metadata) = file_artifact_metadata_from_path(path) {
                    artifacts.push(metadata);
                }
            }
        }
        let Some(object) = handle.metadata.as_object_mut() else {
            return;
        };
        if !artifacts.is_empty() {
            object.insert(kind.metadata_key().to_string(), Value::Array(artifacts));
        }
        return;
    }

    let SuccessCheck::FileExists(path) = success_check else {
        return;
    };
    let Some(metadata) = file_artifact_metadata_from_path(path) else {
        return;
    };
    let Some(object) = handle.metadata.as_object_mut() else {
        return;
    };
    object.insert(kind.metadata_key().to_string(), metadata);
}

fn file_artifact_metadata_from_path(path: &Path) -> Option<Value> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }

    Some(json!({
        "file_name": path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string(),
        "file_path": path.to_string_lossy().to_string(),
        "file_size": i64::try_from(metadata.len()).unwrap_or(i64::MAX),
    }))
}
