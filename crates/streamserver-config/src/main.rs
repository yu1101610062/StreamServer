use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

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

const REQUIRED_PORT_KEYS: &[&str] = &[
    "POSTGRES_PORT",
    "CORE_HTTP_PORT",
    "CORE_GRPC_PORT",
    "AGENT_HTTP_PORT",
    "ZLM_HTTP_PORT",
    "ZLM_RTMP_PORT",
    "ZLM_RTSP_PORT",
];

const OPTIONAL_PORT_KEYS: &[&str] = &[
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

const PORT_RANGE_KEYS: &[&str] = &["ZLM_RTP_PROXY_PORT_RANGE", "ZLM_RTC_PORT_RANGE"];
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
            Self::Hls => "HLS 录制",
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
        Page::Hls => &[FieldDef {
            key: "AGENT_HLS_RECORD_SEGMENT_SEC",
            label: "录制 HLS 分片",
            kind: FieldKind::Choice(HLS_SEGMENT_CHOICES),
            scope: FieldScope::Agent,
            help: "只影响录制归档 HLS，不影响在线低延迟播放 HLS。任务接口显式传值时优先。",
        }],
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

struct RestartTask {
    unit: String,
    receiver: Receiver<anyhow::Result<()>>,
}

impl fmt::Debug for RestartTask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RestartTask")
            .field("unit", &self.unit)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct UninstallConfirm {
    input: String,
}

struct UninstallTask {
    install_dir: PathBuf,
    receiver: Receiver<anyhow::Result<()>>,
}

impl fmt::Debug for UninstallTask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UninstallTask")
            .field("install_dir", &self.install_dir)
            .finish_non_exhaustive()
    }
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
        if matches!(key, "INSTALL_ROLE" | "AGENT_ACCELERATION_MODE") {
            self.message = "该项由安装器决定，配置模块只展示不修改".to_string();
            return;
        }
        if is_hidden_fixed_key(key) {
            self.message = "该项为内部固定项，配置模块不修改".to_string();
            return;
        }

        if key == "AUTH_MODE" {
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
            ensure_configured_port_available(&self.values, key, port)?;
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
            ensure_configured_range_available(&self.values, key, start, end)?;
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

fn parse_env_file(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut values = BTreeMap::new();
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

fn deploy_mode(values: &BTreeMap<String, String>) -> &str {
    values
        .get("DEPLOY_MODE")
        .map(String::as_str)
        .unwrap_or("native")
}

fn native_unit_basename(values: &BTreeMap<String, String>) -> String {
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

fn spawn_restart_task(unit: String) -> RestartTask {
    let (sender, receiver) = mpsc::channel();
    let task_unit = unit.clone();
    thread::spawn(move || {
        let result = restart_and_wait_instance(&task_unit);
        let _ = sender.send(result);
    });
    RestartTask { unit, receiver }
}

fn spawn_uninstall_task(install_dir: PathBuf) -> UninstallTask {
    let (sender, receiver) = mpsc::channel();
    let task_install_dir = install_dir.clone();
    thread::spawn(move || {
        let result = uninstall_instance(&task_install_dir);
        let _ = sender.send(result);
    });
    UninstallTask {
        install_dir,
        receiver,
    }
}

fn uninstall_instance(install_dir: &Path) -> anyhow::Result<()> {
    validate_instance_dir_for_delete(install_dir)?;

    let uninstall_script = install_dir.join("uninstall.sh");
    if uninstall_script.is_file() {
        run_root_command(
            uninstall_script.to_string_lossy().as_ref(),
            &["--purge", "--yes"],
        )?;
        return Ok(());
    }

    let env_values = parse_env_file(&install_dir.join(".env"))?;
    for unit in native_unit_candidates(&env_values) {
        let _ = run_root_command("systemctl", &["stop", &unit]);
        let _ = run_root_command("systemctl", &["disable", &unit]);
        let unit_path = Path::new("/etc/systemd/system").join(&unit);
        let _ = run_root_command("rm", &["-f", unit_path.to_string_lossy().as_ref()]);
        let _ = run_root_command("systemctl", &["reset-failed", &unit]);
    }
    run_root_command("systemctl", &["daemon-reload"])?;

    run_root_command("rm", &["-rf", install_dir.to_string_lossy().as_ref()])?;
    Ok(())
}

fn validate_instance_dir_for_delete(install_dir: &Path) -> anyhow::Result<()> {
    let install_dir = install_dir
        .canonicalize()
        .with_context(|| format!("实例目录不存在：{}", install_dir.display()))?;
    if install_dir == Path::new("/") {
        bail!("拒绝删除根目录");
    }
    if install_dir.parent().is_none() {
        bail!("实例目录不安全，拒绝删除：{}", install_dir.display());
    }
    for required in [".env", "bin/streamserver-config"] {
        if !install_dir.join(required).exists() {
            bail!(
                "目录缺少实例标识文件 {}，拒绝删除：{}",
                required,
                install_dir.display()
            );
        }
    }
    let env_values = parse_env_file(&install_dir.join(".env"))?;
    if deploy_mode(&env_values) != "native" {
        bail!("不是 native 实例目录，拒绝删除：{}", install_dir.display());
    }
    Ok(())
}

fn restart_and_wait_instance(unit: &str) -> anyhow::Result<()> {
    run_root_command("systemctl", &["restart", unit])?;
    wait_for_unit_active(unit, Duration::from_secs(90))?;
    Ok(())
}

fn wait_for_unit_active(unit: &str, timeout: Duration) -> anyhow::Result<()> {
    let started_at = Instant::now();
    loop {
        if unit_is_active(unit) {
            return Ok(());
        }
        if started_at.elapsed() >= timeout {
            bail!("服务 {unit} 重启后未进入运行状态");
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn instance_running(values: &BTreeMap<String, String>) -> bool {
    native_unit_candidates(values)
        .iter()
        .any(|unit| unit_is_active(unit))
}

fn native_unit_candidates(values: &BTreeMap<String, String>) -> Vec<String> {
    let mut units = Vec::new();
    for key in [
        "SYSTEMD_TARGET",
        "SYSTEMD_CORE_UNIT",
        "SYSTEMD_AGENT_UNIT",
        "SYSTEMD_ZLM_UNIT",
        "SYSTEMD_POSTGRES_UNIT",
    ] {
        if let Some(unit) = values.get(key).filter(|value| !value.trim().is_empty()) {
            if !units.contains(unit) {
                units.push(unit.clone());
            }
        }
    }
    if units.is_empty() {
        units.push(format!("{}.target", native_unit_basename(values)));
    }
    units
}

fn unit_is_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .is_ok_and(|status| status.success())
}

fn can_run_root_commands() -> bool {
    is_root()
        || Command::new("sudo")
            .args(["-n", "true"])
            .output()
            .is_ok_and(|output| output.status.success())
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|uid| uid.trim() == "0")
}

fn run_root_command(program: &str, args: &[&str]) -> anyhow::Result<()> {
    if is_root() {
        run_command_capture(program, args, None)
    } else {
        let mut sudo_args = vec!["-n", program];
        sudo_args.extend_from_slice(args);
        run_command_capture("sudo", &sudo_args, None)
    }
}

fn run_command_capture(program: &str, args: &[&str], cwd: Option<&Path>) -> anyhow::Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .with_context(|| format!("failed to run {program}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        "无输出".to_string()
    };
    let args = args.join(" ");
    bail!(
        "{program} {args} exited with status {}: {detail}",
        output.status
    );
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

fn validate_port_value(
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

fn validate_port_range_value(values: &BTreeMap<String, String>, key: &str) -> anyhow::Result<()> {
    let Some(value) = values.get(key) else {
        return Ok(());
    };
    parse_port_range_text(key, value)?;
    Ok(())
}

fn parse_port_text(key: &str, value: &str, allow_zero: bool) -> anyhow::Result<u16> {
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

fn parse_port_range_text(key: &str, value: &str) -> anyhow::Result<(u16, u16)> {
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

fn ensure_configured_port_available(
    values: &BTreeMap<String, String>,
    key: &str,
    port: u16,
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
            let label = field_label_for_key(other_key);
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
            let label = field_label_for_key(other_key);
            bail!("端口 {port} 落在 {label} 的范围 {start}-{end} 内");
        }
    }

    Ok(())
}

fn ensure_configured_range_available(
    values: &BTreeMap<String, String>,
    key: &str,
    start: u16,
    end: u16,
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
            let label = field_label_for_key(other_key);
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
            let label = field_label_for_key(other_key);
            bail!("端口范围 {start}-{end} 与 {label} 的范围 {other_start}-{other_end} 重叠");
        }
    }

    Ok(())
}

fn ensure_host_port_available(port: u16) -> anyhow::Result<()> {
    if occupied_host_ports().contains(&port) {
        bail!("端口 {port} 已被宿主机占用，请更换端口");
    }
    Ok(())
}

fn ensure_host_port_range_available(start: u16, end: u16) -> anyhow::Result<()> {
    let occupied = occupied_host_ports();
    if let Some(port) = occupied.range(start..=end).next() {
        bail!("端口范围 {start}-{end} 中的 {port} 已被宿主机占用，请更换范围");
    }
    Ok(())
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

fn ranges_overlap(a_start: u16, a_end: u16, b_start: u16, b_end: u16) -> bool {
    a_start <= b_end && b_start <= a_end
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

fn is_port_key(key: &str) -> bool {
    REQUIRED_PORT_KEYS.contains(&key) || OPTIONAL_PORT_KEYS.contains(&key)
}

fn is_port_range_key(key: &str) -> bool {
    PORT_RANGE_KEYS.contains(&key)
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
        terminal.draw(|frame| draw(frame, &app))?;
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

fn draw(frame: &mut Frame<'_>, app: &ConfigApp) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(12),
            Constraint::Length(5),
        ])
        .split(frame.area());

    draw_tabs(frame, app, layout[0]);

    if app.uninstall_task.is_some() {
        frame.render_widget(uninstall_progress_panel(app), layout[1]);
    } else if app.uninstall_confirm.is_some() {
        frame.render_widget(uninstall_confirm_panel(app), layout[1]);
    } else if app.restart_task.is_some() {
        frame.render_widget(restart_panel(app), layout[1]);
    } else {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(layout[1]);
        frame.render_widget(field_list_body(app), body[0]);
        frame.render_widget(detail_panel(app), body[1]);
    }

    let footer = Paragraph::new(vec![
        message_line(app.message.as_str()),
        Line::from(vec![
            Span::styled(
                "配置文件: ",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                app.env_path.display().to_string(),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        shortcuts_line(),
    ])
    .wrap(Wrap { trim: true })
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(message_color(app.message.as_str())))
            .title("操作"),
    );
    frame.render_widget(footer, layout[2]);
}

fn restart_panel(app: &ConfigApp) -> Paragraph<'static> {
    let unit = app
        .restart_task
        .as_ref()
        .map(|task| task.unit.as_str())
        .unwrap_or("-");
    Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "重启中",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "正在检查节点启动状态请稍候",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        key_value_line("服务", unit.to_string(), Color::Cyan),
        muted_line("请不要关闭窗口。完成后配置模块会自动退出。"),
    ])
    .wrap(Wrap { trim: true })
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title("服务重启"),
    )
}

fn uninstall_confirm_panel(app: &ConfigApp) -> Paragraph<'static> {
    let install_dir = app.install_dir();
    let unit = app
        .restart_unit_name()
        .unwrap_or_else(|| "未检测到".to_string());
    let input = app
        .uninstall_confirm
        .as_ref()
        .map(|confirm| confirm.input.as_str())
        .unwrap_or("");
    Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "彻底卸载当前实例",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        warning_line("此操作会停止服务、删除 systemd 服务，并删除整个实例文件夹。"),
        warning_line("实例目录中的配置、录像文件、录制产物、本地数据和日志都会被删除。"),
        Line::from(""),
        key_value_line("服务", unit, Color::Yellow),
        key_value_line("实例目录", install_dir.display().to_string(), Color::Red),
        Line::from(""),
        muted_line("确认执行请输入 DELETE，然后按 Enter。Esc 取消。"),
        key_value_line("确认文本", format!("{input}_"), Color::Yellow),
    ])
    .wrap(Wrap { trim: true })
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red))
            .title("危险操作"),
    )
}

