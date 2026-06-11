mod port_validation;
mod system_ops;
mod ui_render;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::TryRecvError,
    time::Duration,
};

use anyhow::{Context, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::port_validation::{
    OPTIONAL_PORT_KEYS, PORT_RANGE_KEYS, REQUIRED_PORT_KEYS, ensure_configured_port_available,
    ensure_configured_range_available, ensure_host_port_available,
    ensure_host_port_range_available, is_port_key, is_port_range_key, parse_port_range_text,
    parse_port_text, validate_port_range_value, validate_port_value,
};
use crate::system_ops::{
    RestartTask, UninstallTask, can_run_root_commands, instance_running, native_unit_candidates,
    spawn_restart_task, spawn_uninstall_task, validate_instance_dir_for_delete,
};

const MANAGED_ORDER: &[&str] = &[
    "DEPLOY_MODE",
    "INSTALL_ROLE",
    "INSTANCE_NAME",
    "SYSTEMD_TARGET",
    "SYSTEMD_CORE_UNIT",
    "SYSTEMD_AGENT_UNIT",
    "SYSTEMD_ZLM_UNIT",
    "SYSTEMD_POSTGRES_UNIT",
    "POSTGRES_DB",
    "POSTGRES_USER",
    "POSTGRES_PASSWORD",
    "POSTGRES_PORT",
    "CORE_HTTP_HOST",
    "CORE_HTTP_PORT",
    "CORE_GRPC_HOST",
    "CORE_GRPC_PORT",
    "HOOK_SHARED_SECRET",
    "HOOK_SOURCE_ALLOWLIST",
    "STORAGE_ALLOWLIST",
    "AUTH_MODE",
    "AUTH_ENABLED",
    "JWT_PUBLIC_KEY",
    "AUTH_JWT_PRIVATE_KEY_PATH",
    "AUTH_JWT_PUBLIC_KEY_PATH",
    "AUTH_ACCESS_TOKEN_TTL",
    "AUTH_REFRESH_TOKEN_TTL",
    "NODE_ID",
    "AGENT_NODE_NAME",
    "PUBLIC_HOST",
    "ZLM_API_HOST",
    "AGENT_HTTP_PORT",
    "ZLM_HTTP_PORT",
    "ZLM_HTTPS_PORT",
    "ZLM_RTMP_PORT",
    "ZLM_RTMPS_PORT",
    "ZLM_RTSP_PORT",
    "ZLM_RTSPS_PORT",
    "ZLM_RTP_PROXY_PORT",
    "ZLM_RTP_PROXY_PORT_RANGE",
    "ZLM_RTC_SIGNALING_PORT",
    "ZLM_RTC_SIGNALING_SSL_PORT",
    "ZLM_RTC_ICE_PORT",
    "ZLM_RTC_ICE_TCP_PORT",
    "ZLM_RTC_PORT",
    "ZLM_RTC_TCP_PORT",
    "ZLM_RTC_PORT_RANGE",
    "ZLM_SRT_PORT",
    "ZLM_SHELL_PORT",
    "ZLM_ONVIF_PORT",
    "AGENT_PRIMARY_INTERFACE_NAME",
    "AGENT_PRIMARY_INTERFACE_IP",
    "ZLM_WWW_MOUNT_HOST_DIR",
    "ZLM_OUTPUT_MOUNT_HOST_DIR",
    "ZLM_WWW_HOST_DIR",
    "ZLM_OUTPUT_HOST_DIR",
    "OUTPUT_MOUNT_RELATIVE_PREFIX_MP4",
    "OUTPUT_MOUNT_RELATIVE_PREFIX_HLS",
    "AGENT_MULTICAST_INTERFACE_NAME",
    "AGENT_MULTICAST_INTERFACE_IP",
    "AGENT_NETWORK_MODE",
    "AGENT_ACCELERATION_MODE",
    "AGENT_LABELS",
    "AGENT_MAX_RUNTIME_SLOTS",
    "AGENT_MP4_RECORD_SEGMENT_SEC",
    "AGENT_HLS_RECORD_SEGMENT_SEC",
    "AGENT_ARTIFACT_CLEANUP_ENABLED",
    "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT",
    "AGENT_ARTIFACT_CLEANUP_STRATEGY",
    "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC",
    "WORK_ROOT",
];

#[derive(Debug, Clone, Copy)]
struct ChoiceDef {
    value: &'static str,
    label: &'static str,
    help: &'static str,
}

const INSTALL_ROLE_CHOICES: &[ChoiceDef] = &[
    ChoiceDef {
        value: "control-plane",
        label: "控制面",
        help: "只运行管理后台和数据库，不处理媒体流。",
    },
    ChoiceDef {
        value: "worker-host-cpu",
        label: "CPU 工作节点",
        help: "只运行媒体工作节点和流媒体服务，使用 CPU 处理任务。",
    },
    ChoiceDef {
        value: "worker-host-gpu",
        label: "GPU 工作节点",
        help: "只运行媒体工作节点和流媒体服务，使用显卡处理任务。",
    },
    ChoiceDef {
        value: "all-in-one-host-cpu",
        label: "单机 CPU",
        help: "控制面、数据库和 CPU 工作节点部署在同一台机器。",
    },
    ChoiceDef {
        value: "all-in-one-host-gpu",
        label: "单机 GPU",
        help: "控制面、数据库和 GPU 工作节点部署在同一台机器。",
    },
];

const ACCELERATION_CHOICES: &[ChoiceDef] = &[
    ChoiceDef {
        value: "cpu",
        label: "CPU",
        help: "不依赖 NVIDIA GPU，适合普通 CPU 节点。",
    },
    ChoiceDef {
        value: "gpu",
        label: "GPU",
        help: "需要宿主机 NVIDIA 驱动可用。",
    },
];

const AUTH_MODE_CHOICES: &[ChoiceDef] = &[
    ChoiceDef {
        value: "disabled",
        label: "关闭",
        help: "接口不启用内建用户名密码鉴权，适合内网受控环境。",
    },
    ChoiceDef {
        value: "local_password",
        label: "用户名密码",
        help: "启用内建账号登录；首次安装建议通过安装器初始化管理员。",
    },
];

const BOOL_CHOICES: &[ChoiceDef] = &[
    ChoiceDef {
        value: "true",
        label: "开启",
        help: "启用该功能。",
    },
    ChoiceDef {
        value: "false",
        label: "关闭",
        help: "禁用该功能。",
    },
];

const HLS_SEGMENT_CHOICES: &[ChoiceDef] = &[
    ChoiceDef {
        value: "60",
        label: "60 秒",
        help: "默认值，分片数量更少，适合多数归档录制场景。",
    },
    ChoiceDef {
        value: "30",
        label: "30 秒",
        help: "单个分片更短，适合希望更快看到归档片段的场景。",
    },
];

const CLEANUP_STRATEGY_CHOICES: &[ChoiceDef] = &[
    ChoiceDef {
        value: "delete_oldest_then_reject",
        label: "先删旧产物",
        help: "磁盘到阈值后先清理旧产物，仍不足时拒绝新产物任务。",
    },
    ChoiceDef {
        value: "reject_only",
        label: "只拒绝任务",
        help: "磁盘到阈值后不删除文件，只拒绝或停止相关产物任务。",
    },
];

const FIELD_LABEL_WIDTH: usize = 28;
const CHOICE_VALUE_WIDTH: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Basic,
    Ports,
    Storage,
    Hls,
    Cleanup,
}

