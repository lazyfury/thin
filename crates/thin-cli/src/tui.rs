use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;
use thin_core::apps::AppInfo;
use thin_core::fmt::human;
use thin_core::model::{CleanItem, Risk};
use thin_core::progress::Progress;
use thin_core::tree::TreeNode;
use thin_core::{apps, clean, fsutil, history, probe, protect, rules, scan, tree};

use crate::browse::{self, BrowseState};
use crate::text;
use crate::toast::{self, Toast};

const TABS: [&str; 5] = ["清理", "应用", "隔离区", "历史", "浏览"];
const N_TABS: usize = TABS.len();
/// 「清理」标签页下标
const CLEAN_TAB: usize = 0;
/// 「应用」标签页下标
const APPS_TAB: usize = 1;
/// 「隔离区」标签页下标
const QUARANTINE_TAB: usize = 2;
/// 「历史」标签页下标
const HISTORY_TAB: usize = 3;
/// 「浏览」标签页下标（独立组件 `crate::browse`）
const BROWSE_TAB: usize = 4;

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

/// 待确认的卸载计划（App 本体 + 关联残留）
struct PendingUninstall {
    app_name: String,
    app_path: PathBuf,
    items: Vec<CleanItem>,
    approved_bytes: u64,
    skipped: usize,
}

/// 树形视图中已展开的一行（由 [`TreeNode`] 扁平化而来，便于用列表光标导航）。
#[derive(Clone)]
struct TreeRow {
    /// 节点显示名（合并的单链用 `/` 连接）
    name: String,
    /// 规则名（目录分组节点为 `None`）
    rule_name: Option<String>,
    /// 去重后的可释放量
    size: u64,
    /// 子树中清理项数量
    count: usize,
    /// 节点自身命中的清理项下标
    item: Option<usize>,
    /// 子树中所有清理项下标（用于按文件夹整组勾选）
    item_indices: Vec<usize>,
    /// 每层的「是否最后一个兄弟」标记，用于画连接线
    is_last: Vec<bool>,
    risk: Option<Risk>,
    protected: bool,
    sudo: bool,
    /// 位于某个命中项之下，已被父项覆盖
    nested: bool,
}

impl TreeRow {
    fn is_dir(&self) -> bool {
        self.item.is_none()
    }
}

fn flatten_forest(forest: &[TreeNode], out: &mut Vec<TreeRow>) {
    let n = forest.len();
    for (i, node) in forest.iter().enumerate() {
        flatten_node(node, &[], i + 1 == n, out);
    }
}

fn flatten_node(node: &TreeNode, ancestors: &[bool], is_last: bool, out: &mut Vec<TreeRow>) {
    let mut flags = ancestors.to_vec();
    flags.push(is_last);
    let mut indices = Vec::new();
    node.item_indices(&mut indices);
    out.push(TreeRow {
        name: node.name.clone(),
        rule_name: node.item.as_ref().map(|i| i.name.clone()),
        size: node.size,
        count: node.count,
        item: node.item_index,
        item_indices: indices,
        is_last: flags.clone(),
        risk: node.item.as_ref().map(|i| i.risk),
        protected: node.item.as_ref().map(|i| i.protected).unwrap_or(false),
        sudo: node.item.as_ref().map(|i| i.sudo).unwrap_or(false),
        nested: node.nested,
    });
    let n = node.children.len();
    for (i, c) in node.children.iter().enumerate() {
        flatten_node(c, &flags, i + 1 == n, out);
    }
}

/// 根据「是否最后一个兄弟」标记生成树形连接线前缀。
fn tree_prefix(flags: &[bool]) -> String {
    let mut s = String::new();
    let last = flags.len().saturating_sub(1);
    for (i, is_last) in flags.iter().enumerate() {
        if i == last {
            s.push_str(if *is_last { "└─ " } else { "├─ " });
        } else {
            s.push_str(if *is_last { "   " } else { "│  " });
        }
    }
    s
}

