//! 目录用途目录库（Purpose Catalog）。
//!
//! 与清理规则解耦：规则回答「能不能清」，用途库回答「这是什么、为什么存在」，
//! 主要面向「学习 / 看懂系统盘」的场景。数据在 `catalog/purposes.json`，
//! 编译期嵌入，可随版本更新、也可被用户覆盖。
//!
//! 匹配规则：
//! - `paths`：绝对路径，精确匹配或作为前缀（按路径组件）匹配；
//!   多个条目命中时取**最具体**（匹配前缀最长）的一条。
//! - `names`：目录名匹配（任意层级），置信度低于路径匹配，只在路径未命中时兜底。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const BUILTIN: &str = include_str!("../../catalog/purposes.json");

/// 用途大类（用于分组与着色）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PurposeKind {
    /// 系统本体（只读系统卷等）
    System,
    /// 可变系统数据
    SystemData,
    /// 用户个人数据
    UserData,
    /// 应用安装
    App,
    /// 应用数据（配置等）
    AppData,
    /// 缓存
    Cache,
    /// 日志
    Log,
    /// 开发相关
    Dev,
    /// 媒体
    Media,
    /// 虚拟机
    Vm,
    /// 引导
    Boot,
    /// 挂载点
    Mount,
    /// 设备
    Device,
    /// 配置
    Config,
    #[default]
    Other,
}

impl PurposeKind {
    pub fn label(&self) -> &'static str {
        match self {
            PurposeKind::System => "系统",
            PurposeKind::SystemData => "系统数据",
            PurposeKind::UserData => "个人数据",
            PurposeKind::App => "应用",
            PurposeKind::AppData => "应用数据",
            PurposeKind::Cache => "缓存",
            PurposeKind::Log => "日志",
            PurposeKind::Dev => "开发",
            PurposeKind::Media => "媒体",
            PurposeKind::Vm => "虚拟机",
            PurposeKind::Boot => "引导",
            PurposeKind::Mount => "挂载",
            PurposeKind::Device => "设备",
            PurposeKind::Config => "配置",
            PurposeKind::Other => "其它",
        }
    }
}

/// 删除安全性（用途库给出的「能不能删」判断，不等于清理规则的 risk）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Safety {
    /// 受保护，永不可动
    Protected,
    /// 重要数据，不该随手删
    Precious,
    /// 可再生，可安全清理
    Regenerable,
    #[default]
    Unknown,
}

impl Safety {
    pub fn label(&self) -> &'static str {
        match self {
            Safety::Protected => "受保护",
            Safety::Precious => "重要",
            Safety::Regenerable => "可再生",
            Safety::Unknown => "未知",
        }
    }
}

/// 一条用途说明
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub names: Vec<String>,
    pub title: String,
    #[serde(default)]
    pub kind: PurposeKind,
    #[serde(default)]
    pub safety: Safety,
    pub note: String,
    /// 学习参考（如 `man hier`）
    #[serde(default)]
    pub reference: Option<String>,
}

#[derive(Debug, Deserialize)]
struct File {
    entries: Vec<Entry>,
}

/// 用途目录库
#[derive(Debug, Default)]
pub struct Catalog {
    entries: Vec<Entry>,
}

impl Catalog {
    /// 仅内置用途库
    pub fn builtin() -> Self {
        let entries = serde_json::from_str::<File>(BUILTIN)
            .map(|f| f.entries)
            .unwrap_or_default();
        Catalog { entries }
    }

    /// 加载内置用途库（保留用户覆盖的扩展点：未来读取 `~/.thin/purposes.json`）
    pub fn load() -> Self {
        Self::builtin()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// 按路径查最具体的用途条目（精确优先，其次前缀最长）。
    pub fn lookup_path(&self, path: &Path) -> Option<&Entry> {
        let canon = soft_canon(path);
        self.entries
            .iter()
            .filter_map(|e| path_specificity(e, &canon).map(|spec| (spec, e)))
            .max_by_key(|(spec, _)| *spec)
            .map(|(_, e)| e)
    }

    /// 按目录名兜底匹配（低置信）。
    pub fn lookup_name(&self, name: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.names.iter().any(|n| n == name))
    }
}

