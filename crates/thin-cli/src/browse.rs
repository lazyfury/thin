//! 文件浏览组件：主 TUI「浏览」标签页（键 7）。
//!
//! 一步步下钻，进入某目录才计算其直接子项的大小，并把能识别的目录标注用途。
//! 只读；`c` 可在确认后移入隔离区。按 `?` 查看快捷键。

use anyhow::Result;
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
use thin_core::catalog::Safety;
use thin_core::clean;
use thin_core::fmt::human;
use thin_core::fsutil::{self, EntryKind};
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
    toast: Option<Toast>,
    /// `:` 跳转输入框内容
    goto: String,
    gotoing: bool,
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
            toast: None,
            goto: String::new(),
            gotoing: false,
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
            self.warn("只能进入目录");
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
        self.confirm || self.bookmark_pick || self.filtering || self.gotoing || self.help
    }

    /// `:` 打开跳转输入框
    fn start_goto(&mut self) {
        self.gotoing = true;
        self.goto.clear();
    }

    /// 解析输入并跳转（支持 `~` 与相对当前目录的路径）
    fn apply_goto(&mut self) {
        let raw = self.goto.trim().to_string();
        if raw.is_empty() {
            return;
        }
        match resolve_dir(&raw, self.cwd()) {
            Some(dir) => {
                self.stack = vec![dir.clone()];
                self.load();
                self.info(format!("已跳转到 {}", shorten(&dir, &home_prefix())));
            }
            None => self.error(format!("目录不存在：{raw}")),
        }
    }

    /// 按 Tab 补全当前输入路径的目录名（只列目录，隐藏项需以 . 开头）
    fn complete_goto(&mut self) {
        let raw = self.goto.clone();
        if raw.is_empty() {
            return;
        }
        let (dir_raw, prefix) = match raw.rfind('/') {
            Some(i) => (raw[..=i].to_string(), raw[i + 1..].to_string()),
            None => (String::new(), raw.clone()),
        };
        let dir_fs = expand_tilde(&dir_raw);
        let dir_path = if dir_fs.is_empty() {
            self.cwd().to_path_buf()
        } else {
            PathBuf::from(&dir_fs)
        };
        let dir_path = if dir_path.is_absolute() {
            dir_path
        } else {
            self.cwd().join(dir_path)
        };
        let Ok(rd) = std::fs::read_dir(&dir_path) else {
            return;
        };
        let show_hidden = prefix.starts_with('.');
        let mut names: Vec<String> = rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with(&prefix) && (show_hidden || !n.starts_with('.')))
            .collect();
        names.sort();
        if names.is_empty() {
            return;
        }
        let completion = if names.len() == 1 {
            format!("{}/", names[0])
        } else {
            common_prefix(&names)
        };
        self.goto = format!("{dir_raw}{completion}");
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
                self.warn(format!("未清理：{reason}"));
            }
            Ok(journal) => {
                self.info(format!(
                    "已移入隔离区；可恢复：thin quarantine restore {}",
                    journal.session
                ));
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
        if self.gotoing {
            match code {
                KeyCode::Enter => {
                    self.gotoing = false;
                    self.apply_goto();
                }
                KeyCode::Esc => self.gotoing = false,
                KeyCode::Tab => self.complete_goto(),
                KeyCode::Backspace => {
                    self.goto.pop();
                }
                KeyCode::Char(c) => self.goto.push(c),
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
            self.info("已取消加载");
            return false;
        }
        // Esc：先关闭底部提示，再按一次才离开浏览页
        if matches!(code, KeyCode::Esc) && self.toast.is_some() {
            self.clear_status();
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
            KeyCode::Char(':') => self.start_goto(),
            KeyCode::Char('t') => self.show_treemap = !self.show_treemap,
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.rebuild_visible();
                self.info(format!("排序：{}", self.sort.label()));
            }
            KeyCode::Char('b') => {
                let dir = self.cwd().to_path_buf();
                if let Some(pos) = self.bookmarks.iter().position(|p| *p == dir) {
                    self.bookmarks.remove(pos);
                    self.info("已取消书签");
                } else {
                    self.bookmarks.push(dir);
                    self.info("已加书签（B 打开列表）");
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
    if state.bookmark_pick {
        render_bookmarks(frame, state);
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
    let meta = format!(
        "{} 项 · 排序:{}{}",
        app.visible.len(),
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
            " 移入隔离区？ y 确认 · n/Esc 取消".to_string(),
            toast::style(toast::Kind::Warn),
        )
    } else if app.bookmark_pick {
        (
            " 书签：j/k 选择 · Enter 跳转 · d 删除 · Esc 关闭".to_string(),
            toast::bar_style(),
        )
    } else if app.filtering {
        (
            format!(" 过滤：{}▏   Enter 确认 · Esc 清除", app.filter),
            toast::bar_style(),
        )
    } else if app.gotoing {
        (
            format!(" 跳转：{}▏   Enter 跳转 · Tab 补全 · Esc 取消", app.goto),
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
            " ↑↓ 移动 · Enter 进入 · Backspace 上级 · / 过滤 · : 跳转 · s 排序 · t 占用图 · b 书签 · c 清理 · q 返回"
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
        Line::from("  :            跳转目录（支持 ~ 与相对路径，Tab 补全）"),
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

/// 展开开头的 `~`（`~` 或 `~/...`）
fn expand_tilde(p: &str) -> String {
    if p == "~" {
        home_prefix()
    } else if let Some(rest) = p.strip_prefix("~/") {
        format!("{}/{}", home_prefix(), rest)
    } else {
        p.to_string()
    }
}

/// 解析跳转输入：展开 `~`、相对路径基于 `cwd`，返回规范化的目录
fn resolve_dir(input: &str, cwd: &Path) -> Option<PathBuf> {
    let expanded = expand_tilde(input);
    let p = PathBuf::from(expanded);
    let p = if p.is_absolute() { p } else { cwd.join(p) };
    let canon = std::fs::canonicalize(&p).ok()?;
    canon.is_dir().then_some(canon)
}

/// 一组字符串的最长公共前缀
fn common_prefix(names: &[String]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut pref = first.clone();
    for n in &names[1..] {
        while !n.starts_with(&pref) {
            pref.pop();
            if pref.is_empty() {
                return pref;
            }
        }
    }
    pref
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

    #[test]
    fn common_prefix_works() {
        let names = vec![
            "Documents".to_string(),
            "Downloads".to_string(),
            "Docker".to_string(),
        ];
        assert_eq!(common_prefix(&names), "Do");
        assert_eq!(common_prefix(&["a".to_string()]), "a");
        assert!(common_prefix(&[]).is_empty());
    }

    #[test]
    fn resolve_dir_handles_relative_and_missing() {
        let base = std::env::temp_dir().join(format!("thin-goto-test-{}", std::process::id()));
        let sub = base.join("child");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(base.join("file.txt"), b"x").unwrap();

        assert_eq!(
            resolve_dir("child", &base),
            Some(std::fs::canonicalize(&sub).unwrap())
        );
        // 普通文件不是目录
        assert_eq!(resolve_dir("file.txt", &base), None);
        assert_eq!(resolve_dir("missing", &base), None);

        let _ = std::fs::remove_dir_all(&base);
    }
}
