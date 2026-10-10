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
use thin_core::orphans::OrphanApp;
use thin_core::progress::Progress;
use thin_core::tree::TreeNode;
use thin_core::{apps, clean, fsutil, history, orphans, probe, protect, rules, scan, tree};

use crate::browse::{self, BrowseState};
use crate::text;
use crate::toast::{self, Toast};

mod render;
mod rows;

use render::{home_prefix, ui};
use rows::{TreeRow, flatten_forest};

const TABS: [&str; 4] = ["清理", "应用", "历史", "浏览"];
const N_TABS: usize = TABS.len();
/// 「清理」标签页下标
const CLEAN_TAB: usize = 0;
/// 「应用」标签页下标
const APPS_TAB: usize = 1;
/// 「历史」标签页下标
const HISTORY_TAB: usize = 2;
/// 「浏览」标签页下标（独立组件 `crate::browse`）
const BROWSE_TAB: usize = 3;

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
    /// 其中需 root、将走系统授权框的项数
    sudo: usize,
}

/// 待确认的「退出运行中的 App 后卸载」
struct PendingKill {
    app_name: String,
    app_path: PathBuf,
}

/// 待确认的「已卸载 App 孤立残留清理」
struct PendingOrphan {
    bundle_id: String,
    items: Vec<CleanItem>,
    approved_bytes: u64,
    skipped: usize,
    /// 其中需 root、将走系统授权框的项数
    sudo: usize,
}