fn uninstall_progress_panel(app: &ConfigApp) -> Paragraph<'static> {
    let install_dir = app
        .uninstall_task
        .as_ref()
        .map(|task| task.install_dir.display().to_string())
        .unwrap_or_else(|| "-".to_string());
    Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "卸载中",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        warning_line("正在停止服务、删除 systemd 服务并删除实例目录，请稍候。"),
        Line::from(""),
        key_value_line("实例目录", install_dir, Color::Red),
        muted_line("完成后配置模块会自动退出。"),
    ])
    .wrap(Wrap { trim: true })
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red))
            .title("彻底卸载"),
    )
}

fn draw_tabs(frame: &mut Frame<'_>, app: &ConfigApp, area: ratatui::layout::Rect) {
    let tabs = Page::ALL
        .iter()
        .map(|page| {
            if *page == app.page {
                Span::styled(
                    format!(" {} ", page.title()),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    format!(" {} ", page.title()),
                    Style::default().fg(Color::Gray),
                )
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(Line::from(tabs)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue))
                .title("StreamServer 配置"),
        ),
        area,
    );
}

fn field_list_body(app: &ConfigApp) -> List<'static> {
    let fields = app.current_fields();
    let items = if fields.is_empty() {
        vec![ListItem::new(Line::from("当前安装角色不需要此页配置"))]
    } else {
        let mut items = vec![
            ListItem::new(field_header_line()),
            ListItem::new(field_separator_line()),
        ];
        items.extend(
            fields
                .iter()
                .enumerate()
                .map(|(index, field)| field_item(app, index, field))
                .collect::<Vec<_>>(),
        );
        items
    };
    List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue))
            .title(app.page.title()),
    )
}

