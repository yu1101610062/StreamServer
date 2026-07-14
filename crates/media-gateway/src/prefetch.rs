use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context, ensure};
use serde::Deserialize;
use tokio::{fs, io::AsyncWriteExt, process::Command};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrefetchSourceKind {
    HttpMp4,
    HttpTs,
    Hls,
}

#[derive(Debug, Clone)]
pub(crate) struct PrefetchJob {
    pub(crate) source_url: String,
    pub(crate) final_path: PathBuf,
    pub(crate) source_kind: Option<PrefetchSourceKind>,
    pub(crate) start_offset_sec: Option<u32>,
    pub(crate) duration_sec: Option<u32>,
}

pub(crate) async fn execute_prefetch(
    http: reqwest::Client,
    ffmpeg_bin: &Path,
    job: PrefetchJob,
) -> anyhow::Result<()> {
    if job.start_offset_sec.is_none() && job.duration_sec.is_none() {
        return download_to_file(http, &job.source_url, &job.final_path).await;
    }
    let source_kind = job
        .source_kind
        .context("source_kind is required for time-slice prefetch")?;
    match source_kind {
        PrefetchSourceKind::HttpMp4 | PrefetchSourceKind::HttpTs => {
            clip_single_file(ffmpeg_bin, &job, source_kind).await
        }
        PrefetchSourceKind::Hls => clip_hls(ffmpeg_bin, &job).await,
    }
}

async fn clip_hls(ffmpeg_bin: &Path, job: &PrefetchJob) -> anyhow::Result<()> {
    let final_dir = job
        .final_path
        .parent()
        .context("HLS target has no parent")?;
    let publish_parent = final_dir
        .parent()
        .context("HLS target directory has no parent")?;
    fs::create_dir_all(publish_parent).await?;
    ensure!(
        !fs::try_exists(final_dir).await?,
        "HLS target directory already exists"
    );

    let final_dir_name = final_dir
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("hls");
    let stage_dir = publish_parent.join(format!(".{final_dir_name}.clip.{}.part", Uuid::now_v7()));
    fs::create_dir_all(&stage_dir).await?;
    let playlist_name = job
        .final_path
        .file_name()
        .context("HLS target has no playlist name")?;
    let playlist_path = stage_dir.join(playlist_name);
    let playlist_stem = job
        .final_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("source");
    let segment_template = stage_dir.join(format!("{playlist_stem}-%05d.ts"));

    let mut args = base_clip_args(job);
    args.extend([
        OsString::from("-f"),
        OsString::from("hls"),
        OsString::from("-hls_playlist_type"),
        OsString::from("vod"),
        OsString::from("-hls_list_size"),
        OsString::from("0"),
        OsString::from("-hls_segment_filename"),
        segment_template.as_os_str().to_os_string(),
        playlist_path.as_os_str().to_os_string(),
    ]);

    let result = async {
        run_ffmpeg(ffmpeg_bin, &args).await?;
        ensure!(
            fs::metadata(&playlist_path).await?.len() > 0,
            "ffmpeg produced an empty HLS playlist"
        );
        let mut entries = fs::read_dir(&stage_dir).await?;
        let mut has_segment = false;
        while let Some(entry) = entries.next_entry().await? {
            if entry.path().extension().and_then(|value| value.to_str()) == Some("ts")
                && entry.metadata().await?.len() > 0
            {
                has_segment = true;
                break;
            }
        }
        ensure!(has_segment, "ffmpeg produced no HLS media segment");
        fs::rename(&stage_dir, final_dir).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage_dir).await;
    }
    result
}

async fn download_to_file(
    http: reqwest::Client,
    source_url: &str,
    final_path: &Path,
) -> anyhow::Result<()> {
    let part_path = temporary_file_path(final_path, "download");
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let result = async {
        let mut response = http
            .get(source_url)
            .send()
            .await?
            .error_for_status()
            .context("source download failed")?;
        let mut file = fs::File::create(&part_path).await?;
        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);
        fs::rename(&part_path, final_path).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&part_path).await;
    }
    result
}

async fn clip_single_file(
    ffmpeg_bin: &Path,
    job: &PrefetchJob,
    source_kind: PrefetchSourceKind,
) -> anyhow::Result<()> {
    let parent = job
        .final_path
        .parent()
        .context("prefetch target has no parent")?;
    fs::create_dir_all(parent).await?;
    let stage_path = temporary_file_path(&job.final_path, "clip");
    let muxer = match source_kind {
        PrefetchSourceKind::HttpMp4 => "mp4",
        PrefetchSourceKind::HttpTs => "mpegts",
        PrefetchSourceKind::Hls => unreachable!("HLS uses directory publishing"),
    };
    let mut args = base_clip_args(job);
    args.extend([
        OsString::from("-f"),
        OsString::from(muxer),
        stage_path.as_os_str().to_os_string(),
    ]);
    let result = async {
        run_ffmpeg(ffmpeg_bin, &args).await?;
        let metadata = fs::metadata(&stage_path).await?;
        ensure!(
            metadata.is_file() && metadata.len() > 0,
            "ffmpeg produced an empty time slice"
        );
        fs::rename(&stage_path, &job.final_path).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&stage_path).await;
    }
    result
}

fn base_clip_args(job: &PrefetchJob) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-hide_banner", "-nostdin", "-y", "-loglevel", "error"]
        .into_iter()
        .map(OsString::from)
        .collect();
    if let Some(start_offset_sec) = job.start_offset_sec.filter(|value| *value > 0) {
        args.extend([
            OsString::from("-ss"),
            OsString::from(start_offset_sec.to_string()),
        ]);
    }
    args.extend([
        OsString::from("-i"),
        OsString::from(job.source_url.as_str()),
    ]);
    if let Some(duration_sec) = job.duration_sec {
        args.extend([
            OsString::from("-t"),
            OsString::from(duration_sec.to_string()),
        ]);
    }
    args.extend(
        [
            "-map",
            "0:v?",
            "-map",
            "0:a?",
            "-map",
            "0:s?",
            "-map_metadata",
            "0",
            "-c",
            "copy",
        ]
        .into_iter()
        .map(OsString::from),
    );
    args
}

async fn run_ffmpeg(ffmpeg_bin: &Path, args: &[OsString]) -> anyhow::Result<()> {
    let output = Command::new(ffmpeg_bin)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to start ffmpeg at {}", ffmpeg_bin.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr: String = String::from_utf8_lossy(&output.stderr)
        .chars()
        .take(4096)
        .collect();
    anyhow::bail!("ffmpeg exited with {}: {}", output.status, stderr.trim());
}

fn temporary_file_path(final_path: &Path, label: &str) -> PathBuf {
    let file_name = final_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("source");
    final_path.with_file_name(format!(".{file_name}.{label}.{}.part", Uuid::now_v7()))
}