impl Page {
    const ALL: [Self; 5] = [
        Self::Basic,
        Self::Ports,
        Self::Storage,
        Self::Hls,
        Self::Cleanup,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Basic => "基础部署",
            Self::Ports => "端口",
            Self::Storage => "存储布局",
            Self::Hls => "录制分段",
            Self::Cleanup => "产物清理",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldScope {
    All,
    Core,
    Agent,
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterfaceTarget {
    Primary,
    Multicast,
}

impl InterfaceTarget {
    fn name_key(self) -> &'static str {
        match self {
            Self::Primary => "AGENT_PRIMARY_INTERFACE_NAME",
            Self::Multicast => "AGENT_MULTICAST_INTERFACE_NAME",
        }
    }

    fn ip_key(self) -> &'static str {
        match self {
            Self::Primary => "AGENT_PRIMARY_INTERFACE_IP",
            Self::Multicast => "AGENT_MULTICAST_INTERFACE_IP",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Primary => "主网卡",
            Self::Multicast => "组播/副网卡",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FieldKind {
    Text,
    Choice(&'static [ChoiceDef]),
    Interface(InterfaceTarget),
    ReadOnly,
}

#[derive(Debug, Clone, Copy)]
struct FieldDef {
    key: &'static str,
    label: &'static str,
    kind: FieldKind,
    scope: FieldScope,
    help: &'static str,
}

fn page_fields(page: Page) -> &'static [FieldDef] {
    match page {
        Page::Basic => &[
            FieldDef {
                key: "INSTALL_ROLE",
                label: "安装角色",
                kind: FieldKind::ReadOnly,
                scope: FieldScope::All,
                help: "这台机器承担的部署角色，由安装器选择。配置模块只展示，不修改。",
            },
            FieldDef {
                key: "INSTANCE_NAME",
                label: "实例名称",
                kind: FieldKind::Text,
                scope: FieldScope::All,
                help: "用于区分同一台机器上的不同 native 实例。同机多套部署时不要重复；只能使用字母、数字、横线 -、下划线 _、点 .、@，并以字母或数字开头。",
            },
            FieldDef {
                key: "POSTGRES_DB",
                label: "数据库名",
                kind: FieldKind::Text,
                scope: FieldScope::Core,
                help: "控制面板使用的数据库名称。普通现场保持默认即可。",
            },
            FieldDef {
                key: "POSTGRES_USER",
                label: "数据库用户",
                kind: FieldKind::Text,
                scope: FieldScope::Core,
                help: "控制面板连接数据库使用的用户名。普通现场保持默认即可。",
            },
            FieldDef {
                key: "CORE_HTTP_HOST",
                label: "控制面板 HTTP 地址",
                kind: FieldKind::Text,
                scope: FieldScope::Worker,
                help: "工作节点访问控制面的 HTTP 地址，填写控制面机器可达的 IP 或域名。",
            },
            FieldDef {
                key: "CORE_GRPC_HOST",
                label: "控制面板通信地址",
                kind: FieldKind::Text,
                scope: FieldScope::Worker,
                help: "工作节点连接控制面板的内部通信地址，通常和控制面板 HTTP 地址相同。",
            },
            FieldDef {
                key: "AUTH_MODE",
                label: "控制台鉴权",
                kind: FieldKind::Choice(AUTH_MODE_CHOICES),
                scope: FieldScope::Core,
                help: "是否启用控制面板内建用户名密码登录。安装后启用时，请确认已有管理员账号。",
            },
            FieldDef {
                key: "NODE_ID",
                label: "节点 UUID",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "工作节点的唯一标识。已上线节点不要随意改，否则控制面会认为是新节点。",
            },
            FieldDef {
                key: "AGENT_NODE_NAME",
                label: "节点名称",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "控制台里展示的节点名称，建议写机房、用途或主机名，方便现场识别。",
            },
            FieldDef {
                key: "PUBLIC_HOST",
                label: "对外访问地址",
                kind: FieldKind::ReadOnly,
                scope: FieldScope::Agent,
                help: "跟随主网卡 IP 自动更新，用于播放地址。需要变更时请选择主网卡。",
            },
            FieldDef {
                key: "ZLM_API_HOST",
                label: "流媒体服务 API 地址",
                kind: FieldKind::ReadOnly,
                scope: FieldScope::Agent,
                help: "跟随主网卡 IP 自动更新，用于工作节点访问本机流媒体服务接口。",
            },
            FieldDef {
                key: "AGENT_PRIMARY_INTERFACE_NAME",
                label: "主网卡",
                kind: FieldKind::Interface(InterfaceTarget::Primary),
                scope: FieldScope::Agent,
                help: "普通网络流量优先使用的网卡。选择后会同时写入网卡名、IP、对外访问地址和流媒体服务 API 地址。",
            },
            FieldDef {
                key: "AGENT_MULTICAST_INTERFACE_NAME",
                label: "组播/副网卡",
                kind: FieldKind::Interface(InterfaceTarget::Multicast),
                scope: FieldScope::Agent,
                help: "组播输入/输出默认使用的网卡。现场只有一张网卡时可与主网卡相同。",
            },
            FieldDef {
                key: "AGENT_NETWORK_MODE",
                label: "网络模式",
                kind: FieldKind::ReadOnly,
                scope: FieldScope::Agent,
                help: "当前离线部署固定使用 host 网络，直接使用宿主机端口和网卡。配置模块只展示，不修改。",
            },
            FieldDef {
                key: "AGENT_ACCELERATION_MODE",
                label: "算力模式",
                kind: FieldKind::ReadOnly,
                scope: FieldScope::Agent,
                help: "节点使用 CPU 还是 GPU 承载任务，由安装角色决定。配置模块只展示，不修改。",
            },
            FieldDef {
                key: "AGENT_LABELS",
                label: "额外节点标签",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "固定的 cpu/gpu 算力标签由安装角色决定，不能删除；这里只编辑额外标签，多个值用英文逗号分隔。例如 room-a,edge-1。",
            },
            FieldDef {
                key: "AGENT_MAX_RUNTIME_SLOTS",
                label: "最大同时任务数",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "限制这个节点最多同时跑多少个媒体任务。填 0 表示自动估算，现场不确定容量时建议填 0。",
            },
        ],
        Page::Ports => &[
            FieldDef {
                key: "POSTGRES_PORT",
                label: "数据库端口",
                kind: FieldKind::Text,
                scope: FieldScope::Core,
                help: "数据库在宿主机监听的端口。控制面板需要通过它访问数据库，不能和本机已有服务冲突。",
            },
            FieldDef {
                key: "CORE_HTTP_PORT",
                label: "控制面板网页/API 端口",
                kind: FieldKind::Text,
                scope: FieldScope::All,
                help: "控制台网页和 HTTP API 使用的宿主机端口。浏览器和外部系统访问控制面板时会用到。",
            },
            FieldDef {
                key: "CORE_GRPC_PORT",
                label: "控制面板内部通信端口",
                kind: FieldKind::Text,
                scope: FieldScope::All,
                help: "工作节点连接控制面板使用的内部通信端口。多机部署时工作节点必须能访问该端口。",
            },
            FieldDef {
                key: "AGENT_HTTP_PORT",
                label: "工作节点本地接口端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "工作节点健康检查和本地接口使用的端口。host 网络下不能和本机已有服务冲突。",
            },
            FieldDef {
                key: "ZLM_HTTP_PORT",
                label: "流媒体 HTTP 播放端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "HTTP 播放、截图和流媒体服务接口使用的端口。一般保持 80。",
            },
            FieldDef {
                key: "ZLM_RTMP_PORT",
                label: "RTMP 播放/推流端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "RTMP 播放和推流使用的端口。需要兼容传统 RTMP 客户端时保持开启。",
            },
            FieldDef {
                key: "ZLM_RTSP_PORT",
                label: "RTSP 播放端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "RTSP 播放使用的端口。需要对外提供 RTSP 地址时保持开启。",
            },
            FieldDef {
                key: "ZLM_RTC_PORT",
                label: "WebRTC UDP 媒体端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "WebRTC UDP 媒体传输端口。填 0 表示关闭该固定端口。",
            },
            FieldDef {
                key: "ZLM_HTTPS_PORT",
                label: "HTTPS 播放端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "流媒体 HTTPS 端口。填 0 表示关闭；现场通常通过前置代理统一做 HTTPS。",
            },
            FieldDef {
                key: "ZLM_RTMPS_PORT",
                label: "RTMPS 端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "加密 RTMP 端口。填 0 表示关闭。",
            },
            FieldDef {
                key: "ZLM_RTSPS_PORT",
                label: "RTSPS 端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "加密 RTSP 端口。填 0 表示关闭。",
            },
            FieldDef {
                key: "ZLM_RTP_PROXY_PORT",
                label: "RTP 接收固定端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "RTP/GB28181 等接入使用的固定接收端口。填 0 表示不使用固定端口。",
            },
            FieldDef {
                key: "ZLM_RTP_PROXY_PORT_RANGE",
                label: "RTP 接收端口范围",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "RTP/GB28181 动态接收端口范围，格式为 start-end。填 0-0 表示关闭动态范围。",
            },
            FieldDef {
                key: "ZLM_RTC_SIGNALING_PORT",
                label: "WebRTC 信令端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "WebRTC HTTP 信令端口。填 0 表示关闭。",
            },
            FieldDef {
                key: "ZLM_RTC_SIGNALING_SSL_PORT",
                label: "WebRTC 加密信令端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "WebRTC HTTPS 信令端口。填 0 表示关闭。",
            },
            FieldDef {
                key: "ZLM_RTC_ICE_PORT",
                label: "STUN/TURN UDP 端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "WebRTC 穿透使用的 UDP 端口。填 0 表示关闭。",
            },
            FieldDef {
                key: "ZLM_RTC_ICE_TCP_PORT",
                label: "STUN/TURN TCP 端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "WebRTC 穿透使用的 TCP 端口。填 0 表示关闭。",
            },
            FieldDef {
                key: "ZLM_RTC_TCP_PORT",
                label: "WebRTC TCP 媒体端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "WebRTC TCP 媒体传输端口。填 0 表示关闭。",
            },
            FieldDef {
                key: "ZLM_RTC_PORT_RANGE",
                label: "WebRTC 媒体端口范围",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "WebRTC 动态媒体端口范围，格式为 start-end。填 0-0 表示关闭动态范围。",
            },
            FieldDef {
                key: "ZLM_SRT_PORT",
                label: "SRT 端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "SRT 协议输入/输出端口。填 0 表示关闭。",
            },
            FieldDef {
                key: "ZLM_SHELL_PORT",
                label: "流媒体 Shell 端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "流媒体服务调试 Shell 端口。现场一般保持 0 关闭。",
            },
            FieldDef {
                key: "ZLM_ONVIF_PORT",
                label: "ONVIF 端口",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "ONVIF 发现/控制相关端口。填 0 表示关闭。",
            },
        ],
        Page::Storage => &[
            FieldDef {
                key: "ZLM_WWW_MOUNT_HOST_DIR",
                label: "在线播放宿主机路径",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "服务挂载源目录，只影响宿主机侧路径。用于在线播放临时文件和截图，建议放本机磁盘，不建议挂网络存储。",
            },
            FieldDef {
                key: "ZLM_OUTPUT_MOUNT_HOST_DIR",
                label: "录制产物宿主机路径",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "服务挂载源目录，只影响宿主机侧路径。用于 MP4/HLS 录制、转码和桥接文件产物，可挂载网络存储。",
            },
        ],
        Page::Hls => &[
            FieldDef {
                key: "AGENT_MP4_RECORD_SEGMENT_SEC",
                label: "录制 MP4 分段",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "任务接口未显式传 record.segment_sec 时使用。默认 7200 秒。",
            },
            FieldDef {
                key: "AGENT_HLS_RECORD_SEGMENT_SEC",
                label: "录制 HLS 分片",
                kind: FieldKind::Choice(HLS_SEGMENT_CHOICES),
                scope: FieldScope::Agent,
                help: "只影响录制归档 HLS，不影响在线低延迟播放 HLS。任务接口显式传值时优先。",
            },
        ],
        Page::Cleanup => &[
            FieldDef {
                key: "AGENT_ARTIFACT_CLEANUP_ENABLED",
                label: "启用产物清理",
                kind: FieldKind::Choice(BOOL_CHOICES),
                scope: FieldScope::Agent,
                help: "开启后会按阈值保护产物盘，避免磁盘被录制或转码产物写满。",
            },
            FieldDef {
                key: "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT",
                label: "磁盘阈值百分比",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "产物盘使用率达到该百分比后触发保护动作。建议 80 到 90 之间。",
            },
            FieldDef {
                key: "AGENT_ARTIFACT_CLEANUP_STRATEGY",
                label: "清理策略",
                kind: FieldKind::Choice(CLEANUP_STRATEGY_CHOICES),
                scope: FieldScope::Agent,
                help: "选择磁盘到阈值后的处理方式。现场不希望程序删文件时选择 reject_only。",
            },
            FieldDef {
                key: "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC",
                label: "检查间隔秒数",
                kind: FieldKind::Text,
                scope: FieldScope::Agent,
                help: "后台检查产物盘空间的间隔。默认 30 秒，通常不需要调整。",
            },
        ],
    }
}

#[derive(Debug)]
struct Args {
    env_path: PathBuf,
    non_interactive: bool,
    no_restart_prompt: bool,
}

#[derive(Debug, Clone)]
struct NetworkInterface {
    name: String,
    ip: String,
}

#[derive(Debug, Clone)]
enum Picker {
    Choice {
        key: &'static str,
        choices: &'static [ChoiceDef],
        selected: usize,
    },
    Interface {
        target: InterfaceTarget,
        selected: usize,
    },
}

#[derive(Debug)]
struct ConfigApp {
    env_path: PathBuf,
    values: BTreeMap<String, String>,
    running_baseline_values: BTreeMap<String, String>,
    interfaces: Vec<NetworkInterface>,
    page: Page,
    selected: usize,
    editing: Option<String>,
    picker: Option<Picker>,
    restart_confirm_unit: Option<String>,
    restart_task: Option<RestartTask>,
    uninstall_confirm: Option<UninstallConfirm>,
    uninstall_task: Option<UninstallTask>,
    restart_prompt_enabled: bool,
    message: String,
    storage_confirmed: bool,
}

#[derive(Debug)]
struct UninstallConfirm {
    input: String,
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let mut app = ConfigApp::load(args.env_path)?;
    app.restart_prompt_enabled = !args.no_restart_prompt;

    if args.non_interactive || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        app.apply_env_overrides();
        app.save()?;
        println!("wrote {}", app.env_path.display());
        return Ok(());
    }

    run_tui(app)
}

fn parse_args() -> anyhow::Result<Args> {
    let mut env_path = None;
    let mut non_interactive = false;
    let mut no_restart_prompt = false;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--env" => {
                let Some(value) = args.next() else {
                    bail!("--env requires a path");
                };
                env_path = Some(PathBuf::from(value));
            }
            "--non-interactive" => non_interactive = true,
            "--no-restart-prompt" => no_restart_prompt = true,
            "-h" | "--help" => {
                println!(
                    "usage: streamserver-config [--env PATH] [--non-interactive] [--no-restart-prompt]\n\n默认读取当前二进制所在目录上一层的 .env，例如 deploy/bin/streamserver-config 会读取 deploy/.env。"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
    }

    Ok(Args {
        env_path: env_path.unwrap_or_else(default_env_path),
        non_interactive,
        no_restart_prompt,
    })
}

fn default_env_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent()
                .and_then(|dir| dir.parent())
                .map(|dir| dir.join(".env"))
        })
        .unwrap_or_else(|| PathBuf::from(".env"))
}

