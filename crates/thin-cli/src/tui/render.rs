//! 各标签页的渲染（header/tabs/body/footer/确认框）。

use super::rows::tree_prefix;
use super::*;

fn risk_color(risk: Risk) -> Color {
    match risk {
        Risk::Safe => Color::Green,
        Risk::Confirm => Color::Yellow,
        Risk::Destructive => Color::Red,
    }
}

fn tier_color(tier: apps::Tier) -> Color {
    match tier {
        apps::Tier::Large => Color::Red,
        apps::Tier::Medium => Color::Yellow,
        apps::Tier::Small => Color::Green,
    }
}

fn bar(ratio: f64, width: usize) -> String {
    let filled = ((ratio.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// HOME 前缀（canonicalize，兼容 `/var` → `/private/var` 等符号链接）
pub(super) fn home_prefix() -> String {
    let raw = std::env::var("HOME").unwrap_or_default();
    if raw.is_empty() {
        return raw;
    }
    std::fs::canonicalize(&raw)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(raw)
}

pub(super) fn ui(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(frame.area());

    render_header(frame, chunks[0]);
    render_tabs(frame, app, chunks[1]);
    render_body(frame, app, chunks[2]);
    render_footer(frame, app, chunks[3]);

    if app.confirm {
        render_confirm(frame, app);
    }
    if let Some(pending) = &app.kill_confirm {
        render_kill_confirm(frame, pending);
    }
    if let Some(plan) = &app.uninstall_confirm {
        render_uninstall_confirm(frame, plan);
    }
    if let Some(plan) = &app.orphan_confirm {
        render_orphan_confirm(frame, plan);
    }
}

/// 「正在运行」的退出确认：退出进程是破坏性动作，单独确认一次。
fn render_kill_confirm(frame: &mut Frame, pending: &PendingKill) {
    let area = centered_rect_fixed(66, 10, frame.area());
    frame.render_widget(Clear, area);
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("{} 正在运行", pending.app_name),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("卸载前需先退出该 App（优雅退出 → 强制结束）。"),
        Line::from(""),
        Line::from(Span::styled(
            "未保存的修改可能丢失；退出后仍会先移入废纸篓/隔离区。",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("[y] 退出并卸载", Style::default().fg(Color::Red)),
            Span::raw("    "),
            Span::styled("[n / Esc] 取消", Style::default().fg(Color::Green)),
        ]),
    ];
    let popup = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title("退出运行中的 App"),
        )
        .alignment(Alignment::Center);
    frame.render_widget(popup, area);
}

fn render_header(frame: &mut Frame, area: Rect) {
    let disk = probe::statfs("/System/Volumes/Data");
    let line1 = Line::from(vec![
        Span::styled(
            " thin ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  Yes, yet another macOS cleaning tool."),
    ]);
    let line2 = if let Some(v) = disk {
        let pct = v.used_pct();
        let color = if pct > 90.0 {
            Color::Red
        } else if pct > 75.0 {
            Color::Yellow
        } else {
            Color::Green
        };
        Line::from(vec![
            Span::raw(" 磁盘 ["),
            Span::styled(bar(pct / 100.0, 24), Style::default().fg(color)),
            Span::raw(format!(
                "] {} / {} ({:.0}%)",
                human(v.used),
                human(v.total),
                pct
            )),
        ])
    } else {
        Line::from(" 磁盘信息不可用")
    };
    frame.render_widget(Paragraph::new(vec![line1, line2]), area);
}

fn render_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::raw(" ")];
    for (i, name) in TABS.iter().enumerate() {
        let label = format!(" {} {} ", i + 1, name);
        if i == app.tab {
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(label, Style::default().fg(Color::Gray)));
        }
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_body(frame: &mut Frame, app: &mut App, area: Rect) {
    match app.tab {
        0 => render_clean(frame, app, area),
        APPS_TAB => render_apps(frame, app, area),
        HISTORY_TAB => render_history(frame, app, area),
        BROWSE_TAB => render_browse(frame, app, area),
        _ => {}
    }
}

