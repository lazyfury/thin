//! 安全清理引擎（M1）——对外 facade。
//!
//! 设计原则：**默认不真正删除**，而是把目标「移入隔离区」，写入 Journal，可随时恢复。
//! 只有显式 purge 才会永久删除。
//!
//! 子模块（见 `docs/thin-fs-plan.md` §9）：
//! - [`policy`]：安全门（受保护路径 / 卷隔离 / 敏感目录名）；
//! - [`plan`]：清理计划（预演 = 执行同一安全门）；
//! - [`apply`]：移动/复制原语、系统废纸篓、统一执行入口；
//! - [`journal`]：隔离区账本、恢复与永久清除；
//! - [`elevate`]：提权清理的单一入口。
//!
//! 安全不变量见 `AGENTS.md` §5；本模块只做 facade re-export，保证对外 API 不变。

mod apply;
mod elevate;
mod journal;
mod plan;
mod policy;

pub use apply::{Applied, Mode, TrashReport, apply, default_mode, trash, trash_with};
pub use elevate::{
    chown_recursive, elevate_sudo, elevated_move, is_root, run_elevated, sudo_items,
};
pub use journal::{
    Journal, JournalEntry, RestoreReport, SkippedItem, list_journals, list_journals_in,
    purge_older_than, purge_older_than_in, purge_session, purge_session_in, quarantine,
    quarantine_into, quarantine_plan_into, quarantine_root, restore_session, restore_session_in,
    sessions_older_than, sessions_older_than_in, thin_home,
};
pub use plan::{Plan, plan, plan_elevated_in, plan_in};
pub use policy::{
    is_sensitive_dir_name, protection_reason, protection_reason_in, static_protection_reason,
};

#[cfg(test)]
mod tests {
    use super::policy::user_home;
    use super::*;
    use crate::model::{Category, CleanItem, Risk};
    use std::path::{Path, PathBuf};

    fn item(path: PathBuf, size: u64) -> CleanItem {
        CleanItem {
            rule_id: "test".into(),
            name: "测试项".into(),
            path,
            category: Category::DevCache,
            risk: Risk::Safe,
            regenerable: true,
            sudo: false,
            size,
            reclaim: "手动删除".into(),
            explain: crate::model::Explain {
                what: "测试".into(),
                cost: "无".into(),
                recover: "重新生成".into(),
            },
            protected: false,
            protected_reason: None,
        }
    }

    #[test]
    fn protected_paths() {
        assert!(protection_reason(Path::new("/")).is_some());
        assert!(protection_reason(Path::new("/System/Library")).is_some());
        assert!(protection_reason(Path::new("/private/var/vm/sleepimage")).is_some());
        assert!(protection_reason(Path::new("/Library/Apple/Support")).is_some());
        let home = user_home().unwrap();
        assert!(protection_reason(&home).is_some());
        // 裸顶层根：整个目录不可删
        assert!(protection_reason(Path::new("/Applications")).is_some());
        assert!(protection_reason(Path::new("/Library")).is_some());
        assert!(protection_reason(Path::new("/private/var")).is_some());
        assert!(protection_reason(Path::new("/usr")).is_some());
        // 裸顶层根即使位于允许清单内，也不能删除目录本身；其子项仍可清理
        assert!(protection_reason(Path::new("/Library/Logs")).is_some());
        assert!(protection_reason(Path::new("/Library/Logs/DiagnosticReports")).is_none());
        assert!(protection_reason(Path::new("/usr/local")).is_some());
        assert!(protection_reason(Path::new("/usr/local/lib/foo")).is_none());
        // 个人目录顶层：整体不可清，但其内部具体缓存/产物允许
        assert!(protection_reason(&home.join("Documents")).is_some());
        assert!(protection_reason(&home.join("Downloads")).is_some());
        assert!(protection_reason(&home.join("Library")).is_some());
        assert!(protection_reason(&home.join("Documents/proj/node_modules")).is_none());
        // 普通文件不受保护
        assert!(protection_reason(Path::new("/tmp/thin-test-nonexistent")).is_none());
    }

    #[test]
    fn rejects_dangerous_shapes() {
        assert!(protection_reason(Path::new("/tmp/../etc/passwd")).is_some());
        assert!(protection_reason(Path::new("/tmp/a\nb")).is_some());
        assert!(protection_reason(Path::new("/private/var/db/other")).is_some());
        // 允许清单内的可重建缓存仍可清理
        assert!(protection_reason(Path::new("/tmp/thin-nonexistent")).is_none());
    }