struct App {
    root: PathBuf,
    min: u64,
    tab: usize,
    list_states: Vec<ListState>,
    clean: Load<Vec<CleanItem>>,
    selected: Vec<bool>,
    /// 清理页以「按文件夹合并」的树形展示（键 t 切换）
    tree_view: bool,
    /// 树形视图扁平化后的行缓存
    clean_rows: Vec<TreeRow>,
    /// 清理页是否显示「需 sudo / 受系统保护」的项（默认隐藏，键 m 切换）
    show_manual: bool,
    apps: Load<Vec<AppInfo>>,
    quarantine: Load<Vec<clean::Journal>>,
    history: Load<Vec<history::Record>>,
    browse: Option<BrowseState>,
    confirm: bool,
    /// 永久删除隔离会话的二次确认（会话 id）
    purge_confirm: Option<String>,
    /// 卸载 App 的二次确认
    uninstall_confirm: Option<PendingUninstall>,
    status: Option<Toast>,
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
            clean: Load::Idle,
            selected: Vec::new(),
            tree_view: true,
            clean_rows: Vec::new(),
            show_manual: false,
            apps: Load::Idle,
            quarantine: Load::Idle,
            history: Load::Idle,
            browse: None,
            confirm: false,
            purge_confirm: None,
            uninstall_confirm: None,
            status: None,
            help: false,
            quit: false,
            tick: 0,
        }
    }

    /// 统一的状态提示入口；`tick` 用于自动过期计时
    fn info(&mut self, text: impl Into<String>) {
        self.status = Some(Toast::info(text, self.tick));
    }

    fn warn(&mut self, text: impl Into<String>) {
        self.status = Some(Toast::warn(text, self.tick));
    }

    fn error(&mut self, text: impl Into<String>) {
        self.status = Some(Toast::error(text, self.tick));
    }

    /// 过期清理：info/warn 自动消失，error 需用户按 Esc 关闭
    fn expire_status(&mut self) {
        if self.status.as_ref().is_some_and(|t| t.expired(self.tick)) {
            self.status = None;
        }
    }

    fn ensure(&mut self, tab: usize) {
        let root = self.root.clone();
        match tab {
            0 if self.clean.is_idle() => {
                let min = self.min;
                let show_manual = self.show_manual;
                self.clean = Load::spawn(move |p| {
                    let catalog = rules::load()?;
                    let mut items = scan::scan_progress(&catalog, true, min, p);
                    if !show_manual {
                        // 与 CLI 默认一致：需 sudo / 受系统保护的项不展示、也不可勾选。
                        items.retain(|it| !scan::is_manual(it));
                    }
                    Ok(items)
                });
            }
            APPS_TAB if self.apps.is_idle() => {
                self.apps = Load::spawn(move |_p| Ok(apps::list_apps()));
            }
            QUARANTINE_TAB if self.quarantine.is_idle() => {
                self.quarantine = Load::spawn(move |_p| clean::list_journals());
            }
            HISTORY_TAB if self.history.is_idle() => {
                self.history = Load::spawn(move |_p| history::load(None));
            }
            BROWSE_TAB if self.browse.is_none() => {
                self.browse = Some(BrowseState::new(root));
            }
            _ => {}
        }
    }

    fn poll_loaders(&mut self) {
        self.clean.poll();
        self.apps.poll();
        self.quarantine.poll();
        self.history.poll();
        if let Some(b) = &mut self.browse {
            b.poll();
            b.tick();
        }
        // 清理项变化时重建选择状态与树形行缓存。
        // 注意：`items` 借用 self.clean，重建需先结束借用，故用标志位延后。
        let rebuild = match &self.clean {
            Load::Ready(items) => {
                if self.selected.len() != items.len() {
                    self.selected = items
                        .iter()
                        .map(|i| i.risk == Risk::Safe && !i.protected)
                        .collect();
                    true
                } else {
                    // 重载为空列表时同样要清掉旧行缓存
                    items.is_empty() && !self.clean_rows.is_empty()
                }
            }
            _ => false,
        };
        if rebuild {
            self.rebuild_clean_rows();
        }
        self.expire_status();
    }

    /// 依据当前清理项重建树形扁平行缓存（仅在 items 变化时调用）。
    fn rebuild_clean_rows(&mut self) {
        let rows = match &self.clean {
            Load::Ready(items) => {
                let home = home_prefix();
                let home_path = (!home.is_empty()).then(|| PathBuf::from(home));
                let forest = tree::build_forest(items, home_path.as_deref());
                let mut rows = Vec::new();
                flatten_forest(&forest, &mut rows);
                rows
            }
            _ => Vec::new(),
        };
        self.clean_rows = rows;
    }

    /// 某个清理项是否在保护名单（用于树形整组勾选时排除）。
    fn item_protected(&self, i: usize) -> bool {
        match &self.clean {
            Load::Ready(items) => items.get(i).map(|it| it.protected).unwrap_or(false),
            _ => false,
        }
    }

    /// 切换树形 / 平铺视图，并把光标重置到首行。
    fn toggle_tree_view(&mut self) {
        self.tree_view = !self.tree_view;
        self.list_states[CLEAN_TAB].select(Some(0));
        if self.tree_view {
            self.info("已切换为按文件夹合并的树形视图（t 切回平铺）");
        } else {
            self.info("已切换为平铺列表（t 切换树形）");
        }
    }

    fn switch_tab(&mut self, tab: usize) {
        self.tab = tab % N_TABS;
        // 隔离区/历史很轻量，每次进入都重新加载，避免清理后看到旧数据
        if self.tab == QUARANTINE_TAB {
            self.quarantine = Load::Idle;
        } else if self.tab == HISTORY_TAB {
            self.history = Load::Idle;
        }
        self.ensure(self.tab);
        self.status = None;
    }

    fn next_tab(&mut self, delta: isize) {
        let t = (self.tab as isize + delta).rem_euclid(N_TABS as isize) as usize;
        self.switch_tab(t);
    }

    fn current_len(&self) -> usize {
        match self.tab {
            0 if self.tree_view => self.clean_rows.len(),
            0 => self.clean.ready().map_or(0, |v| v.len()),
            APPS_TAB => self.apps.ready().map_or(0, |v| v.len()),
            QUARANTINE_TAB => self.quarantine.ready().map_or(0, |v| v.len()),
            HISTORY_TAB => self.history.ready().map_or(0, |v| v.len()),
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
        if self.tree_view {
            let row = self.clean_rows.get(self.cursor()).cloned();
            let Some(row) = row else { return };
            // 排除保护名单项；目录节点整组勾选/取消
            let selectable: Vec<usize> = row
                .item_indices
                .iter()
                .copied()
                .filter(|&i| !self.item_protected(i))
                .collect();
            if selectable.is_empty() {
                if row.is_dir() {
                    self.warn("该目录下的项均已在保护名单");
                } else {
                    self.warn("该路径已在保护名单，按 P 解除后再清理");
                }
                return;
            }
            let all_on = selectable
                .iter()
                .all(|&i| self.selected.get(i).copied().unwrap_or(false));
            for i in selectable {
                self.selected[i] = !all_on;
            }
            return;
        }
        let i = self.cursor();
        if i < self.selected.len() {
            // 保护名单项不可勾选
            if let Load::Ready(items) = &self.clean
                && items.get(i).map(|it| it.protected).unwrap_or(false)
            {
                self.warn("该路径已在保护名单，按 P 解除后再清理");
                return;
            }
            self.selected[i] = !self.selected[i];
        }
    }

    /// 当前光标对应的路径（树形视图下目录节点用其显示路径展开）。
    fn cursor_path(&self) -> Option<PathBuf> {
        if self.tree_view {
            let row = self.clean_rows.get(self.cursor())?;
            if let Some(i) = row.item {
                return match &self.clean {
                    Load::Ready(items) => items.get(i).map(|it| it.path.clone()),
                    _ => None,
                };
            }
            // 目录分组节点：把 `~` 前缀展开为真实路径后保护整个目录
            return fsutil::expand(&row.name);
        }
        match &self.clean {
            Load::Ready(items) => items.get(self.cursor()).map(|it| it.path.clone()),
            _ => None,
        }
    }

    /// 把当前清理项加入保护名单（thin protect），并就地更新标记。
    ///
    /// 树形视图下的目录分组节点会保护整个目录（及其下所有项）。
    fn protect_current(&mut self) {
        let Some(path) = self.cursor_path() else {
            return;
        };
        let affected: Vec<usize> = if self.tree_view {
            self.clean_rows
                .get(self.cursor())
                .map(|r| r.item_indices.clone())
                .unwrap_or_default()
        } else {
            vec![self.cursor()]
        };
        match protect::add(&path) {
            Ok(canon) => {
                if let Load::Ready(items) = &mut self.clean {
                    for &i in &affected {
                        if let Some(it) = items.get_mut(i) {
                            it.protected = true;
                        }
                    }
                }
                for &i in &affected {
                    if i < self.selected.len() {
                        self.selected[i] = false;
                    }
                }
                self.rebuild_clean_rows();
                self.info(format!("已保护 {}（含子目录）", canon.display()));
            }
            Err(e) => self.error(format!("加入保护名单失败：{e}")),
        }
    }

    /// 从保护名单移除覆盖当前项的条目（`thin protect remove`），并重新扫描标注。
    fn unprotect_current(&mut self) {
        let Some(path) = self.cursor_path() else {
            return;
        };
        // 名单里可能是其父目录条目在覆盖该项，先找到真正命中的那条。
        let target = protect::covering(&protect::load(), &path)
            .cloned()
            .unwrap_or(path);
        match protect::remove(&target) {
            Ok(true) => {
                // 解除后该项可能仍受静态安全门保护（SIP 等），重扫一次重新判定。
                self.clean = Load::Idle;
                self.selected.clear();
                self.clean_rows.clear();
                self.ensure(CLEAN_TAB);
                self.info(format!("已解除保护 {}（含子目录）", target.display()));
            }
            Ok(false) => self.warn("该路径不在 thin protect 名单（系统保护无法解除）"),
            Err(e) => self.error(format!("解除保护失败：{e}")),
        }
    }

    /// 切换清理页是否显示「需 sudo / 受系统保护」的项（默认隐藏，改后重扫一次）。
    fn toggle_manual(&mut self) {
        self.show_manual = !self.show_manual;
        self.clean = Load::Idle;
        self.selected.clear();
        self.ensure(CLEAN_TAB);
        if self.show_manual {
            self.warn("已显示需 sudo / 受系统保护的项：thin 不会清理它们");
        } else {
            self.info("已隐藏需 sudo / 受系统保护的项");
        }
    }

    fn reload_current(&mut self) {
        match self.tab {
            0 => {
                self.clean = Load::Idle;
                self.selected.clear();
            }
            APPS_TAB => self.apps = Load::Idle,
            QUARANTINE_TAB => self.quarantine = Load::Idle,
            HISTORY_TAB => self.history = Load::Idle,
            _ => {}
        }
        self.ensure(self.tab);
        self.info("正在重新加载…");
    }

    // -- 隔离区 --

    fn reload_quarantine(&mut self) {
        self.quarantine = Load::Idle;
        self.ensure(QUARANTINE_TAB);
    }

    /// 当前选中的隔离会话 id
    fn current_session(&self) -> Option<String> {
        match &self.quarantine {
            Load::Ready(list) => list.get(self.cursor()).map(|j| j.session.clone()),
            _ => None,
        }
    }

    fn restore_selected(&mut self) {
        let Some(session) = self.current_session() else {
            return;
        };
        match clean::restore_session(&session) {
            Ok(rep) => {
                let mut msg = format!("已恢复 {} 项", rep.restored);
                if !rep.missing.is_empty() {
                    msg.push_str(&format!("，缺失 {}", rep.missing.len()));
                }
                if !rep.conflicts.is_empty() {
                    msg.push_str(&format!("，冲突 {}（仍留在隔离区）", rep.conflicts.len()));
                }
                self.info(msg);
                self.reload_quarantine();
            }
            Err(e) => self.error(format!("恢复失败：{e:#}")),
        }
    }

    fn purge_selected(&mut self) {
        if let Some(session) = self.current_session() {
            self.purge_confirm = Some(session);
        }
    }

    fn purge_session(&mut self, session: &str) {
        match clean::purge_session(session) {
            Ok(freed) => {
                self.info(format!("已永久删除会话 {session}，释放 {}", human(freed)));
                self.reload_quarantine();
            }
            Err(e) => self.error(format!("永久删除失败：{e:#}")),
        }
    }

    // -- 历史 --

    fn reconcile_history(&mut self) {
        match history::reconcile_with_quarantine() {
            Ok(n) => {
                self.info(format!("已从隔离区回填 {n} 条历史记录"));
                self.history = Load::Idle;
                self.ensure(HISTORY_TAB);
            }
            Err(e) => self.error(format!("回填失败：{e:#}")),
        }
    }

    // -- 应用卸载 --

    /// 当前选中的应用
    fn current_app(&self) -> Option<AppInfo> {
        match &self.apps {
            Load::Ready(list) => list.get(self.cursor()).cloned(),
            _ => None,
        }
    }

    /// 组装卸载计划；系统关键 App / 运行中的 App 直接拒绝
    fn begin_uninstall(&mut self) {
        let Some(app) = self.current_app() else {
            return;
        };
        if apps::is_system_protected(app.bundle_id.as_deref()) {
            self.error(format!("{} 是系统关键 App，禁止卸载", app.name));
            return;
        }
        if apps::is_running(&app.path) {
            self.error(format!("{} 正在运行，请先退出后再卸载", app.name));
            return;
        }
        let mut items: Vec<CleanItem> = vec![CleanItem::synthetic(
            app.path.clone(),
            app.size,
            "app",
            &app.name,
            Risk::Confirm,
        )];
        for l in &app.leftovers {
            let mut it = CleanItem::synthetic(
                l.path.clone(),
                l.size,
                "app-leftover",
                &format!("{} 残留", app.name),
                Risk::Confirm,
            );
            // 系统级残留（/Library 等）需 root，交给安全门跳过
            it.sudo = l.sudo;
            items.push(it);
        }
        let plan = clean::plan(&items);
        if plan.approved.is_empty() {
            self.warn(format!("{} 没有可通过安全门的项", app.name));
            return;
        }
        self.uninstall_confirm = Some(PendingUninstall {
            app_name: app.name.clone(),
            app_path: app.path.clone(),
            items,
            approved_bytes: plan.approved_bytes(),
            skipped: plan.skipped.len(),
        });
    }

    fn execute_uninstall(&mut self, plan: PendingUninstall) {
        // 确认期间 App 可能已被启动
        if apps::is_running(&plan.app_path) {
            self.error(format!("{} 正在运行，已取消卸载", plan.app_name));
            return;
        }
        match clean::quarantine(&plan.items, false) {
            Ok(j) => {
                // 与 CLI 一致：记录历史
                let hist_err = {
                    let mut rec = history::Record::new("uninstall");
                    rec.scanned = plan.items.len();
                    rec.approved = j.entries.len();
                    rec.session = Some(j.session.clone());
                    rec.moved = j.entries.len();
                    rec.moved_bytes = j.total_size();
                    rec.skipped = j.skipped.len();
                    history::append(&rec).err()
                };
                let mut msg = format!(
                    "已卸载 {}：移入隔离区 {} 项 · {}",
                    plan.app_name,
                    j.entries.len(),
                    human(j.total_size())
                );
                if !j.skipped.is_empty() {
                    msg.push_str(&format!("，跳过 {} 项", j.skipped.len()));
                }
                if let Some(e) = hist_err {
                    msg.push_str(&format!("（历史写入失败：{e:#}）"));
                }
                self.info(msg);
                self.apps = Load::Idle;
                self.ensure(4);
                self.quarantine = Load::Idle;
                self.history = Load::Idle;
            }
            Err(e) => self.error(format!("卸载失败：{e:#}")),
        }
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
            self.warn("未勾选任何清理项");
            return;
        }

        match clean::quarantine(&chosen, false) {
            Ok(j) => {
                // 与 CLI 一致：把本次清理写入历史
                let hist_err = {
                    let mut rec = history::Record::new("manual");
                    rec.scanned = chosen.len();
                    rec.approved = j.entries.len();
                    rec.session = Some(j.session.clone());
                    rec.moved = j.entries.len();
                    rec.moved_bytes = j.total_size();
                    rec.skipped = j.skipped.len();
                    history::append(&rec).err()
                };
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
                    self.rebuild_clean_rows();
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
                if let Some(e) = hist_err {
                    msg.push_str(&format!("  （历史写入失败：{e:#}）"));
                }
                self.info(msg);
            }
            Err(e) => self.error(format!("清理失败：{e:#}")),
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let code = key.code;
        // Ctrl+C 退出；带修饰键的其它按键不触发动作（避免把 Ctrl+C 当成 'c' 清理）
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(code, KeyCode::Char('c')) {
                self.quit = true;
            }
            return;
        }
        if !key.modifiers.is_empty() {
            return;
        }
        if self.confirm {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.apply(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.confirm = false,
                _ => {}
            }
            return;
        }
        if let Some(session) = self.purge_confirm.clone() {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.purge_confirm = None;
                    self.purge_session(&session);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.purge_confirm = None,
                _ => {}
            }
            return;
        }
        if let Some(plan) = self.uninstall_confirm.take() {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.execute_uninstall(plan)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => self.uninstall_confirm = Some(plan),
            }
            return;
        }

        if self.tab == BROWSE_TAB {
            // 浏览页处于输入/模态状态时，普通按键应交给组件，
            // 否则 Tab 切页、数字跳页、q 退出会抢走过滤输入。
            let modal = self.browse.as_ref().is_some_and(|b| b.captures_input());
            if !modal {
                match code {
                    // 与其它标签页一致：q 退出整个 TUI
                    KeyCode::Char('q') => {
                        self.quit = true;
                        return;
                    }
                    // 切页只用 Tab/Shift-Tab/数字；l/h 留给浏览页自身导航（进入/上级）
                    KeyCode::Tab => {
                        self.next_tab(1);
                        return;
                    }
                    KeyCode::BackTab => {
                        self.next_tab(-1);
                        return;
                    }
                    KeyCode::Char(c @ '1'..='9') => {
                        let i = (c as u8 - b'1') as usize;
                        if i < N_TABS {
                            self.switch_tab(i);
                        }
                        return;
                    }
                    _ => {}
                }
            }
            // 浏览页内部处理 Esc/提示/加载；返回 true 表示「结束浏览上下文」，
            // 与其它标签页的 Esc 对齐：退出整个 TUI。离开浏览用 Tab/数字切页。
            if let Some(b) = &mut self.browse
                && b.on_key(code)
            {
                self.quit = true;
            }
            return;
        }

        match code {
            KeyCode::Char('q') => self.quit = true,
            // Esc：先关闭底部提示，无提示时才退出（避免错误消息关不掉）
            KeyCode::Esc => {
                if self.status.is_some() {
                    self.status = None;
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Tab | KeyCode::Char('l') => self.next_tab(1),
            KeyCode::BackTab | KeyCode::Char('h') => self.next_tab(-1),
            KeyCode::Char(c @ '1'..='9') => {
                let i = (c as u8 - b'1') as usize;
                if i < N_TABS {
                    self.switch_tab(i);
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => self.list_states[self.tab].select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => {
                let len = self.current_len();
                if len > 0 {
                    self.list_states[self.tab].select(Some(len - 1));
                }
            }
            KeyCode::Char(' ') if self.tab == CLEAN_TAB => self.toggle(),
            KeyCode::Char('a') if self.tab == CLEAN_TAB => self.select_kind(true, true),
            KeyCode::Char('A') if self.tab == CLEAN_TAB => self.select_kind(false, true),
            KeyCode::Char('n') if self.tab == CLEAN_TAB => self.select_kind(false, false),
            KeyCode::Char('p') if self.tab == CLEAN_TAB => self.protect_current(),
            KeyCode::Char('P') if self.tab == CLEAN_TAB => self.unprotect_current(),
            KeyCode::Char('m') if self.tab == CLEAN_TAB => self.toggle_manual(),
            KeyCode::Char('t') if self.tab == CLEAN_TAB => self.toggle_tree_view(),
            KeyCode::Char('c') if self.tab == CLEAN_TAB => {
                if self.selected_count() > 0 {
                    self.confirm = true;
                } else {
                    self.warn("未勾选任何清理项");
                }
            }
            KeyCode::Enter if self.tab == QUARANTINE_TAB => self.restore_selected(),
            KeyCode::Char('p') if self.tab == QUARANTINE_TAB => self.purge_selected(),
            KeyCode::Char('c') if self.tab == HISTORY_TAB => self.reconcile_history(),
            KeyCode::Char('u') if self.tab == APPS_TAB => self.begin_uninstall(),
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
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|f| ui(f, app))?;
        if event::poll(Duration::from_millis(80))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key);
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

/// 把家目录前缀显示为 `~`
fn shorten(path: &Path, home: &str) -> String {
    let s = path.display().to_string();
    if !home.is_empty() && s.starts_with(home) {
        s.replacen(home, "~", 1)
    } else {
        s
    }
}

fn ui(frame: &mut Frame, app: &mut App) {
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
    if let Some(session) = app.purge_confirm.clone() {
        render_purge_confirm(frame, app, &session);
    }
    if let Some(plan) = &app.uninstall_confirm {
        render_uninstall_confirm(frame, plan);
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
        Span::raw("  M6 · 定向清理"),
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
        QUARANTINE_TAB => render_quarantine(frame, app, area),
        HISTORY_TAB => render_history(frame, app, area),
        BROWSE_TAB => render_browse(frame, app, area),
        _ => {}
    }
}

/// 隔离区：会话列表 + 明细
fn render_quarantine(frame: &mut Frame, app: &mut App, area: Rect) {
    let home = home_prefix();
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(area);

    match &app.quarantine {
        Load::Ready(list) => {
            if list.is_empty() {
                state_msg(frame, area, "隔离区为空（清理后可在这里恢复）");
                return;
            }
            let items: Vec<ListItem> = list
                .iter()
                .map(|j| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{:>9} ", human(j.total_size())),
                            Style::default().fg(Color::White),
                        ),
                        Span::styled(
                            format!("{:>3} 项  ", j.entries.len()),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::raw(history::format_ts(j.created_at)),
                        Span::styled(
                            format!("  {}", j.session),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                })
                .collect();
            let total: u64 = list.iter().map(|j| j.total_size()).sum();
            let widget = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "隔离会话 · {} 个 · {}",
                    list.len(),
                    human(total)
                )))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("› ");
            frame.render_stateful_widget(widget, parts[0], &mut app.list_states[app.tab]);

            let cur = app.cursor();
            let mut text: Vec<Line> = Vec::new();
            if let Some(j) = list.get(cur) {
                text.push(Line::from(Span::styled(
                    format!("会话 {}", j.session),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                text.push(Line::from(format!(
                    "创建 {}",
                    history::format_ts(j.created_at)
                )));
                text.push(Line::from(format!(
                    "共 {} 项 · {}",
                    j.entries.len(),
                    human(j.total_size())
                )));
                text.push(Line::from(""));
                text.push(Line::from(Span::styled(
                    "条目",
                    Style::default().fg(Color::Cyan),
                )));
                for e in &j.entries {
                    text.push(Line::from(vec![
                        Span::styled(
                            format!("{:>9} ", human(e.size)),
                            Style::default().fg(Color::White),
                        ),
                        Span::raw(shorten(&e.original, &home)),
                    ]));
                }
                if !j.skipped.is_empty() {
                    text.push(Line::from(""));
                    text.push(Line::from(Span::styled(
                        "跳过",
                        Style::default().fg(Color::Yellow),
                    )));
                    for s in &j.skipped {
                        text.push(Line::from(format!(
                            "{} — {}",
                            shorten(&s.path, &home),
                            s.reason
                        )));
                    }
                }
            } else {
                text.push(Line::from("（无）"));
            }
            frame.render_widget(
                Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("详情 · Enter 恢复 · p 永久删除"),
                    )
                    .wrap(Wrap { trim: true }),
                parts[1],
            );
        }
        Load::Loading { progress, .. } => {
            render_loading(frame, area, Some(progress), app.tick, "读取隔离区…")
        }
        Load::Failed(e) => state_msg(frame, area, e),
        Load::Idle => state_msg(frame, area, "等待加载"),
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
            " 1-9/Tab 切换标签 · ↑↓/jk 移动 · space 勾选 · a 选安全 · A 全选 · n 清空 · t 树形 · p 保护 · P 解除 · m 需sudo · r 重载 · c 清理 · Esc 关闭提示/退出 · q 退出"
                .to_string(),
            toast::bar_style(),
        )
    } else {
        let hint = match app.tab {
            0 => {
                " Tab 切页 · ↑↓/jk 移动 · space 勾选 · a 选安全 · A 全选 · n 清空 · t 树形 · p 保护 · P 解除 · m 需sudo · c 清理 · r 重载 · ? 帮助 · q 退出"
            }
            APPS_TAB => " ↑↓/jk 移动 · u 卸载 · r 重载 · ? 帮助 · q 退出",
            QUARANTINE_TAB => " ↑↓/jk 移动 · Enter 恢复 · p 永久删除 · r 重载 · ? 帮助 · q 退出",
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
            Span::styled("[n / Esc] 取消", Style::default().fg(Color::Red)),
        ]),
    ];
    let popup = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title("确认清理"))
        .alignment(Alignment::Center);
    frame.render_widget(popup, area);
}

