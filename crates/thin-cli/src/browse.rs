//! `thin browse [PATH]`：交互式文件浏览器（实验）。
//!
//! 一步步下钻，进入某目录才计算其直接子项的大小，并把能识别的目录标注用途。
//! 只读：不触发任何清理。按 `?` 查看快捷键。

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
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;
use thin_core::catalog::Safety;
use thin_core::fmt::human;
use thin_core::fsutil::{self, EntryKind};
use thin_core::progress::{Progress, ProgressSnapshot};
use thin_core::recognize::{Recognition, Recognizer, Source};

#[derive(clap::Args)]
pub struct BrowseArgs {
    /// 起始目录
    #[arg(default_value = "~")]
    pub path: String,
}

/// 一行：子项 + 用途识别
#[derive(Clone)]
struct Row {
    child: fsutil::ChildEntry,
    rec: Recognition,
}

struct Loader {
    generation: u64,
    dir: PathBuf,
    rx: Receiver<std::result::Result<Vec<Row>, String>>,
    progress: Arc<Progress>,
}

struct App {
    stack: Vec<PathBuf>,
    rows: Vec<Row>,
    /// 过滤后可见的下标（映射到 rows）
    visible: Vec<usize>,
    selected: usize,
    list_state: ListState,
    cache: HashMap<PathBuf, Vec<Row>>,
    loader: Option<Loader>,
    generation: u64,
    show_hidden: bool,
    help: bool,
    quit: bool,
    tick: usize,
    status: Option<String>,
}

impl App {
    fn new(root: PathBuf) -> Self {
        let mut app = Self {
            stack: vec![root],
            rows: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
            cache: HashMap::new(),
            loader: None,
            generation: 0,
            show_hidden: false,
            help: false,
            quit: false,
            tick: 0,
            status: None,
        };
        app.load();
        app
    }

    fn cwd(&self) -> &Path {
        self.stack
            .last()
            .map(PathBuf::as_path)
            .unwrap_or(Path::new("/"))
    }

    fn load(&mut self) {
        let dir = self.cwd().to_path_buf();
        self.generation += 1;
        if let Some(rows) = self.cache.get(&dir) {
            self.rows = rows.clone();
            self.selected = 0;
            self.rebuild_visible();
            self.loader = None;
            return;
        }
        self.rows.clear();
        self.rebuild_visible();
        self.loader = Some(spawn_load(dir, self.generation));
    }

    fn poll(&mut self) {
        let Some(loader) = self.loader.take() else {
            return;
        };
        match loader.rx.try_recv() {
            Ok(Ok(rows)) => {
                if loader.generation == self.generation {
                    self.cache.insert(loader.dir.clone(), rows.clone());
                    self.rows = rows;
                    self.selected = 0;
                    self.rebuild_visible();
                }
            }
            Ok(Err(e)) => {
                if loader.generation == self.generation {
                    self.status = Some(format!("加载失败: {e}"));
                }
            }
            Err(TryRecvError::Empty) => self.loader = Some(loader),
            Err(TryRecvError::Disconnected) => {
                if loader.generation == self.generation {
                    self.status = Some("加载中断".into());
                }
            }
        }
    }