fn field_item(app: &ConfigApp, index: usize, field: &FieldDef) -> ListItem<'static> {
    let mut value = field_display_value(app, field);
    if index == app.selected {
        if let Some(editing) = &app.editing {
            if matches!(field.kind, FieldKind::Text) {
                value = format!("{editing}_");
            }
        }
    }
    let selected = index == app.selected;
    let marker = if selected { ">" } else { " " };
    let label = pad_display_width(field.label, FIELD_LABEL_WIDTH);
    let label_style = if selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let value_style = field_value_style(app, field, selected);
    ListItem::new(Line::from(vec![
        Span::styled(format!("{marker} "), Style::default().fg(Color::Cyan)),
        Span::styled(label, label_style),
        Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
        Span::styled(value, value_style),
    ]))
}

fn field_header_line() -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            pad_display_width("配置项", FIELD_LABEL_WIDTH),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "当前值",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn field_separator_line() -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "─".repeat(FIELD_LABEL_WIDTH),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("─┼─", Style::default().fg(Color::DarkGray)),
        Span::styled("─".repeat(22), Style::default().fg(Color::DarkGray)),
    ])
}

fn field_value_style(app: &ConfigApp, field: &FieldDef, selected: bool) -> Style {
    let mut style = match field.kind {
        FieldKind::ReadOnly => Style::default().fg(Color::DarkGray),
        FieldKind::Choice(_) => Style::default().fg(Color::Green),
        FieldKind::Interface(_) => Style::default().fg(Color::Green),
        FieldKind::Text if app.editing.is_some() && selected => Style::default().fg(Color::Yellow),
        FieldKind::Text if field_display_value(app, field).trim().is_empty() => {
            Style::default().fg(Color::Red)
        }
        FieldKind::Text => Style::default().fg(Color::White),
    };
    if selected {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

fn field_display_value(app: &ConfigApp, field: &FieldDef) -> String {
    match field.kind {
        FieldKind::Interface(target) => {
            let name = app.value(target.name_key());
            let ip = app.value(target.ip_key());
            if name.trim().is_empty() && ip.trim().is_empty() {
                "未选择".to_string()
            } else {
                format!("{name} ({ip})")
            }
        }
        FieldKind::Choice(choices) => {
            let value = app.value(field.key);
            format_choice_value(&value, choices)
        }
        FieldKind::ReadOnly => {
            let value = app.value(field.key);
            match field.key {
                "INSTALL_ROLE" => format_choice_value(&value, INSTALL_ROLE_CHOICES),
                "AGENT_ACCELERATION_MODE" => format_choice_value(&value, ACCELERATION_CHOICES),
                _ => value,
            }
        }
        FieldKind::Text if field.key == "AGENT_LABELS" => agent_labels_display(&app.values),
        FieldKind::Text if app.page == Page::Storage => storage_display_value(app, field.key),
        FieldKind::Text => app.value(field.key),
    }
}

fn agent_labels_display(values: &BTreeMap<String, String>) -> String {
    let fixed = fixed_agent_label(values).unwrap_or("-");
    let extra = extra_agent_labels(values);
    if extra.trim().is_empty() {
        format!("固定: {fixed}；额外: 无")
    } else {
        format!("固定: {fixed}；额外: {extra}")
    }
}

fn format_choice_value(value: &str, choices: &[ChoiceDef]) -> String {
    let label = choices
        .iter()
        .find(|choice| choice.value == value)
        .map(|choice| choice.label)
        .unwrap_or("未知");
    format!("{value} - {label}")
}

fn pad_display_width(value: &str, width: usize) -> String {
    let display_width = UnicodeWidthStr::width(value);
    if display_width >= width {
        value.to_string()
    } else {
        format!("{value}{}", " ".repeat(width - display_width))
    }
}

fn message_line(message: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "状态: ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(message.to_string(), message_style(message)),
    ])
}