fn render_purge_confirm(frame: &mut Frame, app: &App, session: &str) {
    let area = centered_rect_fixed(60, 11, frame.area());
    frame.render_widget(Clear, area);
    let (n, size) = match &app.quarantine {
        Load::Ready(list) => list
            .iter()
            .find(|j| j.session == session)
            .map(|j| (j.entries.len(), j.total_size()))
            .unwrap_or((0, 0)),
        _ => (0, 0),
    };
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "永久删除该隔离会话？",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("会话 {session} · {n} 项 · {}", human(size))),
        Line::from(""),
        Line::from(Span::styled(
            "永久删除后不可恢复。",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("[y] 永久删除", Style::default().fg(Color::Red)),
            Span::raw("    "),
            Span::styled("[n / Esc] 取消", Style::default().fg(Color::Green)),
        ]),
    ];
    let popup = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title("确认永久删除"),
        )
        .alignment(Alignment::Center);
    frame.render_widget(popup, area);
}

fn render_uninstall_confirm(frame: &mut Frame, plan: &PendingUninstall) {
    let area = centered_rect_fixed(66, 12, frame.area());
    frame.render_widget(Clear, area);
    let mut text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("卸载 {}？", plan.app_name),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "将 {} 项（App 本体 + 关联残留）移入隔离区",
            plan.items.len()
        )),
        Line::from(format!("可释放 {}", human(plan.approved_bytes))),
    ];
    if plan.skipped > 0 {
        text.push(Line::from(Span::styled(
            format!("{} 项需 sudo 或受保护，将跳过", plan.skipped),
            Style::default().fg(Color::Yellow),
        )));
    } else {
        text.push(Line::from(""));
    }
    text.push(Line::from(""));
    text.push(Line::from(Span::styled(
        "移入后可随时恢复；彻底删除用 thin quarantine purge",
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    fn test_app() -> App {
        App::new(PathBuf::from("/"), 0)
    }

    /// 把 TestBackend 的缓冲区按行拼成文本（跳过宽字符占用的后续单元格）
    fn buffer_text(buf: &Buffer) -> String {
        let area = buf.area;
        let mut s = String::new();
        for y in 0..area.height {
            let mut skip = 0usize;
            for x in 0..area.width {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                s.push_str(sym);
                skip = text::width(sym).saturating_sub(1);
            }
            s.push('\n');
        }
        s
    }

    /// 渲染一帧并返回可断言的文本（不 spawn 线程、不碰终端）
    fn render(app: &mut App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn journal(session: &str, size: u64) -> clean::Journal {
        clean::Journal {
            session: session.to_string(),
            created_at: 1_700_000_000,
            dry_run: false,
            entries: vec![clean::JournalEntry {
                index: 0,
                original: PathBuf::from("/tmp/foo/cache"),
                stored: PathBuf::from("/tmp/quarantine/foo"),
                size,
                rule_id: "test".into(),
                name: "测试缓存".into(),
                risk: Risk::Safe,
                contents_only: false,
            }],
            skipped: vec![],
        }
    }

    fn history_record() -> history::Record {
        let mut r = history::Record::new("manual");
        r.session = Some("20260101-000000".into());
        r.scanned = 3;
        r.approved = 2;
        r.moved = 2;
        r.moved_bytes = 2048;
        r.skipped = 1;
        r
    }

    fn app_info(name: &str, bundle: Option<&str>) -> AppInfo {
        AppInfo {
            name: name.into(),
            path: PathBuf::from(format!("/Applications/{name}.app")),
            bundle_id: bundle.map(str::to_string),
            size: 1024,
            leftovers: vec![],
        }
    }

    fn clean_item(path: &str, size: u64) -> CleanItem {
        CleanItem::synthetic(PathBuf::from(path), size, "test", "测试项", Risk::Safe)
    }

    #[test]
    fn renders_every_tab_without_panic() {
        let mut app = test_app();
        for tab in 0..N_TABS {
            app.tab = tab;
            // 直接塞 Ready 空数据，避免 ensure 启动线程
            app.clean = Load::Ready(vec![]);
            app.apps = Load::Ready(vec![]);
            app.quarantine = Load::Ready(vec![]);
            app.history = Load::Ready(vec![]);
            assert!(!render(&mut app, 100, 30).is_empty());
        }
        // 小窗口也不能 panic
        app.tab = QUARANTINE_TAB;
        let _ = render(&mut app, 40, 12);
    }

    #[test]
    fn footer_hint_wraps_to_two_lines() {
        let mut app = test_app();
        app.tab = CLEAN_TAB;
        let out = render(&mut app, 80, 24);
        let lines: Vec<&str> = out.lines().collect();
        let n = lines.len();
        let first = lines[n - 2];
        let both = format!("{}{}", first, lines[n - 1]);
        assert!(both.contains("space 勾选"), "页脚应含勾选提示: {both:?}");
        assert!(both.contains("q 退出"), "页脚应含退出提示: {both:?}");
        assert!(
            !first.contains("q 退出"),
            "窄宽度下提示应换行到第二行: {first:?}"
        );
    }

    #[test]
    fn tab_bar_lists_quarantine_and_history() {
        let mut app = test_app();
        let text = render(&mut app, 120, 30);
        assert!(text.contains("隔离区"), "{text}");
        assert!(text.contains("历史"), "{text}");
        assert!(text.contains("浏览"), "{text}");
    }

    #[test]
    fn quarantine_tab_lists_sessions_and_actions() {
        let mut app = test_app();
        app.tab = QUARANTINE_TAB;
        app.quarantine = Load::Ready(vec![journal("20260101-000000", 1024 * 1024)]);
        let text = render(&mut app, 120, 30);
        assert!(text.contains("隔离会话"), "{text}");
        assert!(text.contains("20260101-000000"), "{text}");
        assert!(text.contains("Enter 恢复"), "{text}");
        assert!(text.contains("p 永久删除"), "{text}");
    }

    #[test]
    fn quarantine_empty_state() {
        let mut app = test_app();
        app.tab = QUARANTINE_TAB;
        app.quarantine = Load::Ready(vec![]);
        assert!(render(&mut app, 100, 24).contains("隔离区为空"));
    }

    #[test]
    fn history_tab_lists_records() {
        let mut app = test_app();
        app.tab = HISTORY_TAB;
        app.history = Load::Ready(vec![history_record()]);
        let text = render(&mut app, 120, 30);
        assert!(text.contains("历史 ·"), "{text}");
        assert!(text.contains("manual"), "{text}");
        assert!(text.contains("20260101-000000"), "{text}");
    }

    #[test]
    fn confirm_dialogs_show_action_buttons() {
        // 清理确认
        let mut app = test_app();
        app.clean = Load::Ready(vec![clean_item("/nonexistent/thin-test-clean", 1024)]);
        app.selected = vec![true];
        app.confirm = true;
        let text = render(&mut app, 100, 30);
        assert!(
            text.contains("确认清理") && text.contains("[y] 确认"),
            "{text}"
        );
        assert!(text.contains("[n / Esc] 取消"), "{text}");

        // 永久删除隔离会话
        let mut app = test_app();
        app.tab = QUARANTINE_TAB;
        app.quarantine = Load::Ready(vec![journal("sess-1", 100)]);
        app.purge_confirm = Some("sess-1".into());
        let text = render(&mut app, 100, 30);
        assert!(
            text.contains("确认永久删除") && text.contains("[y] 永久删除"),
            "{text}"
        );
        assert!(text.contains("[n / Esc] 取消"), "{text}");

        // 卸载 App
        let mut app = test_app();
        app.tab = APPS_TAB;
        app.apps = Load::Ready(vec![app_info("Foo", Some("com.example.foo"))]);
        app.uninstall_confirm = Some(PendingUninstall {
            app_name: "Foo".into(),
            app_path: PathBuf::from("/Applications/Foo.app"),
            items: vec![clean_item("/Applications/Foo.app", 2048)],
            approved_bytes: 2048,
            skipped: 1,
        });
        let text = render(&mut app, 100, 30);
        assert!(
            text.contains("确认卸载") && text.contains("[y] 卸载"),
            "{text}"
        );
        assert!(text.contains("[n / Esc] 取消"), "{text}");
    }

    #[test]
    fn apps_tab_hint_mentions_uninstall() {
        let mut app = test_app();
        app.tab = APPS_TAB;
        app.apps = Load::Ready(vec![app_info("Foo", Some("com.example.foo"))]);
        assert!(render(&mut app, 120, 30).contains("u 卸载"));
    }

    #[test]
    fn p_opens_purge_confirm_and_esc_cancels() {
        let mut app = test_app();
        app.tab = QUARANTINE_TAB;
        app.quarantine = Load::Ready(vec![journal("sess-1", 10)]);
        app.on_key(KeyEvent::from(KeyCode::Char('p')));
        assert_eq!(app.purge_confirm.as_deref(), Some("sess-1"));
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.purge_confirm.is_none());
    }

    #[test]
    fn uninstall_refuses_system_app() {
        let mut app = test_app();
        app.tab = APPS_TAB;
        app.apps = Load::Ready(vec![app_info("Safari", Some("com.apple.Safari"))]);
        app.on_key(KeyEvent::from(KeyCode::Char('u')));
        assert!(app.uninstall_confirm.is_none());
        let t = app.status.as_ref().expect("应有提示");
        assert!(t.text().contains("系统关键"), "{}", t.text());
    }

    #[test]
    fn q_quits_everywhere_including_browse() {
        let mut app = test_app();
        app.tab = BROWSE_TAB;
        app.browse = Some(BrowseState::new(PathBuf::from("/")));
        app.on_key(KeyEvent::from(KeyCode::Char('q')));
        assert!(app.quit, "浏览页按 q 应退出整个 TUI");
    }

    /// 构造一个树形视图测试用 App：三个命中项共享 `/tmp`，其中一个目录含两项。
    fn tree_app() -> App {
        let mut app = test_app();
        app.clean = Load::Ready(vec![
            clean_item("/tmp/thin-t/A/blob", 100),
            clean_item("/tmp/thin-t/B/blob", 300),
            clean_item("/tmp/thin-other/foo", 50),
        ]);
        app.selected = vec![false; 3];
        app.tree_view = true;
        app.rebuild_clean_rows();
        app
    }

    #[test]
    fn tree_view_merges_folders_and_renders() {
        let mut app = tree_app();
        // `/tmp` -> `thin-t/`（2 项） + 合并单链的 `thin-other/foo`
        assert_eq!(app.clean_rows.len(), 5);
        assert_eq!(app.clean_rows[0].name, "/tmp");
        assert!(app.clean_rows[0].is_dir());
        assert_eq!(app.clean_rows[0].size, 450);
        assert_eq!(app.clean_rows[1].name, "thin-t");
        assert_eq!(app.clean_rows[2].name, "B/blob");
        assert_eq!(app.clean_rows[4].name, "thin-other/foo");

        let text = render(&mut app, 120, 30);
        assert!(text.contains("树形(t)"), "{text}");
        assert!(text.contains("thin-t/"), "{text}");
        assert!(text.contains("thin-other/foo"), "{text}");
        assert!(text.contains("· 2 项"), "{text}");
    }

    #[test]
    fn t_key_toggles_tree_view() {
        let mut app = test_app();
        app.clean = Load::Ready(vec![clean_item("/tmp/x/a", 1)]);
        app.selected = vec![false];
        // TUI 默认树形，按 t 切回平铺
        assert!(app.tree_view);
        app.on_key(KeyEvent::from(KeyCode::Char('t')));
        assert!(!app.tree_view);
        app.on_key(KeyEvent::from(KeyCode::Char('t')));
        assert!(app.tree_view);
    }

    #[test]
    fn tree_space_selects_whole_folder() {
        let mut app = tree_app();
        // 光标在 `/tmp` 分组行：space 应勾选子树内全部 3 项
        app.list_states[CLEAN_TAB].select(Some(0));
        app.on_key(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(app.selected, vec![true, true, true]);
        // 再次 space 取消整组
        app.on_key(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(app.selected, vec![false, false, false]);
    }

    #[test]
    fn n_key_cancels_uninstall_confirm() {
        let mut app = test_app();
        app.tab = APPS_TAB;
        app.uninstall_confirm = Some(PendingUninstall {
            app_name: "Foo".into(),
            app_path: PathBuf::from("/Applications/Foo.app"),
            items: vec![],
            approved_bytes: 0,
            skipped: 0,
        });
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.uninstall_confirm.is_none());
    }
}
