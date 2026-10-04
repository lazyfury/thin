//! 目录用途识别：把任意路径映射成「这是什么 / 能不能删 / 属于哪类」。
//!
//! 识别来源按优先级叠加：
//! 1. 安全门静态保护（`/System`、`~/.thin` 等）—— 一律标受保护；
//! 2. 清理规则（路径前缀 / findDir 名称）—— 标注「可清理」及风险、说明；
//! 3. 用途目录库（[`crate::catalog`]）—— 学习用的「这是什么」；
//! 4. 目录名启发式 —— 低置信的「疑似缓存/日志/临时」；
//! 5. 未识别。
//!
//! 识别是**只读**的：`cleanable=true` 只表示「命中规则」，真正的清理仍走
//! `clean::plan` 安全门，两者不会绕过彼此。

use crate::catalog::{Catalog, PurposeKind, Safety};
use crate::clean::static_protection_reason;
use crate::fsutil;
use crate::model::{Category, Explain, Matcher, Risk, Rule};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// 识别来源
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Rule,
    Catalog,
    Protected,
    Heuristic,
    Unknown,
}

/// 一次识别结果
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Recognition {
    pub title: String,
    pub note: String,
    pub kind: PurposeKind,
    pub safety: Safety,
    pub source: Source,
    /// 是否命中清理规则（可进入 thin clean 流程）
    pub cleanable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk: Option<Risk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<Category>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explain: Option<Explain>,
    /// 是否受安全门保护（永不可清理）
    pub protected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

impl Recognition {
    pub fn is_unknown(&self) -> bool {
        self.source == Source::Unknown
    }
}

/// 规则索引：把规则预先展开成可**即时匹配**的结构，避免识别时触发 findDir 遍历。
#[derive(Debug, Default)]
struct RuleIndex {
    /// (展开后的绝对路径, 规则下标)
    paths: Vec<(PathBuf, usize)>,
    /// (目录名, 需要的同级文件, 规则下标)
    find_dirs: Vec<(String, Option<String>, usize)>,
    rules: Vec<Rule>,
}

impl RuleIndex {
    fn build(rules: Vec<Rule>) -> Self {
        let mut paths = Vec::new();
        let mut find_dirs = Vec::new();
        for (i, r) in rules.iter().enumerate() {
            match &r.matcher {
                Matcher::Path { paths: ps } => {
                    for raw in ps {
                        if let Some(p) = fsutil::expand(raw) {
                            paths.push((p, i));
                        }
                    }
                }
                Matcher::FindDir {
                    dir_name,
                    require_sibling,
                    ..
                } => find_dirs.push((dir_name.clone(), require_sibling.clone(), i)),
                // 脚本型 matcher 不预展开（避免浏览时执行脚本）；仅清理扫描时运行
                Matcher::Script { .. } => {}
            }
        }
        RuleIndex {
            paths,
            find_dirs,
            rules,
        }
    }

    /// 命中路径规则（取最具体的规则）
    fn match_path(&self, path: &Path) -> Option<&Rule> {
        let canon = soft_canon(path);
        self.paths
            .iter()
            .filter(|(p, _)| canon == *p || canon.starts_with(p))
            .max_by_key(|(p, _)| p.components().count())
            .map(|(_, i)| &self.rules[*i])
    }

    /// 命中 findDir 规则（目录名 + 可选同级标志文件）
    fn match_find_dir(&self, path: &Path) -> Option<&Rule> {
        let name = path.file_name()?.to_str()?;
        self.find_dirs
            .iter()
            .find(|(dn, sibling, _)| {
                dn == name
                    && sibling
                        .as_ref()
                        .map(|s| path.parent().map(|p| p.join(s).exists()).unwrap_or(false))
                        .unwrap_or(true)
            })
            .map(|(_, _, i)| &self.rules[*i])
    }

    fn lookup(&self, path: &Path) -> Option<&Rule> {
        self.match_path(path).or_else(|| self.match_find_dir(path))
    }
}

/// 识别器：持有用途库与规则索引
pub struct Recognizer {
    catalog: Catalog,
    rules: RuleIndex,
}

impl Recognizer {
    pub fn load() -> anyhow::Result<Self> {
        Ok(Recognizer {
            catalog: Catalog::load(),
            rules: RuleIndex::build(crate::rules::load()?),
        })
    }

    /// 识别一个路径
    pub fn recognize(&self, path: &Path) -> Recognition {
        let cat = self.catalog.lookup_path(path);
        let rule = self.rules.lookup(path);
        let protection = static_protection_reason(path);

        let mut r = Recognition {
            title: String::new(),
            note: String::new(),
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
        };

        // 2) 用途库：先给「这是什么」
        if let Some(e) = cat {
            r.title = e.title.clone();
            r.note = e.note.clone();
            if e.kind != PurposeKind::Other {
                r.kind = e.kind;
            }
            r.safety = e.safety;
            r.reference = e.reference.clone();
            r.source = Source::Catalog;
        }

        // 3) 清理规则：叠加「可清理」信息
        if let Some(rule) = rule {
            r.cleanable = true;
            r.risk = Some(rule.risk);
            r.category = Some(rule.category);
            r.rule_id = Some(rule.id.clone());
            r.explain = Some(rule.explain.clone());
            // 规则名更具体（如「Chrome 缓存」优于泛化的「用户缓存」），优先采用
            r.title = rule.name.clone();
            if r.note.is_empty() {
                r.note = rule.explain.what.clone();
            }
            r.kind = kind_for_category(rule.category);
            r.safety = if rule.regenerable {
                Safety::Regenerable
            } else {
                Safety::Precious
            };
            if r.source == Source::Unknown {
                r.source = Source::Rule;
            }
        }

        // 4) 名称启发式（低置信兜底）
        if r.title.is_empty() {
            if let Some(e) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| self.catalog.lookup_name(n))
            {
                r.title = e.title.clone();
                r.note = e.note.clone();
                r.kind = e.kind;
                r.safety = e.safety;
                r.source = Source::Catalog;
            } else if let Some((title, kind)) = heuristic(path) {
                r.title = title.into();
                r.kind = kind;
                r.safety = Safety::Regenerable;
                r.source = Source::Heuristic;
                r.note = "按目录名推测，仅供参考。".into();
            }
        }

        // 1) 静态保护最高优先：标注受保护并取消「可清理」呈报
        if let Some(reason) = protection {
            r.protected = true;
            r.cleanable = false;
            r.risk = None;
            r.safety = Safety::Protected;
            if r.title.is_empty() {
                r.title = "受保护路径".into();
            }
            if r.note.is_empty() {
                r.note = reason;
            }
            if r.source == Source::Unknown {
                r.source = Source::Protected;
            }
        }

        if r.title.is_empty() {
            r.title = "未识别".into();
            r.source = Source::Unknown;
        }
        r
    }
}