impl ConfigApp {
    fn load(env_path: PathBuf) -> anyhow::Result<Self> {
        let mut values = if env_path.exists() {
            parse_env_file(&env_path)?
        } else {
            BTreeMap::new()
        };
        let mut interfaces = discover_interfaces();
        add_existing_interfaces(&mut interfaces, &values);
        apply_defaults(&mut values, &interfaces);
        normalize_storage_mount_host_dirs(&env_path, &mut values);
        let running_baseline_values = if instance_running(&values) {
            values.clone()
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            env_path,
            values,
            running_baseline_values,
            interfaces,
            page: Page::Basic,
            selected: 0,
            editing: None,
            picker: None,
            restart_confirm_unit: None,
            restart_task: None,
            uninstall_confirm: None,
            uninstall_task: None,
            restart_prompt_enabled: true,
            message: "Enter 选择/编辑，Tab 切换页面，S 保存，Q 退出".to_string(),
            storage_confirmed: false,
        })
    }

    fn current_fields(&self) -> Vec<FieldDef> {
        page_fields(self.page)
            .iter()
            .copied()
            .filter(|field| self.field_visible(*field))
            .collect()
    }

    fn current_field(&self) -> Option<FieldDef> {
        let fields = self.current_fields();
        fields
            .get(self.selected.min(fields.len().saturating_sub(1)))
            .copied()
    }

    fn field_visible(&self, field: FieldDef) -> bool {
        match field.scope {
            FieldScope::All => true,
            FieldScope::Core => values_have_core(&self.values),
            FieldScope::Agent => values_have_agent(&self.values),
            FieldScope::Worker => values_have_worker(&self.values),
        }
    }

    fn value(&self, key: &str) -> String {
        self.values.get(key).cloned().unwrap_or_default()
    }

    fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        // 安装角色和 CPU/GPU 基线由安装包决定，TUI 只允许修改附加配置。
        if matches!(key, "INSTALL_ROLE" | "AGENT_ACCELERATION_MODE") {
            self.message = "该项由安装器决定，配置模块只展示不修改".to_string();
            return;
        }
        if is_hidden_fixed_key(key) {
            self.message = "该项为内部固定项，配置模块不修改".to_string();
            return;
        }

        if key == "AUTH_MODE" {
            // UI 暴露的是鉴权模式；底层服务仍需要兼容 AUTH_ENABLED 这个布尔开关。
            let enabled = if value == "local_password" {
                "true"
            } else {
                "false"
            };
            self.values.insert(key.to_string(), value);
            self.values
                .insert("AUTH_ENABLED".to_string(), enabled.to_string());
        } else if key == "AGENT_LABELS" {
            self.values.insert(
                key.to_string(),
                agent_labels_from_extra(&self.values, &value),
            );
        } else {
            self.values.insert(key.to_string(), value);
        }
        self.apply_defaults();
        self.clamp_selection();
    }

    fn save(&mut self) -> anyhow::Result<()> {
        self.apply_defaults();
        validate_values(&self.values)?;
        write_env_file(&self.env_path, &self.values)?;
        self.message = format!("已写入 {}", self.env_path.display());
        Ok(())
    }

    fn apply_env_overrides(&mut self) {
        // 非交互场景允许用环境变量覆盖 .env，但跳过安装器固定的内部字段。
        for key in MANAGED_ORDER {
            if matches!(*key, "INSTALL_ROLE" | "AGENT_ACCELERATION_MODE")
                || is_hidden_fixed_key(key)
            {
                continue;
            }
            if let Ok(value) = std::env::var(key) {
                self.set(key, value.trim().to_string());
            }
        }
        self.apply_defaults();
    }

    fn apply_defaults(&mut self) {
        apply_defaults(&mut self.values, &self.interfaces);
        normalize_storage_mount_host_dirs(&self.env_path, &mut self.values);
    }

    fn next_page(&mut self) {
        let index = Page::ALL
            .iter()
            .position(|page| *page == self.page)
            .unwrap_or(0);
        self.page = Page::ALL[(index + 1) % Page::ALL.len()];
        self.selected = 0;
        self.editing = None;
        self.picker = None;
    }

    fn prev_page(&mut self) {
        let index = Page::ALL
            .iter()
            .position(|page| *page == self.page)
            .unwrap_or(0);
        self.page = Page::ALL[(index + Page::ALL.len() - 1) % Page::ALL.len()];
        self.selected = 0;
        self.editing = None;
        self.picker = None;
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.current_fields().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = (self.selected as isize + delta).rem_euclid(len as isize);
        self.selected = next as usize;
        self.editing = None;
        self.picker = None;
    }

    fn clamp_selection(&mut self) {
        let len = self.current_fields().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn begin_or_pick(&mut self) {
        let Some(field) = self.current_field() else {
            self.message = "当前角色不需要配置这个页面".to_string();
            return;
        };

        match field.kind {
            FieldKind::Text => {
                let edit_value = if field.key == "AGENT_LABELS" {
                    extra_agent_labels(&self.values)
                } else {
                    self.value(field.key)
                };
                self.editing = Some(edit_value);
                self.picker = None;
                self.message = "编辑中：Enter 确认，Esc 取消".to_string();
            }
            FieldKind::Choice(choices) => {
                let current = self.value(field.key);
                let selected = choices
                    .iter()
                    .position(|choice| choice.value == current)
                    .unwrap_or(0);
                self.picker = Some(Picker::Choice {
                    key: field.key,
                    choices,
                    selected,
                });
                self.message = "选择枚举值：↑/↓ 移动，Enter 确认，Esc 取消".to_string();
            }
            FieldKind::Interface(target) => {
                if self.interfaces.is_empty() {
                    self.message = "未检测到可用 IPv4 网卡；请确认宿主机 ip 命令可用".to_string();
                    return;
                }
                let current = self.value(target.name_key());
                let selected = self
                    .interfaces
                    .iter()
                    .position(|interface| interface.name == current)
                    .unwrap_or(0);
                self.picker = Some(Picker::Interface { target, selected });
                self.message = "选择网卡：↑/↓ 移动，Enter 确认，Esc 取消".to_string();
            }
            FieldKind::ReadOnly => {
                self.message = field.help.to_string();
            }
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> bool {
        let Some(mut picker) = self.picker.take() else {
            return false;
        };

        match (&mut picker, key.code) {
            (_, KeyCode::Esc) => {
                self.message = "已取消选择".to_string();
                return true;
            }
            (
                Picker::Choice {
                    choices, selected, ..
                },
                KeyCode::Up | KeyCode::Char('k'),
            ) => {
                if !choices.is_empty() {
                    *selected = (*selected + choices.len() - 1) % choices.len();
                }
            }
            (
                Picker::Choice {
                    choices, selected, ..
                },
                KeyCode::Down | KeyCode::Char('j'),
            ) => {
                if !choices.is_empty() {
                    *selected = (*selected + 1) % choices.len();
                }
            }
            (Picker::Interface { selected, .. }, KeyCode::Up | KeyCode::Char('k')) => {
                if !self.interfaces.is_empty() {
                    *selected = (*selected + self.interfaces.len() - 1) % self.interfaces.len();
                }
            }
            (Picker::Interface { selected, .. }, KeyCode::Down | KeyCode::Char('j')) => {
                if !self.interfaces.is_empty() {
                    *selected = (*selected + 1) % self.interfaces.len();
                }
            }
            (_, KeyCode::Enter | KeyCode::Char(' ')) => {
                match picker {
                    Picker::Choice {
                        key,
                        choices,
                        selected,
                    } => {
                        if let Some(choice) = choices.get(selected) {
                            self.set(key, choice.value);
                            self.message = format!("已选择 {}，按 S 保存到 .env", choice.value);
                        }
                    }
                    Picker::Interface { target, selected } => {
                        if let Some(interface) = self.interfaces.get(selected).cloned() {
                            self.values
                                .insert(target.name_key().to_string(), interface.name.clone());
                            self.values
                                .insert(target.ip_key().to_string(), interface.ip.clone());
                            if target == InterfaceTarget::Primary {
                                sync_primary_interface_followers(&mut self.values);
                            }
                            self.message = format!(
                                "已选择 {}: {} ({})，按 S 保存到 .env",
                                target.title(),
                                interface.name,
                                interface.ip
                            );
                        }
                    }
                }
                return true;
            }
            _ => {}
        }

        self.picker = Some(picker);
        true
    }

    fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        if self.restart_task.is_some() || self.uninstall_task.is_some() {
            return Ok(false);
        }

        if self.uninstall_confirm.is_some() {
            return self.handle_uninstall_confirm_key(key);
        }

        if self.restart_confirm_unit.is_some() {
            return self.handle_restart_confirm_key(key);
        }

        if self.picker.is_some() && self.handle_picker_key(key) {
            return Ok(false);
        }

        if let Some(mut buffer) = self.editing.take() {
            match key.code {
                KeyCode::Esc => {
                    self.message = "已取消编辑".to_string();
                }
                KeyCode::Enter => {
                    let Some(field) = self.current_field() else {
                        self.message = "当前没有可编辑字段".to_string();
                        return Ok(false);
                    };
                    if buffer.trim().is_empty() && required_text_field(field.key) {
                        self.editing = Some(buffer);
                        self.message = "该项不能为空；Esc 可取消编辑".to_string();
                        return Ok(false);
                    }
                    if let Err(error) = validate_text_edit(field.key, &buffer) {
                        self.editing = Some(buffer);
                        self.message = format!("{error}；Esc 放弃修改");
                        return Ok(false);
                    }
                    if let Err(error) = self.validate_port_edit(field.key, &buffer) {
                        self.editing = Some(buffer);
                        self.message = format!("{error}；Esc 放弃修改");
                        return Ok(false);
                    }
                    self.set(field.key, buffer);
                    self.message = "已更新，按 S 保存到 .env".to_string();
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    self.editing = Some(buffer);
                }
                KeyCode::Char(char_value) => {
                    if !key.modifiers.contains(KeyModifiers::CONTROL) {
                        buffer.push(char_value);
                    }
                    self.editing = Some(buffer);
                }
                _ => {
                    self.editing = Some(buffer);
                }
            }
            return Ok(false);
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                if self.begin_restart_confirm() {
                    return Ok(false);
                }
                return Ok(true);
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                if values_have_agent(&self.values) && !self.storage_confirmed {
                    self.page = Page::Storage;
                    self.selected = 1.min(self.current_fields().len().saturating_sub(1));
                    self.message =
                        "请确认录制产物目录已按需完成挂载；按 M 标记确认后再保存".to_string();
                    return Ok(false);
                }
                self.save()?;
            }
            KeyCode::Char('d') | KeyCode::Char('D') => self.begin_uninstall_confirm(),
            KeyCode::Char('m') | KeyCode::Char('M') if self.page == Page::Storage => {
                self.storage_confirmed = !self.storage_confirmed;
                self.message = if self.storage_confirmed {
                    "已标记：录制产物目录挂载状态已确认".to_string()
                } else {
                    "已取消挂载确认".to_string()
                };
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Right | KeyCode::Tab => self.next_page(),
            KeyCode::Left | KeyCode::BackTab => self.prev_page(),
            KeyCode::Enter | KeyCode::Char(' ') => self.begin_or_pick(),
            _ => {}
        }
        Ok(false)
    }

    fn begin_uninstall_confirm(&mut self) {
        self.editing = None;
        self.picker = None;
        self.restart_confirm_unit = None;
        self.uninstall_confirm = Some(UninstallConfirm {
            input: String::new(),
        });
        self.message = "危险操作：输入 DELETE 并按 Enter 彻底卸载当前实例，Esc 取消".to_string();
    }

    fn handle_uninstall_confirm_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        let Some(mut confirm) = self.uninstall_confirm.take() else {
            return Ok(false);
        };

        match key.code {
            KeyCode::Esc => {
                self.message = "已取消卸载".to_string();
                return Ok(false);
            }
            KeyCode::Enter => {
                if confirm.input != "DELETE" {
                    self.uninstall_confirm = Some(confirm);
                    self.message = "确认文本不匹配；请输入 DELETE 后按 Enter，Esc 取消".to_string();
                    return Ok(false);
                }
                if !can_run_root_commands() {
                    self.uninstall_confirm = Some(confirm);
                    self.message = "彻底卸载需要 root 或免密 sudo；Esc 取消".to_string();
                    return Ok(false);
                }
                let install_dir = self.install_dir();
                validate_instance_dir_for_delete(&install_dir)?;
                self.uninstall_task = Some(spawn_uninstall_task(install_dir.clone()));
                self.message = format!(
                    "卸载中，正在停止服务并删除实例目录：{}",
                    install_dir.display()
                );
            }
            KeyCode::Backspace => {
                confirm.input.pop();
                self.uninstall_confirm = Some(confirm);
            }
            KeyCode::Char(char_value) => {
                if !key.modifiers.contains(KeyModifiers::CONTROL) {
                    confirm.input.push(char_value);
                }
                self.uninstall_confirm = Some(confirm);
            }
            _ => {
                self.uninstall_confirm = Some(confirm);
            }
        }

        Ok(false)
    }

    fn install_dir(&self) -> PathBuf {
        self.env_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    }

    fn validate_port_edit(&self, key: &str, value: &str) -> anyhow::Result<()> {
        if is_port_key(key) {
            let allow_zero = OPTIONAL_PORT_KEYS.contains(&key);
            let port = parse_port_text(key, value, allow_zero)?;
            let current = self.value(key);
            let unchanged = parse_port_text(key, &current, allow_zero)
                .is_ok_and(|current_port| current_port == port);
            if port == 0 || unchanged {
                return Ok(());
            }
            ensure_configured_port_available(&self.values, key, port, field_label_for_key)?;
            if self.matches_running_baseline_port(key, port, allow_zero) {
                return Ok(());
            }
            ensure_host_port_available(port)?;
        } else if is_port_range_key(key) {
            let (start, end) = parse_port_range_text(key, value)?;
            let current = self.value(key);
            let unchanged = parse_port_range_text(key, &current)
                .is_ok_and(|current_range| current_range == (start, end));
            if (start, end) == (0, 0) || unchanged {
                return Ok(());
            }
            ensure_configured_range_available(&self.values, key, start, end, field_label_for_key)?;
            if self.matches_running_baseline_range(key, start, end) {
                return Ok(());
            }
            ensure_host_port_range_available(start, end)?;
        }
        Ok(())
    }

    fn matches_running_baseline_port(&self, key: &str, port: u16, allow_zero: bool) -> bool {
        self.running_baseline_values
            .get(key)
            .and_then(|value| parse_port_text(key, value, allow_zero).ok())
            .is_some_and(|baseline_port| baseline_port == port)
    }

    fn matches_running_baseline_range(&self, key: &str, start: u16, end: u16) -> bool {
        self.running_baseline_values
            .get(key)
            .and_then(|value| parse_port_range_text(key, value).ok())
            .is_some_and(|baseline_range| baseline_range == (start, end))
    }

    fn begin_restart_confirm(&mut self) -> bool {
        if !self.restart_prompt_enabled {
            return false;
        }
        let Some(unit) = self.restart_unit_name() else {
            return false;
        };
        self.restart_confirm_unit = Some(unit.clone());
        self.message = format!("退出前是否重启服务 {unit}？Y 重启并退出，N 直接退出，Esc 取消");
        true
    }

    fn restart_unit_name(&self) -> Option<String> {
        native_unit_candidates(&self.values)
            .into_iter()
            .find(|unit| Path::new("/etc/systemd/system").join(unit).exists())
    }

    fn handle_restart_confirm_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.restart_confirm_unit = None;
                self.message = "已取消退出".to_string();
                Ok(false)
            }
            KeyCode::Char('n') | KeyCode::Char('N') => Ok(true),
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                let Some(unit) = self.restart_confirm_unit.take() else {
                    return Ok(true);
                };
                if !can_run_root_commands() {
                    self.restart_confirm_unit = Some(unit);
                    self.message =
                        "重启服务需要 root 或免密 sudo；N 直接退出，Esc 取消".to_string();
                    return Ok(false);
                }
                self.restart_task = Some(spawn_restart_task(unit.clone()));
                self.message = format!("重启中，正在检查节点启动状态请稍候：{unit}");
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn poll_restart_task(&mut self) -> anyhow::Result<bool> {
        let Some(task) = &self.restart_task else {
            return Ok(false);
        };

        match task.receiver.try_recv() {
            Ok(Ok(())) => {
                let unit = task.unit.clone();
                self.restart_task = None;
                self.message = format!("已重启服务 {unit}");
                Ok(true)
            }
            Ok(Err(error)) => {
                let unit = task.unit.clone();
                self.restart_task = None;
                self.message = format!("重启服务 {unit} 失败：{error}；按 Q 退出");
                Ok(false)
            }
            Err(TryRecvError::Empty) => Ok(false),
            Err(TryRecvError::Disconnected) => {
                let unit = task.unit.clone();
                self.restart_task = None;
                self.message = format!("重启服务 {unit} 失败：后台任务异常结束；按 Q 退出");
                Ok(false)
            }
        }
    }

    fn poll_uninstall_task(&mut self) -> anyhow::Result<bool> {
        let Some(task) = &self.uninstall_task else {
            return Ok(false);
        };

        match task.receiver.try_recv() {
            Ok(Ok(())) => Ok(true),
            Ok(Err(error)) => {
                let install_dir = task.install_dir.clone();
                self.uninstall_task = None;
                self.message = format!(
                    "卸载实例 {} 失败：{error}；按 Q 退出或重试",
                    install_dir.display()
                );
                Ok(false)
            }
            Err(TryRecvError::Empty) => Ok(false),
            Err(TryRecvError::Disconnected) => {
                let install_dir = task.install_dir.clone();
                self.uninstall_task = None;
                self.message = format!(
                    "卸载实例 {} 失败：后台任务异常结束；按 Q 退出或重试",
                    install_dir.display()
                );
                Ok(false)
            }
        }
    }
}