fn shortcuts_line() -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "快捷键: ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        shortcut("↑/↓"),
        Span::raw(" 选择  "),
        shortcut("Enter"),
        Span::raw(" 打开/编辑  "),
        shortcut("Tab"),
        Span::raw(" 切页  "),
        shortcut("M"),
        Span::raw(" 确认挂载  "),
        shortcut("S"),
        Span::raw(" 保存  "),
        shortcut("D"),
        Span::raw(" 卸载  "),
        shortcut("Q"),
        Span::raw(" 退出"),
    ])
}

fn shortcut(value: &str) -> Span<'static> {
    Span::styled(
        value.to_string(),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
}

fn message_style(message: &str) -> Style {
    let color = message_color(message);
    let style = Style::default().fg(color);
    if matches!(color, Color::Red | Color::Green | Color::Yellow) {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn message_color(message: &str) -> Color {
    if message_contains_any(
        message,
        &[
            "失败",
            "错误",
            "不能为空",
            "不能",
            "冲突",
            "占用",
            "不合法",
            "不支持",
            "卸载",
            "删除",
        ],
    ) {
        Color::Red
    } else if message_contains_any(
        message,
        &[
            "请确认",
            "需要",
            "未检测到",
            "只展示",
            "取消",
            "放弃",
            "重启中",
        ],
    ) {
        Color::Yellow
    } else if message.starts_with("已") {
        Color::Green
    } else {
        Color::Cyan
    }
}

fn message_contains_any(message: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| message.contains(needle))
}

fn detail_panel(app: &ConfigApp) -> Paragraph<'static> {
    if let Some(picker) = &app.picker {
        return picker_panel(app, picker);
    }

    let Some(field) = app.current_field() else {
        return Paragraph::new(vec![
            warning_line("当前角色不需要配置此页。"),
            muted_line("安装角色由安装器选择，配置模块只展示。"),
        ])
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue))
                .title("说明"),
        );
    };

    let mut lines = vec![
        Line::from(Span::styled(
            field.label,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        muted_line(field.help),
        Line::from(""),
        key_value_line("当前值", field_display_value(app, &field), Color::Green),
    ];

    match field.kind {
        FieldKind::Choice(choices) => {
            lines.push(Line::from(""));
            lines.push(section_title("可选值"));
            for choice in choices {
                lines.push(choice_line(
                    choice.value == app.value(field.key),
                    choice.value,
                    choice.label,
                    choice.help,
                ));
            }
            lines.push(Line::from(""));
            lines.push(muted_line("按 Enter 后用 ↑/↓ 选择，再按 Enter 确认。"));
        }
        FieldKind::Interface(target) => {
            lines.push(Line::from(""));
            lines.push(section_title("检测到的可用 IPv4 网卡"));
            if app.interfaces.is_empty() {
                lines.push(warning_line("未检测到网卡。请在宿主机确认 ip 命令可用。"));
            } else {
                let current = app.value(target.name_key());
                for interface in &app.interfaces {
                    let selected = interface.name == current;
                    lines.push(interface_line(selected, &interface.name, &interface.ip));
                }
            }
            lines.push(Line::from(""));
            lines.push(muted_line("选择后会同时写入网卡名称和 IP。"));
        }
        FieldKind::ReadOnly => {
            lines.push(Line::from(""));
            if matches!(field.key, "PUBLIC_HOST" | "ZLM_API_HOST") {
                lines.push(warning_line(
                    "该项跟随主网卡 IP 自动更新，配置模块不单独修改。",
                ));
            } else {
                lines.push(warning_line("该项只展示当前安装结果，配置模块不修改。"));
            }
        }
        FieldKind::Text => {}
    }

    if app.page == Page::Ports {
        lines.push(Line::from(""));
        lines.push(warning_line(
            "端口都监听在宿主机上，不能和本机已有服务冲突。",
        ));
        lines.push(muted_line(
            "可选端口填 0 表示关闭；端口范围使用 start-end，0-0 表示关闭。",
        ));
    } else if app.page == Page::Storage {
        lines.extend(storage_detail_lines(app));
    }

    Paragraph::new(lines).wrap(Wrap { trim: true }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue))
            .title("说明 / 候选项"),
    )
}

