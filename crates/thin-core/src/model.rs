use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 清理项类别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    SystemCache,
    AppCache,
    DevCache,
    Vm,
    Log,
    Trash,
    Leftover,
    Other,
}

impl Category {
    pub fn label(&self) -> &'static str {
        match self {
            Category::SystemCache => "系统缓存",
            Category::AppCache => "应用缓存",
            Category::DevCache => "开发缓存",
            Category::Vm => "虚拟机",
            Category::Log => "日志",
            Category::Trash => "回收站",
            Category::Leftover => "残留",
            Category::Other => "其它",
        }
    }
}

/// 风险等级：Safe < Confirm < Destructive
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Safe,
    Confirm,
    Destructive,
}

impl Risk {
    pub fn label(&self) -> &'static str {
        match self {
            Risk::Safe => "安全",
            Risk::Confirm => "需确认",
            Risk::Destructive => "不可再生",
        }
    }

    pub fn color(&self) -> &'static str {
        match self {
            Risk::Safe => "\x1b[32m",        // 绿
            Risk::Confirm => "\x1b[33m",     // 黄
            Risk::Destructive => "\x1b[31m", // 红
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Explain {
    pub what: String,
    pub cost: String,
    pub recover: String,
}

/// 规则匹配方式
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Matcher {
    /// 固定路径（支持 ~ 展开）
    Path { paths: Vec<String> },
    /// 在某组根目录下查找指定名字的目录
    #[serde(rename_all = "camelCase")]
    FindDir {
        roots: Vec<String>,
        dir_name: String,
        require_sibling: Option<String>,
        max_depth: Option<usize>,
    },
}

/// 一条清理规则
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub category: Category,
    pub risk: Risk,
    pub regenerable: bool,
    #[serde(default)]
    pub sudo: bool,
    pub matcher: Matcher,
    pub reclaim: String,
    pub explain: Explain,
}

/// 扫描命中的一条可清理项
#[derive(Debug, Clone, Serialize)]
pub struct CleanItem {
    pub rule_id: String,
    pub name: String,
    pub path: PathBuf,
    pub category: Category,
    pub risk: Risk,
    pub regenerable: bool,
    pub sudo: bool,
    pub size: u64,
    pub reclaim: String,
    pub explain: Explain,
    /// 命中 `thin protect` 保护名单：展示但绝不清除、不计入可回收
    #[serde(default)]
    pub protected: bool,
}

impl CleanItem {
    /// 构造一个非规则来源的清理项（大文件/重复文件/App 卸载等）
    pub fn synthetic(path: PathBuf, size: u64, rule_id: &str, name: &str, risk: Risk) -> Self {
        CleanItem {
            rule_id: rule_id.to_string(),
            name: name.to_string(),
            path,
            category: Category::Other,
            risk,
            regenerable: false,
            sudo: false,
            size,
            reclaim: "移入隔离区".to_string(),
            explain: Explain {
                what: "由命令动态生成".to_string(),
                cost: "移入隔离区，可随时恢复".to_string(),
                recover: "quarantine restore".to_string(),
            },
            protected: false,
        }
    }
}

/// 可回收空间汇总
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReclaimSummary {
    pub safe: u64,
    pub confirm: u64,
    pub destructive: u64,
    /// 需 sudo、thin 会自动跳过、只能手动处理的体积（不计入可回收）
    pub manual: u64,
    /// 命中 `thin protect` 保护名单、不参与清理的体积（不计入可回收）
    #[serde(default)]
    pub protected: u64,
}

impl ReclaimSummary {
    pub fn total_reclaimable(&self) -> u64 {
        self.safe + self.confirm // 默认只把 safe + confirm 视为可回收
    }
}