pub(crate) fn parse_env_file(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut values = BTreeMap::new();
    // 这里只支持安装器生成的简单 KEY=VALUE，不尝试实现完整 shell 语法。
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        values.insert(key.trim().to_string(), unquote_env_value(value.trim()));
    }
    Ok(values)
}

fn unquote_env_value(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn write_env_file(path: &Path, values: &BTreeMap<String, String>) -> anyhow::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let managed = MANAGED_ORDER.iter().copied().collect::<BTreeSet<_>>();
    let mut output = String::new();

    // 写回时固定字段顺序，减少配置界面保存后产生的大面积 diff。
    output.push_str("# Generated by streamserver-config. Manual comments are not preserved.\n");
    for key in MANAGED_ORDER {
        if let Some(value) = values.get(*key) {
            if let Some(comment) = env_comment(key) {
                output.push_str("# ");
                output.push_str(comment);
                output.push('\n');
            }
            output.push_str(key);
            output.push('=');
            output.push_str(value);
            output.push('\n');
        }
    }

    for (key, value) in values {
        if managed.contains(key.as_str())
            || is_legacy_allocation_key(key)
            || is_hidden_fixed_key(key)
        {
            continue;
        }
        output.push_str(key);
        output.push('=');
        output.push_str(value);
        output.push('\n');
    }

    fs::write(path, output).with_context(|| format!("failed to write {}", path.display()))
}

