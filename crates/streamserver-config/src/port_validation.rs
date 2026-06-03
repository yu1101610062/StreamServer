use std::{collections::BTreeMap, collections::BTreeSet, fs};

use anyhow::{Context, bail};

pub(crate) const REQUIRED_PORT_KEYS: &[&str] = &[
    "POSTGRES_PORT",
    "CORE_HTTP_PORT",
    "CORE_GRPC_PORT",
    "AGENT_HTTP_PORT",
    "ZLM_HTTP_PORT",
    "ZLM_RTMP_PORT",
    "ZLM_RTSP_PORT",
];

pub(crate) const OPTIONAL_PORT_KEYS: &[&str] = &[
    "ZLM_HTTPS_PORT",
    "ZLM_RTMPS_PORT",
    "ZLM_RTSPS_PORT",
    "ZLM_RTP_PROXY_PORT",
    "ZLM_RTC_SIGNALING_PORT",
    "ZLM_RTC_SIGNALING_SSL_PORT",
    "ZLM_RTC_ICE_PORT",
    "ZLM_RTC_ICE_TCP_PORT",
    "ZLM_RTC_PORT",
    "ZLM_RTC_TCP_PORT",
    "ZLM_SRT_PORT",
    "ZLM_SHELL_PORT",
    "ZLM_ONVIF_PORT",
];

pub(crate) const PORT_RANGE_KEYS: &[&str] = &["ZLM_RTP_PROXY_PORT_RANGE", "ZLM_RTC_PORT_RANGE"];

pub(crate) fn validate_port_value(
    values: &BTreeMap<String, String>,
    key: &str,
    allow_zero: bool,
) -> anyhow::Result<()> {
    let Some(value) = values.get(key) else {
        return Ok(());
    };
    parse_port_text(key, value, allow_zero)?;
    Ok(())
}

pub(crate) fn validate_port_range_value(
    values: &BTreeMap<String, String>,
    key: &str,
) -> anyhow::Result<()> {
    let Some(value) = values.get(key) else {
        return Ok(());
    };
    parse_port_range_text(key, value)?;
    Ok(())
}

pub(crate) fn parse_port_text(key: &str, value: &str, allow_zero: bool) -> anyhow::Result<u16> {
    let trimmed = value.trim();
    let parsed = trimmed
        .parse::<u32>()
        .with_context(|| format!("{key} 必须是 0-65535 之间的整数"))?;
    if parsed > 65535 {
        bail!("{key} 必须是 0-65535 之间的整数");
    }
    if !allow_zero && parsed == 0 {
        bail!("{key} 不能为 0");
    }
    Ok(parsed as u16)
}

pub(crate) fn parse_port_range_text(key: &str, value: &str) -> anyhow::Result<(u16, u16)> {
    let trimmed = value.trim();
    let Some((start, end)) = trimmed.split_once('-') else {
        bail!("{key} 必须使用 start-end 格式，例如 10000-10100 或 0-0");
    };
    let start = start
        .trim()
        .parse::<u32>()
        .with_context(|| format!("{key} 起始端口必须是整数"))?;
    let end = end
        .trim()
        .parse::<u32>()
        .with_context(|| format!("{key} 结束端口必须是整数"))?;
    if start == 0 && end == 0 {
        return Ok((0, 0));
    }
    if start == 0 || end == 0 || start > 65535 || end > 65535 || start > end {
        bail!("{key} 必须是有效端口范围，例如 10000-10100；0-0 表示关闭");
    }
    Ok((start as u16, end as u16))
}

pub(crate) fn ensure_configured_port_available(
    values: &BTreeMap<String, String>,
    key: &str,
    port: u16,
    label_for_key: impl Fn(&str) -> String,
) -> anyhow::Result<()> {
    for other_key in REQUIRED_PORT_KEYS.iter().chain(OPTIONAL_PORT_KEYS.iter()) {
        let other_key = *other_key;
        if other_key == key {
            continue;
        }
        let Some(value) = values.get(other_key) else {
            continue;
        };
        let allow_zero = OPTIONAL_PORT_KEYS.contains(&other_key);
        let Ok(other_port) = parse_port_text(other_key, value, allow_zero) else {
            continue;
        };
        if other_port != 0 && other_port == port {
            let label = label_for_key(other_key);
            bail!("端口 {port} 与 {label} 重复");
        }
    }

    for other_key in PORT_RANGE_KEYS {
        if *other_key == key {
            continue;
        }
        let Some(value) = values.get(*other_key) else {
            continue;
        };
        let Ok((start, end)) = parse_port_range_text(other_key, value) else {
            continue;
        };
        if (start, end) != (0, 0) && (start..=end).contains(&port) {
            let label = label_for_key(other_key);
            bail!("端口 {port} 落在 {label} 的范围 {start}-{end} 内");
        }
    }

    Ok(())
}

