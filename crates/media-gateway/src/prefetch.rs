use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const FFMPEG_STDERR_TAIL_LIMIT: usize = 4096;
const FFMPEG_TERMINATE_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrefetchSourceKind {
    HttpMp4,
    HttpTs,
    Hls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionClass {
    Download,
    Ffmpeg,
}

#[derive(Debug, Clone)]
pub(crate) struct PrefetchJob {
    pub(crate) source_url: String,
    pub(crate) final_path: PathBuf,
    pub(crate) source_kind: Option<PrefetchSourceKind>,
    pub(crate) start_offset_sec: Option<u32>,
    pub(crate) duration_sec: Option<u32>,
    pub(crate) read_idle_timeout: Duration,
}

impl PrefetchJob {
    pub(crate) fn execution_class(&self) -> ExecutionClass {
        if self.source_kind == Some(PrefetchSourceKind::Hls)
            || self.start_offset_sec.is_some()
            || self.duration_sec.is_some()
        {
            ExecutionClass::Ffmpeg
        } else {
            ExecutionClass::Download
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrefetchOutcome {
    FullDownload,
    TimeSlice,
}

impl PrefetchOutcome {
    pub(crate) fn time_slice_applied(self) -> bool {
        matches!(self, Self::TimeSlice)
    }
}

pub(crate) async fn execute_prefetch(
    http: reqwest::Client,
    ffmpeg_bin: &Path,
    ffprobe_bin: Option<&Path>,
    job: PrefetchJob,
    cancellation: CancellationToken,
) -> anyhow::Result<PrefetchOutcome> {
    ensure_not_canceled(&cancellation)?;
    let is_time_slice = job.start_offset_sec.is_some() || job.duration_sec.is_some();
    match job.source_kind {
        Some(PrefetchSourceKind::Hls) => {
            clip_hls(ffmpeg_bin, ffprobe_bin, &job, &cancellation).await?
        }
        Some(source_kind @ (PrefetchSourceKind::HttpMp4 | PrefetchSourceKind::HttpTs))
            if is_time_slice =>
        {
            clip_single_file(ffmpeg_bin, ffprobe_bin, &job, source_kind, &cancellation).await?
        }
        _ if is_time_slice => {
            anyhow::bail!("source_kind is required for time-slice prefetch")
        }
        _ => {
            download_to_file(
                http,
                &job.source_url,
                &job.final_path,
                job.read_idle_timeout,
                &cancellation,
            )
            .await?
        }
    }
    Ok(if is_time_slice {
        PrefetchOutcome::TimeSlice
    } else {
        PrefetchOutcome::FullDownload
    })
}

pub(crate) async fn published_target_exists(job: &PrefetchJob) -> anyhow::Result<bool> {
    let target = published_target(job)?;
    fs::try_exists(target)
        .await
        .context("inspect published prefetch target")
}

pub(crate) async fn cleanup_published_target(job: &PrefetchJob) -> anyhow::Result<()> {
    let target = published_target(job)?;
    if job.source_kind == Some(PrefetchSourceKind::Hls) {
        match fs::remove_dir_all(target).await {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("remove published HLS directory"),
        }
    }

    match fs::remove_file(target).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("remove published prefetch file"),
    }
    let Some(parent) = job.final_path.parent() else {
        return Ok(());
    };
    match fs::remove_dir(parent).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("remove prefetch task directory"),
    }
}

pub(crate) async fn task_directory_exists(work_root: &Path, task_id: Uuid) -> anyhow::Result<bool> {
    fs::try_exists(prefetch_task_directory(work_root, task_id))
        .await
        .context("inspect prefetch task directory")
}

pub(crate) async fn cleanup_task_directory(work_root: &Path, task_id: Uuid) -> anyhow::Result<()> {
    match fs::remove_dir_all(prefetch_task_directory(work_root, task_id)).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("remove prefetch task directory"),
    }
}

fn prefetch_task_directory(work_root: &Path, task_id: Uuid) -> PathBuf {
    work_root.join("imports").join(task_id.to_string())
}

fn published_target(job: &PrefetchJob) -> anyhow::Result<&Path> {
    if job.source_kind == Some(PrefetchSourceKind::Hls) {
        job.final_path
            .parent()
            .context("HLS target has no task directory")
    } else {
        Ok(&job.final_path)
    }
}

async fn clip_hls(
    ffmpeg_bin: &Path,
    ffprobe_bin: Option<&Path>,
    job: &PrefetchJob,
    cancellation: &CancellationToken,
) -> anyhow::Result<()> {
    let final_dir = job
        .final_path
        .parent()
        .context("HLS target has no parent")?;
    let publish_parent = final_dir
        .parent()
        .context("HLS target directory has no parent")?;
    fs::create_dir_all(publish_parent).await?;
    let final_dir_name = final_dir
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("hls");
    let playlist_name = job
        .final_path
        .file_name()
        .context("HLS target has no playlist name")?;
    if fs::try_exists(final_dir).await? {
        let published_playlist = final_dir.join(playlist_name);
        ensure!(
            fs::metadata(&published_playlist).await?.is_file(),
            "existing HLS target is incomplete"
        );
        validate_staged_media(ffmpeg_bin, ffprobe_bin, &published_playlist, cancellation)
            .await
            .context("existing HLS target validation failed")?;
        return Ok(());
    }

    let stage_dir = publish_parent.join(format!(".{final_dir_name}.clip.{}.part", Uuid::now_v7()));
    fs::create_dir_all(&stage_dir).await?;
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
        run_ffmpeg(ffmpeg_bin, &args, cancellation).await?;
        ensure_not_canceled(cancellation)?;
        ensure!(
            fs::metadata(&playlist_path).await?.len() > 0,
            "ffmpeg produced an empty HLS playlist"
        );
        let mut entries = fs::read_dir(&stage_dir).await?;
        let mut has_segment = false;
        while let Some(entry) = entries.next_entry().await? {
            if matches!(
                entry.path().extension().and_then(|value| value.to_str()),
                Some("ts" | "m4s" | "mp4")
            ) && entry.metadata().await?.len() > 0
            {
                has_segment = true;
                break;
            }
        }
        ensure!(has_segment, "ffmpeg produced no HLS media segment");
        validate_staged_media(ffmpeg_bin, ffprobe_bin, &playlist_path, cancellation).await?;
        ensure_not_canceled(cancellation)?;
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
    read_idle_timeout: Duration,
    cancellation: &CancellationToken,
) -> anyhow::Result<()> {
    let part_path = temporary_file_path(final_path, "download");
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let result = async {
        let response = tokio::select! {
            _ = cancellation.cancelled() => return canceled(),
            response = http.get(source_url).send() => response?,
        }
        .error_for_status()
        .context("source download failed")?;
        let mut response = response;
        let mut file = fs::File::create(&part_path).await?;
        loop {
            let chunk = tokio::select! {
                _ = cancellation.cancelled() => return canceled(),
                chunk = timeout(read_idle_timeout, response.chunk()) => {
                    chunk.context("source read idle timeout")??
                }
            };
            let Some(chunk) = chunk else { break };
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);
        ensure_not_canceled(cancellation)?;
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
    ffprobe_bin: Option<&Path>,
    job: &PrefetchJob,
    source_kind: PrefetchSourceKind,
    cancellation: &CancellationToken,
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
        run_ffmpeg(ffmpeg_bin, &args, cancellation).await?;
        let metadata = fs::metadata(&stage_path).await?;
        ensure!(
            metadata.is_file() && metadata.len() > 0,
            "ffmpeg produced an empty time slice"
        );
        validate_staged_media(ffmpeg_bin, ffprobe_bin, &stage_path, cancellation).await?;
        ensure_not_canceled(cancellation)?;
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

async fn validate_staged_media(
    ffmpeg_bin: &Path,
    ffprobe_bin: Option<&Path>,
    input_path: &Path,
    cancellation: &CancellationToken,
) -> anyhow::Result<()> {
    if let Some(ffprobe_bin) = ffprobe_bin {
        let args = [
            OsString::from("-v"),
            OsString::from("error"),
            OsString::from("-show_entries"),
            OsString::from("stream=codec_type"),
            OsString::from("-of"),
            OsString::from("csv=p=0"),
            input_path.as_os_str().to_os_string(),
        ];
        let output = run_ffprobe(ffprobe_bin, &args, cancellation)
            .await
            .context("ffprobe output validation failed")?;
        let streams = String::from_utf8_lossy(&output);
        ensure!(
            streams
                .lines()
                .any(|line| matches!(line.trim(), "video" | "audio")),
            "ffprobe found no audio or video streams"
        );
        return Ok(());
    }

    let args = [
        OsString::from("-hide_banner"),
        OsString::from("-nostdin"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-i"),
        input_path.as_os_str().to_os_string(),
        OsString::from("-map"),
        OsString::from("0:v?"),
        OsString::from("-map"),
        OsString::from("0:a?"),
        OsString::from("-c"),
        OsString::from("copy"),
        OsString::from("-f"),
        OsString::from("null"),
        OsString::from("-"),
    ];
    run_ffmpeg(ffmpeg_bin, &args, cancellation)
        .await
        .context("ffmpeg output validation failed")
}

async fn run_ffprobe(
    ffprobe_bin: &Path,
    args: &[OsString],
    cancellation: &CancellationToken,
) -> anyhow::Result<Vec<u8>> {
    ensure_not_canceled(cancellation)?;
    let mut command = Command::new(ffprobe_bin);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start ffprobe at {}", ffprobe_bin.display()))?;
    let stdout = child
        .stdout
        .take()
        .context("failed to capture ffprobe stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture ffprobe stderr")?;
    let stdout_handle = tokio::spawn(async move { drain_bounded_output(stdout).await });
    let stderr_handle = tokio::spawn(async move { drain_bounded_output(stderr).await });

    let status = tokio::select! {
        status = child.wait() => status.context("failed while waiting for ffprobe")?,
        _ = cancellation.cancelled() => {
            terminate_child(&mut child).await;
            let _ = stdout_handle.await;
            let _ = stderr_handle.await;
            return canceled();
        }
    };
    let stdout = stdout_handle
        .await
        .context("failed to join ffprobe stdout reader")?
        .context("failed while reading ffprobe stdout")?;
    let stderr = stderr_handle
        .await
        .context("failed to join ffprobe stderr reader")?
        .context("failed while reading ffprobe stderr")?;
    if status.success() {
        return Ok(stdout);
    }
    anyhow::bail!(
        "ffprobe exited with {status}: {}",
        String::from_utf8_lossy(&stderr).trim()
    )
}

async fn run_ffmpeg(
    ffmpeg_bin: &Path,
    args: &[OsString],
    cancellation: &CancellationToken,
) -> anyhow::Result<()> {
    ensure_not_canceled(cancellation)?;
    let mut command = Command::new(ffmpeg_bin);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start ffmpeg at {}", ffmpeg_bin.display()))?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture ffmpeg stderr")?;
    let stderr_handle = tokio::spawn(async move { drain_bounded_output(stderr).await });

    let status = tokio::select! {
        status = child.wait() => status.context("failed while waiting for ffmpeg")?,
        _ = cancellation.cancelled() => {
            terminate_child(&mut child).await;
            let _ = stderr_handle.await;
            return canceled();
        }
    };
    let stderr_tail = stderr_handle
        .await
        .context("failed to join ffmpeg stderr reader")?
        .context("failed while reading ffmpeg stderr")?;
    if status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&stderr_tail);
    anyhow::bail!("ffmpeg exited with {status}: {}", stderr.trim());
}

async fn terminate_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: kill only sends SIGTERM to the child PID returned by tokio.
        let _ = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
    #[cfg(not(unix))]
    let _ = child.start_kill();

    if timeout(FFMPEG_TERMINATE_GRACE, child.wait()).await.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

async fn drain_bounded_output(mut stderr: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut tail = Vec::with_capacity(FFMPEG_STDERR_TAIL_LIMIT);
    let mut chunk = [0_u8; 8192];
    loop {
        let read = stderr.read(&mut chunk).await?;
        if read == 0 {
            return Ok(tail);
        }
        if read >= FFMPEG_STDERR_TAIL_LIMIT {
            tail.clear();
            tail.extend_from_slice(&chunk[read - FFMPEG_STDERR_TAIL_LIMIT..read]);
            continue;
        }
        let overflow = tail
            .len()
            .saturating_add(read)
            .saturating_sub(FFMPEG_STDERR_TAIL_LIMIT);
        if overflow > 0 {
            tail.drain(..overflow);
        }
        tail.extend_from_slice(&chunk[..read]);
    }
}

fn ensure_not_canceled(cancellation: &CancellationToken) -> anyhow::Result<()> {
    if cancellation.is_cancelled() {
        return canceled();
    }
    Ok(())
}

fn canceled<T>() -> anyhow::Result<T> {
    anyhow::bail!("prefetch canceled")
}

fn temporary_file_path(final_path: &Path, label: &str) -> PathBuf {
    let file_name = final_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("source");
    final_path.with_file_name(format!(".{file_name}.{label}.{}.part", Uuid::now_v7()))
}