fn kind_for_category(c: Category) -> PurposeKind {
    match c {
        Category::SystemCache | Category::AppCache | Category::Trash => PurposeKind::Cache,
        Category::DevCache => PurposeKind::Dev,
        Category::Vm => PurposeKind::Vm,
        Category::Log => PurposeKind::Log,
        Category::Leftover => PurposeKind::AppData,
        Category::Other => PurposeKind::Other,
    }
}

/// 目录名启发式（低置信）
fn heuristic(path: &Path) -> Option<(&'static str, PurposeKind)> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if name.contains("cache") {
        Some(("疑似缓存", PurposeKind::Cache))
    } else if name.contains("log") {
        Some(("疑似日志", PurposeKind::Log))
    } else if name.contains("tmp") || name.contains("temp") {
        Some(("疑似临时文件", PurposeKind::Cache))
    } else {
        None
    }
}

fn soft_canon(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(path: &str) -> Recognition {
        Recognizer::load().unwrap().recognize(Path::new(path))
    }

    #[test]
    fn root_unix_dirs_are_described_and_protected() {
        let r = rec("/bin");
        assert_eq!(r.title, "Unix 基础命令");
        assert!(r.protected);
        assert!(!r.cleanable);
        assert_eq!(r.safety, Safety::Protected);
    }

    #[test]
    fn personal_dir_protected_with_catalog_note() {
        if let Ok(home) = std::env::var("HOME") {
            let r = rec(&format!("{home}/Documents"));
            assert!(r.protected, "个人目录顶层应受保护");
            // 已由安全门保护，但用途库/启发式可补充说明
            assert!(!r.cleanable);
        }
    }

    #[test]
    fn unresolvable_path_is_unknown() {
        let r = rec("/definitely/not/a/real/path/xyz");
        assert!(r.is_unknown());
        assert!(!r.cleanable && !r.protected);
    }

    #[test]
    fn heuristic_flags_cache_like_name() {
        let r = rec("/tmp/thin-nonexistent/SomeAppCache");
        assert_eq!(r.source, Source::Heuristic);
        assert_eq!(r.kind, PurposeKind::Cache);
    }
}
