//! 清理项的「按文件夹合并」树形视图。
//!
//! 把一组 [`CleanItem`] 按路径层级聚合成树：公共父目录成为分组节点，
//! 独占单链（如 `~/Library/Caches/Homebrew`）会被合并成一行，
//! 让「同一文件夹下的一堆缓存」在 UI 里收拢成一个可折叠的分组。
//!
//! 大小核算沿用 [`crate::scan::top_level`] 的口径：被某个命中项覆盖的
//! 嵌套项不再重复计入聚合大小（`nested=true`），因此文件夹节点的体积
//! 始终等于「从这里真正能清出来的量」。

use crate::model::CleanItem;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 树中的一个节点：目录分组或清理项。
///
/// 一个节点可以同时是「命中的清理项」和「多个子项的父目录」——例如
/// `~/Library/Caches` 本身命中规则，其下 `homebrew` 也命中的情况。
#[derive(Debug, Clone)]
pub struct TreeNode {
    /// 显示名（合并的单链会用 `/` 连接，如 `Library/Caches`）
    pub name: String,
    /// 该节点对应的路径（家目录前缀显示为 `~`）
    pub path: PathBuf,
    /// 去重后的可释放量：节点自身命中则取自身大小，否则取子树之和
    pub size: u64,
    /// 子树中清理项总数（含被父项覆盖的嵌套项）
    pub count: usize,
    /// 节点自身命中的清理项（目录分组节点为 `None`）
    pub item: Option<CleanItem>,
    /// `item` 在构建时输入切片中的下标（用于 TUI 勾选映射）
    pub item_index: Option<usize>,
    /// 位于某个命中项之下，已被父项覆盖，不计入聚合大小
    pub nested: bool,
    /// 子节点，按聚合大小降序
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    /// 收集本节点（含子树）中所有清理项在输入切片中的下标。
    pub fn item_indices(&self, out: &mut Vec<usize>) {
        if let Some(i) = self.item_index {
            out.push(i);
        }
        for c in &self.children {
            c.item_indices(out);
        }
    }

    /// 本节点（含子树）的所有清理项数量。
    pub fn item_count(&self) -> usize {
        self.count
    }
}

#[derive(Default)]
struct Builder {
    name: String,
    path: PathBuf,
    item: Option<CleanItem>,
    item_index: Option<usize>,
    children: BTreeMap<String, Builder>,
}

/// 把清理项按文件夹合并成森林（多棵树），按聚合大小降序。
///
/// `home` 提供时，位于家目录下的路径会显示成 `~/...`。
/// 相同路径的重复项只保留第一项，避免重复计数。
pub fn build_forest(items: &[CleanItem], home: Option<&Path>) -> Vec<TreeNode> {
    let mut roots: BTreeMap<String, Builder> = BTreeMap::new();
    for (idx, it) in items.iter().enumerate() {
        let comps = display_components(&it.path, home);
        let Some((head, rest)) = comps.split_first() else {
            continue;
        };
        let base = PathBuf::from(head);
        insert(&mut roots, head, rest, &base, it, idx);
    }
    let mut forest: Vec<TreeNode> = roots.into_values().map(|b| finalize(b, false)).collect();
    for r in &mut forest {
        sort_and_collapse(r);
    }
    forest.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    forest
}

/// 把绝对路径转成用于建树的组件；家目录前缀替换为 `~`。
fn display_components(path: &Path, home: Option<&Path>) -> Vec<String> {
    if let Some(home) = home
        && let Ok(rel) = path.strip_prefix(home)
    {
        let mut comps = vec!["~".to_string()];
        comps.extend(
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy().to_string()),
        );
        return comps;
    }
    path.components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy().to_string();
            if s.is_empty() { "/".to_string() } else { s }
        })
        .collect()
}

fn insert(
    map: &mut BTreeMap<String, Builder>,
    name: &str,
    rest: &[String],
    path: &Path,
    item: &CleanItem,
    idx: usize,
) {
    let entry = map.entry(name.to_string()).or_insert_with(|| Builder {
        name: name.to_string(),
        path: path.to_path_buf(),
        ..Default::default()
    });
    if rest.is_empty() {
        // 同路径重复命中只保留第一条，避免聚合大小翻倍
        if entry.item.is_none() {
            entry.item = Some(item.clone());
            entry.item_index = Some(idx);
        }
        return;
    }
    let child = path.join(&rest[0]);
    insert(&mut entry.children, &rest[0], &rest[1..], &child, item, idx);
}

