//! 运行时输入与路径工具：负责输入 URL、组播地址、尝试目录和命令行展示文本的构造。

use std::{ffi::CStr, net::Ipv4Addr, path::PathBuf, ptr};

use media_domain::{InputKind, InputSpec, normalize_relative_file_input_path};
use uuid::Uuid;

use crate::{config::AgentSettings, runtime::ExecutorError};

pub(crate) fn build_input_url(
    settings: &AgentSettings,
    input: &InputSpec,
) -> Result<String, ExecutorError> {
    match input.kind {
        Some(InputKind::File) => {
            let raw_value = input.url.as_deref().ok_or_else(|| {
                ExecutorError::InvalidRequest("input.url must be provided".to_string())
            })?;
            let normalized = normalize_relative_file_input_path(raw_value)
                .map_err(|message| ExecutorError::InvalidRequest(format!("input.url {message}")))?;
            Ok(PathBuf::from(&settings.work_root)
                .join(normalized)
                .to_string_lossy()
                .to_string())
        }
        Some(
            InputKind::Rtsp
            | InputKind::Rtmp
            | InputKind::Hls
            | InputKind::Ftp
            | InputKind::HttpMp4
            | InputKind::HttpFlv
            | InputKind::HttpTs,
        ) => input
            .url
            .clone()
            .ok_or_else(|| ExecutorError::InvalidRequest("input.url must be provided".to_string())),
        Some(InputKind::UdpMpegtsMulticast | InputKind::RtpMulticast) => build_multicast_url(
            input.kind.expect("kind checked"),
            input.group.as_deref(),
            input.port,
            resolve_interface_binding_ip(
                input.interface_name.as_deref(),
                input.interface_ip.as_deref(),
                Some(settings.multicast_interface_name.as_str()),
                Some(settings.multicast_interface_ip.as_str()),
                "input",
                true,
            )?
            .as_deref(),
            input.ttl,
            input.reuse,
            input.pkt_size,
            input.dscp,
            input.buffer_size,
            input.fifo_size,
            true,
            "input",
        ),
        Some(InputKind::GbRtp) | None => Err(ExecutorError::InvalidRequest(
            "managed executor requires a supported input kind".to_string(),
        )),
    }
}

pub(crate) fn resolve_interface_binding_ip(
    explicit_name: Option<&str>,
    explicit_ip: Option<&str>,
    default_name: Option<&str>,
    default_ip: Option<&str>,
    field_prefix: &str,
    required: bool,
) -> Result<Option<String>, ExecutorError> {
    if let Some(ip) = nonempty(explicit_ip) {
        return Ok(Some(ip.to_string()));
    }
    if let Some(name) = nonempty(explicit_name) {
        let ip = resolve_interface_name_to_ipv4(name).ok_or_else(|| {
            ExecutorError::InvalidRequest(format!(
                "{field_prefix}.interface_name refers to an unknown interface or one without IPv4: {name}"
            ))
        })?;
        return Ok(Some(ip));
    }
    if let Some(name) = nonempty(default_name) {
        if let Some(ip) = resolve_interface_name_to_ipv4(name) {
            return Ok(Some(ip));
        }
        if let Some(ip) = nonempty(default_ip) {
            return Ok(Some(ip.to_string()));
        }
        return Err(ExecutorError::InvalidRequest(format!(
            "configured default multicast interface has no IPv4 address: {name}"
        )));
    }
    if let Some(ip) = nonempty(default_ip) {
        return Ok(Some(ip.to_string()));
    }
    if required {
        return Err(ExecutorError::InvalidRequest(format!(
            "{field_prefix}.interface_name or a configured default multicast interface must be provided"
        )));
    }
    Ok(None)
}

pub(crate) fn resolve_interface_name_to_ipv4(name: &str) -> Option<String> {
    let target = name.trim();
    if target.is_empty() {
        return None;
    }

    unsafe {
        let mut addrs: *mut libc::ifaddrs = ptr::null_mut();
        if libc::getifaddrs(&mut addrs) != 0 || addrs.is_null() {
            return None;
        }

        let mut current = addrs;
        let mut resolved = None;
        while !current.is_null() {
            let ifa = &*current;
            if !ifa.ifa_name.is_null()
                && !ifa.ifa_addr.is_null()
                && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET
            {
                let if_name = CStr::from_ptr(ifa.ifa_name).to_string_lossy();
                if if_name == target {
                    let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
                    resolved = Some(ip.to_string());
                    break;
                }
            }
            current = ifa.ifa_next;
        }
        libc::freeifaddrs(addrs);
        resolved
    }
}

pub(crate) fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_multicast_url(
    kind: InputKind,
    group: Option<&str>,
    port: Option<u16>,
    interface_ip: Option<&str>,
    ttl: Option<u8>,
    reuse: Option<bool>,
    pkt_size: Option<u16>,
    dscp: Option<u8>,
    buffer_size: Option<u32>,
    fifo_size: Option<u32>,
    require_interface_ip: bool,
    field_prefix: &str,
) -> Result<String, ExecutorError> {
    let group = required_nonempty(&format!("{field_prefix}.group"), group)?;
    let port = port.ok_or_else(|| {
        ExecutorError::InvalidRequest(format!("{field_prefix}.port must be provided"))
    })?;
    let scheme = match kind {
        InputKind::UdpMpegtsMulticast => "udp",
        InputKind::RtpMulticast => "rtp",
        _ => {
            return Err(ExecutorError::InvalidRequest(format!(
                "{field_prefix}.kind must be a multicast kind"
            )));
        }
    };

    let mut query = Vec::new();
    match interface_ip
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(interface_ip) => query.push(format!("localaddr={interface_ip}")),
        None if require_interface_ip => {
            return Err(ExecutorError::InvalidRequest(format!(
                "{field_prefix}.interface_ip must be provided"
            )));
        }
        None => {}
    }
    if let Some(reuse) = reuse {
        query.push(format!("reuse={}", if reuse { 1 } else { 0 }));
    }
    if let Some(ttl) = ttl {
        query.push(format!("ttl={ttl}"));
    }
    if let Some(pkt_size) = pkt_size {
        query.push(format!("pkt_size={pkt_size}"));
    }
    if let Some(dscp) = dscp {
        query.push(format!("dscp={dscp}"));
    }
    if let Some(buffer_size) = buffer_size {
        query.push(format!("buffer_size={buffer_size}"));
    }
    if let Some(fifo_size) = fifo_size {
        query.push(format!("fifo_size={fifo_size}"));
    }

    if query.is_empty() {
        Ok(format!("{scheme}://{group}:{port}"))
    } else {
        Ok(format!("{scheme}://{group}:{port}?{}", query.join("&")))
    }
}

pub(crate) fn required_nonempty(field: &str, value: Option<&str>) -> Result<String, ExecutorError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ExecutorError::InvalidRequest(format!("{field} must be provided")))
}

pub(crate) fn attempt_work_dir(
    settings: &AgentSettings,
    task_id: Uuid,
    attempt_no: i32,
) -> PathBuf {
    PathBuf::from(&settings.work_root)
        .join(task_id.to_string())
        .join(format!("attempt-{attempt_no}"))
}

pub(crate) fn bool_as_flag(value: bool) -> String {
    if value { "1" } else { "0" }.to_string()
}

pub(crate) fn input_timeout_seconds(timeout_ms: Option<u64>) -> u64 {
    timeout_ms
        .map(|value| value / 1000)
        .filter(|value| *value > 0)
        .unwrap_or(15)
}

pub(crate) fn render_command_line(executable: &str, args: &[String]) -> String {
    std::iter::once(executable.to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}