fn picker_panel(app: &ConfigApp, picker: &Picker) -> Paragraph<'static> {
    let mut lines = vec![Line::from(Span::styled(
        "选择中",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ))];

    match picker {
        Picker::Choice {
            choices, selected, ..
        } => {
            for (index, choice) in choices.iter().enumerate() {
                lines.push(choice_line(
                    index == *selected,
                    choice.value,
                    choice.label,
                    choice.help,
                ));
            }
        }
        Picker::Interface { target, selected } => {
            lines.push(section_title(format!("{} 候选网卡", target.title())));
            for (index, interface) in app.interfaces.iter().enumerate() {
                lines.push(interface_line(
                    index == *selected,
                    &interface.name,
                    &interface.ip,
                ));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(muted_line("↑/↓ 移动，Enter 确认，Esc 取消"));
    Paragraph::new(lines).wrap(Wrap { trim: true }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title("选择"),
    )
}

fn choice_line(selected: bool, value: &str, label: &str, help: &str) -> Line<'static> {
    let marker = if selected { ">" } else { " " };
    Line::from(vec![
        Span::styled(
            format!("{marker} {}", pad_display_width(value, CHOICE_VALUE_WIDTH)),
            if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            },
        ),
        Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{label}  "),
            Style::default()
                .fg(if selected {
                    Color::Yellow
                } else {
                    Color::White
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(help.to_string(), Style::default().fg(Color::Gray)),
    ])
}

fn section_title(title: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        title.into(),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ))
}

