//! 路径转换辅助：把内部产物路径转换为 HTTP 可见路径，并校验托管输出路径边界。

use std::path::{Component, Path};

use reqwest::Url;
use serde_json::Value;
use sqlx::{Row, postgres::PgRow};
use uuid::Uuid;

use crate::repository::{RepoError, validation_error};

const ZLM_HTTP_ROOT_SEGMENT: &str = "/data/zlm/www";
const ZLM_OUTPUT_HTTP_ROOT_SEGMENT: &str = "/data/zlm/www/output";
const ZLM_OUTPUT_MP4_RELATIVE_ROOT: &str = "output/mp4";
const ZLM_OUTPUT_HLS_RELATIVE_ROOT: &str = "output/hls";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedOutputBucket {
    Mp4,
    Hls,
}

#[derive(Debug, Clone)]
pub(crate) struct OutputMountPrefixes {
    pub(crate) mp4: String,
    pub(crate) hls: String,
}

impl OutputMountPrefixes {
    pub(crate) fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            mp4: row.try_get("output_mount_relative_prefix_mp4")?,
            hls: row.try_get("output_mount_relative_prefix_hls")?,
        })
    }

    pub(crate) fn from_optional_row(row: &PgRow) -> Result<Option<Self>, RepoError> {
        let mp4: Option<String> = row.try_get("output_mount_relative_prefix_mp4")?;
        let hls: Option<String> = row.try_get("output_mount_relative_prefix_hls")?;
        match (mp4, hls) {
            (Some(mp4), Some(hls)) => Ok(Some(Self { mp4, hls })),
            (None, None) => Ok(None),
            _ => Err(validation_error(
                "file_path",
                "node output mount prefixes are incomplete",
            )),
        }
    }

    fn relative_prefix_for_bucket(&self, bucket: ManagedOutputBucket) -> &str {
        match bucket {
            ManagedOutputBucket::Mp4 => self.mp4.as_str(),
            ManagedOutputBucket::Hls => self.hls.as_str(),
        }
    }
}

pub(crate) fn task_id_from_managed_output_path(path: &str) -> Option<Uuid> {
    // Managed output paths are output/{mp4,hls}/node-*/{task_id}/..., which lets hooks map back.
    let normalized = normalized_absolute_path(path).ok()?;
    let relative = relative_path_under_output_root(&normalized, ManagedOutputBucket::Mp4)
        .or_else(|| relative_path_under_output_root(&normalized, ManagedOutputBucket::Hls))?;
    let mut segments = relative.split('/').filter(|segment| !segment.is_empty());
    let _node_dir = segments.next()?;
    Uuid::parse_str(segments.next()?).ok()
}

pub(crate) fn relative_http_url_from_path(file_path: &str) -> Result<String, RepoError> {
    let normalized = normalized_absolute_path(file_path)?;
    let relative = relative_path_under_zlm_http_root(&normalized).ok_or_else(|| {
        validation_error("publish.url", "output path must be under */data/zlm/www")
    })?;
    Ok(format!("/{}", relative.trim_start_matches('/')))
}

pub(crate) fn externalize_managed_path(
    path: &str,
    field: &'static str,
    prefixes: &OutputMountPrefixes,
) -> Result<String, RepoError> {
    let normalized = normalized_absolute_path(path)?;
    if let Some(relative) = external_relative_path_from_normalized(&normalized, prefixes) {
        return Ok(relative);
    }

    tracing::warn!(
        field,
        path = %normalized,
        "managed path is outside outward-facing storage roots"
    );
    Err(validation_error(
        field,
        format!("must be under *{ZLM_OUTPUT_HTTP_ROOT_SEGMENT}"),
    ))
}

pub(crate) fn externalize_path_fields_in_payload(
    value: Value,
    prefixes: Option<&OutputMountPrefixes>,
) -> Result<Value, RepoError> {
    match value {
        Value::Array(items) => items
            .into_iter()
            .map(|item| externalize_path_fields_in_payload(item, prefixes))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(entries) => {
            let mut normalized = serde_json::Map::with_capacity(entries.len());
            for (key, value) in entries {
                let rewritten = match key.as_str() {
                    "file_path" => externalize_path_field_value(value, "file_path", prefixes)?,
                    "folder" => externalize_path_field_value(value, "folder", prefixes)?,
                    _ => externalize_path_fields_in_payload(value, prefixes)?,
                };
                normalized.insert(key, rewritten);
            }
            Ok(Value::Object(normalized))
        }
        other => Ok(other),
    }
}

pub(crate) fn absolute_http_url_from_relative(
    agent_stream_addr: &str,
    relative: &str,
) -> Option<String> {
    let base = Url::parse(agent_stream_addr)
        .map_err(|error| {
            tracing::warn!(
                %agent_stream_addr,
                %error,
                "invalid node stream base while building HTTP URL"
            );
        })
        .ok()?;
    base.join(relative).ok().map(|value| value.to_string())
}

