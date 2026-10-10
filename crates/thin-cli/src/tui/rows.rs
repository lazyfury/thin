//! 树形行的扁平化与 ASCII 前缀绘制。

use super::*;

#[derive(Clone)]
pub(super) struct TreeRow {
    /// 节点显示名（合并的单链用 `/` 连接）
    pub(super) name: String,
    /// 规则名（目录分组节点为 `None`）
    pub(super) rule_name: Option<String>,
    /// 去重后的可释放量
    pub(super) size: u64,
    /// 子树中清理项数量
    pub(super) count: usize,
    /// 节点自身命中的清理项下标
    pub(super) item: Option<usize>,
    /// 子树中所有清理项下标（用于按文件夹整组勾选）
    pub(super) item_indices: Vec<usize>,
    /// 每层的「是否最后一个兄弟」标记，用于画连接线
    pub(super) is_last: Vec<bool>,
    pub(super) risk: Option<Risk>,
    pub(super) protected: bool,
    pub(super) sudo: bool,
    /// 位于某个命中项之下，已被父项覆盖
    pub(super) nested: bool,
}

impl TreeRow {
    pub(super) fn is_dir(&self) -> bool {
        self.item.is_none()
    }
}

pub(super) fn flatten_forest(forest: &[TreeNode], out: &mut Vec<TreeRow>) {
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
pub(super) fn tree_prefix(flags: &[bool]) -> String {
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