    fn rebuild_visible(&mut self) {
        self.visible = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| self.show_hidden || !is_hidden(&r.child.path))
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.visible.len() {
            self.selected = self.visible.len().saturating_sub(1);
        }
        self.list_state.select(if self.visible.is_empty() {
            None
        } else {
            Some(self.selected)
        });
    }

    fn current(&self) -> Option<&Row> {
        self.visible.get(self.selected).map(|i| &self.rows[*i])
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.visible.len();
        if len == 0 {
            return;
        }
        let next = (self.selected as isize + delta).clamp(0, len as isize - 1) as usize;
        self.selected = next;
        self.list_state.select(Some(next));
    }

    fn enter(&mut self) {
        let Some(row) = self.current() else { return };
        if matches!(row.child.kind, EntryKind::Dir | EntryKind::Mount) {
            let dir = row.child.path.clone();
            self.stack.push(dir);
            self.load();
        } else {
            self.status = Some("只能进入目录".into());
        }
    }

    fn up(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
            self.load();
        } else {
            self.status = Some("已在起点".into());
        }
    }

    fn reload(&mut self) {
        let dir = self.cwd().to_path_buf();
        self.cache.remove(&dir);
        self.load();
    }

    fn on_key(&mut self, code: KeyCode) {
        if self.help && !matches!(code, KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q')) {
            self.help = false;
            return;
        }
        if matches!(code, KeyCode::Esc) && self.loader.is_some() {
            // 取消正在进行的加载
            self.loader = None;
            self.status = Some("已取消加载".into());
            return;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => {
                self.selected = 0;
                self.list_state.select(Some(0));
            }
            KeyCode::Char('G') | KeyCode::End => {
                if !self.visible.is_empty() {
                    self.selected = self.visible.len() - 1;
                    self.list_state.select(Some(self.selected));
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.enter(),
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.up(),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                self.rebuild_visible();
            }
            KeyCode::Char('?') => self.help = !self.help,
            _ => {}
        }
    }
}

fn spawn_load(dir: PathBuf, generation: u64) -> Loader {
    let progress = Arc::new(Progress::new());
    let p = progress.clone();
    let (tx, rx) = mpsc::channel();
    let work_dir = dir.clone();
    std::thread::spawn(move || {
        let res = (|| -> std::result::Result<Vec<Row>, String> {
            let entries = fsutil::children_entries_progress(&work_dir, &p);
            let recognizer = Recognizer::load().map_err(|e| format!("{e:#}"))?;
            Ok(entries
                .into_iter()
                .map(|child| {
                    let rec = recognizer.recognize(&child.path);
                    Row { child, rec }
                })
                .collect())
        })();
        let _ = tx.send(res);
    });
    Loader {
        generation,
        dir,
        rx,
        progress,
    }
}

