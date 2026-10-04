use crate::model::{CleanItem, ReclaimSummary, Risk, Rule};
use crate::rules;
use rayon::prelude::*;

/// 执行规则扫描（并行），返回按大小降序的清理项
pub fn scan(rules: &[Rule], include_destructive: bool, min_size: u64) -> Vec<CleanItem> {
    scan_progress(
        rules,
        include_destructive,
        min_size,
        &crate::progress::Progress::new(),
    )
}

/// 定向扫描：只保留位于 `scope` 之下的命中项（`scope=None` 等价于全量）。
///
/// 与 [`scope_items`] 的区别：这里把 `scope` 传入规则展开，使 `findDir` 规则
/// 会去 `scope` 下找 `target/` 等产物，而不只是事后过滤。
pub fn scan_scoped(
    rules: &[Rule],
    include_destructive: bool,
    min_size: u64,
    scope: Option<&std::path::Path>,
) -> Vec<CleanItem> {
    scan_progress_scoped(
        rules,
        include_destructive,
        min_size,
        scope,
        &crate::progress::Progress::new(),
    )
}

/// 按根目录过滤清理项：仅保留 `scope` 之下（含自身）的项。
///
/// 用于 `--root` 定向清理（如只清当前项目）：先全量扫描，再按规范化路径收窄，
/// 避免为「项目作用域」另造一套 matcher。`scope=None` 原样返回。
pub fn scope_items(items: Vec<CleanItem>, scope: Option<&std::path::Path>) -> Vec<CleanItem> {
    let Some(root) = scope else {
        return items;
    };
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    items
        .into_iter()
        .filter(|it| {
            let path = std::fs::canonicalize(&it.path).unwrap_or_else(|_| it.path.clone());
            path == root || path.starts_with(&root)
        })
        .collect()
}

/// 带进度上报的扫描
pub fn scan_progress(
    rules: &[Rule],
    include_destructive: bool,
    min_size: u64,
    progress: &crate::progress::Progress,
) -> Vec<CleanItem> {
    scan_progress_scoped(rules, include_destructive, min_size, None, progress)
}

/// 带进度上报 + 作用域（`--root`）的扫描。
///
/// `scope=Some(root)` 时：`findDir` 规则在 `root` 下查找，`path` 规则命中项随后
/// 被裁到 `root` 之内。两者结合使 `--root .` 成为真正的「只清这个项目」。
pub fn scan_progress_scoped(
    rules: &[Rule],
    include_destructive: bool,
    min_size: u64,
    scope: Option<&std::path::Path>,
    progress: &crate::progress::Progress,
) -> Vec<CleanItem> {
    progress.set_label("扫描规则");
    progress.set_total(rules.len() as u64);
    let per_rule: Vec<Vec<CleanItem>> = rules
        .par_iter()
        .map(|rule| {
            let paths = rules::expand_rule_scoped(rule, scope);
            let out: Vec<CleanItem> = paths
                .into_iter()
                .map(|path| {
                    let size = crate::fsutil::size_of(&path);
                    CleanItem {
                        rule_id: rule.id.clone(),
                        name: rule.name.clone(),
                        path,
                        category: rule.category,
                        risk: rule.risk,
                        regenerable: rule.regenerable,
                        sudo: rule.sudo,
                        size,
                        reclaim: rule.reclaim.clone(),
                        explain: rule.explain.clone(),
                        protected: false,
                        protected_reason: None,
                    }
                })
                .filter(|item| item.size >= min_size)
                .collect();
            progress.inc();
            out
        })
        .collect();

    let mut items: Vec<CleanItem> = per_rule.into_iter().flatten().collect();
    if !include_destructive {
        items.retain(|i| i.risk != Risk::Destructive);
    }
    // 作用域收窄：只保留位于 `--root` 之下的项
    if scope.is_some() {
        items = scope_items(items, scope);
    }
    // 标注保护：展示但不清除、不计入可回收。
    // ① 运行期 `thin protect` 名单；② 静态安全门（受保护路径 / 个人目录顶层 /
    //    裸顶层根等）。后者保证 `scan` 不会宣传 `clean` 必然跳过的项——
    //    `clean::plan` 与这里共用同一个 `static_protection_reason`。
    // 需 sudo 的项已由 `summarize` 归入「需手动」，不在此重复标记。
    mark_protection(&mut items);
    items.sort_by_key(|i| std::cmp::Reverse(i.size));
    items
}

/// 批量标注保护状态（供 scan 使用，加载一次保护名单）。
pub fn mark_protection(items: &mut [CleanItem]) {
    let protect_list = crate::protect::load();
    mark_protection_with(items, &protect_list);
}

/// 在已加载的保护名单上标注，避免逐项读盘（便于测试）。
///
/// 需 sudo 的项不参与静态保护标记：它们本来就不会被 thin 自动移动，
/// 已由 `summarize` 计入「需手动」，交给用户手动处理。
pub fn mark_protection_with(items: &mut [CleanItem], protect_list: &[std::path::PathBuf]) {
    for it in items {
        it.protected = false;
        it.protected_reason = None;
        if crate::protect::matches(protect_list, &it.path) {
            it.protected = true;
            it.protected_reason = Some(crate::protect::REASON.to_string());
        } else if !it.sudo
            && let Some(reason) = crate::clean::static_protection_reason(&it.path)
        {
            it.protected = true;
            it.protected_reason = Some(reason);
        }
    }
}