fn muted_line(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), Style::default().fg(Color::Gray)))
}

fn warning_line(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ))
}

fn success_line(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    ))
}

fn key_value_line(label: &str, value: impl Into<String>, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{}: ", pad_display_width(label, 10)),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(value.into(), Style::default().fg(color)),
    ])
}

fn interface_line(selected: bool, name: &str, ip: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if selected { "> " } else { "  " },
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(
            pad_display_width(name, 18),
            if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            },
        ),
        Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
        Span::styled(ip.to_string(), Style::default().fg(Color::Green)),
    ])
}

fn storage_detail_lines(app: &ConfigApp) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        section_title("存储检查"),
        muted_line("这里只展示和检查宿主机目录。"),
        warning_line("在线播放目录建议放本机盘；录制产物目录可挂载 NAS/NFS 等网络存储。"),
        muted_line("程序只检查和写配置，不会自动执行 mount。"),
        Line::from(""),
    ];

    for key in ["ZLM_WWW_MOUNT_HOST_DIR", "ZLM_OUTPUT_MOUNT_HOST_DIR"] {
        lines.push(Line::from(Span::styled(
            format!("{}:", storage_label(key)),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(key_value_line("建议", storage_advice(key), Color::Yellow));
        lines.extend(describe_storage_path(app, key));
        lines.push(Line::from(""));
    }

    lines.push(if app.storage_confirmed {
        success_line("挂载确认: 已确认录制产物目录挂载状态")
    } else {
        warning_line("挂载确认: 按 M 标记已完成挂载检查")
    });
    lines
}

fn storage_label(key: &str) -> &'static str {
    match key {
        "ZLM_WWW_HOST_DIR" => "在线播放目录",
        "ZLM_OUTPUT_HOST_DIR" => "录制产物目录",
        "ZLM_WWW_MOUNT_HOST_DIR" => "在线播放宿主机路径",
        "ZLM_OUTPUT_MOUNT_HOST_DIR" => "录制产物宿主机路径",
        _ => "目录",
    }
}

fn storage_advice(key: &str) -> &'static str {
    match key {
        "ZLM_WWW_HOST_DIR" => "使用本机磁盘，不建议挂网络存储。",
        "ZLM_OUTPUT_HOST_DIR" => "可挂载网络存储，用于保存录制和转码产物。",
        "ZLM_WWW_MOUNT_HOST_DIR" => "使用本机磁盘，不建议挂网络存储。",
        "ZLM_OUTPUT_MOUNT_HOST_DIR" => "可挂载网络存储，用于保存录制和转码产物。",
        _ => "",
    }
}

