//! `thin browse [PATH]`：交互式文件浏览器（实验）。
//!
//! 一步步下钻，进入某目录才计算其直接子项的大小，并把能识别的目录标注用途。
//! 只读：不触发任何清理。按 `?` 查看快捷键。

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
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
use thin_core::clean;
use thin_core::fmt::human;
use thin_core::fsutil::{self, EntryKind};
use thin_core::model::{Category, CleanItem, Explain, Risk};
use thin_core::progress::{Progress, ProgressSnapshot};
use thin_core::recognize::{Recognition, Recognizer, Source};

use crate::treemap;

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

#[derive(Clone, Copy, PartialEq)]
enum SortMode {
    Size,
    Name,
    Kind,
}

impl SortMode {
    fn label(self) -> &'static str {
        match self {
            SortMode::Size => "大小",
            SortMode::Name => "名称",
            SortMode::Kind => "类型",
        }
    }
    fn next(self) -> Self {
        match self {
            SortMode::Size => SortMode::Name,
            SortMode::Name => SortMode::Kind,
            SortMode::Kind => SortMode::Size,
        }
    }
}

pub struct BrowseState {
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
    show_treemap: bool,
    sort: SortMode,
    filter: String,
    filtering: bool,
    confirm: bool,
    bookmarks: Vec<PathBuf>,
    bookmark_pick: bool,
    bookmark_sel: usize,
    help: bool,
    tick: usize,
    status: Option<String>,
    status_at: usize,
    /// 是否嵌入在主 TUI 标签页（影响页脚文案）
    embedded: bool,
}