/// thin 默认**不展示、也不执行**的「需手动」项：
///
/// - 需要 root（`item.sudo`）；
/// - 受静态安全门保护（SIP / 裸顶层目录 / 个人目录顶层 / 跨卷等）。
///
/// `thin protect` 名单**不算**在这里：那是用户主动保护，仍应可见（标为「已保护」）。
pub fn is_manual(item: &CleanItem) -> bool {
    item.sudo
        || (item.protected && item.protected_reason.as_deref() != Some(crate::protect::REASON))
}

/// 判断某路径是否被列表中另一个路径包含（嵌套重复）
fn is_nested<'a>(path: &std::path::Path, all: &'a [CleanItem]) -> Option<&'a CleanItem> {
    all.iter()
        .find(|o| o.path != path && path.starts_with(&o.path))
}

/// 汇总可回收空间（排除嵌套重复项与需 sudo 的项，避免虚高与不可执行）。
///
/// 关键点：**按风险分层去重**。低风险项在默认清理时会被独立选中，
/// 若它恰好嵌在一个高风险父项内（如 `~/Library/Caches/Google` 嵌在
/// `~/Library/Caches` 内），按全量去重会把它的体积吞进父项，导致
/// `scan` 报「安全 0 B」而 `clean` 默认实际又能安全清理，口径自相矛盾。
///
/// 因此每一档只对「该档及更低风险的项」做顶层去重，再取增量：
/// - `safe`       = 只选安全项时的可释放量
/// - `confirm`    = 加上需确认项后**新增**的可释放量
/// - `destructive`= 再加上不可再生项后**新增**的量（仅 `--id` 会用到）
pub fn summarize(items: &[CleanItem]) -> ReclaimSummary {
    let mut s = ReclaimSummary::default();

    // 保护名单项单独计一档，且不参与可回收核算
    let active: Vec<CleanItem> = items.iter().filter(|i| !i.protected).cloned().collect();
    let protected: Vec<CleanItem> = items.iter().filter(|i| i.protected).cloned().collect();
    s.protected = top_level(&protected)
        .iter()
        .fold(0u64, |acc, i| acc.saturating_add(i.size));

    // 某风险档及以下的「顶层非 sudo」体积
    let top_bytes = |max: Risk| -> u64 {
        let subset: Vec<CleanItem> = active.iter().filter(|i| i.risk <= max).cloned().collect();
        top_level(&subset)
            .iter()
            .filter(|i| !i.sudo)
            .fold(0u64, |acc, i| acc.saturating_add(i.size))
    };

    s.safe = top_bytes(Risk::Safe);
    s.confirm = top_bytes(Risk::Confirm).saturating_sub(s.safe);
    s.destructive = top_bytes(Risk::Destructive)
        .saturating_sub(s.safe)
        .saturating_sub(s.confirm);

    // 需 sudo 的项：只在「非保护项的顶层」中统计，被父项覆盖的子项不重复计入
    s.manual = top_level(&active)
        .iter()
        .filter(|i| i.sudo)
        .fold(0u64, |acc, i| acc.saturating_add(i.size));

    s
}

/// 只保留最顶层的项（排除被其它项包含的嵌套项），保持输入顺序。
///
/// 清理时用它去重：若同时选中 `~/Library/Caches` 与其子目录，只处理父目录即可，
/// 否则会重复计数、且子项会在父项被移走后报「路径不存在」。
pub fn top_level(items: &[CleanItem]) -> Vec<CleanItem> {
    items
        .iter()
        .filter(|it| is_nested(&it.path, items).is_none())
        .cloned()
        .collect()
}

/// 一组「将真正执行」的项预计可释放的字节数。
///
/// 与 [`summarize`] 的区别：这里按清理实际会发生的情况计算，
/// 排除需 sudo 的项与嵌套重复项，但不区分 risk（供 `--id` 显式指定 destructive 时使用）。
pub fn planned_bytes(items: &[CleanItem]) -> u64 {
    items
        .iter()
        .filter(|it| !it.sudo)
        .filter(|it| is_nested(&it.path, items).is_none())
        .fold(0u64, |acc, it| acc.saturating_add(it.size))
}

/// 规则命中的总量（去除嵌套重复项，含所有风险等级）。
///
/// 用于覆盖率自检：与卷已用量对比，说明「已知可清理项」只占已用空间的一小部分，
/// 其余为系统/应用/用户数据。注意：它**不等于**「未归类」——未归类需用 `discover` 局部归因。
pub fn accounted_bytes(items: &[CleanItem]) -> u64 {
    items
        .iter()
        .filter(|it| is_nested(&it.path, items).is_none())
        .fold(0u64, |acc, it| acc.saturating_add(it.size))
}