/// 匹配优先级：先看是否精确命中，再比路径组件数（更长更具体）。
/// 返回 `(是否精确, 组件数)`，未匹配为 None。
fn path_specificity(entry: &Entry, path: &Path) -> Option<(u8, usize)> {
    entry
        .paths
        .iter()
        .filter_map(|raw| {
            let p = crate::fsutil::expand(raw)?;
            let p = soft_canon(&p);
            // 根目录是通用前缀，会匹配一切，故只允许精确匹配
            if p == Path::new("/") {
                return (path == p).then_some((1u8, 1usize));
            }
            if path == p {
                Some((1, p.components().count()))
            } else if path.starts_with(&p) {
                Some((0, p.components().count()))
            } else {
                None
            }
        })
        .max()
}

fn soft_canon(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_parses_and_covers_root_unix_dirs() {
        let c = Catalog::builtin();
        assert!(c.entries().len() >= 20, "用途库应覆盖根目录常用目录");
        for p in [
            "/bin", "/usr", "/etc", "/var", "/System", "/Library", "/Users",
        ] {
            let e = c
                .lookup_path(Path::new(p))
                .unwrap_or_else(|| panic!("缺少 {p} 的用途说明"));
            assert!(!e.title.is_empty() && !e.note.is_empty());
        }
    }

    #[test]
    fn most_specific_path_wins() {
        let c = Catalog::builtin();
        // /System 与 /System/Library 同时匹配，应取更具体的后者
        let e = c.lookup_path(Path::new("/System/Library")).unwrap();
        assert_eq!(e.title, "系统资源库");
        // 前缀匹配：更深的子路径仍归到最近的已描述祖先
        let e = c
            .lookup_path(Path::new("/System/Library/Frameworks"))
            .unwrap();
        assert_eq!(e.title, "系统资源库");
    }

    #[test]
    fn root_entry_matches_only_root() {
        let c = Catalog::builtin();
        assert_eq!(c.lookup_path(Path::new("/")).unwrap().title, "根目录");
        // 根条目是通用前缀，必须只精确匹配，不能把未知路径也标成「根目录」
        assert!(
            c.lookup_path(Path::new("/definitely-unknown-xyz"))
                .is_none()
        );
    }

    #[test]
    fn name_fallback_and_safety() {
        let c = Catalog::builtin();
        let e = c.lookup_name("node_modules").unwrap();
        assert_eq!(e.safety, Safety::Regenerable);
        assert!(c.lookup_name("no-such-dir-xyz").is_none());
    }

    #[test]
    fn home_dotdirs_do_not_just_inherit_home() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        let c = Catalog::builtin();
        let lookup = |rel: &str| c.lookup_path(&Path::new(&home).join(rel));

        // 常见 .xx 工具目录应各自有条目，而不是都显示「用户主目录」
        assert_eq!(lookup(".pi").unwrap().title, "pi Agent 数据");
        assert_eq!(lookup(".cargo").unwrap().title, "Cargo 主目录");
        assert_eq!(lookup(".vscode").unwrap().title, "VS Code 扩展与 CLI");
        // 更深子路径仍归到最近的已描述祖先
        assert_eq!(lookup(".pi/agent").unwrap().title, "pi Agent 数据");

        // 敏感/机密目录按受保护处理
        assert_eq!(lookup(".gnupg").unwrap().safety, Safety::Protected);
        assert_eq!(lookup(".ssh").unwrap().safety, Safety::Protected);
        // shell 会话记录是可再生的临时缓存
        assert_eq!(lookup(".zsh_sessions").unwrap().safety, Safety::Regenerable);
    }
}