/// 历史：记录列表 + 明细
fn render_history(frame: &mut Frame, app: &mut App, area: Rect) {
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);

    match &app.history {
        Load::Ready(records) => {
            if records.is_empty() {
                state_msg(frame, area, "暂无历史记录");
                return;
            }
            let items: Vec<ListItem> = records
                .iter()
                .map(|r| {
                    let preset = r.preset.clone().unwrap_or_else(|| "-".into());
                    ListItem::new(Line::from(vec![
                        Span::raw(format!("{}  ", history::format_ts(r.timestamp))),
                        Span::styled(
                            text::pad_end(&r.trigger, 9),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(
                            format!("{:>9} ", human(r.moved_bytes)),
                            Style::default().fg(Color::White),
                        ),
                        Span::styled(
                            format!("{} 项", r.moved),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::raw(format!("  {preset}")),
                    ]))
                })
                .collect();
            let widget = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("历史 · {} 条", records.len())),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("› ");
            frame.render_stateful_widget(widget, parts[0], &mut app.list_states[app.tab]);

            let cur = app.cursor();
            let mut text: Vec<Line> = Vec::new();
            if let Some(r) = records.get(cur) {
                text.push(field("时间", &history::format_ts(r.timestamp)));
                text.push(field("触发", &r.trigger));
                text.push(field("预设", r.preset.as_deref().unwrap_or("-")));
                if let Some(s) = &r.session {
                    text.push(field("会话", s));
                }
                text.push(Line::from(""));
                text.push(field("扫描", &format!("{} 项", r.scanned)));
                text.push(field("通过", &format!("{} 项", r.approved)));
                text.push(field(
                    "移入",
                    &format!("{} 项 · {}", r.moved, human(r.moved_bytes)),
                ));
                text.push(field("跳过", &format!("{} 项", r.skipped)));
                if r.purged_sessions > 0 {
                    text.push(field(
                        "清理旧会话",
                        &format!("{} 个 · {}", r.purged_sessions, human(r.purged_bytes)),
                    ));
                }
            } else {
                text.push(Line::from("（无）"));
            }
            frame.render_widget(
                Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("详情 · c 回填隔离区遗漏记录"),
                    )
                    .wrap(Wrap { trim: true }),
                parts[1],
            );
        }
        Load::Loading { progress, .. } => {
            render_loading(frame, area, Some(progress), app.tick, "读取历史…")
        }
        Load::Failed(e) => state_msg(frame, area, e),
        Load::Idle => state_msg(frame, area, "等待加载"),
    }
}

fn render_browse(frame: &mut Frame, app: &mut App, area: Rect) {
    if let Some(b) = &mut app.browse {
        browse::render(frame, b, area);
    } else {
        state_msg(frame, area, "初始化浏览…");
    }
}

fn state_msg(frame: &mut Frame, area: Rect, msg: &str) {
    let p = Paragraph::new(msg)
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(p, area);
}

/// 加载中：确定进度用 Gauge 进度条，不确定进度用 spinner 动画
fn render_loading(
    frame: &mut Frame,
    area: Rect,
    progress: Option<&Arc<Progress>>,
    tick: usize,
    fallback: &str,
) {
    let Some(p) = progress else {
        state_msg(frame, area, fallback);
        return;
    };
    let s = p.snapshot();
    if s.total > 0 {
        let ratio = (s.done as f64 / s.total as f64).clamp(0.0, 1.0);
        let gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(s.label))
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(ratio)
            .label(format!("{}/{}  {:.0}%", s.done, s.total, ratio * 100.0));
        let a = centered_rect(60, 24, area);
        frame.render_widget(Clear, a);
        frame.render_widget(gauge, a);
    } else {
        const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let spin = SPINNER[tick % SPINNER.len()];
        let msg = if s.count > 0 {
            format!("{spin} {} …  已处理 {}", s.label, s.count)
        } else {
            format!("{spin} {} …", s.label)
        };
        state_msg(frame, area, &msg);
    }
}