/// 统计被嵌套（重复）的项数
pub fn nested_count(items: &[CleanItem]) -> usize {
    items
        .iter()
        .filter(|it| is_nested(&it.path, items).is_some())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Category, CleanItem, Explain, Risk};
    use std::path::PathBuf;

    fn item(path: &str, size: u64, risk: Risk, sudo: bool) -> CleanItem {
        CleanItem {
            rule_id: "test".into(),
            name: "测试项".into(),
            path: PathBuf::from(path),
            category: Category::DevCache,
            risk,
            regenerable: true,
            sudo,
            size,
            reclaim: "手动删除".into(),
            explain: Explain {
                what: "测试".into(),
                cost: "无".into(),
                recover: "重新生成".into(),
            },
            protected: false,
            protected_reason: None,
        }
    }

    #[test]
    fn scope_items_keeps_only_under_root() {
        let items = vec![
            item("/a/proj/target", 10, Risk::Safe, false),
            item("/a/other/target", 10, Risk::Safe, false),
            item("/a/proj", 10, Risk::Safe, false),
        ];
        let scoped = scope_items(items, Some(std::path::Path::new("/a/proj")));
        let paths: Vec<String> = scoped
            .iter()
            .map(|i| i.path.display().to_string())
            .collect();
        assert_eq!(paths, vec!["/a/proj/target", "/a/proj"]);
    }

    #[test]
    fn summarize_excludes_nested_children() {
        let items = vec![
            item("/a/caches", 1000, Risk::Safe, false),
            item("/a/caches/homebrew", 400, Risk::Safe, false),
        ];
        let s = summarize(&items);
        assert_eq!(s.safe, 1000, "子目录不应重复计入");
        assert_eq!(s.confirm, 0);
        assert_eq!(nested_count(&items), 1);
        assert_eq!(top_level(&items).len(), 1);
        assert_eq!(planned_bytes(&items), 1000);
    }

    #[test]
    fn summarize_keeps_safe_child_under_confirm_parent() {
        // `~/Library/Caches`(confirm) 里嵌着 `.../homebrew`(safe)：默认清理只选 safe，
        // 所以 safe 档必须报出 400（否则就是「scan 说安全 0 B，clean 却能清」的矛盾）。
        let items = vec![
            item("/a/caches", 1000, Risk::Confirm, false),
            item("/a/caches/homebrew", 400, Risk::Safe, false),
        ];
        let s = summarize(&items);
        assert_eq!(s.safe, 400);
        assert_eq!(
            s.confirm, 600,
            "confirm 增量 = 父项 1000 - 已计入的 safe 400"
        );
        assert_eq!(s.total_reclaimable(), 1000);
        assert_eq!(s.manual, 0);
    }

    #[test]
    fn summarize_buckets_sudo_as_manual() {
        let items = vec![
            item("/a/safe", 1000, Risk::Safe, false),
            item("/a/logs", 2000, Risk::Safe, true),
        ];
        let s = summarize(&items);
        assert_eq!(s.safe, 1000);
        assert_eq!(s.manual, 2000);
        assert_eq!(s.total_reclaimable(), 1000, "sudo 项不计入可回收");
    }

    #[test]
    fn planned_bytes_keeps_destructive_but_drops_sudo() {
        let items = vec![
            item("/a/vm", 5000, Risk::Destructive, false),
            item("/a/logs", 2000, Risk::Safe, true),
        ];
        // 显式指定 destructive 时仍应计入预计释放；sudo 始终排除
        assert_eq!(planned_bytes(&items), 5000);
    }

    #[test]
    fn nested_sudo_child_is_suppressed_by_parent() {
        let items = vec![
            item("/a/caches", 1000, Risk::Confirm, false),
            item("/a/caches/sys", 800, Risk::Safe, true),
        ];
        let s = summarize(&items);
        assert_eq!(s.confirm, 1000);
        assert_eq!(s.manual, 0, "被父项覆盖的 sudo 子项不单独计入 manual");
    }

    #[test]
    fn accounted_bytes_dedupes_nested_regardless_of_risk() {
        let items = vec![
            item("/a/caches", 1000, Risk::Safe, false),
            item("/a/caches/homebrew", 400, Risk::Safe, false),
            item("/a/vm", 5000, Risk::Destructive, false),
            item("/a/logs", 2000, Risk::Safe, true),
        ];
        assert_eq!(accounted_bytes(&items), 8000);
    }

    #[test]
    fn static_protected_items_are_marked_but_sudo_is_not() {
        let mut items = vec![
            item("/System/Library", 10, Risk::Safe, false),
            item("/System/Library", 10, Risk::Safe, true),
            item("/tmp/thin-nonexistent-xyz", 10, Risk::Safe, false),
        ];
        mark_protection_with(&mut items, &[]);
        assert!(items[0].protected, "静态保护路径应被标记");
        assert!(items[0].protected_reason.is_some());
        assert!(!items[1].protected, "需 sudo 项归入「需手动」，不重复标记");
        assert!(!items[2].protected);
    }
}
