use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use spacekit_core::clean;
use spacekit_core::fmt::human;
use spacekit_core::model::{CleanItem, Risk};
use std::collections::HashSet;

struct App {
    items: Vec<CleanItem>,
    selected: Vec<bool>,
    state: ListState,
    show_help: bool,
    confirm: bool,
    status: Option<String>,
    quit: bool,
}

impl App {
    fn new(items: Vec<CleanItem>) -> Self {
        // 默认勾选「安全」项
        let selected = items.iter().map(|i| i.risk == Risk::Safe).collect();
        let mut state = ListState::default();
        if !items.is_empty() {
            state.select(Some(0));
        }
        Self {
            items,
            selected,
            state,
            show_help: false,
            confirm: false,
            status: None,
            quit: false,
        }
    }

    fn cursor(&self) -> usize {
        self.state.selected().unwrap_or(0)
    }

    fn toggle(&mut self) {
        let i = self.cursor();
        if i < self.selected.len() {
            self.selected[i] = !self.selected[i];
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let len = self.items.len() as isize;
        let next = (self.cursor() as isize + delta).clamp(0, len - 1);
        self.state.select(Some(next as usize));
    }

    /// 可回收总量（不含不可再生项）
    fn total(&self) -> u64 {
        self.items
            .iter()
            .filter(|i| i.risk != Risk::Destructive)
            .map(|i| i.size)
            .sum()
    }

    fn selected_total(&self) -> u64 {
        self.items
            .iter()
            .zip(&self.selected)
            .filter(|(_, s)| **s)
            .map(|(i, _)| i.size)
            .sum()
    }

    fn selected_count(&self) -> usize {
        self.selected.iter().filter(|s| **s).count()
    }

    /// 执行：把选中项移入隔离区
    fn apply(&mut self) {
        self.confirm = false;
        let chosen: Vec<CleanItem> = self
            .items
            .iter()
            .zip(&self.selected)
            .filter(|(_, s)| **s)
            .map(|(i, _)| i.clone())
            .collect();

        match clean::quarantine(&chosen, false) {
            Ok(j) => {
                let moved: HashSet<_> = j.entries.iter().map(|e| e.original.clone()).collect();
                let items = std::mem::take(&mut self.items);
                let selected = std::mem::take(&mut self.selected);
                for (it, sel) in items.into_iter().zip(selected) {
                    if !moved.contains(&it.path) {
                        self.items.push(it);
                        self.selected.push(sel);
                    }
                }
                if self.items.is_empty() {
                    self.state.select(None);
                } else {
                    let c = self.cursor().min(self.items.len() - 1);
                    self.state.select(Some(c));
                }

                let mut msg = format!(
                    "已移入隔离区 {} 项 · {}  （会话 {}）",
                    j.entries.len(),
                    human(j.total_size()),
                    j.session
                );
                if !j.skipped.is_empty() {
                    msg.push_str(&format!("  跳过 {} 项", j.skipped.len()));
                }
                msg.push_str("   恢复: spacekit quarantine restore");
                self.status = Some(msg);
            }
            Err(e) => self.status = Some(format!("失败: {e:#}")),
        }
    }

    fn on_key(&mut self, code: KeyCode) {
        if self.confirm {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.apply(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.confirm = false,
                _ => {}
            }
            return;
        }

        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => self.state.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => {
                if !self.items.is_empty() {
                    self.state.select(Some(self.items.len() - 1));
                }
            }
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('a') => {
                for (i, it) in self.items.iter().enumerate() {
                    self.selected[i] = it.risk == Risk::Safe;
                }
            }
            KeyCode::Char('A') => self.selected.iter_mut().for_each(|s| *s = true),
            KeyCode::Char('n') => self.selected.iter_mut().for_each(|s| *s = false),
            KeyCode::Char('c') => {
                if self.selected_count() > 0 {
                    self.confirm = true;
                } else {
                    self.status = Some("未勾选任何项".into());
                }
            }
            KeyCode::Char('?') | KeyCode::Char('h') => self.show_help = !self.show_help,
            _ => {}
        }
    }
}

fn risk_color(risk: Risk) -> Color {
    match risk {
        Risk::Safe => Color::Green,
        Risk::Confirm => Color::Yellow,
        Risk::Destructive => Color::Red,
    }
}

/// 启动 TUI。退出后打印（dry-run）计划。
pub fn run(items: Vec<CleanItem>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(items);
    let res = event_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if res.is_ok() {
        print_plan(&app);
    }
    res
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    while !app.quit {
        terminal.draw(|f| ui(f, app))?;
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                app.on_key(key.code);
            }
        }
    }
    Ok(())
}