pub(crate) fn ensure_configured_range_available(
    values: &BTreeMap<String, String>,
    key: &str,
    start: u16,
    end: u16,
    label_for_key: impl Fn(&str) -> String,
) -> anyhow::Result<()> {
    for other_key in REQUIRED_PORT_KEYS.iter().chain(OPTIONAL_PORT_KEYS.iter()) {
        let other_key = *other_key;
        let Some(value) = values.get(other_key) else {
            continue;
        };
        let allow_zero = OPTIONAL_PORT_KEYS.contains(&other_key);
        let Ok(port) = parse_port_text(other_key, value, allow_zero) else {
            continue;
        };
        if port != 0 && (start..=end).contains(&port) {
            let label = label_for_key(other_key);
            bail!("端口范围 {start}-{end} 包含已配置的 {label} 端口 {port}");
        }
    }

    for other_key in PORT_RANGE_KEYS {
        if *other_key == key {
            continue;
        }
        let Some(value) = values.get(*other_key) else {
            continue;
        };
        let Ok((other_start, other_end)) = parse_port_range_text(other_key, value) else {
            continue;
        };
        if (other_start, other_end) != (0, 0) && ranges_overlap(start, end, other_start, other_end)
        {
            let label = label_for_key(other_key);
            bail!("端口范围 {start}-{end} 与 {label} 的范围 {other_start}-{other_end} 重叠");
        }
    }

    Ok(())
}

pub(crate) fn ensure_host_port_available(port: u16) -> anyhow::Result<()> {
    if host_port_is_occupied(port) {
        bail!("端口 {port} 已被宿主机占用，请更换端口");
    }
    Ok(())
}

pub(crate) fn ensure_host_port_range_available(start: u16, end: u16) -> anyhow::Result<()> {
    let occupied = occupied_host_ports();
    if let Some(port) = occupied.range(start..=end).next() {
        bail!("端口范围 {start}-{end} 中的 {port} 已被宿主机占用，请更换范围");
    }
    for port in start..=end {
        if tcp_port_is_bound(port) {
            bail!("端口范围 {start}-{end} 中的 {port} 已被宿主机占用，请更换范围");
        }
    }
    Ok(())
}

pub(crate) fn is_port_key(key: &str) -> bool {
    REQUIRED_PORT_KEYS.contains(&key) || OPTIONAL_PORT_KEYS.contains(&key)
}

pub(crate) fn is_port_range_key(key: &str) -> bool {
    PORT_RANGE_KEYS.contains(&key)
}

fn host_port_is_occupied(port: u16) -> bool {
    occupied_host_ports().contains(&port) || tcp_port_is_bound(port)
}

fn occupied_host_ports() -> BTreeSet<u16> {
    let mut ports = BTreeSet::new();
    read_proc_net_ports("/proc/net/tcp", true, &mut ports);
    read_proc_net_ports("/proc/net/tcp6", true, &mut ports);
    read_proc_net_ports("/proc/net/udp", false, &mut ports);
    read_proc_net_ports("/proc/net/udp6", false, &mut ports);
    ports
}

fn read_proc_net_ports(path: &str, tcp: bool, ports: &mut BTreeSet<u16>) {
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };

    for line in contents.lines().skip(1) {
        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() < 4 {
            continue;
        }
        if tcp && parts[3] != "0A" {
            continue;
        }
        let Some((_, port_hex)) = parts[1].rsplit_once(':') else {
            continue;
        };
        let Ok(port) = u16::from_str_radix(port_hex, 16) else {
            continue;
        };
        if port != 0 {
            ports.insert(port);
        }
    }
}

fn tcp_port_is_bound(port: u16) -> bool {
    if port == 0 {
        return false;
    }
    std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
}

fn ranges_overlap(a_start: u16, a_end: u16, b_start: u16, b_end: u16) -> bool {
    a_start <= b_end && b_start <= a_end
}