    #[test]
    fn plan_matches_real_skip_reasons() {
        let base = std::env::temp_dir().join(format!("thin-plan-{}", std::process::id()));
        let data_home = base.join("data");
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let target = work.join("cache");
        std::fs::create_dir_all(&target).unwrap();

        let mut sudo_item = item(target.clone(), 10);
        sudo_item.sudo = true;
        let missing = item(work.join("nope"), 10);
        let protected = item(PathBuf::from("/System/Library"), 10);
        let ok = item(target.clone(), 1024);

        let candidates = vec![sudo_item.clone(), missing, protected, ok];
        let p = plan_in(&data_home, &candidates);
        assert_eq!(p.approved.len(), 1, "只有合法项应通过");
        assert_eq!(p.approved_bytes(), 1024);
        assert_eq!(p.skipped.len(), 3);
        assert!(p.sudo.is_empty(), "默认规划不暴露 sudo 项");

        // 真实执行使用同一安全门：跳过项数量一致
        let j = quarantine_into(&data_home, &candidates, false).unwrap();
        assert_eq!(j.entries.len(), p.approved.len());
        assert_eq!(j.skipped.len(), p.skipped.len());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn plan_elevated_buckets_sudo_but_still_enforces_gate() {
        let base = std::env::temp_dir().join(format!("thin-elev-{}", std::process::id()));
        let data_home = base.join("data");
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let target = work.join("cache");
        std::fs::create_dir_all(&target).unwrap();

        let mut sudo_item = item(target.clone(), 10);
        sudo_item.sudo = true;
        let normal = item(target.clone(), 1024);
        let protected = item(PathBuf::from("/System/Library"), 10);
        let missing = item(work.join("nope"), 10);

        let candidates = vec![sudo_item, normal, protected, missing];
        let p = plan_elevated_in(&data_home, &candidates);
        assert_eq!(p.approved.len(), 1, "非 sudo 项走普通通道：{p:?}");
        assert_eq!(p.sudo.len(), 1, "提权项进 sudo 桶：{p:?}");
        assert_eq!(p.skipped.len(), 2, "受保护 / 不存在仍被拒");
        assert_eq!(p.approved[0].size, 1024);
        assert_eq!(p.sudo[0].path, target);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sudo_items_only_returns_sudo() {
        let mut s = item(PathBuf::from("/tmp/thin-sudo-a"), 1);
        s.sudo = true;
        let n = item(PathBuf::from("/tmp/thin-sudo-b"), 1);
        let out = sudo_items(&[s, n]);
        assert_eq!(out.len(), 1);
        assert!(out[0].sudo);
    }

    #[test]
    fn run_elevated_rejects_non_sudo_items_without_spawning() {
        let mut s = item(PathBuf::from("/tmp/thin-elev-a"), 1);
        s.sudo = true;
        let n = item(PathBuf::from("/tmp/thin-elev-b"), 1);
        // 混入非 sudo 项：应在调用 osascript 之前就报错（不会弹授权框）
        let err = run_elevated(&[s, n], Path::new("/tmp")).unwrap_err();
        assert!(err.to_string().contains("非 sudo 项"), "{err:#}");
        assert!(run_elevated(&[], Path::new("/tmp")).is_err());
    }

    #[test]
    fn elevate_sudo_is_a_noop_error_without_sudo_items() {
        // 无 sudo 项时直接报错，绝不启动 osascript（不会弹授权框）
        let n = item(PathBuf::from("/tmp/thin-elev-sudo-x"), 1);
        assert!(elevate_sudo(&[]).is_err());
        assert!(elevate_sudo(&[n]).is_err());
    }

    #[test]
    fn quarantine_and_restore_roundtrip() {
        let base = std::env::temp_dir().join(format!("thin-test-{}", std::process::id()));
        let data_home = base.join("data");
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();

        // 造一个待清理目录
        let target = work.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.bin"), vec![0u8; 1024]).unwrap();

        let j = quarantine_into(&data_home, &[item(target.clone(), 1024)], false).unwrap();
        assert_eq!(j.entries.len(), 1);
        assert!(!target.exists(), "原路径应已被移走");
        assert!(j.entries[0].stored.exists());

        // 能列出
        let list = list_journals_in(&data_home).unwrap();
        assert_eq!(list.len(), 1);

        // 恢复
        let report = restore_session_in(&data_home, &j.session).unwrap();
        assert_eq!(report.restored, 1);
        assert!(target.exists(), "恢复后原路径应存在");
        assert!(target.join("a.bin").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `~/Library/Caches` 这类带 `deny delete` ACL 的目录无法整体 rename；
    /// 应退化为「只搬内容」，且恢复时能把内容移回原目录。
    #[cfg(target_os = "macos")]
    #[test]
    fn quarantine_falls_back_to_contents_on_deny_delete_acl() {
        use std::process::Command;
        let base = std::env::temp_dir().join(format!("thin-deny-{}", std::process::id()));
        let data_home = base.join("data");
        let work = base.join("work");
        let cache = work.join("Caches");
        std::fs::create_dir_all(cache.join("a")).unwrap();
        std::fs::write(cache.join("a").join("x.bin"), vec![0u8; 1024]).unwrap();
        std::fs::write(cache.join("b.bin"), vec![0u8; 512]).unwrap();

        // 加 `deny delete` ACL：整体 rename 会被拒，触发退化路径。
        let acl = Command::new("chmod")
            .args(["+a", "group:everyone deny delete"])
            .arg(&cache)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !acl {
            let _ = std::fs::remove_dir_all(&base);
            return; // 环境不支持 ACL，跳过
        }

        let j = quarantine_into(&data_home, &[item(cache.clone(), 1536)], false).unwrap();
        assert_eq!(j.entries.len(), 1);
        assert!(j.entries[0].contents_only, "应退化为只搬内容");
        assert!(cache.exists(), "原目录因 ACL 仍在原位");
        assert!(!cache.join("a").exists(), "子目录应已被移走");
        assert!(!cache.join("b.bin").exists(), "子文件应已被移走");
        assert!(j.entries[0].stored.join("a").exists());
        assert!(j.entries[0].stored.join("b.bin").exists());

        let report = restore_session_in(&data_home, &j.session).unwrap();
        assert_eq!(report.restored, 1);
        assert!(cache.join("a").join("x.bin").exists());
        assert!(cache.join("b.bin").exists());

        let _ = Command::new("chmod")
            .args(["-a", "group:everyone deny delete"])
            .arg(&cache)
            .status();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 测试替身：对指定目录返回整体移动失败（模拟 `deny delete` ACL），
    /// 其余路径（含其子项）返回成功。
    struct DenyDirPlatform {
        deny: PathBuf,
    }

    impl crate::platform::Platform for DenyDirPlatform {
        fn capacity(&self, _path: &str) -> Option<crate::probe::Capacity> {
            None
        }
        fn is_app_running(&self, _app: &Path) -> Option<bool> {
            None
        }
        fn bundle_id(&self, _app: &Path) -> Option<String> {
            None
        }
        fn full_disk_access(&self) -> Option<bool> {
            None
        }
        fn dir_usage(&self, _path: &Path) -> Option<crate::fsutil::Usage> {
            None
        }
        fn app_sandbox_info(&self, _app: &Path) -> Option<crate::platform::SandboxInfo> {
            None
        }
        fn sandbox_containers(
            &self,
            _home: &Path,
        ) -> Option<Vec<crate::platform::SandboxContainer>> {
            None
        }
        fn trash_item(&self, path: &Path) -> Option<bool> {
            Some(path != self.deny)
        }
        fn trash_available(&self) -> bool {
            true
        }
    }

    /// 系统废纸篓模式下，带 `deny delete` ACL 的目录（如 `~/Library/Caches`）
    /// 无法整体移走，应退化为只搬内容，而不是整项失败。
    #[test]
    fn trash_falls_back_to_contents_on_deny_delete_acl() {
        let base = std::env::temp_dir().join(format!("thin-trash-deny-{}", std::process::id()));
        let work = base.join("work");
        let cache = work.join("Caches");
        std::fs::create_dir_all(cache.join("a")).unwrap();
        std::fs::write(cache.join("a").join("x.bin"), vec![0u8; 1024]).unwrap();
        std::fs::write(cache.join("b.bin"), vec![0u8; 512]).unwrap();

        let report = trash_with(
            &DenyDirPlatform {
                deny: cache.clone(),
            },
            &[item(cache.clone(), 1536)],
        )
        .unwrap();
        assert_eq!(report.trashed.len(), 0, "目录整体移入应失败");
        assert_eq!(
            report.contents_only,
            vec![cache.clone()],
            "应退化为只搬内容"
        );
        assert_eq!(report.moved_count(), 1);
        assert_eq!(report.contents_count(), 1);
        assert!(
            report.failed.is_empty(),
            "子项应全部成功: {:?}",
            report.failed
        );
        assert!(
            report.trashed_bytes >= 1536,
            "字节数应计入内容: {}",
            report.trashed_bytes
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn purge_removes_session() {
        let base = std::env::temp_dir().join(format!("thin-purge-{}", std::process::id()));
        let data_home = base.join("data");
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let f = work.join("cache.bin");
        std::fs::write(&f, vec![0u8; 2048]).unwrap();

        let j = quarantine_into(&data_home, &[item(f.clone(), 2048)], false).unwrap();
        let freed = purge_session_in(&data_home, &j.session).unwrap();
        assert_eq!(freed, 2048);
        assert!(!f.exists());
        assert!(list_journals_in(&data_home).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&base);
    }
}