fn finalize(b: Builder, under_item: bool) -> TreeNode {
    let has_item = b.item.is_some();
    let nested = under_item && has_item;
    let child_under = under_item || has_item;
    let children: Vec<TreeNode> = b
        .children
        .into_values()
        .map(|c| finalize(c, child_under))
        .collect();
    let count = children.iter().map(|c| c.count).sum::<usize>() + usize::from(has_item);
    let size = if has_item {
        b.item.as_ref().map(|i| i.size).unwrap_or(0)
    } else {
        children.iter().map(|c| c.size).sum()
    };
    TreeNode {
        name: b.name,
        path: b.path,
        size,
        count,
        item: b.item,
        item_index: b.item_index,
        nested,
        children,
    }
}

/// 排序并合并单链：自己不是命中项、且只有一个子目录时，把它并入本行。
fn sort_and_collapse(node: &mut TreeNode) {
    for c in &mut node.children {
        sort_and_collapse(c);
    }
    node.children
        .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    while node.item.is_none() && node.children.len() == 1 {
        let child = node.children.pop().expect("len == 1");
        node.name = join_name(&node.name, &child.name);
        node.path = child.path;
        node.item = child.item;
        node.item_index = child.item_index;
        node.nested = child.nested;
        node.size = child.size;
        node.count = child.count;
        node.children = child.children;
    }
}

fn join_name(parent: &str, child: &str) -> String {
    if parent == "/" {
        format!("/{child}")
    } else {
        format!("{parent}/{child}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Category, Explain, Risk};

    fn item(path: &str, size: u64) -> CleanItem {
        CleanItem {
            rule_id: "test".into(),
            name: format!("规则-{}", path.rsplit('/').next().unwrap_or("x")),
            path: PathBuf::from(path),
            category: Category::AppCache,
            risk: Risk::Safe,
            regenerable: true,
            sudo: false,
            size,
            reclaim: "rm".into(),
            explain: Explain {
                what: "x".into(),
                cost: "y".into(),
                recover: "z".into(),
            },
            protected: false,
            protected_reason: None,
        }
    }

    #[test]
    fn merges_shared_folders_and_collapses_chains() {
        let items = vec![
            item("/Users/me/Library/Caches/A", 100),
            item("/Users/me/Library/Caches/B", 300),
            item("/Users/me/Downloads/x", 50),
        ];
        let forest = build_forest(&items, Some(Path::new("/Users/me")));
        assert_eq!(forest.len(), 1);
        let root = &forest[0];
        assert_eq!(root.name, "~");
        assert_eq!(root.size, 450);
        assert_eq!(root.count, 3);
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].name, "Library/Caches");
        assert_eq!(root.children[0].size, 400);
        assert_eq!(root.children[1].name, "Downloads/x");
        // 单项独占的单链被合并到一行，并保留命中项
        assert!(root.children[1].item.is_some());
        assert_eq!(root.children[1].item_index, Some(2));
    }

    #[test]
    fn nested_item_not_double_counted() {
        let items = vec![
            item("/home/u/Caches", 1000),
            item("/home/u/Caches/homebrew", 400),
        ];
        let forest = build_forest(&items, Some(Path::new("/home/u")));
        let root = &forest[0];
        assert_eq!(root.name, "~/Caches");
        assert_eq!(root.size, 1000, "父项覆盖子项，不重复计入");
        assert_eq!(root.count, 2);
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].name, "homebrew");
        assert!(root.children[0].nested, "嵌套项应标记为 nested");
        let mut idx = Vec::new();
        root.item_indices(&mut idx);
        assert_eq!(idx, vec![0, 1]);
    }

    #[test]
    fn folder_node_aggregates_and_orders_by_size() {
        let items = vec![item("/data/A", 10), item("/data/B", 900)];
        let forest = build_forest(&items, None);
        let root = &forest[0];
        assert_eq!(root.name, "/data");
        assert_eq!(root.size, 910);
        assert!(root.item.is_none());
        assert_eq!(root.children[0].name, "B");
        assert_eq!(root.children[1].name, "A");
    }

    #[test]
    fn duplicate_path_kept_once() {
        let items = vec![item("/x/y", 10), item("/x/y", 999)];
        let forest = build_forest(&items, None);
        let root = &forest[0];
        assert_eq!(root.size, 10, "重复路径只计一次");
        assert_eq!(root.count, 1);
    }
}