fn apply_defaults(values: &mut BTreeMap<String, String>, interfaces: &[NetworkInterface]) {
    // native 迁移后保留旧挂载键的输入兼容，但最终配置只写回当前 native 键。
    values.retain(|key, _| !is_legacy_allocation_key(key));
    let legacy_www_mount_host_dir = values
        .get("ZLM_WWW_MOUNT_HOST_DIR")
        .or_else(|| values.get("ZLM_WWW_HOST_DIR"))
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| "./data/zlm/www".to_string());
    let legacy_output_mount_host_dir = values
        .get("ZLM_OUTPUT_MOUNT_HOST_DIR")
        .or_else(|| values.get("ZLM_OUTPUT_HOST_DIR"))
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| "./data/zlm/www/output".to_string());
    values.retain(|key, _| !is_hidden_fixed_key(key));

    values.insert("DEPLOY_MODE".to_string(), "native".to_string());
    default_if_missing(values, "INSTALL_ROLE", "control-plane");
    let role = values.get("INSTALL_ROLE").cloned().unwrap_or_default();
    if values
        .get("INSTANCE_NAME")
        .is_none_or(|value| value.trim().is_empty())
    {
        if let Some(project) = role_default_project(&role) {
            values.insert("INSTANCE_NAME".to_string(), project.to_string());
        }
    }
    default_native_systemd_units(values);

    if values_have_core(values) {
        default_if_missing(values, "POSTGRES_DB", "streamserver");
        default_if_missing(values, "POSTGRES_USER", "postgres");
        default_if_missing(values, "POSTGRES_PORT", "5432");
        default_if_missing(values, "CORE_HTTP_PORT", "8080");
        default_if_missing(values, "CORE_GRPC_PORT", "50051");
        default_if_missing(values, "HOOK_SOURCE_ALLOWLIST", "");
        default_if_missing(
            values,
            "STORAGE_ALLOWLIST",
            "/data/media/work,/data/zlm/www",
        );
        default_if_missing(values, "AUTH_MODE", "disabled");
        default_if_missing(values, "AUTH_ENABLED", "false");
        default_if_missing(values, "JWT_PUBLIC_KEY", "");
        default_if_missing(values, "AUTH_JWT_PRIVATE_KEY_PATH", "");
        default_if_missing(values, "AUTH_JWT_PUBLIC_KEY_PATH", "");
        default_if_missing(values, "AUTH_ACCESS_TOKEN_TTL", "15m");
        default_if_missing(values, "AUTH_REFRESH_TOKEN_TTL", "7d");
    }

    if values_have_worker(values) {
        let default_host = default_interface(interfaces)
            .map(|interface| interface.ip.clone())
            .unwrap_or_else(|| "127.0.0.1".to_string());
        default_if_missing(values, "CORE_HTTP_HOST", &default_host);
        default_if_missing(values, "CORE_GRPC_HOST", &default_host);
        default_if_missing(values, "CORE_HTTP_PORT", "8080");
        default_if_missing(values, "CORE_GRPC_PORT", "50051");
    }

    if values_have_agent(values) {
        // Agent 默认使用主网卡地址作为对外访问地址和本机 ZLM API 绑定地址。
        default_if_missing(values, "NODE_ID", &generate_uuid());
        default_if_missing(values, "AGENT_NODE_NAME", &default_hostname());
        default_interface_values(values, interfaces);
        let primary_ip = values
            .get("AGENT_PRIMARY_INTERFACE_IP")
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| "127.0.0.1".to_string());
        values.insert("PUBLIC_HOST".to_string(), primary_ip.clone());
        values.insert("ZLM_API_HOST".to_string(), primary_ip);
        default_if_missing(values, "AGENT_HTTP_PORT", "8081");
        default_zlm_ports(values);
        default_if_missing(values, "ZLM_WWW_MOUNT_HOST_DIR", &legacy_www_mount_host_dir);
        default_if_missing(
            values,
            "ZLM_OUTPUT_MOUNT_HOST_DIR",
            &legacy_output_mount_host_dir,
        );
        values.insert("AGENT_NETWORK_MODE".to_string(), "host".to_string());
        let mode = if role.contains("gpu") { "gpu" } else { "cpu" };
        default_if_missing(values, "AGENT_ACCELERATION_MODE", mode);
        default_if_missing(values, "AGENT_LABELS", mode);
        normalize_agent_labels(values);
        default_if_missing(values, "AGENT_MAX_RUNTIME_SLOTS", "0");
        default_if_missing(values, "AGENT_MP4_RECORD_SEGMENT_SEC", "7200");
        default_if_missing(values, "AGENT_HLS_RECORD_SEGMENT_SEC", "60");
        default_if_missing(values, "AGENT_ARTIFACT_CLEANUP_ENABLED", "true");
        default_if_missing(values, "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT", "85");
        default_if_missing(
            values,
            "AGENT_ARTIFACT_CLEANUP_STRATEGY",
            "delete_oldest_then_reject",
        );
        default_if_missing(values, "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC", "30");
        default_if_missing(values, "WORK_ROOT", "/data/media/work");
    }
}

fn default_if_missing(values: &mut BTreeMap<String, String>, key: &str, default_value: &str) {
    if values.get(key).is_none_or(|value| value.trim().is_empty()) {
        values.insert(key.to_string(), default_value.to_string());
    }
}

pub(crate) fn deploy_mode(values: &BTreeMap<String, String>) -> &str {
    values
        .get("DEPLOY_MODE")
        .map(String::as_str)
        .unwrap_or("native")
}

pub(crate) fn native_unit_basename(values: &BTreeMap<String, String>) -> String {
    let instance_name = values
        .get("INSTANCE_NAME")
        .filter(|value| !value.trim().is_empty())
        .map(String::as_str)
        .unwrap_or("streamserver");
    if instance_name.starts_with("ss-") {
        instance_name.to_string()
    } else {
        format!("ss-{instance_name}")
    }
}

fn default_native_systemd_units(values: &mut BTreeMap<String, String>) {
    let unit_base = native_unit_basename(values);
    default_if_missing(values, "SYSTEMD_TARGET", &format!("{unit_base}.target"));
    default_if_missing(
        values,
        "SYSTEMD_CORE_UNIT",
        &format!("{unit_base}-core.service"),
    );
    default_if_missing(
        values,
        "SYSTEMD_AGENT_UNIT",
        &format!("{unit_base}-agent.service"),
    );
    default_if_missing(
        values,
        "SYSTEMD_ZLM_UNIT",
        &format!("{unit_base}-zlm.service"),
    );
    default_if_missing(
        values,
        "SYSTEMD_POSTGRES_UNIT",
        &format!("{unit_base}-postgres.service"),
    );
}

fn sync_primary_interface_followers(values: &mut BTreeMap<String, String>) {
    if let Some(primary_ip) = values
        .get("AGENT_PRIMARY_INTERFACE_IP")
        .filter(|value| !value.trim().is_empty())
        .cloned()
    {
        values.insert("PUBLIC_HOST".to_string(), primary_ip.clone());
        values.insert("ZLM_API_HOST".to_string(), primary_ip);
    }
}

fn normalize_agent_labels(values: &mut BTreeMap<String, String>) {
    if !values_have_agent(values) {
        return;
    }
    // cpu/gpu 是调度硬约束标签，用户只能追加额外标签，不能移除固定标签。
    let normalized = agent_labels_from_extra(values, &extra_agent_labels(values));
    values.insert("AGENT_LABELS".to_string(), normalized);
}

fn agent_labels_from_extra(values: &BTreeMap<String, String>, extra_labels: &str) -> String {
    let Some(fixed_label) = fixed_agent_label(values) else {
        return normalize_csv_labels(extra_labels);
    };

    let mut labels = vec![fixed_label.to_string()];
    for label in split_csv_labels(extra_labels) {
        if is_fixed_agent_label(&label) || labels.iter().any(|existing| existing == &label) {
            continue;
        }
        labels.push(label);
    }
    labels.join(",")
}

fn extra_agent_labels(values: &BTreeMap<String, String>) -> String {
    let labels = values.get("AGENT_LABELS").cloned().unwrap_or_default();
    split_csv_labels(&labels)
        .into_iter()
        .filter(|label| !is_fixed_agent_label(label))
        .collect::<Vec<_>>()
        .join(",")
}

fn fixed_agent_label(values: &BTreeMap<String, String>) -> Option<&'static str> {
    if !values_have_agent(values) {
        return None;
    }
    if values
        .get("AGENT_ACCELERATION_MODE")
        .is_some_and(|mode| mode.trim() == "gpu")
        || values
            .get("INSTALL_ROLE")
            .is_some_and(|role| role.contains("gpu"))
    {
        Some("gpu")
    } else {
        Some("cpu")
    }
}

