use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;
use thin_core::apps::AppInfo;
use thin_core::finder::{DupeGroup, LargeFile};
use thin_core::fmt::human;
use thin_core::model::{CleanItem, Risk};
use thin_core::progress::Progress;
use thin_core::{apps, clean, finder, fsutil, probe, protect, rules, scan, status};

use crate::treemap;

const TABS: [&str; 6] = ["清理", "概览", "大文件", "重复", "应用", "状态"];
const N_TABS: usize = TABS.len();

// ---------------------------------------------------------------------------
// 懒加载状态
// ---------------------------------------------------------------------------

enum Load<T> {
    Idle,
    Loading {
        rx: Receiver<std::result::Result<T, String>>,
        progress: Arc<Progress>,
    },
    Ready(T),
    Failed(String),
}

impl<T> Load<T> {
    fn spawn(f: impl FnOnce(&Progress) -> Result<T> + Send + 'static) -> Self
    where
        T: Send + 'static,
    {
        let progress = Arc::new(Progress::new());
        let p = progress.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f(&p).map_err(|e| format!("{e:#}")));
        });
        Load::Loading { rx, progress }
    }

    fn poll(&mut self) {
        if let Load::Loading { rx, .. } = self {
            match rx.try_recv() {
                Ok(Ok(v)) => *self = Load::Ready(v),
                Ok(Err(e)) => *self = Load::Failed(e),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => *self = Load::Failed("加载中断".into()),
            }
        }
    }

    fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(v) => Some(v),
            _ => None,
        }
    }

    fn is_idle(&self) -> bool {
        matches!(self, Load::Idle)
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct App {
    root: PathBuf,
    min: u64,
    tab: usize,
    list_states: Vec<ListState>,
    overview: Load<Vec<(PathBuf, u64)>>,
    clean: Load<Vec<CleanItem>>,
    selected: Vec<bool>,
    large: Load<Vec<LargeFile>>,
    dupes: Load<Vec<DupeGroup>>,
    apps: Load<Vec<AppInfo>>,
    sysinfo: Option<status::SystemInfo>,
    live: Option<status::LiveStats>,
    cpu: status::CpuSampler,
    status_at: Option<std::time::Instant>,
    confirm: bool,
    status: Option<String>,
    help: bool,
    quit: bool,
    tick: usize,
}

impl App {
    fn new(root: PathBuf, min: u64) -> Self {
        let mut list_states = Vec::with_capacity(N_TABS);
        for _ in 0..N_TABS {
            let mut s = ListState::default();
            s.select(Some(0));
            list_states.push(s);
        }
        Self {
            root,
            min,
            tab: 0,
            list_states,
            overview: Load::Idle,
            clean: Load::Idle,
            selected: Vec::new(),
            large: Load::Idle,
            dupes: Load::Idle,
            apps: Load::Idle,
            sysinfo: None,
            live: None,
            cpu: status::CpuSampler::new(),
            status_at: None,
            confirm: false,
            status: None,
            help: false,
            quit: false,
            tick: 0,
        }
    }

    fn ensure(&mut self, tab: usize) {
        let root = self.root.clone();
        match tab {
            0 if self.clean.is_idle() => {
                let min = self.min;
                self.clean = Load::spawn(move |p| {
                    let catalog = rules::load()?;
                    Ok(scan::scan_progress(&catalog, true, min, p))
                });
            }
            1 if self.overview.is_idle() => {
                self.overview = Load::spawn(move |p| Ok(fsutil::children_sizes_progress(&root, p)));
            }
            2 if self.large.is_idle() => {
                self.large = Load::spawn(move |p| {
                    Ok(finder::find_large_progress(
                        &[root],
                        100 * 1024 * 1024,
                        300,
                        p,
                    ))
                });
            }
            3 if self.dupes.is_idle() => {
                self.dupes = Load::spawn(move |p| {
                    Ok(finder::find_duplicates_progress(
                        &[root],
                        1024 * 1024,
                        200,
                        p,
                    ))
                });
            }
            4 if self.apps.is_idle() => {
                self.apps = Load::spawn(move |_p| Ok(apps::list_apps()));
            }
            5 => {
                if self.sysinfo.is_none() {
                    self.sysinfo = Some(status::collect_info());
                }
            }
            _ => {}
        }
    }

    fn poll_loaders(&mut self) {
        self.overview.poll();
        self.clean.poll();
        self.large.poll();
        self.dupes.poll();
        self.apps.poll();
        if let Load::Ready(items) = &self.clean {
            if self.selected.len() != items.len() {
                self.selected = items
                    .iter()
                    .map(|i| i.risk == Risk::Safe && !i.protected)
                    .collect();
            }
        }
    }

    fn switch_tab(&mut self, tab: usize) {
        self.tab = tab % N_TABS;
        self.ensure(self.tab);
        if self.tab == 5 {
            self.refresh_status(true);
        }
        self.status = None;
    }

    /// 刷新实时状态（仅在状态页，且距上次 >=1s）
    fn refresh_status(&mut self, force: bool) {
        if self.tab != 5 {
            return;
        }
        let now = std::time::Instant::now();
        let due = self
            .status_at
            .map(|t| now.duration_since(t) >= Duration::from_millis(1000))
            .unwrap_or(true);
        if force || due {
            self.live = Some(status::collect_live(&mut self.cpu));
            self.status_at = Some(now);
        }
    }

    fn next_tab(&mut self, delta: isize) {
        let t = (self.tab as isize + delta).rem_euclid(N_TABS as isize) as usize;
        self.switch_tab(t);
    }

    fn current_len(&self) -> usize {
        match self.tab {
            0 => self.clean.ready().map_or(0, |v| v.len()),
            1 => self.overview.ready().map_or(0, |v| v.len()),
            2 => self.large.ready().map_or(0, |v| v.len()),
            3 => self.dupes.ready().map_or(0, |v| v.len()),
            4 => self.apps.ready().map_or(0, |v| v.len()),
            _ => 0,
        }
    }

    fn cursor(&self) -> usize {
        self.list_states[self.tab].selected().unwrap_or(0)
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.current_len();
        if len == 0 {
            return;
        }
        let next = (self.cursor() as isize + delta).clamp(0, len as isize - 1);
        self.list_states[self.tab].select(Some(next as usize));
    }

    fn selected_count(&self) -> usize {
        self.selected.iter().filter(|s| **s).count()
    }

    fn selected_total(&self) -> u64 {
        self.clean
            .ready()
            .map(|items| {
                let chosen: Vec<CleanItem> = items
                    .iter()
                    .zip(&self.selected)
                    .filter(|(_, s)| **s)
                    .map(|(i, _)| i.clone())
                    .collect();
                // 与真实执行共用安全门；先去重嵌套，再按安全门计算可释放量
                let chosen = scan::top_level(&chosen);
                clean::plan(&chosen).approved_bytes()
            })
            .unwrap_or(0)
    }

    fn select_kind(&mut self, safe_only: bool, value: bool) {
        if let Load::Ready(items) = &self.clean {
            if safe_only {
                for (i, it) in items.iter().enumerate() {
                    self.selected[i] = value && it.risk == Risk::Safe && !it.protected;
                }
            } else {
                self.selected.iter_mut().for_each(|s| *s = value);
            }
        }
    }

    fn toggle(&mut self) {
        let i = self.cursor();
        if i < self.selected.len() {
            // 保护名单项不可勾选
            if let Load::Ready(items) = &self.clean {
                if items.get(i).map(|it| it.protected).unwrap_or(false) {
                    self.status = Some("该项已在保护名单，thin protect remove 后可清理".into());
                    return;
                }
            }
            self.selected[i] = !self.selected[i];
        }
    }

    /// 把当前清理项加入保护名单（thin protect），并就地更新标记
    fn protect_current(&mut self) {
        let i = self.cursor();
        let path = match &self.clean {
            Load::Ready(items) => items.get(i).map(|it| it.path.clone()),
            _ => None,
        };
        let Some(path) = path else { return };
        match protect::add(&path) {
            Ok(canon) => {
                if let Load::Ready(items) = &mut self.clean {
                    if let Some(it) = items.get_mut(i) {
                        it.protected = true;
                    }
                }
                if i < self.selected.len() {
                    self.selected[i] = false;
                }
                self.status = Some(format!("已保护 {}（含子目录）", canon.display()));
            }
            Err(e) => self.status = Some(format!("保护失败: {e}")),
        }
    }

    fn reload_current(&mut self) {
        match self.tab {
            0 => {
                self.clean = Load::Idle;
                self.selected.clear();
            }
            1 => self.overview = Load::Idle,
            2 => self.large = Load::Idle,
            3 => self.dupes = Load::Idle,
            4 => self.apps = Load::Idle,
            _ => {}
        }
        self.ensure(self.tab);
        self.status = Some("重新加载…".into());
    }

    fn apply(&mut self) {
        self.confirm = false;
        let chosen: Vec<CleanItem> = match &self.clean {
            Load::Ready(items) => items
                .iter()
                .zip(&self.selected)
                .filter(|(_, s)| **s)
                .map(|(i, _)| i.clone())
                .collect(),
            _ => return,
        };
        // 只处理最顶层项，避免父子路径重复计入/重复移动
        let chosen = scan::top_level(&chosen);
        if chosen.is_empty() {
            self.status = Some("未勾选任何项".into());
            return;
        }

        match clean::quarantine(&chosen, false) {
            Ok(j) => {
                let moved: std::collections::HashSet<PathBuf> =
                    j.entries.iter().map(|e| e.original.clone()).collect();
                if let Load::Ready(items) = std::mem::replace(&mut self.clean, Load::Idle) {
                    let mut new_items = Vec::new();
                    let mut new_sel = Vec::new();
                    for (it, sel) in items.into_iter().zip(std::mem::take(&mut self.selected)) {
                        // 丢弃已移动的项，以及被已移动父目录覆盖的嵌套子项
                        let gone = moved.contains(&it.path)
                            || moved
                                .iter()
                                .any(|m| it.path != *m && it.path.starts_with(m));
                        if !gone {
                            new_sel.push(sel);
                            new_items.push(it);
                        }
                    }
                    self.selected = new_sel;
                    self.clean = Load::Ready(new_items);
                }
                let c = self.cursor().min(self.current_len().saturating_sub(1));
                self.list_states[0].select(Some(c));
                let mut msg = format!(
                    "已移入隔离区 {} 项 · {}  （会话 {}）",
                    j.entries.len(),
                    human(j.total_size()),
                    j.session
                );
                if !j.skipped.is_empty() {
                    msg.push_str(&format!("  跳过 {} 项", j.skipped.len()));
                }
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
            KeyCode::Tab | KeyCode::Char('l') => self.next_tab(1),
            KeyCode::BackTab | KeyCode::Char('h') => self.next_tab(-1),
            KeyCode::Char(c @ '1'..='6') => self.switch_tab((c as u8 - b'1') as usize),
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => self.list_states[self.tab].select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => {
                let len = self.current_len();
                if len > 0 {
                    self.list_states[self.tab].select(Some(len - 1));
                }
            }
            KeyCode::Char(' ') if self.tab == 0 => self.toggle(),
            KeyCode::Char('a') if self.tab == 0 => self.select_kind(true, true),
            KeyCode::Char('A') if self.tab == 0 => self.select_kind(false, true),
            KeyCode::Char('n') if self.tab == 0 => self.select_kind(false, false),
            KeyCode::Char('p') if self.tab == 0 => self.protect_current(),
            KeyCode::Char('c') if self.tab == 0 => {
                if self.selected_count() > 0 {
                    self.confirm = true;
                } else {
                    self.status = Some("未勾选任何项".into());
                }
            }
            KeyCode::Char('r') => self.reload_current(),
            KeyCode::Char('?') => self.help = !self.help,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 入口 / 事件循环
// ---------------------------------------------------------------------------

pub fn run(root: PathBuf, min: u64) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(root, min);
    app.ensure(0);
    let res = event_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    while !app.quit {
        app.poll_loaders();
        app.refresh_status(false);
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|f| ui(f, app))?;
        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.on_key(key.code);
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 绘制
// ---------------------------------------------------------------------------

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
fn home_prefix() -> String {
    let raw = std::env::var("HOME").unwrap_or_default();
    if raw.is_empty() {
        return raw;
    }
    std::fs::canonicalize(&raw)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(raw)
}

fn ui(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_header(frame, chunks[0]);
    render_tabs(frame, app, chunks[1]);
    render_body(frame, app, chunks[2]);
    render_footer(frame, app, chunks[3]);

    if app.confirm {
        render_confirm(frame, app);
    }
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
        Span::raw("  M5 · 预设/历史/定时"),
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
        1 => render_overview(frame, app, area),
        2 => render_large(frame, app, area),
        3 => render_dupes(frame, app, area),
        4 => render_apps(frame, app, area),
        5 => render_status(frame, app, area),
        _ => {}
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

/// 概览：磁盘占用 treemap
fn render_overview(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("硬盘占用（{}）", app.root.display()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match &app.overview {
        Load::Ready(children) => {
            if children.is_empty() {
                state_msg(frame, inner, "（空）");
                return;
            }
            // 取前 12 项，其余合并为「其他」
            let mut entries: Vec<(String, u64)> = children
                .iter()
                .take(12)
                .map(|(p, s)| {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.display().to_string());
                    (name, *s)
                })
                .collect();
            if children.len() > 12 {
                let rest: u64 = children.iter().skip(12).map(|(_, s)| *s).sum();
                if rest > 0 {
                    entries.push(("其他".into(), rest));
                }
            }
            treemap::render(frame, inner, &entries);
        }
        Load::Loading { progress, .. } => {
            render_loading(frame, inner, Some(progress), app.tick, "统计目录占用中…")
        }
        Load::Failed(e) => state_msg(frame, inner, e),
        Load::Idle => state_msg(frame, inner, "等待加载"),
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
            let list_items: Vec<ListItem> = items
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
                        Span::styled(format!("{mark} "), Style::default().fg(Color::Green)),
                        Span::styled(
                            format!("{:>9} ", human(it.size)),
                            Style::default().fg(Color::White),
                        ),
                        Span::styled(
                            format!(
                                "{:<5} ",
                                if it.protected {
                                    "已保护"
                                } else {
                                    it.risk.label()
                                }
                            ),
                            Style::default().fg(if it.protected {
                                Color::DarkGray
                            } else {
                                risk_color(it.risk)
                            }),
                        ),
                        Span::raw(it.name.clone()),
                        Span::styled(format!("  {shown}"), Style::default().fg(Color::DarkGray)),
                    ]))
                })
                .collect();

            let title = format!(
                "清理项 · 已选 {} · {}",
                app.selected_count(),
                human(app.selected_total())
            );
            let list = List::new(list_items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("› ");
            frame.render_stateful_widget(list, parts[0], &mut app.list_states[app.tab]);

            // 详情
            let cur = app.cursor();
            let text = if let Some(it) = items.get(cur) {
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
                            } else {
                                it.risk.label()
                            }
                            .to_string(),
                            Style::default().fg(if it.protected {
                                Color::DarkGray
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

/// 大文件
fn render_large(frame: &mut Frame, app: &mut App, area: Rect) {
    let home = home_prefix();
    match &app.large {
        Load::Ready(files) => {
            let items: Vec<ListItem> = files
                .iter()
                .map(|f| {
                    let path = f.path.display().to_string();
                    let shown = if !home.is_empty() && path.starts_with(&home) {
                        path.replacen(&home, "~", 1)
                    } else {
                        path
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{:>9} ", human(f.size)),
                            Style::default().fg(Color::White),
                        ),
                        Span::raw(shown),
                    ]))
                })
                .collect();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("大文件 · {} 个", files.len())),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("› ");
            frame.render_stateful_widget(list, area, &mut app.list_states[app.tab]);
        }
        Load::Loading { progress, .. } => {
            render_loading(frame, area, Some(progress), app.tick, "扫描大文件中…")
        }
        Load::Failed(e) => state_msg(frame, area, e),
        Load::Idle => state_msg(frame, area, "等待加载"),
    }
}

/// 重复文件
fn render_dupes(frame: &mut Frame, app: &mut App, area: Rect) {
    let home = home_prefix();
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(area);

    match &app.dupes {
        Load::Ready(groups) => {
            let items: Vec<ListItem> = groups
                .iter()
                .map(|g| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{:>9} ", human(g.wasted())),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::raw(format!("× {} 同内容", g.paths.len())),
                    ]))
                })
                .collect();
            let total: u64 = groups.iter().map(|g| g.wasted()).sum();
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "重复组 · {} 组 · 可省 {}",
                    groups.len(),
                    human(total)
                )))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("› ");
            frame.render_stateful_widget(list, parts[0], &mut app.list_states[app.tab]);

            let mut text = Vec::new();
            if let Some(g) = groups.get(app.cursor()) {
                text.push(Line::from(Span::styled(
                    format!("{} 个副本 · 每组保留首个", g.paths.len()),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                text.push(Line::from(""));
                for p in &g.paths {
                    let s = p.display().to_string();
                    let shown = if !home.is_empty() && s.starts_with(&home) {
                        s.replacen(&home, "~", 1)
                    } else {
                        s
                    };
                    text.push(Line::from(shown));
                }
            }
            frame.render_widget(
                Paragraph::new(text)
                    .block(Block::default().borders(Borders::ALL).title("副本"))
                    .wrap(Wrap { trim: true }),
                parts[1],
            );
        }
        Load::Loading { progress, .. } => {
            render_loading(
                frame,
                parts[0],
                Some(progress),
                app.tick,
                "检测重复中…（需读取内容）",
            );
            state_msg(frame, parts[1], "");
        }
        Load::Failed(e) => state_msg(frame, parts[0], e),
        Load::Idle => state_msg(frame, parts[0], "等待加载"),
    }
}