pub(crate) fn absolute_http_url_from_file_path(
    agent_stream_addr: &str,
    file_path: &str,
) -> Option<String> {
    let relative = relative_http_url_from_path(file_path).ok()?;
    absolute_http_url_from_relative(agent_stream_addr, &relative)
}

pub(crate) fn is_hls_playlist_record_path(file_path: &str) -> bool {
    let Ok(normalized) = normalized_absolute_path(file_path) else {
        return false;
    };
    let in_record_root =
        relative_path_under_output_root(&normalized, ManagedOutputBucket::Hls).is_some();
    in_record_root
        && Path::new(&normalized)
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("m3u8"))
}

fn relative_path_under_root<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    if path == root {
        return None;
    }
    path.strip_prefix(root)?.strip_prefix('/')
}

fn zlm_http_root_in_path(path: &str) -> Option<&str> {
    // Compatible with install-dir mounts and legacy container paths.
    for (index, _) in path.match_indices(ZLM_HTTP_ROOT_SEGMENT) {
        let end = index + ZLM_HTTP_ROOT_SEGMENT.len();
        let suffix = &path[end..];
        if suffix.is_empty() || suffix.starts_with('/') {
            return Some(&path[..end]);
        }
    }
    None
}

fn relative_path_under_zlm_http_root(path: &str) -> Option<&str> {
    let root = zlm_http_root_in_path(path)?;
    relative_path_under_root(path, root)
}

fn relative_path_under_output_root<'a>(
    path: &'a str,
    bucket: ManagedOutputBucket,
) -> Option<&'a str> {
    let relative = relative_path_under_zlm_http_root(path)?;
    let root = match bucket {
        ManagedOutputBucket::Mp4 => ZLM_OUTPUT_MP4_RELATIVE_ROOT,
        ManagedOutputBucket::Hls => ZLM_OUTPUT_HLS_RELATIVE_ROOT,
    };
    if relative == root {
        return Some("");
    }
    relative_path_under_root(relative, root)
}

fn normalized_absolute_path(path: &str) -> Result<String, RepoError> {
    let path = Path::new(path.trim());
    if !path.is_absolute() {
        return Err(validation_error("publish.url", "must be an absolute path"));
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => parts.push(value.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(validation_error(
                    "publish.url",
                    "must not contain parent segments",
                ));
            }
            Component::Prefix(_) => {
                return Err(validation_error("publish.url", "must be a POSIX path"));
            }
        }
    }

    Ok(format!("/{}", parts.join("/")))
}

fn managed_output_bucket_from_path(path: &str) -> Option<ManagedOutputBucket> {
    if relative_path_under_output_root(path, ManagedOutputBucket::Mp4).is_some() {
        return Some(ManagedOutputBucket::Mp4);
    }
    if relative_path_under_output_root(path, ManagedOutputBucket::Hls).is_some() {
        return Some(ManagedOutputBucket::Hls);
    }
    None
}

fn visible_root_for_bucket(
    path: &str,
    bucket: ManagedOutputBucket,
    prefixes: &OutputMountPrefixes,
) -> Option<String> {
    let zlm_http_root = zlm_http_root_in_path(path)?;
    let relative_prefix = prefixes.relative_prefix_for_bucket(bucket);
    Some(if relative_prefix.is_empty() {
        zlm_http_root.to_string()
    } else {
        format!("{zlm_http_root}/{relative_prefix}")
    })
}

fn external_relative_path_from_normalized(
    path: &str,
    prefixes: &OutputMountPrefixes,
) -> Option<String> {
    let bucket = managed_output_bucket_from_path(path)?;
    let visible_root = visible_root_for_bucket(path, bucket, prefixes)?;
    if path == visible_root {
        return Some("/".to_string());
    }
    relative_path_under_root(path, &visible_root)
        .map(|relative| format!("/{}", relative.trim_start_matches('/')))
}

fn externalize_path_field_value(
    value: Value,
    field: &'static str,
    prefixes: Option<&OutputMountPrefixes>,
) -> Result<Value, RepoError> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::String(path) if path.trim().is_empty() => Ok(Value::String(path)),
        Value::String(path) => {
            if let Some(prefixes) = prefixes {
                externalize_managed_path(&path, field, prefixes).map(Value::String)
            } else {
                Ok(Value::String(path))
            }
        }
        Value::Array(items) => items
            .into_iter()
            .map(|item| externalize_path_field_value(item, field, prefixes))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(entries) => {
            externalize_path_fields_in_payload(Value::Object(entries), prefixes)
        }
        other => Ok(other),
    }
}