fn ui(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_header(frame, app, chunks[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
        .split(chunks[1]);

    render_list(frame, app, body[0]);
    render_detail(frame, app, body[1]);
    render_footer(frame, app, chunks[2]);

    if app.confirm {
        render_confirm(frame, app);
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let text = vec![
        Line::from(vec![
            Span::styled(
                " spacekit ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  M1 · 隔离区  "),
            Span::styled(
                format!("可回收 {} ", human(app.total())),
                Style::default().fg(Color::Green),
            ),
        ]),
        Line::from(format!(
            " 已选 {} 项 · 预计释放 {}",
            app.selected_count(),
            human(app.selected_total())
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title("macOS 空间扫描");
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn render_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let home = std::env::var("HOME").unwrap_or_default();
    let items: Vec<ListItem> = app
        .items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let mark = if app.selected[i] { "[x]" } else { "[ ]" };
            let mark_color = if app.selected[i] {
                Color::Green
            } else {
                Color::DarkGray
            };
            let path = it.path.display().to_string();
            let shown = if !home.is_empty() && path.starts_with(&home) {
                path.replacen(&home, "~", 1)
            } else {
                path
            };
            let line = Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(mark_color)),
                Span::styled(
                    format!("{:>10} ", human(it.size)),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("{:<6} ", it.risk.label()),
                    Style::default().fg(risk_color(it.risk)),
                ),
                Span::raw(it.name.clone()),
                Span::styled(format!("  {shown}"), Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("清理项"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("› ");

    frame.render_stateful_widget(list, area, &mut app.state);
}

fn render_detail(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("详情");
    let text = if let Some(it) = app.items.get(app.cursor()) {
        let label = |s: &str| {
            Line::from(Span::styled(
                s.to_string(),
                Style::default().fg(Color::Cyan),
            ))
        };
        vec![
            Line::from(Span::styled(
                it.name.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "大小: {}   类别: {}",
                human(it.size),
                it.category.label()
            )),
            Line::from(vec![
                Span::raw("风险: "),
                Span::styled(
                    it.risk.label().to_string(),
                    Style::default().fg(risk_color(it.risk)),
                ),
                Span::raw(format!(
                    "   可再生: {}",
                    if it.regenerable { "是" } else { "否" }
                )),
            ]),
            Line::from(""),
            label("这是什么"),
            Line::from(it.explain.what.clone()),
            Line::from(""),
            label("删了会怎样"),
            Line::from(it.explain.cost.clone()),
            Line::from(""),
            label("能否恢复"),
            Line::from(it.explain.recover.clone()),
            Line::from(""),
            label("清理方式"),
            Line::from(it.reclaim.clone()),
            Line::from(""),
            Line::from(Span::styled(
                it.path.display().to_string(),
                Style::default().fg(Color::DarkGray),
            )),
        ]
    } else {
        vec![Line::from("（无）")]
    };
    frame.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let text = if let Some(s) = &app.status {
        s.clone()
    } else if app.show_help {
        " ↑/k 上  ↓/j 下  space 勾选  a 选安全  A 全选  n 清空  c 清理  ? 关闭帮助  q 退出".into()
    } else {
        " ↑↓/jk 移动 · space 勾选 · a 选安全 · A 全选 · n 清空 · c 移入隔离区 · ? 帮助 · q 退出"
            .into()
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::Black).bg(Color::Gray)),
        area,
    );
}

fn render_confirm(frame: &mut Frame, app: &App) {
    let area = centered_rect(56, 22, frame.area());
    frame.render_widget(Clear, area);
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("将 {} 项移入隔离区？", app.selected_count()),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("预计释放 {}", human(app.selected_total()))),
        Line::from(""),
        Line::from(Span::styled(
            "移入后可随时恢复（quarantine restore）",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("[y] 确认", Style::default().fg(Color::Green)),
            Span::raw("    "),
            Span::styled("[n] 取消", Style::default().fg(Color::Red)),
        ]),
    ];
    let popup = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title("确认清理"))
        .alignment(ratatui::layout::Alignment::Center);
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

fn print_plan(app: &App) {
    println!("\n\x1b[1mspacekit 清理计划 (dry-run)\x1b[0m\n");
    let mut total: u64 = 0;
    for (it, sel) in app.items.iter().zip(&app.selected) {
        if *sel {
            total = total.saturating_add(it.size);
            println!(
                "  [x] {:>10}  {}  →  {}",
                human(it.size),
                it.name,
                it.reclaim
            );
        }
    }
    println!(
        "\n共 {} 项，预计释放 \x1b[1m{}\x1b[0m",
        app.selected_count(),
        human(total)
    );
    println!("（已在 TUI 中执行的项已移入隔离区，可用 spacekit quarantine list 查看）");
}