/// 应用
fn render_apps(frame: &mut Frame, app: &mut App, area: Rect) {
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
                        Span::raw(truncate(&a.name, 26)),
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

/// 实时状态 + 基本信息
fn render_status(frame: &mut Frame, app: &mut App, area: Rect) {
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(area);

    // 左：基本信息
    let mut info: Vec<Line> = Vec::new();
    if let Some(i) = &app.sysinfo {
        info.push(field("型号", &i.model));
        info.push(field("芯片", &i.chip));
        info.push(field(
            "核心",
            &format!("{} 物理 / {} 逻辑", i.physical_cores, i.logical_cores),
        ));
        info.push(field(
            "系统",
            &format!("macOS {} ({})", i.os_version, i.os_build),
        ));
        info.push(field("主机名", &i.hostname));
        info.push(field("运行时间", &status::format_uptime(i.uptime_secs)));
        info.push(Line::from(""));
        info.push(field("内存总量", &human(i.mem_total)));
    } else {
        info.push(Line::from("采集失败"));
    }
    frame.render_widget(
        Paragraph::new(info)
            .block(Block::default().borders(Borders::ALL).title("基本信息"))
            .wrap(Wrap { trim: true }),
        parts[0],
    );

    // 右：实时指标
    let mut lines: Vec<Line> = Vec::new();
    if let Some(l) = &app.live {
        lines.push(metric_line(
            "CPU",
            l.cpu_usage / 100.0,
            Some(format!("{:.1}%", l.cpu_usage)),
        ));
        let mem_ratio = if l.mem_total > 0 {
            l.mem_used as f64 / l.mem_total as f64
        } else {
            0.0
        };
        lines.push(metric_line(
            "内存",
            mem_ratio,
            Some(format!("{} / {}", human(l.mem_used), human(l.mem_total))),
        ));
        let disk_ratio = if l.disk_total > 0 {
            l.disk_used as f64 / l.disk_total as f64
        } else {
            0.0
        };
        lines.push(metric_line(
            "磁盘",
            disk_ratio,
            Some(format!("{} / {}", human(l.disk_used), human(l.disk_total))),
        ));
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "负载     {:.2}  {:.2}  {:.2}",
            l.load1, l.load5, l.load15
        )));
        lines.push(Line::from(format!(
            "有线内存 {}    压缩内存 {}",
            human(l.mem_wired),
            human(l.mem_compressed)
        )));
        if l.swap_total > 0 {
            lines.push(Line::from(format!(
                "交换空间 {} / {}",
                human(l.swap_used),
                human(l.swap_total)
            )));
        }
        if let Some(b) = &l.battery {
            lines.push(Line::from(format!(
                "电池     {}%  {}",
                b.percent,
                if b.charging { "充电中" } else { "使用中" }
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "每 1s 刷新",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        lines.push(Line::from("采集中…"));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("实时状态"))
            .wrap(Wrap { trim: true }),
        parts[1],
    );
}

/// 带颜色的指标行：名称 + 进度条 + 数值
fn metric_line(name: &str, ratio: f64, value: Option<String>) -> Line<'static> {
    let pct = ratio * 100.0;
    let color = if pct > 90.0 {
        Color::Red
    } else if pct > 75.0 {
        Color::Yellow
    } else {
        Color::Green
    };
    let mut spans = vec![
        Span::raw(format!("{name:<4} ")),
        Span::styled(bar(ratio, 22), Style::default().fg(color)),
    ];
    if let Some(v) = value {
        spans.push(Span::raw(format!("  {v}")));
    }
    Line::from(spans)
}

/// 基本信息的一行：标签 + 值
fn field(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let text = if let Some(s) = &app.status {
        s.clone()
    } else if app.help {
        " 1-6/Tab 切换标签 · ↑↓/jk 移动 · space 勾选 · a 选安全 · A 全选 · n 清空 · p 保护 · r 重载 · c 清理 · q 退出"
            .into()
    } else {
        match app.tab {
            0 => " Tab 切页 · ↑↓ 移动 · space 勾选 · a 选安全 · A 全选 · n 清空 · p 保护·不清理 · c 移入隔离区 · r 重载 · ? 帮助 · q 退出",
            _ => " 1-6/Tab 切换标签 · ↑↓/jk 移动 · r 重载 · ? 帮助 · q 退出",
        }
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
        Line::from(format!("预计可释放 {}", human(app.selected_total()))),
        Line::from(""),
        Line::from(Span::styled(
            "移入后可随时恢复（thin quarantine restore）",
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

fn truncate(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    let mut out: String = chars[..width.saturating_sub(1)].iter().collect();
    out.push('…');
    out
}