/// 清理：列表 + 详情
fn render_clean(frame: &mut Frame, app: &mut App, area: Rect) {
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
        .split(area);
    let home = home_prefix();

    match &app.clean {
        Load::Ready(items) => {
            if items.is_empty() {
                render_clean_empty(frame, app, &parts);
                return;
            }
            let list_items: Vec<ListItem> = if app.tree_view {
                app.clean_rows
                    .iter()
                    .map(|row| {
                        let (mark, mark_color) = if let Some(i) = row.item {
                            if row.protected {
                                ("[ ]", Color::DarkGray)
                            } else if app.selected.get(i).copied().unwrap_or(false) {
                                ("[x]", Color::Green)
                            } else {
                                ("[ ]", Color::DarkGray)
                            }
                        } else {
                            // 目录分组：全选 [x] / 部分 [-]
                            let total = row
                                .item_indices
                                .iter()
                                .filter(|&&i| !app.item_protected(i))
                                .count();
                            let sel = row
                                .item_indices
                                .iter()
                                .filter(|&&i| {
                                    !app.item_protected(i)
                                        && app.selected.get(i).copied().unwrap_or(false)
                                })
                                .count();
                            if total == 0 {
                                ("[ ]", Color::DarkGray)
                            } else if sel == total {
                                ("[x]", Color::Green)
                            } else if sel > 0 {
                                ("[-]", Color::Yellow)
                            } else {
                                ("[ ]", Color::DarkGray)
                            }
                        };
                        let (label, color) = match row.risk {
                            Some(_) if row.protected => ("已保护", Color::DarkGray),
                            Some(_) if row.sudo => ("需 sudo", Color::Yellow),
                            Some(risk) => (risk.label(), risk_color(risk)),
                            None => ("", Color::DarkGray),
                        };
                        let mut spans = vec![
                            Span::styled(format!("{mark} "), Style::default().fg(mark_color)),
                            Span::styled(
                                format!("{:>9} ", human(row.size)),
                                Style::default().fg(Color::White),
                            ),
                            Span::styled(
                                format!("{} ", text::pad_end(label, 8)),
                                Style::default().fg(color),
                            ),
                            Span::styled(
                                tree_prefix(&row.is_last),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(
                                if row.is_dir() {
                                    format!("{}/", row.name)
                                } else {
                                    row.name.clone()
                                },
                                Style::default().fg(if row.is_dir() {
                                    Color::Cyan
                                } else {
                                    Color::Reset
                                }),
                            ),
                        ];
                        if row.is_dir() {
                            spans.push(Span::styled(
                                format!("  · {} 项", row.count),
                                Style::default().fg(Color::DarkGray),
                            ));
                        } else if let Some(rule) = &row.rule_name
                            && *rule != row.name
                        {
                            spans.push(Span::styled(
                                format!("  ({rule})"),
                                Style::default().fg(Color::DarkGray),
                            ));
                        }
                        if row.nested {
                            spans.push(Span::styled(
                                "  (嵌套)",
                                Style::default().fg(Color::DarkGray),
                            ));
                        }
                        ListItem::new(Line::from(spans))
                    })
                    .collect()
            } else {
                items
                    .iter()
                    .enumerate()
                    .map(|(i, it)| {
                        let mark = if app.selected.get(i).copied().unwrap_or(false) {
                            "[x]"
                        } else {
                            "[ ]"
                        };
                        let path = it.path.display().to_string();
                        let shown = if !home.is_empty() && path.starts_with(&home) {
                            path.replacen(&home, "~", 1)
                        } else {
                            path
                        };
                        ListItem::new(Line::from(vec![
                            Span::styled(
                                format!("{mark} "),
                                Style::default().fg(
                                    if app.selected.get(i).copied().unwrap_or(false) {
                                        Color::Green
                                    } else {
                                        Color::DarkGray
                                    },
                                ),
                            ),
                            Span::styled(
                                format!("{:>9} ", human(it.size)),
                                Style::default().fg(Color::White),
                            ),
                            Span::styled(
                                format!(
                                    "{} ",
                                    text::pad_end(
                                        if it.protected {
                                            "已保护"
                                        } else if it.sudo {
                                            "需 sudo"
                                        } else {
                                            it.risk.label()
                                        },
                                        8,
                                    )
                                ),
                                Style::default().fg(if it.protected {
                                    Color::DarkGray
                                } else if it.sudo {
                                    Color::Yellow
                                } else {
                                    risk_color(it.risk)
                                }),
                            ),
                            Span::raw(it.name.clone()),
                            Span::styled(
                                format!("  {shown}"),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]))
                    })
                    .collect()
            };

            let title = format!(
                "清理项 · 已选 {} · {} · {}(t)",
                app.selected_count(),
                human(app.selected_total()),
                if app.tree_view { "树形" } else { "平铺" }
            );
            let list = List::new(list_items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("› ");
            frame.render_stateful_widget(list, parts[0], &mut app.list_states[app.tab]);

            // 详情：树形视图先取行自身的命中项；目录分组则展示聚合信息
            let cur = app.cursor();
            let detail = if app.tree_view {
                app.clean_rows
                    .get(cur)
                    .and_then(|r| r.item)
                    .and_then(|i| items.get(i))
            } else {
                items.get(cur)
            };
            let text = if let Some(it) = detail {
                vec![
                    Line::from(Span::styled(
                        it.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(format!("大小: {}", human(it.size))),
                    Line::from(vec![
                        Span::raw("风险: "),
                        Span::styled(
                            if it.protected {
                                "已保护（thin protect）"
                            } else if it.sudo {
                                "需 sudo（thin 不会清理）"
                            } else {
                                it.risk.label()
                            }
                            .to_string(),
                            Style::default().fg(if it.protected {
                                Color::DarkGray
                            } else if it.sudo {
                                Color::Yellow
                            } else {
                                risk_color(it.risk)
                            }),
                        ),
                        Span::raw(format!(
                            "  可再生: {}",
                            if it.regenerable { "是" } else { "否" }
                        )),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled("这是什么", Style::default().fg(Color::Cyan))),
                    Line::from(it.explain.what.clone()),
                    Line::from(Span::styled("删了会怎样", Style::default().fg(Color::Cyan))),
                    Line::from(it.explain.cost.clone()),
                    Line::from(Span::styled("能否恢复", Style::default().fg(Color::Cyan))),
                    Line::from(it.explain.recover.clone()),
                    Line::from(""),
                    Line::from(Span::styled(
                        it.path.display().to_string(),
                        Style::default().fg(Color::DarkGray),
                    )),
                ]
            } else if app.tree_view
                && let Some(row) = app.clean_rows.get(cur)
            {
                vec![
                    Line::from(Span::styled(
                        format!("{}/", row.name),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(format!("大小: {}  ·  {} 项", human(row.size), row.count)),
                    Line::from(""),
                    Line::from(Span::styled(
                        "目录分组（按文件夹合并）",
                        Style::default().fg(Color::Cyan),
                    )),
                    Line::from("space 整组勾选/取消；p 保护整个目录。"),
                ]
            } else {
                vec![Line::from("（无）")]
            };
            frame.render_widget(
                Paragraph::new(text)
                    .block(Block::default().borders(Borders::ALL).title("详情"))
                    .wrap(Wrap { trim: true }),
                parts[1],
            );
        }
        Load::Loading { progress, .. } => {
            render_loading(frame, parts[0], Some(progress), app.tick, "扫描中…");
            state_msg(frame, parts[1], "");
        }
        Load::Failed(e) => state_msg(frame, parts[0], e),
        Load::Idle => state_msg(frame, parts[0], "等待加载"),
    }
}

/// 清理页空状态：真的没得清（或都被过滤隐藏）时给一句鼓励 + 显示开关提示。
fn render_clean_empty(frame: &mut Frame, app: &App, parts: &[Rect]) {
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "✨ 您的电脑很干净！",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "没有已知的可清理项。",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let mut hint = Vec::new();
    if !app.show_manual {
        hint.push("m 显示需 sudo / 受系统保护项");
    }
    if !app.show_protected {
        hint.push("b 显示 protect 保护项");
    }
    if !hint.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("（{}）", hint.join(" · ")),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title("清理项")),
        parts[0],
    );
    frame.render_widget(
        Paragraph::new(vec![Line::from("（无）")])
            .block(Block::default().borders(Borders::ALL).title("详情")),
        parts[1],
    );
}

/// 应用
fn render_apps(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.show_orphans {
        render_orphans(frame, app, area);
        return;
    }
    let home = home_prefix();
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    match &app.apps {
        Load::Ready(list) => {
            let items: Vec<ListItem> = list
                .iter()
                .map(|a| {
                    let t = apps::tier(a.total());
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{:>9} ", human(a.total())),
                            Style::default().fg(tier_color(t)),
                        ),
                        Span::styled(
                            format!("{:<4} ", t.label()),
                            Style::default().fg(tier_color(t)),
                        ),
                        Span::styled(
                            if a.running { "● " } else { "  " },
                            Style::default().fg(Color::Green),
                        ),
                        Span::raw(truncate(&a.name, 24)),
                        Span::styled(
                            format!("  {}", a.bundle_id.clone().unwrap_or_default()),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                })
                .collect();
            let widget = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("应用 · {} 个（含关联残留）", list.len())),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("› ");
            frame.render_stateful_widget(widget, parts[0], &mut app.list_states[app.tab]);

            let mut text = Vec::new();
            if let Some(a) = list.get(app.cursor()) {
                text.push(Line::from(Span::styled(
                    a.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                text.push(Line::from(format!("应用本体: {}", human(a.size))));
                text.push(Line::from(format!(
                    "关联残留: {}",
                    human(a.leftovers_size())
                )));
                if let Some(bid) = &a.bundle_id {
                    text.push(Line::from(format!("Bundle ID: {bid}")));
                }
                text.push(Line::from(vec![
                    Span::raw("状态: "),
                    Span::styled(
                        if a.running { "运行中" } else { "未运行" },
                        Style::default().fg(if a.running {
                            Color::Green
                        } else {
                            Color::DarkGray
                        }),
                    ),
                ]));
                text.push(Line::from(""));
                for l in &a.leftovers {
                    let sp = l.path.display().to_string();
                    let shown = if !home.is_empty() && sp.starts_with(&home) {
                        sp.replacen(&home, "~", 1)
                    } else {
                        sp
                    };
                    let tag = if l.sudo { "  需 sudo" } else { "" };
                    text.push(Line::from(format!("{:<9} {}{}", human(l.size), shown, tag)));
                }
            }
            frame.render_widget(
                Paragraph::new(text)
                    .block(Block::default().borders(Borders::ALL).title("详情"))
                    .wrap(Wrap { trim: true }),
                parts[1],
            );
        }
        Load::Loading { progress, .. } => {
            render_loading(frame, parts[0], Some(progress), app.tick, "读取应用中…");
            state_msg(frame, parts[1], "");
        }
        Load::Failed(e) => state_msg(frame, parts[0], e),
        Load::Idle => state_msg(frame, parts[0], "等待加载"),
    }
}

/// 已卸载 App 的孤立残留列表（Apps 页按 `o` 切换）
fn render_orphans(frame: &mut Frame, app: &mut App, area: Rect) {
    let home = home_prefix();
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    match &app.orphans {
        Load::Ready(list) => {
            let items: Vec<ListItem> = list
                .iter()
                .map(|o| {
                    let t = apps::tier(o.total());
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{:>9} ", human(o.total())),
                            Style::default().fg(tier_color(t)),
                        ),
                        Span::styled(
                            format!("{:<4} ", t.label()),
                            Style::default().fg(tier_color(t)),
                        ),
                        Span::styled("○ ", Style::default().fg(Color::DarkGray)),
                        Span::raw(truncate(&o.bundle_id, 30)),
                    ]))
                })
                .collect();
            let widget = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("已卸载残留 · {} 个（o 切回应用）", list.len())),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("› ");
            frame.render_stateful_widget(widget, parts[0], &mut app.list_states[app.tab]);

            let mut text = Vec::new();
            if let Some(o) = list.get(app.cursor()) {
                text.push(Line::from(Span::styled(
                    o.bundle_id.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                text.push(Line::from(Span::styled(
                    "App 本体已不存在，以下残留未匹配到已安装应用",
                    Style::default().fg(Color::DarkGray),
                )));
                text.push(Line::from(""));
                for l in &o.leftovers {
                    let sp = l.path.display().to_string();
                    let shown = if !home.is_empty() && sp.starts_with(&home) {
                        sp.replacen(&home, "~", 1)
                    } else {
                        sp
                    };
                    let tag = if l.sudo { "  需 sudo" } else { "" };
                    text.push(Line::from(format!("{:<9} {}{}", human(l.size), shown, tag)));
                }
            }
            frame.render_widget(
                Paragraph::new(text)
                    .block(Block::default().borders(Borders::ALL).title("详情"))
                    .wrap(Wrap { trim: true }),
                parts[1],
            );
        }
        Load::Loading { progress, .. } => {
            render_loading(frame, parts[0], Some(progress), app.tick, "扫描孤立残留…");
            state_msg(frame, parts[1], "");
        }
        Load::Failed(e) => state_msg(frame, parts[0], e),
        Load::Idle => state_msg(frame, parts[0], "等待加载"),
    }
}

/// 基本信息的一行：标签 + 值
fn field(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            text::pad_end(label, 10),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(value.to_string()),
    ])
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    // 「浏览」页有自己的页脚（上下文提示/状态），此处留空避免重复
    if app.tab == BROWSE_TAB {
        frame.render_widget(Paragraph::new(""), area);
        return;
    }
    let (text, style) = if let Some(t) = &app.status {
        (
            format!(" {}   · Esc 关闭", t.text()),
            toast::style(t.kind()),
        )
    } else if app.help {
        (
            " 1-9/Tab 切换标签 · ↑↓/jk 移动 · space 勾选 · a 选安全 · A 全选 · n 清空 · t 树形 · p 保护 · P 解除 · m 需sudo · b 保护项 · r 重载 · c 清理 · Esc 关闭提示/退出 · q 退出"
                .to_string(),
            toast::bar_style(),
        )
    } else {
        let hint = match app.tab {
            0 => {
                " Tab 切页 · ↑↓/jk 移动 · space 勾选 · a 选安全 · A 全选 · n 清空 · t 树形 · p 保护 · P 解除 · m 需sudo · b 保护项 · c 清理 · r 重载 · ? 帮助 · q 退出"
            }
            APPS_TAB => " ↑↓/jk 移动 · u 卸载/清理 · o 孤立残留 · r 重载 · ? 帮助 · q 退出",
            HISTORY_TAB => " ↑↓/jk 移动 · c 回填隔离区遗漏记录 · r 重载 · ? 帮助 · q 退出",
            _ => " 1-5/Tab 切换标签 · ↑↓/jk 移动 · r 重载 · ? 帮助 · q 退出",
        };
        (hint.to_string(), toast::bar_style())
    };
    frame.render_widget(
        Paragraph::new(text).style(style).wrap(Wrap { trim: false }),
        area,
    );
}

fn render_confirm(frame: &mut Frame, app: &App) {
    let area = centered_rect_fixed(60, 11, frame.area());
    frame.render_widget(Clear, area);
    let mode = clean::default_mode();
    let recover = match mode {
        clean::Mode::Trash => "可在 Finder 废纸篓中恢复",
        clean::Mode::Quarantine => "移入后可随时恢复（thin quarantine restore）",
    };
    let mut text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("将 {} 项移入{}？", app.selected_count(), mode.label()),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("预计可释放 {}", human(app.selected_total()))),
        Line::from(""),
        Line::from(Span::styled(recover, Style::default().fg(Color::DarkGray))),
        Line::from(""),
        Line::from(vec![
            Span::styled("[y] 确认", Style::default().fg(Color::Green)),
            Span::raw("    "),
            Span::styled("[n / Esc] 取消", Style::default().fg(Color::Red)),
        ]),
    ];
    let sudo_n = app.selected_sudo_count();
    if sudo_n > 0 {
        // 插在释放提示之前，提醒后面还有一次系统授权框
        text.insert(
            4,
            Line::from(Span::styled(
                format!("其中 {sudo_n} 项需管理员权限，将弹系统授权框"),
                Style::default().fg(Color::Yellow),
            )),
        );
    }
    let popup = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title("确认清理"))
        .alignment(Alignment::Center);
    frame.render_widget(popup, area);
}

fn render_orphan_confirm(frame: &mut Frame, plan: &PendingOrphan) {
    let area = centered_rect_fixed(66, 12, frame.area());
    frame.render_widget(Clear, area);
    let mode = clean::default_mode();
    let mut text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("清理 {} 的孤立残留？", plan.bundle_id),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from("该 App 本体已不存在，以下残留保留无益。"),
        Line::from(format!("将 {} 项移入{}", plan.items.len(), mode.label())),
        Line::from(format!("可释放 {}", human(plan.approved_bytes))),
    ];
    if plan.sudo > 0 {
        text.push(Line::from(Span::styled(
            format!("其中 {} 项需管理员权限，将弹系统授权框", plan.sudo),
            Style::default().fg(Color::Yellow),
        )));
    }
    let skipped_other = plan.skipped.saturating_sub(plan.sudo);
    if skipped_other > 0 {
        text.push(Line::from(Span::styled(
            format!("{skipped_other} 项受保护/不存在，将跳过"),
            Style::default().fg(Color::Yellow),
        )));
    }
    if plan.sudo == 0 && skipped_other == 0 {
        text.push(Line::from(""));
    }
    text.push(Line::from(""));
    text.push(Line::from(Span::styled(
        match mode {
            clean::Mode::Trash => "可在 Finder 废纸篓中恢复",
            clean::Mode::Quarantine => "移入后可随时恢复；彻底删除用 thin quarantine purge",
        },
        Style::default().fg(Color::DarkGray),
    )));
    text.push(Line::from(""));
    text.push(Line::from(vec![
        Span::styled("[y] 清理", Style::default().fg(Color::Red)),
        Span::raw("    "),
        Span::styled("[n / Esc] 取消", Style::default().fg(Color::Green)),
    ]));
    let popup = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title("确认清理孤立残留"),
        )
        .alignment(Alignment::Center);
    frame.render_widget(popup, area);
}