fn describe_storage_path(app: &ConfigApp, key: &str) -> Vec<Line<'static>> {
    let configured = app.value(key);
    let resolved = resolve_host_path(&app.env_path, &configured);
    let exists = resolved.exists();
    let mut description = vec![
        key_value_line("宿主机目录", configured, Color::Cyan),
        key_value_line("展开后路径", resolved.display().to_string(), Color::Cyan),
        key_value_line(
            "状态",
            if exists { "已存在" } else { "尚未创建" },
            if exists { Color::Green } else { Color::Yellow },
        ),
    ];
    if let Some(sample) = df_sample(&resolved) {
        description.push(key_value_line("文件系统", sample.fs_type, Color::Green));
        description.push(key_value_line("可用空间", sample.available, Color::Green));
        description.push(key_value_line("挂载点", sample.mount, Color::Cyan));
    }
    description
}

fn storage_display_value(app: &ConfigApp, key: &str) -> String {
    let configured = app.value(key);
    if configured.trim().is_empty() {
        return String::new();
    }
    resolve_host_path(&app.env_path, &configured)
        .display()
        .to_string()
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

fn env_comment(key: &str) -> Option<&'static str> {
    match key {
        "DEPLOY_MODE" => Some("部署模式，固定为 native，由 systemd 管理。"),
        "INSTALL_ROLE" => Some("这台机器的部署角色，由安装器选择，配置模块只展示。"),
        "INSTANCE_NAME" => Some("native 实例名，用于生成 systemd 服务名。"),
        "SYSTEMD_TARGET" => Some("native 聚合 target。"),
        "SYSTEMD_CORE_UNIT" => Some("native media-core systemd 服务名。"),
        "SYSTEMD_AGENT_UNIT" => Some("native media-agent systemd 服务名。"),
        "SYSTEMD_ZLM_UNIT" => Some("native ZLMediaKit systemd 服务名。"),
        "SYSTEMD_POSTGRES_UNIT" => Some("native PostgreSQL systemd 服务名。"),
        "POSTGRES_DB" => Some("PostgreSQL 数据库名。"),
        "POSTGRES_USER" => Some("PostgreSQL 用户名。"),
        "POSTGRES_PORT" => Some("数据库宿主机端口。"),
        "CORE_HTTP_HOST" => Some("工作节点访问控制面的 HTTP 地址。"),
        "CORE_HTTP_PORT" => Some("控制面板网页和 HTTP API 端口。"),
        "CORE_GRPC_HOST" => Some("工作节点访问控制面的内部通信地址。"),
        "CORE_GRPC_PORT" => Some("控制面板内部通信端口。"),
        "AUTH_MODE" => Some("控制台鉴权模式，可选 disabled/local_password。"),
        "AUTH_ENABLED" => Some("是否启用鉴权。"),
        "NODE_ID" => Some("工作节点唯一 ID，已上线后不要随意修改。"),
        "AGENT_NODE_NAME" => Some("控制台展示的节点名称。"),
        "PUBLIC_HOST" => Some("客户端播放地址使用的宿主机 IP 或域名。"),
        "ZLM_API_HOST" => Some("工作节点访问本机流媒体服务接口的地址。"),
        "AGENT_HTTP_PORT" => Some("工作节点本地接口端口。"),
        "ZLM_HTTP_PORT" => Some("流媒体 HTTP 播放和接口端口。"),
        "ZLM_HTTPS_PORT" => Some("流媒体 HTTPS 端口，0 表示关闭。"),
        "ZLM_RTMP_PORT" => Some("RTMP 播放/推流端口。"),
        "ZLM_RTMPS_PORT" => Some("RTMPS 端口，0 表示关闭。"),
        "ZLM_RTSP_PORT" => Some("RTSP 播放端口。"),
        "ZLM_RTSPS_PORT" => Some("RTSPS 端口，0 表示关闭。"),
        "ZLM_RTP_PROXY_PORT" => Some("RTP 接收固定端口，0 表示关闭。"),
        "ZLM_RTP_PROXY_PORT_RANGE" => Some("RTP 接收端口范围，格式 start-end；0-0 表示关闭。"),
        "ZLM_RTC_SIGNALING_PORT" => Some("WebRTC 信令端口，0 表示关闭。"),
        "ZLM_RTC_SIGNALING_SSL_PORT" => Some("WebRTC 加密信令端口，0 表示关闭。"),
        "ZLM_RTC_ICE_PORT" => Some("STUN/TURN UDP 端口，0 表示关闭。"),
        "ZLM_RTC_ICE_TCP_PORT" => Some("STUN/TURN TCP 端口，0 表示关闭。"),
        "ZLM_RTC_PORT" => Some("WebRTC UDP 媒体端口，0 表示关闭。"),
        "ZLM_RTC_TCP_PORT" => Some("WebRTC TCP 媒体端口，0 表示关闭。"),
        "ZLM_RTC_PORT_RANGE" => Some("WebRTC 媒体端口范围，格式 start-end；0-0 表示关闭。"),
        "ZLM_SRT_PORT" => Some("SRT 端口，0 表示关闭。"),
        "ZLM_SHELL_PORT" => Some("流媒体调试 Shell 端口，0 表示关闭。"),
        "ZLM_ONVIF_PORT" => Some("ONVIF 端口，0 表示关闭。"),
        "AGENT_PRIMARY_INTERFACE_NAME" => Some("主网卡名称。"),
        "AGENT_PRIMARY_INTERFACE_IP" => Some("主网卡 IP。"),
        "AGENT_MULTICAST_INTERFACE_NAME" => Some("组播/副网卡名称。"),
        "AGENT_MULTICAST_INTERFACE_IP" => Some("组播/副网卡 IP。"),
        "AGENT_NETWORK_MODE" => Some("固定为 host，直接使用宿主机网络。"),
        "AGENT_ACCELERATION_MODE" => Some("节点算力模式，由安装角色决定，配置模块只展示。"),
        "AGENT_LABELS" => Some("节点标签，固定包含算力标签 cpu/gpu，额外标签用英文逗号分隔。"),
        "AGENT_MAX_RUNTIME_SLOTS" => Some("最大同时任务数，0 表示自动估算。"),
        "ZLM_WWW_MOUNT_HOST_DIR" => {
            Some("服务挂载源宿主机目录，用于在线播放临时文件，建议本机磁盘。")
        }
        "ZLM_OUTPUT_MOUNT_HOST_DIR" => {
            Some("服务挂载源宿主机目录，用于录制和转码产物，可挂载网络存储。")
        }
        "AGENT_HLS_RECORD_SEGMENT_SEC" => Some("录制 HLS 默认分片秒数，可选 30/60。"),
        "AGENT_ARTIFACT_CLEANUP_ENABLED" => Some("是否启用产物盘空间保护。"),
        "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT" => Some("产物盘使用率达到该百分比后触发保护。"),
        "AGENT_ARTIFACT_CLEANUP_STRATEGY" => {
            Some("产物盘保护策略，可选 delete_oldest_then_reject/reject_only。")
        }
        "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC" => Some("产物盘空间检查间隔秒数。"),
        "WORK_ROOT" => Some("工作节点工作目录。"),
        _ => None,
    }
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

        assert!(ensure_configured_port_available(&values, "CORE_GRPC_PORT", 8080).is_err());
        assert!(ensure_configured_port_available(&values, "ZLM_RTC_PORT", 10050).is_err());
        assert!(ensure_configured_port_available(&values, "CORE_GRPC_PORT", 8082).is_ok());

        assert!(
            ensure_configured_range_available(&values, "ZLM_RTP_PROXY_PORT_RANGE", 8081, 8090)
                .is_err()
        );
        assert!(
            ensure_configured_range_available(&values, "ZLM_RTP_PROXY_PORT_RANGE", 10050, 10200)
                .is_err()
        );
        assert!(
            ensure_configured_range_available(&values, "ZLM_RTP_PROXY_PORT_RANGE", 20000, 20100)
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

        terminal.draw(|frame| draw(frame, &app)).unwrap();
    }
}