fn normalize_csv_labels(value: &str) -> String {
    let mut labels = Vec::new();
    for label in split_csv_labels(value) {
        if labels.iter().any(|existing| existing == &label) {
            continue;
        }
        labels.push(label);
    }
    labels.join(",")
}

fn split_csv_labels(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn is_fixed_agent_label(label: &str) -> bool {
    matches!(label, "cpu" | "gpu")
}

fn default_zlm_ports(values: &mut BTreeMap<String, String>) {
    default_if_missing(values, "ZLM_HTTP_PORT", "80");
    default_if_missing(values, "ZLM_HTTPS_PORT", "0");
    default_if_missing(values, "ZLM_RTMP_PORT", "1935");
    default_if_missing(values, "ZLM_RTMPS_PORT", "0");
    default_if_missing(values, "ZLM_RTSP_PORT", "554");
    default_if_missing(values, "ZLM_RTSPS_PORT", "0");
    default_if_missing(values, "ZLM_RTP_PROXY_PORT", "0");
    default_if_missing(values, "ZLM_RTP_PROXY_PORT_RANGE", "0-0");
    default_if_missing(values, "ZLM_RTC_SIGNALING_PORT", "0");
    default_if_missing(values, "ZLM_RTC_SIGNALING_SSL_PORT", "0");
    default_if_missing(values, "ZLM_RTC_ICE_PORT", "0");
    default_if_missing(values, "ZLM_RTC_ICE_TCP_PORT", "0");
    default_if_missing(values, "ZLM_RTC_PORT", "0");
    default_if_missing(values, "ZLM_RTC_TCP_PORT", "0");
    default_if_missing(values, "ZLM_RTC_PORT_RANGE", "0-0");
    default_if_missing(values, "ZLM_SRT_PORT", "0");
    default_if_missing(values, "ZLM_SHELL_PORT", "0");
    default_if_missing(values, "ZLM_ONVIF_PORT", "0");
}

fn default_interface_values(
    values: &mut BTreeMap<String, String>,
    interfaces: &[NetworkInterface],
) {
    if let Some(interface) = default_interface(interfaces) {
        default_if_missing(values, "AGENT_PRIMARY_INTERFACE_NAME", &interface.name);
        default_if_missing(values, "AGENT_PRIMARY_INTERFACE_IP", &interface.ip);
        default_if_missing(values, "AGENT_MULTICAST_INTERFACE_NAME", &interface.name);
        default_if_missing(values, "AGENT_MULTICAST_INTERFACE_IP", &interface.ip);
    }
}

fn default_interface(interfaces: &[NetworkInterface]) -> Option<&NetworkInterface> {
    let route = detect_default_route_interface();
    if let Some(route) = route {
        if let Some(interface) = interfaces
            .iter()
            .find(|interface| interface.name == route.name && interface.ip == route.ip)
        {
            return Some(interface);
        }
        if let Some(interface) = interfaces
            .iter()
            .find(|interface| interface.name == route.name)
        {
            return Some(interface);
        }
    }
    interfaces
        .iter()
        .find(|interface| is_preferred_host_interface(&interface.name))
        .or_else(|| interfaces.first())
}

fn is_preferred_host_interface(name: &str) -> bool {
    !name.starts_with("br-")
        && !name.starts_with("veth")
        && !name.starts_with("virbr")
        && !name.starts_with("cni")
        && name != "lo"
}

fn detect_default_route_interface() -> Option<NetworkInterface> {
    let output = Command::new("ip")
        .args(["route", "get", "1.1.1.1"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parts = stdout.split_whitespace().collect::<Vec<_>>();
    let mut name = None;
    let mut ip = None;
    for pair in parts.windows(2) {
        match pair[0] {
            "dev" => name = Some(normalize_interface_name(pair[1])),
            "src" => ip = Some(pair[1].to_string()),
            _ => {}
        }
    }
    Some(NetworkInterface {
        name: name?,
        ip: ip?,
    })
}

fn discover_interfaces() -> Vec<NetworkInterface> {
    let output = Command::new("ip")
        .args(["-o", "-4", "addr", "show", "scope", "global"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut interfaces = Vec::new();
    for line in stdout.lines() {
        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() < 4 {
            continue;
        }
        let name = normalize_interface_name(parts[1]);
        let Some(inet_index) = parts.iter().position(|part| *part == "inet") else {
            continue;
        };
        let Some(cidr) = parts.get(inet_index + 1) else {
            continue;
        };
        let ip = cidr.split('/').next().unwrap_or_default();
        if name.is_empty() || ip.is_empty() {
            continue;
        }
        push_unique_interface(
            &mut interfaces,
            NetworkInterface {
                name,
                ip: ip.to_string(),
            },
        );
    }
    interfaces
}

fn normalize_interface_name(raw: &str) -> String {
    raw.trim_end_matches(':')
        .split('@')
        .next()
        .unwrap_or(raw)
        .to_string()
}

fn add_existing_interfaces(
    interfaces: &mut Vec<NetworkInterface>,
    values: &BTreeMap<String, String>,
) {
    for target in [InterfaceTarget::Primary, InterfaceTarget::Multicast] {
        let name = values.get(target.name_key()).cloned().unwrap_or_default();
        let ip = values.get(target.ip_key()).cloned().unwrap_or_default();
        if !name.trim().is_empty() && !ip.trim().is_empty() {
            push_unique_interface(interfaces, NetworkInterface { name, ip });
        }
    }
}

fn push_unique_interface(interfaces: &mut Vec<NetworkInterface>, candidate: NetworkInterface) {
    if interfaces
        .iter()
        .any(|interface| interface.name == candidate.name && interface.ip == candidate.ip)
    {
        return;
    }
    interfaces.push(candidate);
}

fn is_legacy_allocation_key(key: &str) -> bool {
    key.starts_with("AGENT_ARTIFACT_CLEANUP_PRE")
        && matches!(
            key.strip_prefix("AGENT_ARTIFACT_CLEANUP_PRE"),
            Some("ALLOCATE_PERCENT" | "ALLOCATE_HEADROOM_PERCENT")
        )
}

fn is_hidden_fixed_key(key: &str) -> bool {
    matches!(
        key,
        "ZLM_WWW_HOST_DIR"
            | "ZLM_OUTPUT_HOST_DIR"
            | "OUTPUT_MOUNT_RELATIVE_PREFIX_MP4"
            | "OUTPUT_MOUNT_RELATIVE_PREFIX_HLS"
    )
}

fn install_role_has_agent(role: Option<&str>) -> bool {
    matches!(
        role.unwrap_or_default(),
        "worker-host-cpu" | "worker-host-gpu" | "all-in-one-host-cpu" | "all-in-one-host-gpu"
    )
}

fn install_role_has_core(role: Option<&str>) -> bool {
    matches!(
        role.unwrap_or_default(),
        "control-plane" | "all-in-one-host-cpu" | "all-in-one-host-gpu"
    )
}

fn install_role_is_worker(role: Option<&str>) -> bool {
    matches!(
        role.unwrap_or_default(),
        "worker-host-cpu" | "worker-host-gpu"
    )
}

fn values_have_agent(values: &BTreeMap<String, String>) -> bool {
    if let Some(role) = values
        .get("INSTALL_ROLE")
        .filter(|role| !role.trim().is_empty())
    {
        return install_role_has_agent(Some(role));
    }
    values.contains_key("AGENT_NODE_NAME")
}

fn values_have_core(values: &BTreeMap<String, String>) -> bool {
    if let Some(role) = values
        .get("INSTALL_ROLE")
        .filter(|role| !role.trim().is_empty())
    {
        return install_role_has_core(Some(role));
    }
    values.contains_key("POSTGRES_DB") || values.contains_key("CORE_HTTP_PORT")
}

fn values_have_worker(values: &BTreeMap<String, String>) -> bool {
    values
        .get("INSTALL_ROLE")
        .is_some_and(|role| install_role_is_worker(Some(role)))
}

fn role_default_project(role: &str) -> Option<&'static str> {
    match role {
        "control-plane" => Some("ss-core"),
        "worker-host-cpu" => Some("ss-worker-cpu"),
        "worker-host-gpu" => Some("ss-worker-gpu"),
        "all-in-one-host-cpu" => Some("ss-aio-cpu"),
        "all-in-one-host-gpu" => Some("ss-aio-gpu"),
        _ => None,
    }
}

fn generate_uuid() -> String {
    if let Ok(output) = Command::new("uuidgen").output() {
        if output.status.success() {
            let value = String::from_utf8_lossy(&output.stdout)
                .trim()
                .to_lowercase();
            if !value.is_empty() {
                return value;
            }
        }
    }
    fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|_| "00000000-0000-0000-0000-000000000000".to_string())
}

fn default_hostname() -> String {
    if let Ok(output) = Command::new("hostname").arg("-s").output() {
        if output.status.success() {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !value.is_empty() {
                return value;
            }
        }
    }
    "node-1".to_string()
}

fn required_text_field(key: &str) -> bool {
    matches!(
        key,
        "INSTALL_ROLE"
            | "INSTANCE_NAME"
            | "POSTGRES_DB"
            | "POSTGRES_USER"
            | "CORE_HTTP_HOST"
            | "CORE_GRPC_HOST"
            | "NODE_ID"
            | "AGENT_NODE_NAME"
            | "PUBLIC_HOST"
            | "ZLM_API_HOST"
            | "ZLM_WWW_MOUNT_HOST_DIR"
            | "ZLM_OUTPUT_MOUNT_HOST_DIR"
            | "AGENT_MAX_RUNTIME_SLOTS"
            | "AGENT_MP4_RECORD_SEGMENT_SEC"
            | "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT"
            | "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC"
    ) || is_port_key(key)
        || is_port_range_key(key)
}

fn validate_values(values: &BTreeMap<String, String>) -> anyhow::Result<()> {
    if deploy_mode(values) != "native" {
        bail!("DEPLOY_MODE must be native");
    }
    validate_instance_name(
        values
            .get("INSTANCE_NAME")
            .map(String::as_str)
            .unwrap_or_default(),
    )?;

    let role = values
        .get("INSTALL_ROLE")
        .map(String::as_str)
        .unwrap_or_default();
    if !choice_contains(INSTALL_ROLE_CHOICES, role) {
        bail!("INSTALL_ROLE must be one of the supported roles");
    }

    if values_have_agent(values) {
        let network_mode = values
            .get("AGENT_NETWORK_MODE")
            .map(String::as_str)
            .unwrap_or_default();
        if network_mode != "host" {
            bail!("AGENT_NETWORK_MODE is fixed to host");
        }
    }

    validate_choice(values, "AUTH_MODE", AUTH_MODE_CHOICES)?;
    validate_choice(values, "AGENT_ACCELERATION_MODE", ACCELERATION_CHOICES)?;
    validate_choice(values, "AGENT_ARTIFACT_CLEANUP_ENABLED", BOOL_CHOICES)?;
    validate_choice(values, "AGENT_HLS_RECORD_SEGMENT_SEC", HLS_SEGMENT_CHOICES)?;
    validate_choice(
        values,
        "AGENT_ARTIFACT_CLEANUP_STRATEGY",
        CLEANUP_STRATEGY_CHOICES,
    )?;

    for key in REQUIRED_PORT_KEYS {
        validate_port_value(values, key, false)?;
    }
    for key in OPTIONAL_PORT_KEYS {
        validate_port_value(values, key, true)?;
    }
    for key in PORT_RANGE_KEYS {
        validate_port_range_value(values, key)?;
    }

    if let Some(value) = values.get("AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT") {
        let parsed = value
            .trim()
            .parse::<f64>()
            .with_context(|| "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT must be numeric")?;
        if !(0.0..=100.0).contains(&parsed) {
            bail!("AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT must be between 0 and 100");
        }
    }

    if let Some(value) = values.get("AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC") {
        let parsed = value
            .trim()
            .parse::<u64>()
            .with_context(|| "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC must be an integer")?;
        if parsed == 0 {
            bail!("AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC must be greater than 0");
        }
    }

    if let Some(value) = values.get("AGENT_MAX_RUNTIME_SLOTS") {
        value
            .trim()
            .parse::<u64>()
            .with_context(|| "AGENT_MAX_RUNTIME_SLOTS must be zero or a positive integer")?;
    }

    if let Some(value) = values.get("AGENT_MP4_RECORD_SEGMENT_SEC") {
        let parsed = value
            .trim()
            .parse::<u64>()
            .with_context(|| "AGENT_MP4_RECORD_SEGMENT_SEC must be an integer")?;
        if parsed == 0 {
            bail!("AGENT_MP4_RECORD_SEGMENT_SEC must be greater than 0");
        }
    }

    Ok(())
}

fn validate_text_edit(key: &str, value: &str) -> anyhow::Result<()> {
    if key == "INSTANCE_NAME" {
        validate_instance_name(value)?;
    }
    Ok(())
}

fn validate_instance_name(value: &str) -> anyhow::Result<()> {
    let value = value.trim();
    if value.is_empty() {
        bail!("实例名称不能为空");
    }
    if value.len() > 63 {
        bail!("实例名称不能超过 63 个字符");
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        bail!("实例名称不能为空");
    };
    if !first.is_ascii_alphanumeric() {
        bail!("实例名称必须以字母或数字开头");
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '@')) {
        bail!("实例名称只能使用字母、数字、横线 -、下划线 _、点 .、@");
    }
    Ok(())
}

fn validate_choice(
    values: &BTreeMap<String, String>,
    key: &str,
    choices: &[ChoiceDef],
) -> anyhow::Result<()> {
    if let Some(value) = values.get(key) {
        if !value.trim().is_empty() && !choice_contains(choices, value.trim()) {
            bail!("{key} has unsupported value: {value}");
        }
    }
    Ok(())
}

fn field_label_for_key(key: &str) -> String {
    Page::ALL
        .iter()
        .flat_map(|page| page_fields(*page))
        .find(|field| field.key == key)
        .map(|field| field.label)
        .unwrap_or(key)
        .to_string()
}

fn choice_contains(choices: &[ChoiceDef], value: &str) -> bool {
    choices.iter().any(|choice| choice.value == value)
}

fn run_tui(mut app: ConfigApp) -> anyhow::Result<()> {
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let _guard = TerminalGuard;

    loop {
        terminal.draw(|frame| ui_render::draw(frame, &app))?;
        if app.poll_uninstall_task()? {
            break;
        }
        if app.poll_restart_task()? {
            break;
        }
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if app.handle_key(key)? {
                    break;
                }
            }
        }
    }

    Ok(())
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn normalize_storage_mount_host_dirs(env_path: &Path, values: &mut BTreeMap<String, String>) {
    for key in ["ZLM_WWW_MOUNT_HOST_DIR", "ZLM_OUTPUT_MOUNT_HOST_DIR"] {
        let Some(value) = values
            .get(key)
            .filter(|value| !value.trim().is_empty())
            .cloned()
        else {
            continue;
        };
        values.insert(
            key.to_string(),
            resolve_host_path(env_path, &value).display().to_string(),
        );
    }
}

fn resolve_host_path(env_path: &Path, configured: &str) -> PathBuf {
    let path = Path::new(configured);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let base = env_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let base = if base.is_absolute() {
        base.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(base)
    };
    if let Some(stripped) = configured.strip_prefix("./") {
        base.join(stripped)
    } else {
        base.join(configured)
    }
}

#[derive(Debug)]
struct DfSample {
    fs_type: String,
    available: String,
    mount: String,
}

fn df_sample(path: &Path) -> Option<DfSample> {
    let target = if path.exists() {
        path.to_path_buf()
    } else {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    };
    let output = Command::new("df").arg("-PTk").arg(target).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().nth(1)?;
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 7 {
        return None;
    }
    Some(DfSample {
        fs_type: parts[1].to_string(),
        available: format_kib(parts[4]),
        mount: parts[6].to_string(),
    })
}

fn format_kib(value: &str) -> String {
    let Ok(kib) = value.parse::<u64>() else {
        return value.to_string();
    };
    let gib = kib as f64 / 1024.0 / 1024.0;
    if gib >= 1.0 {
        format!("{gib:.1}GiB")
    } else {
        format!("{:.1}MiB", kib as f64 / 1024.0)
    }
}

// 配置页展示用说明表，只描述安装器生成的 native .env 字段含义；
// 实际校验逻辑仍由端口检查、实例校验和保存流程负责。
const ENV_COMMENTS: &[(&str, &str)] = &[
    ("DEPLOY_MODE", "部署模式，固定为 native，由 systemd 管理。"),
    (
        "INSTALL_ROLE",
        "这台机器的部署角色，由安装器选择，配置模块只展示。",
    ),
    ("INSTANCE_NAME", "native 实例名，用于生成 systemd 服务名。"),
    ("SYSTEMD_TARGET", "native 聚合 target。"),
    ("SYSTEMD_CORE_UNIT", "native media-core systemd 服务名。"),
    ("SYSTEMD_AGENT_UNIT", "native media-agent systemd 服务名。"),
    ("SYSTEMD_ZLM_UNIT", "native ZLMediaKit systemd 服务名。"),
    (
        "SYSTEMD_POSTGRES_UNIT",
        "native PostgreSQL systemd 服务名。",
    ),
    ("POSTGRES_DB", "PostgreSQL 数据库名。"),
    ("POSTGRES_USER", "PostgreSQL 用户名。"),
    ("POSTGRES_PORT", "数据库宿主机端口。"),
    ("CORE_HTTP_HOST", "工作节点访问控制面的 HTTP 地址。"),
    ("CORE_HTTP_PORT", "控制面板网页和 HTTP API 端口。"),
    ("CORE_GRPC_HOST", "工作节点访问控制面的内部通信地址。"),
    ("CORE_GRPC_PORT", "控制面板内部通信端口。"),
    (
        "AUTH_MODE",
        "控制台鉴权模式，可选 disabled/local_password。",
    ),
    ("AUTH_ENABLED", "是否启用鉴权。"),
    ("NODE_ID", "工作节点唯一 ID，已上线后不要随意修改。"),
    ("AGENT_NODE_NAME", "控制台展示的节点名称。"),
    ("PUBLIC_HOST", "客户端播放地址使用的宿主机 IP 或域名。"),
    ("ZLM_API_HOST", "工作节点访问本机流媒体服务接口的地址。"),
    ("AGENT_HTTP_PORT", "工作节点本地接口端口。"),
    ("ZLM_HTTP_PORT", "流媒体 HTTP 播放和接口端口。"),
    ("ZLM_HTTPS_PORT", "流媒体 HTTPS 端口，0 表示关闭。"),
    ("ZLM_RTMP_PORT", "RTMP 播放/推流端口。"),
    ("ZLM_RTMPS_PORT", "RTMPS 端口，0 表示关闭。"),
    ("ZLM_RTSP_PORT", "RTSP 播放端口。"),
    ("ZLM_RTSPS_PORT", "RTSPS 端口，0 表示关闭。"),
    ("ZLM_RTP_PROXY_PORT", "RTP 接收固定端口，0 表示关闭。"),
    (
        "ZLM_RTP_PROXY_PORT_RANGE",
        "RTP 接收端口范围，格式 start-end；0-0 表示关闭。",
    ),
    ("ZLM_RTC_SIGNALING_PORT", "WebRTC 信令端口，0 表示关闭。"),
    (
        "ZLM_RTC_SIGNALING_SSL_PORT",
        "WebRTC 加密信令端口，0 表示关闭。",
    ),
    ("ZLM_RTC_ICE_PORT", "STUN/TURN UDP 端口，0 表示关闭。"),
    ("ZLM_RTC_ICE_TCP_PORT", "STUN/TURN TCP 端口，0 表示关闭。"),
    ("ZLM_RTC_PORT", "WebRTC UDP 媒体端口，0 表示关闭。"),
    ("ZLM_RTC_TCP_PORT", "WebRTC TCP 媒体端口，0 表示关闭。"),
    (
        "ZLM_RTC_PORT_RANGE",
        "WebRTC 媒体端口范围，格式 start-end；0-0 表示关闭。",
    ),
    ("ZLM_SRT_PORT", "SRT 端口，0 表示关闭。"),
    ("ZLM_SHELL_PORT", "流媒体调试 Shell 端口，0 表示关闭。"),
    ("ZLM_ONVIF_PORT", "ONVIF 端口，0 表示关闭。"),
    ("AGENT_PRIMARY_INTERFACE_NAME", "主网卡名称。"),
    ("AGENT_PRIMARY_INTERFACE_IP", "主网卡 IP。"),
    ("AGENT_MULTICAST_INTERFACE_NAME", "组播/副网卡名称。"),
    ("AGENT_MULTICAST_INTERFACE_IP", "组播/副网卡 IP。"),
    ("AGENT_NETWORK_MODE", "固定为 host，直接使用宿主机网络。"),
    (
        "AGENT_ACCELERATION_MODE",
        "节点算力模式，由安装角色决定，配置模块只展示。",
    ),
    (
        "AGENT_LABELS",
        "节点标签，固定包含算力标签 cpu/gpu，额外标签用英文逗号分隔。",
    ),
    (
        "AGENT_MAX_RUNTIME_SLOTS",
        "最大同时任务数，0 表示自动估算。",
    ),
    (
        "ZLM_WWW_MOUNT_HOST_DIR",
        "服务挂载源宿主机目录，用于在线播放临时文件，建议本机磁盘。",
    ),
    (
        "ZLM_OUTPUT_MOUNT_HOST_DIR",
        "服务挂载源宿主机目录，用于录制和转码产物，可挂载网络存储。",
    ),
    (
        "AGENT_MP4_RECORD_SEGMENT_SEC",
        "录制 MP4 默认分段秒数，默认 7200。",
    ),
    (
        "AGENT_HLS_RECORD_SEGMENT_SEC",
        "录制 HLS 默认分片秒数，可选 30/60。",
    ),
    ("AGENT_ARTIFACT_CLEANUP_ENABLED", "是否启用产物盘空间保护。"),
    (
        "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT",
        "产物盘使用率达到该百分比后触发保护。",
    ),
    (
        "AGENT_ARTIFACT_CLEANUP_STRATEGY",
        "产物盘保护策略，可选 delete_oldest_then_reject/reject_only。",
    ),
    (
        "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC",
        "产物盘空间检查间隔秒数。",
    ),
    ("WORK_ROOT", "工作节点工作目录。"),
];

fn env_comment(key: &str) -> Option<&'static str> {
    ENV_COMMENTS
        .iter()
        .find_map(|(entry_key, comment)| (*entry_key == key).then_some(*comment))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    use ratatui::backend::TestBackend;

    #[test]
    fn parses_single_ports_and_ranges() {
        assert_eq!(parse_port_text("TEST_PORT", "65535", false).unwrap(), 65535);
        assert_eq!(parse_port_text("TEST_PORT", "0", true).unwrap(), 0);
        assert!(parse_port_text("TEST_PORT", "0", false).is_err());
        assert!(parse_port_text("TEST_PORT", "65536", true).is_err());

        assert_eq!(
            parse_port_range_text("TEST_RANGE", "10000-10100").unwrap(),
            (10000, 10100)
        );
        assert_eq!(parse_port_range_text("TEST_RANGE", "0-0").unwrap(), (0, 0));
        assert!(parse_port_range_text("TEST_RANGE", "10100-10000").is_err());
        assert!(parse_port_range_text("TEST_RANGE", "0-10000").is_err());
    }

    #[test]
    fn rejects_duplicate_configured_ports_and_ranges() {
        let mut values = BTreeMap::new();
        values.insert("CORE_HTTP_PORT".to_string(), "8080".to_string());
        values.insert("AGENT_HTTP_PORT".to_string(), "8081".to_string());
        values.insert("ZLM_RTC_PORT_RANGE".to_string(), "10000-10100".to_string());

        assert!(
            ensure_configured_port_available(&values, "CORE_GRPC_PORT", 8080, field_label_for_key)
                .is_err()
        );
        assert!(
            ensure_configured_port_available(&values, "ZLM_RTC_PORT", 10050, field_label_for_key)
                .is_err()
        );
        assert!(
            ensure_configured_port_available(&values, "CORE_GRPC_PORT", 8082, field_label_for_key)
                .is_ok()
        );

        assert!(
            ensure_configured_range_available(
                &values,
                "ZLM_RTP_PROXY_PORT_RANGE",
                8081,
                8090,
                field_label_for_key,
            )
            .is_err()
        );
        assert!(
            ensure_configured_range_available(
                &values,
                "ZLM_RTP_PROXY_PORT_RANGE",
                10050,
                10200,
                field_label_for_key,
            )
            .is_err()
        );
        assert!(
            ensure_configured_range_available(
                &values,
                "ZLM_RTP_PROXY_PORT_RANGE",
                20000,
                20100,
                field_label_for_key,
            )
            .is_ok()
        );
    }

    #[test]
    fn detects_bound_host_tcp_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        assert!(ensure_host_port_available(port).is_err());
    }

    #[test]
    fn keeps_editing_when_enter_confirms_occupied_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let port_text = port.to_string();
        let env_path = std::env::temp_dir().join(format!(
            "streamserver-config-port-test-{}.env",
            std::process::id()
        ));
        let mut app = ConfigApp::load(env_path).unwrap();

        app.page = Page::Ports;
        app.selected = 1;
        app.editing = Some(port_text.clone());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.editing.as_deref(), Some(port_text.as_str()));
        assert_eq!(app.value("CORE_HTTP_PORT"), "8080");
        assert!(app.message.contains("已被宿主机占用"));
    }

    #[test]
    fn allows_reverting_to_running_baseline_port_after_save() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let original_port = listener.local_addr().unwrap().port();
        let original_port_text = original_port.to_string();
        let env_path = std::env::temp_dir().join(format!(
            "streamserver-config-revert-port-test-{}.env",
            std::process::id()
        ));
        let mut app = ConfigApp::load(env_path).unwrap();

        app.values
            .insert("CORE_HTTP_PORT".to_string(), original_port_text.clone());
        app.running_baseline_values = app.values.clone();
        app.values
            .insert("CORE_HTTP_PORT".to_string(), "18080".to_string());
        app.page = Page::Ports;
        app.selected = 1;
        app.editing = Some(original_port_text.clone());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert!(app.editing.is_none());
        assert_eq!(app.value("CORE_HTTP_PORT"), original_port_text);
        assert!(app.message.contains("已更新"));
    }

    #[test]
    fn agent_labels_keep_fixed_acceleration_label() {
        let mut values = BTreeMap::new();
        values.insert("INSTALL_ROLE".to_string(), "worker-host-gpu".to_string());
        values.insert("AGENT_ACCELERATION_MODE".to_string(), "gpu".to_string());
        values.insert(
            "AGENT_LABELS".to_string(),
            "room-a,cpu,gpu,edge-1".to_string(),
        );

        normalize_agent_labels(&mut values);
        assert_eq!(values.get("AGENT_LABELS").unwrap(), "gpu,room-a,edge-1");
        assert_eq!(extra_agent_labels(&values), "room-a,edge-1");
        assert_eq!(
            agent_labels_from_extra(&values, "cpu,gpu,room-b"),
            "gpu,room-b"
        );
    }

    #[test]
    fn primary_interface_updates_public_and_api_hosts() {
        let mut values = BTreeMap::new();
        values.insert("INSTALL_ROLE".to_string(), "worker-host-cpu".to_string());
        values.insert(
            "AGENT_PRIMARY_INTERFACE_IP".to_string(),
            "10.0.0.8".to_string(),
        );
        values.insert("PUBLIC_HOST".to_string(), "old.example".to_string());
        values.insert("ZLM_API_HOST".to_string(), "old-api.example".to_string());

        sync_primary_interface_followers(&mut values);

        assert_eq!(values.get("PUBLIC_HOST").unwrap(), "10.0.0.8");
        assert_eq!(values.get("ZLM_API_HOST").unwrap(), "10.0.0.8");
    }

    #[test]
    fn validates_native_instance_name() {
        assert!(validate_instance_name("streamserver").is_ok());
        assert!(validate_instance_name("ss-aio_cpu1").is_ok());
        assert!(validate_instance_name("StreamServer").is_ok());
        assert!(validate_instance_name("stream.server@1").is_ok());
        assert!(validate_instance_name("-streamserver").is_err());
        assert!(validate_instance_name("stream server").is_err());
    }

    #[test]
    fn keeps_editing_when_instance_name_is_invalid() {
        let env_path = std::env::temp_dir().join(format!(
            "streamserver-config-instance-name-test-{}.env",
            std::process::id()
        ));
        let mut app = ConfigApp::load(env_path).unwrap();
        app.page = Page::Basic;
        app.selected = 1;
        app.editing = Some("Stream Server".to_string());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.editing.as_deref(), Some("Stream Server"));
        assert!(app.message.contains("实例名称"));
    }

    #[test]
    fn validates_instance_dir_before_delete() {
        let install_dir = std::env::temp_dir().join(format!(
            "streamserver-config-delete-test-{}",
            std::process::id()
        ));
        let bin_dir = install_dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(
            install_dir.join(".env"),
            "DEPLOY_MODE=native\nINSTANCE_NAME=test\n",
        )
        .unwrap();
        fs::write(bin_dir.join("streamserver-config"), "").unwrap();

        assert!(validate_instance_dir_for_delete(&install_dir).is_ok());

        fs::write(install_dir.join(".env"), "DEPLOY_MODE=legacy\n").unwrap();
        assert!(validate_instance_dir_for_delete(&install_dir).is_err());
        let _ = fs::remove_dir_all(&install_dir);
    }

    #[test]
    fn opens_uninstall_confirm_from_keyboard() {
        let env_path = std::env::temp_dir().join(format!(
            "streamserver-config-uninstall-confirm-test-{}.env",
            std::process::id()
        ));
        let mut app = ConfigApp::load(env_path).unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE))
            .unwrap();

        assert!(app.uninstall_confirm.is_some());
        assert!(app.message.contains("DELETE"));
    }

    #[test]
    fn renders_tui_frame() {
        let env_path = std::env::temp_dir().join(format!(
            "streamserver-config-render-test-{}.env",
            std::process::id()
        ));
        let app = ConfigApp::load(env_path).unwrap();
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| ui_render::draw(frame, &app)).unwrap();
    }
}
