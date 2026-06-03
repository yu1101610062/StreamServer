use std::collections::BTreeMap;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    ACCELERATION_CHOICES, CHOICE_VALUE_WIDTH, ChoiceDef, ConfigApp, FIELD_LABEL_WIDTH, FieldDef,
    FieldKind, INSTALL_ROLE_CHOICES, Page, Picker, df_sample, extra_agent_labels,
    fixed_agent_label, resolve_host_path,
};

pub(crate) fn draw(frame: &mut Frame<'_>, app: &ConfigApp) {
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
