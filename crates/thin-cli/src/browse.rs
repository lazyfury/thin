//! 文件浏览组件：主 TUI「浏览」标签页（键 5）。
//!
//! 一步步下钻，进入某目录才计算其直接子项的大小，并把能识别的目录标注用途。
//! 只读；`c` 可在确认后移入隔离区。按 `?` 查看快捷键。

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use thin_core::catalog::{PurposeKind, Safety};
use thin_core::clean;
use thin_core::fmt::human;
use thin_core::fsutil::{self, EntryKind};
use thin_core::history;
use thin_core::model::{Category, CleanItem, Explain, Risk};
use thin_core::progress::{Progress, ProgressSnapshot};
use thin_core::recognize::{Recognition, Recognizer, Source};

use crate::text;
use crate::toast::{self, Toast};
use crate::treemap;

/// 一行：子项 + 用途识别
#[derive(Clone)]
struct Row {
    child: fsutil::ChildEntry,
    rec: Recognition,
    /// 置顶的「..」虚拟行：回到上级目录（可越过浏览起点，直到 `/`）。
    parent: bool,
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
    help: bool,
    tick: usize,
    toast: Option<Toast>,
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
            help: false,
            tick: 0,
            toast: None,
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
        if let Some(rows) = self.cache.get(&dir).cloned() {
            self.set_rows(rows);
            self.loader = None;
            return;
        }
        self.rows.clear();
        self.rebuild_visible();
        self.loader = Some(spawn_load(dir, self.generation));
    }

    /// 设置真实子项列表，并在最前插入「..」虚拟行（回到上级）。
    fn set_rows(&mut self, mut rows: Vec<Row>) {
        let dir = self.cwd().to_path_buf();
        if let Some(p) = parent_row(&dir) {
            rows.insert(0, p);
        }
        self.rows = rows;
        self.selected = 0;
        self.rebuild_visible();
    }

    pub fn poll(&mut self) {
        let Some(loader) = self.loader.take() else {
            return;
        };
        match loader.rx.try_recv() {
            Ok(Ok(rows)) => {
                if loader.generation == self.generation {
                    self.cache.insert(loader.dir.clone(), rows.clone());
                    self.set_rows(rows);
                }
            }
            Ok(Err(e)) => {
                if loader.generation == self.generation {
                    self.error(format!("加载目录失败：{e}"));
                }
            }
            Err(TryRecvError::Empty) => self.loader = Some(loader),
            Err(TryRecvError::Disconnected) => {
                if loader.generation == self.generation {
                    self.error("加载目录中断");
                }
            }
        }
    }

    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        // info/warn 几秒后自动消失；error 保留到用户按 Esc 关闭
        if self.toast.as_ref().is_some_and(|t| t.expired(self.tick)) {
            self.toast = None;
        }
    }

    fn info(&mut self, s: impl Into<String>) {
        self.toast = Some(Toast::info(s, self.tick));
    }

    fn warn(&mut self, s: impl Into<String>) {
        self.toast = Some(Toast::warn(s, self.tick));
    }

    fn error(&mut self, s: impl Into<String>) {
        self.toast = Some(Toast::error(s, self.tick));
    }

    fn clear_status(&mut self) {
        self.toast = None;
    }

    fn rebuild_visible(&mut self) {
        let needle = self.filter.to_lowercase();
        let show_hidden = self.show_hidden;
        // 「..」虚拟行置顶，不参与过滤/排序。
        let parent_idx = self.rows.iter().position(|r| r.parent);
        let mut idx: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.parent)
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
        let mut visible: Vec<usize> = Vec::with_capacity(idx.len() + 1);
        if let Some(p) = parent_idx {
            visible.push(p);
        }
        visible.extend(idx);
        self.visible = visible;
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
        if matches!(self.current(), Some(r) if r.parent) {
            self.goto_parent();
            return;
        }
        let Some(row) = self.current() else { return };
        if matches!(row.child.kind, EntryKind::Dir | EntryKind::Mount) {
            let dir = row.child.path.clone();
            self.stack.push(dir);
            self.load();
        } else {
            self.warn("只能进入目录");
        }
    }

    /// 通过「..」虚拟行回到上级目录（可越过浏览起点，直到文件系统根）。
    fn goto_parent(&mut self) {
        let cur = self.cwd().to_path_buf();
        match cur.parent() {
            Some(parent) => {
                self.stack.push(parent.to_path_buf());
                self.load();
            }
            None => self.info("已在文件系统根目录"),
        }
    }

    fn up(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
            self.load();
        } else {
            self.info("已在浏览根目录");
        }
    }

    fn reload(&mut self) {
        let dir = self.cwd().to_path_buf();
        self.cache.remove(&dir);
        self.load();
    }

    /// 是否处于会消费普通字符的输入/模态状态。
    /// 主 TUI 据此决定是否把 Tab/数字等按键原样交给浏览页。
    pub fn captures_input(&self) -> bool {
        self.confirm || self.filtering || self.help
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
            protected_reason: None,
        };
        let mode = clean::default_mode();
        match clean::apply(&[item], mode) {
            Ok(applied) if applied.moved() == 0 => {
                let reason = match &applied {
                    clean::Applied::Trash(r) => r
                        .skipped
                        .first()
                        .map(|s| s.reason.clone())
                        .unwrap_or_default(),
                    clean::Applied::Quarantine(j) => j
                        .skipped
                        .first()
                        .map(|s| s.reason.clone())
                        .unwrap_or_default(),
                };
                self.warn(format!("未清理：{reason}"));
            }
            Ok(applied) => {
                // 与 CLI 一致：记录历史
                let mut rec = history::Record::new(match mode {
                    clean::Mode::Trash => "browse-trash",
                    clean::Mode::Quarantine => "browse",
                });
                rec.scanned = 1;
                rec.approved = applied.moved();
                rec.session = applied.session().map(str::to_string);
                rec.moved = applied.moved();
                rec.moved_bytes = applied.moved_bytes();
                rec.skipped = applied.skipped() + applied.failed();
                let _ = history::append(&rec);
                self.info(match mode {
                    clean::Mode::Trash => "已移入系统废纸篓（可在 Finder 恢复）".to_string(),
                    clean::Mode::Quarantine => format!(
                        "已移入隔离区；可恢复：thin quarantine restore {}",
                        applied.session().unwrap_or("")
                    ),
                });
                let dir = self.cwd().to_path_buf();
                self.cache.remove(&dir);
                self.load();
            }
            Err(e) => self.error(format!("清理失败：{e:#}")),
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
        // 帮助层：任意键先关闭帮助（含 Esc），避免 Esc 直接退出
        if self.help {
            self.help = false;
            return false;
        }
        if matches!(code, KeyCode::Esc) && self.loader.is_some() {
            // 取消正在进行的加载
            self.loader = None;
            self.info("已取消加载");
            return false;
        }
        // Esc：先关闭底部提示，再按一次才退出（与其它标签页一致）
        if matches!(code, KeyCode::Esc) && self.toast.is_some() {
            self.clear_status();
            return false;
        }
        match code {
            KeyCode::Esc => return true,
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
                self.info(format!("排序：{}", self.sort.label()));
            }
            KeyCode::Char('c') => {
                if let Some(row) = self.current() {
                    if row.rec.cleanable && !row.rec.protected && !row.parent {
                        self.confirm = true;
                    } else {
                        self.warn("该路径不可清理（未被规则命中或已在保护名单）");
                    }
                }
            }
            KeyCode::Char('?') => self.help = !self.help,
            _ => {}
        }
        false
    }
}