impl BrowseState {
    pub fn new(root: PathBuf) -> Self {
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
            show_treemap: false,
            sort: SortMode::Size,
            filter: String::new(),
            filtering: false,
            confirm: false,
            bookmarks: load_bookmarks(),
            bookmark_pick: false,
            bookmark_sel: 0,
            help: false,
            tick: 0,
            status: None,
            status_at: 0,
            embedded: false,
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

    pub fn poll(&mut self) {
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
                    self.set_status(format!("加载失败: {e}"));
                }
            }
            Err(TryRecvError::Empty) => self.loader = Some(loader),
            Err(TryRecvError::Disconnected) => {
                if loader.generation == self.generation {
                    self.set_status("加载中断");
                }
            }
        }
    }

    /// 标记为嵌入模式（主 TUI 标签页）
    pub fn set_embedded(&mut self) {
        self.embedded = true;
    }

    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        // 瞬时提示几秒后自动消失，恢复底部快捷键提示
        if self.status.is_some() && self.tick.wrapping_sub(self.status_at) > 45 {
            self.status = None;
        }
    }

    /// 设置底部瞬时提示（带自动消失计时）
    fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some(s.into());
        self.status_at = self.tick;
    }

    fn rebuild_visible(&mut self) {
        let needle = self.filter.to_lowercase();
        let show_hidden = self.show_hidden;
        let mut idx: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| show_hidden || !is_hidden(&r.child.path))
            .filter(|(_, r)| needle.is_empty() || row_matches(r, &needle))
            .map(|(i, _)| i)
            .collect();
        match self.sort {
            SortMode::Size => {
                idx.sort_by(|&a, &b| self.rows[b].child.size.cmp(&self.rows[a].child.size))
            }
            SortMode::Name => {
                idx.sort_by(|&a, &b| name_key(&self.rows[a]).cmp(&name_key(&self.rows[b])))
            }
            SortMode::Kind => idx.sort_by(|&a, &b| {
                kind_rank(self.rows[a].child.kind)
                    .cmp(&kind_rank(self.rows[b].child.kind))
                    .then(self.rows[b].child.size.cmp(&self.rows[a].child.size))
            }),
        }
        self.visible = idx;
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
            self.set_status("只能进入目录");
        }
    }

    fn up(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
            self.load();
        } else {
            self.set_status("已在起点");
        }
    }

    fn reload(&mut self) {
        let dir = self.cwd().to_path_buf();
        self.cache.remove(&dir);
        self.load();
    }

    /// 把当前项移入隔离区（复用 clean 安全门）
    fn clean_current(&mut self) {
        let Some(row) = self.current().cloned() else {
            return;
        };
        let rec = row.rec;
        let item = CleanItem {
            rule_id: rec.rule_id.clone().unwrap_or_else(|| "browse".into()),
            name: rec.title.clone(),
            path: row.child.path.clone(),
            category: rec.category.unwrap_or(Category::Other),
            risk: rec.risk.unwrap_or(Risk::Confirm),
            regenerable: rec.safety == Safety::Regenerable,
            sudo: false,
            size: row.child.size,
            reclaim: String::new(),
            explain: rec.explain.clone().unwrap_or(Explain {
                what: rec.note.clone(),
                cost: String::new(),
                recover: String::new(),
            }),
            protected: rec.protected,
        };
        match clean::quarantine(&[item], false) {
            Ok(journal) if journal.entries.is_empty() => {
                let reason = journal
                    .skipped
                    .first()
                    .map(|s| s.reason.clone())
                    .unwrap_or_default();
                self.set_status(format!("未清理：{reason}"));
            }
            Ok(journal) => {
                self.set_status(format!(
                    "已移入隔离区（thin quarantine restore {}）",
                    journal.session
                ));
                let dir = self.cwd().to_path_buf();
                self.cache.remove(&dir);
                self.load();
            }
            Err(e) => self.set_status(format!("清理失败: {e:#}")),
        }
    }

    /// 处理按键，返回 true 表示请求离开浏览（退出或切回其它标签）
    pub fn on_key(&mut self, code: KeyCode) -> bool {
        if self.confirm {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.confirm = false;
                    self.clean_current();
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.confirm = false,
                _ => {}
            }
            return false;
        }
        if self.bookmark_pick {
            match code {
                KeyCode::Char('j') | KeyCode::Down => {
                    if !self.bookmarks.is_empty() {
                        self.bookmark_sel = (self.bookmark_sel + 1).min(self.bookmarks.len() - 1);
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.bookmark_sel = self.bookmark_sel.saturating_sub(1);
                }
                KeyCode::Enter => {
                    if let Some(p) = self.bookmarks.get(self.bookmark_sel).cloned() {
                        self.bookmark_pick = false;
                        self.stack = vec![p];
                        self.load();
                    }
                }
                KeyCode::Char('d') => {
                    if self.bookmark_sel < self.bookmarks.len() {
                        self.bookmarks.remove(self.bookmark_sel);
                        if self.bookmark_sel >= self.bookmarks.len() {
                            self.bookmark_sel = self.bookmarks.len().saturating_sub(1);
                        }
                        let _ = save_bookmarks(&self.bookmarks);
                    }
                }
                KeyCode::Char('B') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.bookmark_pick = false
                }
                _ => {}
            }
            return false;
        }
        if self.filtering {
            match code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filtering = false;
                    self.rebuild_visible();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.rebuild_visible();
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.rebuild_visible();
                }
                _ => {}
            }
            return false;
        }
        if self.help && !matches!(code, KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q')) {
            self.help = false;
            return false;
        }
        if matches!(code, KeyCode::Esc) && self.loader.is_some() {
            // 取消正在进行的加载
            self.loader = None;
            self.set_status("已取消加载");
            return false;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
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
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('t') => self.show_treemap = !self.show_treemap,
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.rebuild_visible();
                self.set_status(format!("排序: {}", self.sort.label()));
            }
            KeyCode::Char('b') => {
                let dir = self.cwd().to_path_buf();
                if let Some(pos) = self.bookmarks.iter().position(|p| *p == dir) {
                    self.bookmarks.remove(pos);
                    self.set_status("已取消书签");
                } else {
                    self.bookmarks.push(dir);
                    self.set_status("已加书签（B 打开列表）");
                }
                let _ = save_bookmarks(&self.bookmarks);
            }
            KeyCode::Char('B') => {
                self.bookmark_pick = true;
                self.bookmark_sel = 0;
            }
            KeyCode::Char('c') => {
                if let Some(row) = self.current() {
                    if row.rec.cleanable && !row.rec.protected {
                        self.confirm = true;
                    } else {
                        self.set_status("该项不可清理（未被规则命中或受保护）");
                    }
                }
            }
            KeyCode::Char('?') => self.help = !self.help,
            _ => {}
        }
        false
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
            depth: 1,
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

    let mut state = BrowseState::new(root);
    let res = event_loop(&mut terminal, &mut state);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &mut BrowseState,
) -> Result<()> {
    let mut quit = false;
    while !quit {
        state.poll();
        state.tick();
        terminal.draw(|f| state.render(f, f.area()))?;
        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('c'))
                    {
                        quit = true;
                    } else if key.modifiers.is_empty() && state.on_key(key.code) {
                        quit = true;
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 绘制
// ---------------------------------------------------------------------------

/// 在给定区域内绘制浏览界面（独立运行或嵌入 TUI 标签页均可）
pub fn render(frame: &mut Frame, state: &mut BrowseState, area: Rect) {
    // 内嵌到 TUI 标签页时砍掉自带的标题行（面包屑），当前路径改放到列表块标题
    let header_h = if state.embedded { 0 } else { 2 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);

    if !state.embedded {
        render_header(frame, state, chunks[0]);
    }
    render_body(frame, state, chunks[1]);
    render_footer(frame, state, chunks[2]);

    if state.help {
        render_help(frame, area);
    }
    if state.confirm {
        render_confirm(frame, state);
    }
    if state.bookmark_pick {
        render_bookmarks(frame, state);
    }
}

impl BrowseState {
    /// 在给定区域绘制（方法形式，便于 `terminal.draw`）
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        render(frame, self, area);
    }
}

fn render_header(frame: &mut Frame, app: &BrowseState, area: Rect) {
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

fn render_body(frame: &mut Frame, app: &mut BrowseState, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(area);

    if let Some(loader) = &app.loader {
        let title = app.cwd().display().to_string();
        render_loading(frame, chunks[0], Some(&loader.progress), app.tick, &title);
    } else {
        render_list(frame, app, chunks[0]);
    }
    if app.show_treemap {
        render_treemap(frame, app, chunks[1]);
    } else {
        render_detail(frame, app, chunks[1]);
    }
}

/// 当前层占用图（按体积切分矩形）
fn render_treemap(frame: &mut Frame, app: &BrowseState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("占用图 (t 切换)");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows: Vec<&Row> = app.visible.iter().map(|&i| &app.rows[i]).collect();
    if rows.is_empty() {
        return;
    }
    let values: Vec<u64> = rows.iter().map(|r| r.child.size.max(1)).collect();
    for (rect, idx) in treemap::layout(inner, &values) {
        let row = rows[idx];
        let color = status_color(&row.rec);
        frame.render_widget(Block::default().style(Style::default().bg(color)), rect);
        if rect.width >= 6 && rect.height >= 1 {
            let name = row
                .child
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            frame.render_widget(
                Paragraph::new(truncate(&name, rect.width.saturating_sub(1) as usize))
                    .style(Style::default().fg(Color::Black)),
                rect,
            );
        }
    }
}

fn render_list(frame: &mut Frame, app: &mut BrowseState, area: Rect) {
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

    // 内嵌时无面包屑，当前路径放在块标题里（避免与标题行重复）
    let meta = format!(
        "{} 项 · 排序:{}{}",
        app.visible.len(),
        app.sort.label(),
        if app.show_hidden { " · 含隐藏" } else { "" }
    );
    let title = if app.embedded {
        format!("{}  ·  {meta}", app.cwd().display())
    } else {
        meta
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_detail(frame: &mut Frame, app: &BrowseState, area: Rect) {
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

fn render_footer(frame: &mut Frame, app: &BrowseState, area: Rect) {
    let text = if app.confirm {
        "移入隔离区？ y 确认 / n 取消".into()
    } else if app.bookmark_pick {
        "书签：j/k 选择 · Enter 跳转 · d 删除 · Esc 关闭".into()
    } else if app.filtering {
        format!("过滤: {}▏  Enter 确认 · Esc 清除", app.filter)
    } else if let Some(s) = &app.status {
        s.clone()
    } else if app.loader.is_some() {
        "加载中…  Esc 取消".into()
    } else if app.embedded {
        " ↑↓ 移动 · Enter 进入 · Backspace 上级 · / 过滤 · s 排序 · t 占用图 · b 书签 · c 清理 · q 返回".into()
    } else {
        " ↑↓ 移动 · Enter 进入 · Backspace 上级 · / 过滤 · s 排序 · t 占用图 · b 书签 · c 清理 · ? 帮助 · q 退出"
            .into()
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

fn render_bookmarks(frame: &mut Frame, app: &BrowseState) {
    let a = centered_rect(72, 60, frame.area());
    frame.render_widget(Clear, a);
    let lines: Vec<Line> = if app.bookmarks.is_empty() {
        vec![Line::from(Span::styled(
            "（暂无书签，按 b 把当前目录加入）",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.bookmarks
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let mark = if i == app.bookmark_sel { "› " } else { "  " };
                let style = if i == app.bookmark_sel {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(format!("{mark}{}", p.display()), style))
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title("书签  Enter 跳转 · d 删除 · Esc 关闭"),
        ),
        a,
    );
}

fn render_confirm(frame: &mut Frame, app: &BrowseState) {
    let Some(row) = app.current() else {
        return;
    };
    let a = centered_rect(64, 25, frame.area());
    frame.render_widget(Clear, a);
    let text = vec![
        Line::from(Span::styled(
            "移入隔离区？",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(row.child.path.display().to_string()),
        Line::from(format!(
            "用途: {}  大小: {}",
            row.rec.title,
            human(row.child.size)
        )),
        Line::from(""),
        Line::from(Span::styled(
            "y 确认   n / Esc 取消",
            Style::default().fg(Color::Yellow),
        )),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL)),
        a,
    );
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
        Line::from("  /            过滤（按名称/用途）"),
        Line::from("  s            切换排序（大小/名称/类型）"),
        Line::from("  t            占用图切换"),
        Line::from("  b / B        加/删当前目录书签 · 打开书签列表"),
        Line::from("  c            把当前可清理项移入隔离区（需确认）"),
        Line::from("  r            重新计算当前目录"),
        Line::from("  .            显示/隐藏 . 开头项"),
        Line::from("  Esc          取消加载 / 退出"),
        Line::from("  q            退出"),
        Line::from(""),
        Line::from(Span::styled(
            "浏览只读；只有显式按 c 并确认才会移入隔离区（可恢复）。",
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

/// 过滤匹配：名称或用途标题包含关键字（均已小写）
fn name_key(r: &Row) -> String {
    r.child
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn kind_rank(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::Dir => 0,
        EntryKind::Mount => 1,
        EntryKind::File => 2,
        EntryKind::Symlink => 3,
        EntryKind::Inaccessible => 4,
    }
}

fn bookmarks_path() -> PathBuf {
    clean::thin_home().join("bookmarks.json")
}

fn load_bookmarks() -> Vec<PathBuf> {
    std::fs::read_to_string(bookmarks_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<PathBuf>>(&s).ok())
        .unwrap_or_default()
}

fn save_bookmarks(list: &[PathBuf]) -> Result<()> {
    let p = bookmarks_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(p, serde_json::to_vec_pretty(list)?)?;
    Ok(())
}

fn row_matches(r: &Row, needle: &str) -> bool {
    let name = r
        .child
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    name.contains(needle) || r.rec.title.to_lowercase().contains(needle)
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