pub fn run(args: BrowseArgs) -> Result<()> {
    let root = fsutil::expand(&args.path).unwrap_or_else(|| PathBuf::from(&args.path));
    if !std::io::stdout().is_terminal() {
        // 非 TTY：退化为只读一层列表
        return crate::ls::run(crate::ls::LsArgs {
            path: args.path,
            all: false,
            long: true,
            json: false,
        });
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(root);
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
        app.poll();
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

fn ui(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_header(frame, app, chunks[0]);
    render_body(frame, app, chunks[1]);
    render_footer(frame, app, chunks[2]);

    if app.help {
        render_help(frame, frame.area());
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let home = home_prefix();
    let crumbs: Vec<Span> = breadcrumb(&app.stack, &home)
        .into_iter()
        .flat_map(|(name, is_last)| {
            let color = if is_last {
                Color::White
            } else {
                Color::DarkGray
            };
            vec![
                Span::styled(name, Style::default().fg(color)),
                Span::styled(" › ", Style::default().fg(Color::DarkGray)),
            ]
        })
        .collect();
    let mut line = vec![Span::styled(
        " 磁盘浏览 ",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    line.extend(crumbs);
    frame.render_widget(Paragraph::new(Line::from(line)), area);
}

fn render_body(frame: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(area);

    if let Some(loader) = &app.loader {
        let title = format!("{}", app.cwd().display());
        render_loading(frame, chunks[0], Some(&loader.progress), app.tick, &title);
    } else {
        render_list(frame, app, chunks[0]);
    }
    render_detail(frame, app, chunks[1]);
}

fn render_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let home = home_prefix();
    let items: Vec<ListItem> = app
        .visible
        .iter()
        .map(|&i| {
            let row = &app.rows[i];
            let name = row
                .child
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let icon = match row.child.kind {
                EntryKind::Dir => "▸",
                EntryKind::Mount => "⛃",
                EntryKind::Symlink => "→",
                EntryKind::Inaccessible => "✕",
                EntryKind::File => " ",
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{icon} "),
                    Style::default().fg(kind_color(row.child.kind)),
                ),
                Span::styled(
                    format!("{:>9} ", size_cell(&row.child)),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("{:<10} ", truncate(&row.rec.title, 10)),
                    Style::default().fg(status_color(&row.rec)),
                ),
                Span::raw(name),
                Span::styled(
                    format!("  {}", shorten(&row.child.path, &home)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let title = format!(
        "{}  ·  {} 项{}",
        app.cwd().display(),
        app.visible.len(),
        if app.show_hidden {
            "（含隐藏）"
        } else {
            ""
        }
    );
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_detail(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("用途 / 详情");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(row) = app.current() else {
        frame.render_widget(
            Paragraph::new("（无可选项）").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };
    let r = &row.rec;
    let mut lines = vec![
        Line::from(Span::styled(
            r.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            row.child.path.display().to_string(),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(vec![
            Span::raw("类型: "),
            Span::styled(r.kind.label().to_string(), Style::default().fg(Color::Cyan)),
            Span::raw("   安全性: "),
            Span::styled(
                r.safety.label().to_string(),
                Style::default().fg(safety_color(r.safety)),
            ),
        ]),
        Line::from(vec![
            Span::raw("来源: "),
            Span::raw(source_label(r)),
            Span::raw(if r.protected { "   [受保护]" } else { "" }),
        ]),
        Line::from(""),
    ];
    if !r.note.is_empty() {
        lines.push(Line::from(Span::styled(
            "这是什么",
            Style::default().fg(Color::Cyan),
        )));
        lines.push(Line::from(r.note.clone()));
    }
    if let Some(ex) = &r.explain {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "删了会怎样",
            Style::default().fg(Color::Cyan),
        )));
        lines.push(Line::from(ex.cost.clone()));
        lines.push(Line::from(Span::styled(
            "能否恢复",
            Style::default().fg(Color::Cyan),
        )));
        lines.push(Line::from(ex.recover.clone()));
    }
    if let Some(reference) = &r.reference {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("参考: {reference}"),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let text = if let Some(s) = &app.status {
        s.clone()
    } else if app.loader.is_some() {
        "加载中…  Esc 取消".into()
    } else {
        " ↑↓ 移动 · Enter 进入 · Backspace 上级 · r 重载 · . 隐藏文件 · ? 帮助 · q 退出".into()
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn render_loading(
    frame: &mut Frame,
    area: Rect,
    progress: Option<&Arc<Progress>>,
    tick: usize,
    path: &str,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(path.to_string());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let snap: ProgressSnapshot = progress.map(|p| p.snapshot()).unwrap_or_default();
    if snap.total > 0 {
        let ratio = (snap.done as f64 / snap.total as f64).clamp(0.0, 1.0);
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(ratio)
            .label(format!(
                "{}/{}  {:.0}%",
                snap.done,
                snap.total,
                ratio * 100.0
            ));
        let a = centered_rect(60, 5, inner);
        frame.render_widget(Clear, a);
        frame.render_widget(gauge, a);
    } else {
        const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let spin = SPINNER[tick % SPINNER.len()];
        let msg = format!("{spin} {} …", snap.label);
        frame.render_widget(
            Paragraph::new(msg).alignment(Alignment::Center),
            centered_rect(60, 3, inner),
        );
    }
}

fn render_help(frame: &mut Frame, area: Rect) {
    let a = centered_rect(64, 50, area);
    frame.render_widget(Clear, a);
    let lines = vec![
        Line::from(Span::styled(
            "thin browse · 快捷键",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  ↑↓ / j k    移动"),
        Line::from("  Enter / →    进入目录"),
        Line::from("  Backspace / ← 返回上级"),
        Line::from("  g / G        跳到顶部 / 底部"),
        Line::from("  r            重新计算当前目录"),
        Line::from("  .            显示/隐藏 . 开头项"),
        Line::from("  Esc          取消加载 / 退出"),
        Line::from("  q            退出"),
        Line::from(""),
        Line::from(Span::styled(
            "只读浏览：不会移动或删除任何文件。",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("帮助")),
        a,
    );
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn breadcrumb(stack: &[PathBuf], home: &str) -> Vec<(String, bool)> {
    stack
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let name = if i == 0 {
                shorten(p, home)
            } else {
                p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.display().to_string())
            };
            (name, i + 1 == stack.len())
        })
        .collect()
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}

fn size_cell(child: &fsutil::ChildEntry) -> String {
    match child.kind {
        EntryKind::Symlink | EntryKind::Mount | EntryKind::Inaccessible if child.size == 0 => {
            "—".into()
        }
        _ => human(child.size),
    }
}

fn status_color(r: &Recognition) -> Color {
    if r.protected {
        return Color::DarkGray;
    }
    if r.cleanable {
        return match r.risk {
            Some(thin_core::model::Risk::Safe) => Color::Green,
            Some(thin_core::model::Risk::Confirm) => Color::Yellow,
            Some(thin_core::model::Risk::Destructive) => Color::Red,
            None => Color::White,
        };
    }
    if r.source == Source::Heuristic {
        return Color::Yellow;
    }
    safety_color(r.safety)
}

fn safety_color(s: Safety) -> Color {
    match s {
        Safety::Regenerable => Color::Green,
        Safety::Precious => Color::Yellow,
        Safety::Protected => Color::DarkGray,
        Safety::Unknown => Color::White,
    }
}

fn kind_color(kind: EntryKind) -> Color {
    match kind {
        EntryKind::Dir => Color::Blue,
        EntryKind::Mount => Color::Magenta,
        EntryKind::Symlink => Color::Cyan,
        EntryKind::Inaccessible => Color::Red,
        EntryKind::File => Color::DarkGray,
    }
}

fn source_label(r: &Recognition) -> String {
    match r.source {
        Source::Rule => format!("清理规则({})", r.rule_id.clone().unwrap_or_default()),
        Source::Catalog => "用途库".into(),
        Source::Protected => "安全门".into(),
        Source::Heuristic => "名称启发式".into(),
        Source::Unknown => "未识别".into(),
    }
}

fn home_prefix() -> String {
    let raw = std::env::var("HOME").unwrap_or_default();
    if raw.is_empty() {
        return raw;
    }
    std::fs::canonicalize(&raw)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(raw)
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

fn shorten(path: &Path, home: &str) -> String {
    let s = path.display().to_string();
    if !home.is_empty() && s.starts_with(home) {
        s.replacen(home, "~", 1)
    } else {
        s
    }
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(area.height.saturating_sub(height) / 2),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_and_truncate() {
        assert!(is_hidden(Path::new("/a/.vol")));
        assert!(!is_hidden(Path::new("/a/bin")));
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("ab", 4), "ab");
    }

    #[test]
    fn breadcrumb_marks_last() {
        let stack = vec![PathBuf::from("/Users/x"), PathBuf::from("/Users/x/Library")];
        let crumbs = breadcrumb(&stack, "/Users/x");
        assert_eq!(crumbs[0], ("~".to_string(), false));
        assert_eq!(crumbs[1], ("Library".to_string(), true));
    }

    #[test]
    fn size_cell_marks_unknown_kinds() {
        let mk = |kind| fsutil::ChildEntry {
            path: PathBuf::from("/x"),
            kind,
            size: 0,
            target: None,
        };
        assert_eq!(size_cell(&mk(EntryKind::Symlink)), "—");
        assert_eq!(size_cell(&mk(EntryKind::Mount)), "—");
        assert_eq!(size_cell(&mk(EntryKind::Dir)), "0 B");
    }
}
