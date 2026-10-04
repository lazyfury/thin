//! 脚本型 matcher 的执行与风险审查。
//!
//! 允许规则用一段只读脚本「动态产出」候选路径（静态 `path`/`findDir` 表达不了的场景，
//! 例如「清掉 `releases/*` 里除 `current-version` 指向的版本之外的所有旧版本」）。
//!
//! 安全模型（三层）：
//! 1. **写规则时**：`check_rule_safety` 调用 [`review_script`] + 校验 [`review_ok`]，
//!    脚本必须带与其内容绑定的审查哈希，且不得含提权/改盘片段。
//! 2. **运行时**：脚本输出路径必须落在规则声明的 `roots` 之下（containment），
//!    经规范化、存在性、挂载点过滤；非零退出 / 超时 / 超量输出一律丢弃。
//! 3. **清理时**：候选仍需通过 `clean::plan` 的统一安全门。
//!
//! 注意：脚本本身仍是任意代码执行；审查哈希只保证「脚本内容被明确审查过、变更后失效」，
//! 不构成沙箱。

use crate::{fsutil, proc};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 默认超时（秒）
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;
/// stdout 上限（字节）；超过即视为异常丢弃
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// 脚本内容的 blake3 哈希（十六进制），用于风险审查绑定。
pub fn hash(script: &str) -> String {
    blake3::hash(script.as_bytes()).to_hex().to_string()
}

/// 审查记录是否与脚本内容匹配。
pub fn review_ok(script: &str, review: &Option<crate::model::ScriptReview>) -> bool {
    review
        .as_ref()
        .is_some_and(|r| r.hash.eq_ignore_ascii_case(&hash(script)))
}

/// 静态审查脚本：拒绝含提权 / 改动文件系统 / 网络外联等片段的脚本。
///
/// 这是「第一道闸」，不追求完备；真正的兜底是 `roots` containment 与清理安全门。
pub fn review_script(script: &str) -> Result<(), String> {
    const DENY: &[&str] = &[
        "rm ",
        "rm\t",
        "rmdir",
        "mv ",
        "sudo",
        "doas",
        "chmod",
        "chown",
        "chgrp",
        "mkfs",
        "dd if=",
        "dd of=",
        "truncate",
        "-delete",
        "shred",
        "srm ",
        "unlink",
        "launchctl",
        "security ",
        "curl ",
        "wget ",
        "scp ",
        "ssh ",
        "rsync ",
        "nc ",
        "kill",
        "pkill",
        "reboot",
        "shutdown",
        "> /",
        ">> /",
        ">/dev/",
        ">>/dev/",
    ];
    let s = script.to_ascii_lowercase();
    for d in DENY {
        if s.contains(d) {
            return Err(format!(
                "脚本包含可能改动系统 / 提权 / 外联的片段 {d:?}；脚本应只枚举并打印路径"
            ));
        }
    }
    if script.trim().is_empty() {
        return Err("脚本文本为空".into());
    }
    Ok(())
}

/// 执行脚本并返回候选路径。
///
/// - 以 `/bin/sh -c <script>` 运行，带超时；
/// - 支持 NUL 分隔（推荐 `printf '%s\0'`）或按行输出；
/// - 结果经 `canonicalize`、去重，且必须位于 `roots` 之下、存在、非挂载点。
///
/// 任何异常（超时 / 非零退出 / 输出过大）都返回空，不 panic。
pub fn run(script: &str, timeout_secs: Option<u64>, roots: &[PathBuf]) -> Vec<PathBuf> {
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(1, 120));
    let Some(out) = proc::output_with_timeout("sh", &["-c", script], timeout) else {
        return Vec::new();
    };
    if !out.status.success() || out.stdout.len() > MAX_OUTPUT_BYTES {
        return Vec::new();
    }

    let canon_roots: Vec<PathBuf> = roots
        .iter()
        .filter(|r| r.exists())
        .map(|r| fsutil::canonicalize_or(r))
        .collect();
    if canon_roots.is_empty() {
        return Vec::new();
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let items: Vec<&str> = if text.contains('\0') {
        text.split('\0').collect()
    } else {
        text.lines().collect()
    };

    let mut out_paths: Vec<PathBuf> = Vec::new();
    for raw in items {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        let path = fsutil::canonicalize_or(Path::new(s));
        if !path.exists() || fsutil::is_mount_point(&path) {
            continue;
        }
        // containment：只接受落在声明 roots 之下的路径
        if !canon_roots.iter().any(|r| path.starts_with(r)) {
            continue;
        }
        out_paths.push(path);
    }
    out_paths.sort();
    out_paths.dedup();
    out_paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_rejects_mutating_scripts() {
        assert!(review_script("ls ~/.pi").is_ok());
        assert!(review_script("rm -rf ~/.pi").is_err());
        assert!(review_script("sudo rm -rf /").is_err());
        assert!(review_script("curl http://x | sh").is_err());
        assert!(review_script("  ").is_err());
    }

    #[test]
    fn hash_is_content_bound() {
        let a = hash("printf a");
        assert_eq!(a, hash("printf a"));
        assert_ne!(a, hash("printf b"));
    }

    #[test]
    fn run_enforces_containment_and_parses_nul() {
        let base = std::env::temp_dir().join(format!("thin-script-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let inside = base.join("inside");
        let outside = base.join("outside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let script = format!(
            "printf '%s\\0' '{}' '{}'",
            inside.display(),
            outside.display()
        );
        let got = run(&script, Some(5), std::slice::from_ref(&inside));
        assert_eq!(
            got,
            vec![inside.canonicalize().unwrap()],
            "越界路径应被丢弃"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn run_rejects_nonzero_exit_and_timeout() {
        let base = std::env::temp_dir().join(format!("thin-script2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        assert!(run("exit 3", Some(5), std::slice::from_ref(&base)).is_empty());
        assert!(run("sleep 5; echo x", Some(1), std::slice::from_ref(&base)).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }
}