/// 树形视图中已展开的一行（由 [`TreeNode`] 扁平化而来，便于用列表光标导航）。
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
    /// 清理页是否显示「受 thin protect 保护」的项（默认隐藏，键 b 切换）
    show_protected: bool,
    apps: Load<Vec<AppInfo>>,
    /// 已卸载 App 的孤立残留（Apps 页按 `o` 切换）
    orphans: Load<Vec<OrphanApp>>,
    /// Apps 页当前是否展示孤立残留（false = 已安装应用列表）
    show_orphans: bool,
    history: Load<Vec<history::Record>>,
    browse: Option<BrowseState>,
    confirm: bool,
    /// 卸载 App 的二次确认
    uninstall_confirm: Option<PendingUninstall>,
    /// 运行中的 App：是否先退出再卸载的确认
    kill_confirm: Option<PendingKill>,
    /// 孤立残留清理的二次确认
    orphan_confirm: Option<PendingOrphan>,
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
            show_protected: false,
            apps: Load::Idle,
            orphans: Load::Idle,
            show_orphans: false,
            history: Load::Idle,
            browse: None,
            confirm: false,
            uninstall_confirm: None,
            kill_confirm: None,
            orphan_confirm: None,
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
                let show_protected = self.show_protected;
                self.clean = Load::spawn(move |p| {
                    let catalog = rules::load()?;
                    let mut items = scan::scan_progress(&catalog, true, min, p);
                    if !show_manual {
                        // 与 CLI 默认一致：需 sudo / 受系统保护的项不展示、也不可勾选。
                        items.retain(|it| !scan::is_manual(it));
                    }
                    if !show_protected {
                        // 受 thin protect 保护的项默认也隐藏（键 b 显示）。
                        items.retain(|it| !it.protected);
                    }
                    Ok(items)
                });
            }
            APPS_TAB if self.show_orphans && self.orphans.is_idle() => {
                self.orphans = Load::spawn(move |_p| Ok(orphans::find_orphans()));
            }
            APPS_TAB if !self.show_orphans && self.apps.is_idle() => {
                self.apps = Load::spawn(move |_p| Ok(apps::list_apps()));
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
        self.orphans.poll();
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
        // 历史很轻量，每次进入都重新加载，避免清理后看到旧数据
        if self.tab == HISTORY_TAB {
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
            APPS_TAB if self.show_orphans => self.orphans.ready().map_or(0, |v| v.len()),
            APPS_TAB => self.apps.ready().map_or(0, |v| v.len()),
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

    /// 已勾选中需 root 的项数（弹授权框前提示用）。
    fn selected_sudo_count(&self) -> usize {
        match &self.clean {
            Load::Ready(items) => items
                .iter()
                .zip(&self.selected)
                .filter(|(it, s)| **s && it.sudo)
                .count(),
            _ => 0,
        }
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
                    self.selected[i] = value && it.risk == Risk::Safe && !it.protected && !it.sudo;
                }
            } else {
                // 批量选择不包含需 root 的项：提权必须逐项显式勾选，避免误触授权框
                for (i, it) in items.iter().enumerate() {
                    self.selected[i] = value && !it.sudo;
                }
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
            self.warn("已显示需 sudo / 受系统保护的项：需 sudo 的可勾选并弹授权框清理，受系统保护的仍会被跳过");
        } else {
            self.info("已隐藏需 sudo / 受系统保护的项");
        }
    }

    /// 切换清理页是否显示「受 thin protect 保护」的项（默认隐藏，改后重扫一次）。
    fn toggle_protected(&mut self) {
        self.show_protected = !self.show_protected;
        self.clean = Load::Idle;
        self.selected.clear();
        self.ensure(CLEAN_TAB);
        if self.show_protected {
            self.warn("已显示受 thin protect 保护的项：thin 不会清理它们");
        } else {
            self.info("已隐藏受 thin protect 保护的项");
        }
    }

    fn reload_current(&mut self) {
        match self.tab {
            0 => {
                self.clean = Load::Idle;
                self.selected.clear();
            }
            APPS_TAB => {
                self.apps = Load::Idle;
                self.orphans = Load::Idle;
            }
            HISTORY_TAB => self.history = Load::Idle,
            _ => {}
        }
        self.ensure(self.tab);
        self.info("正在重新加载…");
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

    /// 当前选中的孤立残留
    fn current_orphan(&self) -> Option<OrphanApp> {
        match &self.orphans {
            Load::Ready(list) => list.get(self.cursor()).cloned(),
            _ => None,
        }
    }

    /// Apps 页在「已安装应用」与「已卸载残留」间切换
    fn toggle_orphans(&mut self) {
        self.show_orphans = !self.show_orphans;
        self.list_states[APPS_TAB].select(Some(0));
        self.ensure(APPS_TAB);
        if self.show_orphans {
            self.info("显示已卸载 App 的孤立残留（o 切回应用列表）");
        } else {
            self.info("显示已安装应用列表（o 查看孤立残留）");
        }
    }

    /// 组装卸载计划；系统关键 App 直接拒绝，运行中的 App 先走「退出」确认
    fn begin_uninstall(&mut self) {
        if self.show_orphans {
            self.begin_orphan_cleanup();
            return;
        }
        let Some(app) = self.current_app() else {
            return;
        };
        if apps::is_system_protected(app.bundle_id.as_deref()) {
            self.error(format!("{} 是系统关键 App，禁止卸载", app.name));
            return;
        }
        if apps::is_running(&app.path) {
            self.kill_confirm = Some(PendingKill {
                app_name: app.name.clone(),
                app_path: app.path.clone(),
            });
            return;
        }
        self.plan_uninstall(&app);
    }

    /// 退出运行中的 App，成功后重新组装卸载计划
    fn kill_and_uninstall(&mut self, pending: PendingKill) {
        if let Err(e) = apps::kill_app(&pending.app_path) {
            self.error(format!("退出 {} 失败：{e:#}", pending.app_name));
            return;
        }
        // 列表可能已重载，按路径重新取回 App 组装计划
        let app = match &self.apps {
            Load::Ready(list) => list.iter().find(|a| a.path == pending.app_path).cloned(),
            _ => None,
        };
        let Some(app) = app else {
            self.warn(format!(
                "已退出 {}，但 App 列表已变化，请重试",
                pending.app_name
            ));
            return;
        };
        self.plan_uninstall(&app);
    }

    fn plan_uninstall(&mut self, app: &AppInfo) {
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
        let sudo = clean::sudo_items(&items).len();
        if plan.approved.is_empty() && sudo == 0 {
            self.warn(format!("{} 没有可通过安全门的项", app.name));
            return;
        }
        self.uninstall_confirm = Some(PendingUninstall {
            app_name: app.name.clone(),
            app_path: app.path.clone(),
            items,
            approved_bytes: plan.approved_bytes(),
            skipped: plan.skipped.len(),
            sudo,
        });
    }

    /// 组装孤立残留清理计划（无 App 本体，不需运行状态检查）
    fn begin_orphan_cleanup(&mut self) {
        let Some(o) = self.current_orphan() else {
            return;
        };
        let items: Vec<CleanItem> = o
            .leftovers
            .iter()
            .map(|l| {
                let mut it = CleanItem::synthetic(
                    l.path.clone(),
                    l.size,
                    "orphan-leftover",
                    &format!("{} 孤立残留", o.bundle_id),
                    Risk::Confirm,
                );
                it.sudo = l.sudo;
                it
            })
            .collect();
        let plan = clean::plan(&items);
        let sudo = clean::sudo_items(&items).len();
        if plan.approved.is_empty() && sudo == 0 {
            self.warn(format!("{} 没有可通过安全门的项", o.bundle_id));
            return;
        }
        self.orphan_confirm = Some(PendingOrphan {
            bundle_id: o.bundle_id.clone(),
            items,
            approved_bytes: plan.approved_bytes(),
            skipped: plan.skipped.len(),
            sudo,
        });
    }

    fn execute_uninstall(&mut self, plan: PendingUninstall) {
        // 确认期间 App 可能已被启动
        if apps::is_running(&plan.app_path) {
            self.error(format!("{} 正在运行，已取消卸载", plan.app_name));
            return;
        }
        let normal: Vec<CleanItem> = plan.items.iter().filter(|it| !it.sudo).cloned().collect();
        let sudo_pending: Vec<CleanItem> =
            plan.items.iter().filter(|it| it.sudo).cloned().collect();
        let mut msg = format!("已卸载 {}", plan.app_name);
        let mut elevated_err: Option<String> = None;

        match clean::apply(&normal, clean::default_mode()) {
            Ok(applied) => {
                // 与 CLI 一致：记录历史
                let hist_err = {
                    let mut rec = history::Record::new("uninstall");
                    rec.scanned = plan.items.len();
                    rec.approved = applied.moved();
                    rec.session = applied.session().map(str::to_string);
                    rec.moved = applied.moved();
                    rec.moved_bytes = applied.moved_bytes();
                    rec.skipped = applied.skipped() + applied.failed();
                    history::append(&rec).err()
                };
                msg.push_str(&format!(
                    "：移入{} {} 项 · {}",
                    applied.mode().label(),
                    applied.moved(),
                    human(applied.moved_bytes())
                ));
                if applied.skipped() > 0 {
                    msg.push_str(&format!("，跳过 {} 项", applied.skipped()));
                }
                if applied.failed() > 0 {
                    msg.push_str(&format!("，失败 {} 项", applied.failed()));
                }
                if let Some(e) = hist_err {
                    msg.push_str(&format!("（历史写入失败：{e:#}）"));
                }
            }
            Err(e) => {
                self.error(format!("卸载失败：{e:#}"));
                return;
            }
        }

        // 需 root 的系统级残留（LaunchDaemons / PrivilegedHelperTools 等）：弹授权框
        if !sudo_pending.is_empty() {
            match clean::elevate_sudo(&sudo_pending) {
                Ok(journal) => {
                    msg.push_str(&format!(
                        "；提权清理 {} 项 · {}（会话 {}）",
                        journal.entries.len(),
                        human(journal.total_size()),
                        journal.session
                    ));
                    let mut rec = history::Record::new("uninstall-sudo");
                    rec.scanned = sudo_pending.len();
                    rec.approved = journal.entries.len();
                    rec.session = Some(journal.session.clone());
                    rec.moved = journal.entries.len();
                    rec.moved_bytes = journal.total_size();
                    rec.skipped = journal.skipped.len();
                    if let Err(e) = history::append(&rec) {
                        msg.push_str(&format!("（历史写入失败：{e:#}）"));
                    }
                }
                Err(e) => elevated_err = Some(format!("{e:#}")),
            }
        }

        if let Some(e) = elevated_err {
            self.warn(format!("{msg}；提权清理未执行：{e}"));
        } else {
            self.info(msg);
        }
        self.apps = Load::Idle;
        self.ensure(APPS_TAB);
        self.history = Load::Idle;
    }

    /// 清理孤立残留：无 App 本体，直接走安全门并记录历史
    fn execute_orphan_cleanup(&mut self, plan: PendingOrphan) {
        let normal: Vec<CleanItem> = plan.items.iter().filter(|it| !it.sudo).cloned().collect();
        let sudo_pending: Vec<CleanItem> =
            plan.items.iter().filter(|it| it.sudo).cloned().collect();
        let mut msg = format!("已清理 {} 孤立残留", plan.bundle_id);
        let mut elevated_err: Option<String> = None;

        match clean::apply(&normal, clean::default_mode()) {
            Ok(applied) => {
                let hist_err = {
                    let mut rec = history::Record::new("orphans");
                    rec.scanned = plan.items.len();
                    rec.approved = applied.moved();
                    rec.session = applied.session().map(str::to_string);
                    rec.moved = applied.moved();
                    rec.moved_bytes = applied.moved_bytes();
                    rec.skipped = applied.skipped() + applied.failed();
                    history::append(&rec).err()
                };
                msg.push_str(&format!(
                    "：移入{} {} 项 · {}",
                    applied.mode().label(),
                    applied.moved(),
                    human(applied.moved_bytes())
                ));
                if applied.skipped() > 0 {
                    msg.push_str(&format!("，跳过 {} 项", applied.skipped()));
                }
                if applied.failed() > 0 {
                    msg.push_str(&format!("，失败 {} 项", applied.failed()));
                }
                if let Some(e) = hist_err {
                    msg.push_str(&format!("（历史写入失败：{e:#}）"));
                }
            }
            Err(e) => {
                self.error(format!("清理失败：{e:#}"));
                return;
            }
        }

        // 需 root 的系统级残留：弹授权框
        if !sudo_pending.is_empty() {
            match clean::elevate_sudo(&sudo_pending) {
                Ok(journal) => {
                    msg.push_str(&format!(
                        "；提权清理 {} 项 · {}（会话 {}）",
                        journal.entries.len(),
                        human(journal.total_size()),
                        journal.session
                    ));
                    let mut rec = history::Record::new("orphans-sudo");
                    rec.scanned = sudo_pending.len();
                    rec.approved = journal.entries.len();
                    rec.session = Some(journal.session.clone());
                    rec.moved = journal.entries.len();
                    rec.moved_bytes = journal.total_size();
                    rec.skipped = journal.skipped.len();
                    if let Err(e) = history::append(&rec) {
                        msg.push_str(&format!("（历史写入失败：{e:#}）"));
                    }
                }
                Err(e) => elevated_err = Some(format!("{e:#}")),
            }
        }

        if let Some(e) = elevated_err {
            self.warn(format!("{msg}；提权清理未执行：{e}"));
        } else {
            self.info(msg);
        }
        self.orphans = Load::Idle;
        self.ensure(APPS_TAB);
        self.history = Load::Idle;
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

        let mode = clean::default_mode();
        // sudo 项不能用普通身份移动，单独走提权通道（弹系统授权框）
        let normal: Vec<CleanItem> = chosen.iter().filter(|it| !it.sudo).cloned().collect();
        let sudo_pending: Vec<CleanItem> = chosen.iter().filter(|it| it.sudo).cloned().collect();

        let mut moved: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut msg = String::new();
        let mut elevated_err: Option<String> = None;

        if !normal.is_empty() {
            match clean::apply(&normal, mode) {
                Ok(applied) => {
                    // 与 CLI 一致：把本次清理写入历史
                    let hist_err = {
                        let mut rec = history::Record::new(match mode {
                            clean::Mode::Trash => "manual-trash",
                            clean::Mode::Quarantine => "manual",
                        });
                        rec.scanned = normal.len();
                        rec.approved = applied.moved();
                        rec.session = applied.session().map(str::to_string);
                        rec.moved = applied.moved();
                        rec.moved_bytes = applied.moved_bytes();
                        rec.skipped = applied.skipped() + applied.failed();
                        history::append(&rec).err()
                    };
                    moved.extend(applied.originals());
                    msg.push_str(&format!(
                        "已移入{} {} 项 · {}",
                        mode.label(),
                        applied.moved(),
                        human(applied.moved_bytes())
                    ));
                    if let Some(s) = applied.session() {
                        msg.push_str(&format!("  （会话 {s}）"));
                    }
                    if applied.skipped() > 0 {
                        msg.push_str(&format!("  跳过 {} 项", applied.skipped()));
                    }
                    if applied.failed() > 0 {
                        msg.push_str(&format!("  失败 {} 项", applied.failed()));
                    }
                    if let Some(e) = hist_err {
                        msg.push_str(&format!("  （历史写入失败：{e:#}）"));
                    }
                }
                Err(e) => {
                    self.error(format!("清理失败：{e:#}"));
                    return;
                }
            }
        }

        // 需 root 的项：通过系统授权框提权，复核安全门后移入调用者隔离区
        if !sudo_pending.is_empty() {
            let home = std::env::var("HOME").unwrap_or_default();
            match clean::run_elevated(&sudo_pending, Path::new(&home)) {
                Ok(journal) => {
                    if !msg.is_empty() {
                        msg.push('；');
                    }
                    msg.push_str(&format!(
                        "提权清理 {} 项 · {}（会话 {}）",
                        journal.entries.len(),
                        human(journal.total_size()),
                        journal.session
                    ));
                    moved.extend(journal.entries.iter().map(|e| e.original.clone()));
                    let mut rec = history::Record::new("manual-sudo");
                    rec.scanned = sudo_pending.len();
                    rec.approved = journal.entries.len();
                    rec.session = Some(journal.session.clone());
                    rec.moved = journal.entries.len();
                    rec.moved_bytes = journal.total_size();
                    rec.skipped = journal.skipped.len();
                    if let Err(e) = history::append(&rec) {
                        msg.push_str(&format!("（历史写入失败：{e:#}）"));
                    }
                }
                Err(e) => elevated_err = Some(format!("{e:#}")),
            }
        }

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
        if let Some(e) = elevated_err {
            if msg.is_empty() {
                self.error(format!("提权清理未执行：{e}"));
            } else {
                self.warn(format!("{msg}；提权清理未执行：{e}"));
            }
        } else if !msg.is_empty() {
            self.info(msg);
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let mut code = key.code;
        // Ctrl+C 退出；带修饰键的其它按键不触发动作（避免把 Ctrl+C 当成 'c' 清理）
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(code, KeyCode::Char('c') | KeyCode::Char('C')) {
                self.quit = true;
            }
            return;
        }
        // Alt/Super 组合键一律忽略；Shift 是输入大写字母的正常方式，必须放行，
        // 否则 kitty/wezterm 等增强键盘协议终端会把 Shift+P 标成 SHIFT 修饰，
        // 导致「大写 P 解除保护」被误判为组合键而无响应。
        if key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return;
        }
        // 部分终端把 Shift+字母上报为「小写 Char + SHIFT」，归一成大写，
        // 保证 'P'/'A'/'G' 等分支能被命中。
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && let KeyCode::Char(c) = code
            && c.is_ascii_lowercase()
        {
            code = KeyCode::Char(c.to_ascii_uppercase());
        }
        if self.confirm {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.apply(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.confirm = false,
                _ => {}
            }
            return;
        }
        if let Some(pending) = self.kill_confirm.take() {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.kill_and_uninstall(pending)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => self.kill_confirm = Some(pending),
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
        if let Some(plan) = self.orphan_confirm.take() {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.execute_orphan_cleanup(plan)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => self.orphan_confirm = Some(plan),
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
            KeyCode::Char('b') if self.tab == CLEAN_TAB => self.toggle_protected(),
            KeyCode::Char('t') if self.tab == CLEAN_TAB => self.toggle_tree_view(),
            KeyCode::Char('c') if self.tab == CLEAN_TAB => {
                if self.selected_count() > 0 {
                    self.confirm = true;
                } else {
                    self.warn("未勾选任何清理项");
                }
            }
            KeyCode::Char('c') if self.tab == HISTORY_TAB => self.reconcile_history(),
            KeyCode::Char('u') if self.tab == APPS_TAB => self.begin_uninstall(),
            KeyCode::Char('o') if self.tab == APPS_TAB => self.toggle_orphans(),
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
            running: false,
            leftovers: vec![],
        }
    }

    fn clean_item(path: &str, size: u64) -> CleanItem {
        CleanItem::synthetic(PathBuf::from(path), size, "test", "测试项", Risk::Safe)
    }

    #[test]
    fn shifted_uppercase_keys_reach_action_handlers() {
        // 回归：增强键盘协议终端（kitty/wezterm 等）会为 Shift+P 附带 SHIFT 修饰，
        // 之前「非空修饰键一律 return」会让所有大写键失效（P 解除保护、A 全选等）。
        let items = vec![
            clean_item("/tmp/thin-shift-a", 1024),
            clean_item("/tmp/thin-shift-b", 2048),
        ];
        let mut app = test_app();
        app.tab = CLEAN_TAB;
        app.clean = Load::Ready(items);

        app.selected = vec![false, false];
        app.on_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
        assert_eq!(app.selected, vec![true, true], "Shift+A 应全选");

        // 部分终端把 Shift+字母上报为「小写 Char + SHIFT」，也应识别为大写。
        app.selected = vec![false, false];
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT));
        assert_eq!(app.selected, vec![true, true], "小写+SHIFT 应归一为大写");

        // Alt/Super 等组合键仍不得触发动作（保留原有安全意图）。
        app.selected = vec![false, false];
        app.on_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::ALT));
        assert_eq!(app.selected, vec![false, false], "Alt 组合键不应触发");
    }

    fn sudo_clean_item(path: &str, size: u64) -> CleanItem {
        let mut it = clean_item(path, size);
        it.sudo = true;
        it
    }

    #[test]
    fn bulk_select_skips_sudo_items() {
        let mut app = test_app();
        app.tab = CLEAN_TAB;
        app.clean = Load::Ready(vec![
            clean_item("/tmp/thin-bulk-a", 1),
            sudo_clean_item("/tmp/thin-bulk-sudo", 1),
        ]);
        app.selected = vec![false, false];
        app.on_key(KeyEvent::from(KeyCode::Char('A')));
        assert_eq!(app.selected, vec![true, false], "A 全选不应包含 sudo 项");

        app.selected = vec![false, false];
        app.on_key(KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(app.selected, vec![true, false], "a 选安全不应包含 sudo 项");
    }

    #[test]
    fn selected_sudo_count_counts_only_selected_sudo() {
        let mut app = test_app();
        app.clean = Load::Ready(vec![
            clean_item("/tmp/thin-sudo-count-a", 1),
            sudo_clean_item("/tmp/thin-sudo-count-b", 1),
        ]);
        app.selected = vec![false, true];
        assert_eq!(app.selected_sudo_count(), 1);
        app.selected = vec![true, false];
        assert_eq!(app.selected_sudo_count(), 0);
    }

    #[test]
    fn confirm_dialog_warns_about_sudo() {
        let mut app = test_app();
        app.tab = CLEAN_TAB;
        app.clean = Load::Ready(vec![sudo_clean_item("/tmp/thin-confirm-sudo", 1)]);
        app.selected = vec![true];
        app.confirm = true;
        let text = render(&mut app, 100, 30);
        assert!(text.contains("管理员权限"), "{text}");
    }

    #[test]
    fn renders_every_tab_without_panic() {
        let mut app = test_app();
        for tab in 0..N_TABS {
            app.tab = tab;
            // 直接塞 Ready 空数据，避免 ensure 启动线程
            app.clean = Load::Ready(vec![]);
            app.apps = Load::Ready(vec![]);
            app.history = Load::Ready(vec![]);
            assert!(!render(&mut app, 100, 30).is_empty());
        }
        // 小窗口也不能 panic
        app.tab = HISTORY_TAB;
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
    fn clean_empty_state_is_friendly() {
        let mut app = test_app();
        app.tab = CLEAN_TAB;
        app.clean = Load::Ready(vec![]);
        let text = render(&mut app, 100, 30);
        assert!(text.contains("您的电脑很干净"), "{text}");
        // 默认隐藏，提示如何显示被隐藏的项
        assert!(text.contains("m 显示需 sudo"), "{text}");
        assert!(text.contains("b 显示 protect"), "{text}");

        // 已开启显示时，不再提示对应开关
        app.show_manual = true;
        app.show_protected = true;
        let text = render(&mut app, 100, 30);
        assert!(!text.contains("m 显示需 sudo"), "{text}");
        assert!(!text.contains("b 显示 protect"), "{text}");
    }

    #[test]
    fn apps_tab_shows_running_status() {
        let mut app = test_app();
        app.tab = APPS_TAB;
        let mut running = app_info("Running", Some("com.example.run"));
        running.running = true;
        app.apps = Load::Ready(vec![running, app_info("Idle", Some("com.example.idle"))]);
        let text = render(&mut app, 120, 30);
        assert!(text.contains('●'), "列表应有运行标记: {text}");
        assert!(text.contains("状态: 运行中"), "详情应显示运行状态: {text}");
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
            sudo: 0,
        });
        let text = render(&mut app, 100, 30);
        assert!(
            text.contains("确认卸载") && text.contains("[y] 卸载"),
            "{text}"
        );
        assert!(text.contains("[n / Esc] 取消"), "{text}");
    }

    #[test]
    fn uninstall_confirm_warns_about_sudo() {
        let mut app = test_app();
        app.tab = APPS_TAB;
        app.apps = Load::Ready(vec![app_info("Foo", Some("com.example.foo"))]);
        app.uninstall_confirm = Some(PendingUninstall {
            app_name: "Foo".into(),
            app_path: PathBuf::from("/Applications/Foo.app"),
            items: vec![clean_item("/Applications/Foo.app", 2048)],
            approved_bytes: 2048,
            skipped: 2,
            sudo: 2,
        });
        let text = render(&mut app, 100, 30);
        assert!(text.contains("需管理员权限"), "{text}");
        assert!(
            !text.contains("需 sudo 或受保护"),
            "不应再显示「将跳过」文案：{text}"
        );
    }

    #[test]
    fn apps_tab_hint_mentions_uninstall() {
        let mut app = test_app();
        app.tab = APPS_TAB;
        app.apps = Load::Ready(vec![app_info("Foo", Some("com.example.foo"))]);
        assert!(render(&mut app, 120, 30).contains("u 卸载"));
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
    fn o_key_toggles_orphans_view() {
        let mut app = test_app();
        app.tab = APPS_TAB;
        // 预置为 Ready，避免 toggle 时 spawn 真实扫描线程
        app.apps = Load::Ready(vec![]);
        app.orphans = Load::Ready(vec![]);
        app.on_key(KeyEvent::from(KeyCode::Char('o')));
        assert!(app.show_orphans, "按 o 应切换到孤立残留视图");
        app.on_key(KeyEvent::from(KeyCode::Char('o')));
        assert!(!app.show_orphans, "再按 o 应切回应用列表");
    }

    #[test]
    fn render_orphans_lists_bundle_ids() {
        use thin_core::apps::Leftover;
        let mut app = test_app();
        app.tab = APPS_TAB;
        app.show_orphans = true;
        app.orphans = Load::Ready(vec![OrphanApp {
            bundle_id: "com.example.gone".into(),
            leftovers: vec![Leftover {
                path: PathBuf::from("/Users/x/Library/Containers/com.example.gone"),
                size: 4096,
                sudo: false,
            }],
        }]);
        let text = render(&mut app, 120, 30);
        assert!(text.contains("已卸载残留"), "{text}");
        assert!(text.contains("com.example.gone"), "{text}");
    }

    #[test]
    fn orphan_confirm_prompts_to_clean() {
        let mut app = test_app();
        app.tab = APPS_TAB;
        app.show_orphans = true;
        app.orphan_confirm = Some(PendingOrphan {
            bundle_id: "com.example.gone".into(),
            items: vec![clean_item("/x/com.example.gone", 2048)],
            approved_bytes: 2048,
            skipped: 0,
            sudo: 0,
        });
        let text = render(&mut app, 120, 30);
        assert!(text.contains("确认清理孤立残留"), "{text}");
        assert!(text.contains("[y] 清理"), "{text}");
        assert!(text.contains("[n / Esc] 取消"), "{text}");
    }

    #[test]
    fn n_key_cancels_orphan_confirm() {
        let mut app = test_app();
        app.tab = APPS_TAB;
        app.orphan_confirm = Some(PendingOrphan {
            bundle_id: "com.example.gone".into(),
            items: vec![],
            approved_bytes: 0,
            skipped: 0,
            sudo: 0,
        });
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.orphan_confirm.is_none());
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
    fn kill_confirm_prompts_to_quit_running_app() {
        let mut app = test_app();
        app.tab = APPS_TAB;
        app.kill_confirm = Some(PendingKill {
            app_name: "Foo".into(),
            app_path: PathBuf::from("/Applications/Foo.app"),
        });
        let text = render(&mut app, 100, 30);
        assert!(text.contains("退出运行中的 App"), "{text}");
        assert!(text.contains("[y] 退出并卸载"), "{text}");
        assert!(text.contains("[n / Esc] 取消"), "{text}");
    }

    #[test]
    fn n_key_cancels_kill_confirm() {
        let mut app = test_app();
        app.tab = APPS_TAB;
        app.kill_confirm = Some(PendingKill {
            app_name: "Foo".into(),
            app_path: PathBuf::from("/Applications/Foo.app"),
        });
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.kill_confirm.is_none());
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
            sudo: 0,
        });
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.uninstall_confirm.is_none());
    }
}