fn render_uninstall_confirm(frame: &mut Frame, plan: &PendingUninstall) {
    let area = centered_rect_fixed(66, 12, frame.area());
    frame.render_widget(Clear, area);
    let mode = clean::default_mode();
    let mut text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("卸载 {}？", plan.app_name),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "将 {} 项（App 本体 + 关联残留）移入{}",
            plan.items.len(),
            mode.label()
        )),
        Line::from(format!("可释放 {}", human(plan.approved_bytes))),
    ];
    if plan.sudo > 0 {
        text.push(Line::from(Span::styled(
            format!("其中 {} 项需管理员权限，将弹系统授权框", plan.sudo),
            Style::default().fg(Color::Yellow),
        )));
    }
    let skipped_other = plan.skipped.saturating_sub(plan.sudo);
    if skipped_other > 0 {
        text.push(Line::from(Span::styled(
            format!("{skipped_other} 项受保护/不存在，将跳过"),
            Style::default().fg(Color::Yellow),
        )));
    }
    if plan.sudo == 0 && skipped_other == 0 {
        text.push(Line::from(""));
    }
    text.push(Line::from(""));
    text.push(Line::from(Span::styled(
        match mode {
            clean::Mode::Trash => "可在 Finder 废纸篓中恢复",
            clean::Mode::Quarantine => "移入后可随时恢复；彻底删除用 thin quarantine purge",
        },
        Style::default().fg(Color::DarkGray),
    )));
    text.push(Line::from(""));
    text.push(Line::from(vec![
        Span::styled("[y] 卸载", Style::default().fg(Color::Red)),
        Span::raw("    "),
        Span::styled("[n / Esc] 取消", Style::default().fg(Color::Green)),
    ]));
    let popup = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title("确认卸载"),
        )
        .alignment(Alignment::Center);
    frame.render_widget(popup, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// 固定尺寸的居中矩形（避免内容被百分比高度截断）
fn centered_rect_fixed(width: u16, height: u16, r: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(r.height.saturating_sub(height) / 2),
            Constraint::Length(height.min(r.height)),
            Constraint::Min(0),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(r.width.saturating_sub(width) / 2),
            Constraint::Length(width.min(r.width)),
            Constraint::Min(0),
        ])
        .split(vertical[1])[1]
}

fn truncate(s: &str, width: usize) -> String {
    text::truncate(s, width)
}