/// 构造置顶的「..」虚拟行（回到上级目录）；文件系统根目录没有上级时返回 `None`。
fn parent_row(dir: &Path) -> Option<Row> {
    let parent = dir.parent()?;
    Some(Row {
        child: fsutil::ChildEntry {
            path: parent.to_path_buf(),
            kind: EntryKind::Dir,
            size: 0,
            target: None,
        },
        rec: Recognition {
            title: "上级目录".into(),
            note: "回到上一级".into(),
            kind: PurposeKind::Other,
            safety: Safety::Unknown,
            source: Source::Unknown,
            cleanable: false,
            risk: None,
            category: None,
            rule_id: None,
            explain: None,
            protected: false,
            reference: None,
        },
        parent: true,
    })
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
                    Row {
                        child,
                        rec,
                        parent: false,
                    }
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

// ---------------------------------------------------------------------------
// 绘制
// ---------------------------------------------------------------------------

/// 在给定区域内绘制浏览界面（主 TUI 标签页；当前路径显示在列表块标题）
pub fn render(frame: &mut Frame, state: &mut BrowseState, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(1)])
        .split(area);

    render_body(frame, state, chunks[0]);
    render_footer(frame, state, chunks[1]);

    if state.help {
        render_help(frame, area);
    }
    if state.confirm {
        render_confirm(frame, state);
    }
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
    let rows: Vec<&Row> = app
        .visible
        .iter()
        .map(|&i| &app.rows[i])
        .filter(|r| !r.parent)
        .collect();
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
            let (icon, name, size) = if row.parent {
                ("↰", "..".to_string(), " ".repeat(10))
            } else {
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
                (icon, name, format!("{:>9} ", size_cell(&row.child)))
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{icon} "),
                    Style::default().fg(kind_color(row.child.kind)),
                ),
                Span::styled(size, Style::default().fg(Color::White)),
                Span::styled(
                    format!(
                        "{} ",
                        text::pad_end(&text::truncate(&row.rec.title, 10), 10)
                    ),
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
    let count = app.visible.iter().filter(|&&i| !app.rows[i].parent).count();
    let meta = format!(
        "{} 项 · 排序:{}{}",
        count,
        app.sort.label(),
        if app.show_hidden { " · 含隐藏" } else { "" }
    );
    let title = format!("{}  ·  {meta}", app.cwd().display());
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
    if row.parent {
        let lines = vec![
            Line::from(Span::styled(
                "上级目录",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                row.child.path.display().to_string(),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from("按 Enter 回到上一级。"),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
        return;
    }
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
    let (text, style) = if app.confirm {
        (
            format!(
                " 移入{}？ y 确认 · n/Esc 取消",
                clean::default_mode().label()
            ),
            toast::style(toast::Kind::Warn),
        )
    } else if app.filtering {
        (
            format!(" 过滤：{}▏   Enter 确认 · Esc 清除", app.filter),
            toast::bar_style(),
        )
    } else if let Some(t) = &app.toast {
        (
            format!(" {}   · Esc 关闭", t.text()),
            toast::style(t.kind()),
        )
    } else if app.loader.is_some() {
        (" 加载中…   Esc 取消".to_string(), toast::bar_style())
    } else {
        (
            " ↑↓/jk 移动 · Enter/l 进入 · .. 或 Backspace/h 上级 · / 过滤 · s 排序 · t 占用图 · c 清理 · q 退出"
                .to_string(),
            toast::bar_style(),
        )
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
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

fn render_confirm(frame: &mut Frame, app: &BrowseState) {
    let Some(row) = app.current() else {
        return;
    };
    let a = centered_rect(64, 25, frame.area());
    frame.render_widget(Clear, a);
    let text = vec![
        Line::from(Span::styled(
            format!("移入{}？", clean::default_mode().label()),
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
        Line::from("  Enter / → / l  进入目录"),
        Line::from("  列表首行 ..   回到上级（可一直回到 /）"),
        Line::from("  Backspace / ← / h  返回上级"),
        Line::from("  g / G        跳到顶部 / 底部"),
        Line::from("  /            过滤（按名称/用途）"),
        Line::from("  s            切换排序（大小/名称/类型）"),
        Line::from("  t            占用图切换"),
        Line::from(format!(
            "  c            把当前可清理项移入{}（需确认）",
            clean::default_mode().label()
        )),
        Line::from("  r            重新计算当前目录"),
        Line::from("  .            显示/隐藏 . 开头项"),
        Line::from("  Esc          关提示/取消加载；无提示时退出"),
        Line::from("  q            退出"),
        Line::from("  Tab / 1-4    切换标签页"),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "浏览只读；只有显式按 c 并确认才会移入{}（可恢复）。",
                clean::default_mode().label()
            ),
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
    text::truncate(s, width)
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
    fn esc_quits_but_q_is_left_to_main_tui() {
        let mut st = BrowseState::new(std::env::temp_dir());
        st.loader = None;
        st.toast = None;
        // q 不再由浏览页处理，统一由主 TUI 退出
        assert!(!st.on_key(KeyCode::Char('q')));
        // 无提示/加载时 Esc 请求结束浏览上下文
        assert!(st.on_key(KeyCode::Esc));
    }

    #[test]
    fn esc_closes_help_before_quitting() {
        let mut st = BrowseState::new(std::env::temp_dir());
        st.loader = None;
        st.help = true;
        assert!(!st.on_key(KeyCode::Esc), "Esc 应先关闭帮助");
        assert!(!st.help);
        assert!(st.on_key(KeyCode::Esc), "再按才退出");
    }

    #[test]
    fn hidden_and_truncate() {
        assert!(is_hidden(Path::new("/a/.vol")));
        assert!(!is_hidden(Path::new("/a/bin")));
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("ab", 4), "ab");
    }

    #[test]
    fn parent_row_only_below_root() {
        assert!(parent_row(Path::new("/")).is_none(), "根目录没有上级");
        let p = parent_row(Path::new("/Users/me")).expect("应有上级");
        assert!(p.parent);
        assert_eq!(p.child.path, Path::new("/Users"));
    }

    #[test]
    fn dotdot_enters_parent() {
        let dir = std::env::temp_dir();
        let mut st = BrowseState::new(dir.clone());
        st.loader = None;
        st.set_rows(Vec::new());
        // 第一项应是「..」虚拟行
        assert!(st.current().map(|r| r.parent).unwrap_or(false));
        st.enter();
        assert_eq!(st.cwd(), dir.parent().unwrap());
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
