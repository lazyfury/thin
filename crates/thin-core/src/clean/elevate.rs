//! 提权清理：**单一入口**。
//!
//! 拒绝 `sudo thin` 整个进程提权（会让 `thin_home` / `protect` / 废纸篓落到 root 名下）；
//! 需 root 的项统一走 [`run_elevated`] → 内置 `__elevated-move` 子进程：
//! root 侧重过 [`plan_elevated_in`] 安全门、只接受 `sudo==true` 项、仍移入调用者
//! 隔离区并 `chown` 回用户，**绝不 `rm`**。

use super::journal::{Journal, quarantine_plan_into, thin_home};
use super::plan::plan_elevated_in;
use super::policy::user_home;
use crate::model::CleanItem;
use anyhow::{Context, Result};
use std::path::Path;

/// 当前进程是否以 root 运行。
///
/// 用于拒绝「`sudo thin` 整个进程提权」——那会让 `thin_home` / `protect` /
/// 废纸篓都落到 root 名下，静默绕过用户保护名单。提权只应走内置的
/// `__elevated-move` 子进程（见 [`elevated_move`]）。
pub fn is_root() -> bool {
    // SAFETY: geteuid 无副作用。
    unsafe { libc::geteuid() == 0 }
}

/// 取出候选项里需要 root 才能移动的项（原始列表，未过安全门）。
pub fn sudo_items(items: &[CleanItem]) -> Vec<CleanItem> {
    items.iter().filter(|it| it.sudo).cloned().collect()
}

/// 通过系统授权框（`osascript`）提权，把 `items` 移入调用者 `~/.thin/quarantine`。
///
/// - 仅接受 `sudo == true` 的项；混入其它项一律拒绝（提权通道专用）。
/// - 提权后由内置子命令 `__elevated-move` 重新过安全门再移动，**绝不 `rm`**。
/// - 隔离物 `chown` 回调用者，保证 `thin quarantine restore` 可恢复。
///
/// 用户取消授权或超时时返回 `Err`，调用方应回退到「跳过」语义。
pub fn run_elevated(items: &[CleanItem], user_home: &Path) -> Result<Journal> {
    let sudo: Vec<CleanItem> = items.iter().filter(|it| it.sudo).cloned().collect();
    if sudo.is_empty() {
        anyhow::bail!("没有需要提权的项");
    }
    if sudo.len() != items.len() {
        anyhow::bail!("提权通道只处理 sudo 项，收到非 sudo 项");
    }
    let exe = std::env::current_exe().context("无法定位 thin 可执行文件")?;
    let data_home = thin_home();
    std::fs::create_dir_all(&data_home).context("创建 thin 数据目录失败")?;
    let manifest = data_home.join(format!("elevate-{}.json", std::process::id()));
    std::fs::write(&manifest, serde_json::to_vec(&sudo)?).context("写入提权清单失败")?;

    let script = elevation_applescript(&exe, &manifest, user_home);
    let out = crate::proc::output_with_timeout(
        "osascript",
        &["-e", &script],
        // 用户可能在授权框前停留很久；给足超时但仍避免永久挂死。
        std::time::Duration::from_secs(600),
    );
    let _ = std::fs::remove_file(&manifest);
    let out = out.ok_or_else(|| anyhow::anyhow!("未能启动系统授权（osascript 不可用或超时）"))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        if err.contains("User canceled") || err.contains("-128") {
            anyhow::bail!("已取消授权");
        }
        anyhow::bail!("提权清理失败：{err}");
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let raw = raw.trim();
    let journal: Journal = serde_json::from_str(raw).with_context(|| {
        let preview: String = raw.chars().take(200).collect();
        format!("无法解析提权子进程输出：{preview}")
    })?;
    Ok(journal)
}

/// 便捷入口：取出 `items` 中的 sudo 项，以调用者主目录为隔离区提权移动。
///
/// 供 `clean` / `uninstall` / `orphans` 与 TUI 共用；无 sudo 项时报错。
pub fn elevate_sudo(items: &[CleanItem]) -> Result<Journal> {
    let sudo = sudo_items(items);
    if sudo.is_empty() {
        anyhow::bail!("没有需要提权的项");
    }
    let home =
        user_home().ok_or_else(|| anyhow::anyhow!("无法确定 HOME，提权清理需要用户主目录"))?;
    run_elevated(&sudo, &home)
}

/// root 子命令 `__elevated-move` 的实现：复核清单并移入调用者隔离区。
///
/// 清单内容一律不可信：重新过 [`plan_elevated_in`]，只移动仍能通过安全门的
/// sudo 项；结束后把隔离物所有者改回调用者。
pub fn elevated_move(manifest: &Path, user_home: &Path) -> Result<Journal> {
    if !is_root() {
        anyhow::bail!("内部错误：__elevated-move 必须以 root 运行");
    }
    // root 子进程的 HOME 默认是 /var/root；重置为调用者主目录，
    // 让 `static_protection_reason` 里基于 HOME 的隐私目录（Keychains、iCloud 等）
    // 保护对调用者仍然生效。
    // SAFETY: 这是提权子进程入口，此刻尚未启动其它线程，修改环境变量不会竞争。
    unsafe { std::env::set_var("HOME", user_home) };
    let raw = std::fs::read(manifest).context("读取提权清单失败")?;
    let items: Vec<CleanItem> = serde_json::from_slice(&raw).context("解析提权清单失败")?;
    if items.is_empty() {
        anyhow::bail!("提权清单为空");
    }
    if items.iter().any(|it| !it.sudo) {
        anyhow::bail!("提权清单包含非 sudo 项，拒绝执行");
    }
    // 重新过安全门；`plan_elevated_in` 会把 sudo 项放进 `sudo`、其它放进 `approved`。
    let plan = plan_elevated_in(user_home, &items);
    if !plan.approved.is_empty() {
        // 清单已要求全为 sudo 项，理论上不可达；双重保险。
        anyhow::bail!("提权清单安全检查异常，拒绝执行");
    }
    let journal = quarantine_plan_into(user_home, plan, false)?;
    if !journal.entries.is_empty() {
        let md = std::fs::metadata(user_home).context("读取用户主目录属性失败")?;
        use std::os::unix::fs::MetadataExt;
        chown_recursive(&journal.session_dir(user_home), md.uid(), md.gid())
            .context("恢复隔离物归属失败")?;
    }
    Ok(journal)
}

/// 递归把 `dir` 及其内容的所有者改为 `uid:gid`（提权子进程专用）。
pub fn chown_recursive(dir: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        let p = entry.path();
        let c = std::ffi::CString::new(p.as_os_str().as_bytes())
            .with_context(|| format!("路径含 NUL：{}", p.display()))?;
        // SAFETY: `c` 是合法 C 字符串；lchown 失败仅影响归属，不影响数据安全。
        let rc = unsafe { libc::lchown(c.as_ptr(), uid, gid) };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!("chown {} 失败：{e}", p.display());
        }
    }
    Ok(())
}

/// 构造 `osascript` 调用的 AppleScript：`do shell script … with administrator privileges`。
///
/// 路径先包成 AppleScript 字符串字面量，再用 `quoted form of` 转成 shell 安全形式，
/// 避免空格 / 引号 / 特殊字符造成注入。
fn elevation_applescript(exe: &Path, manifest: &Path, user_home: &Path) -> String {
    let exe_lit = applescript_literal(&exe.to_string_lossy());
    let man_lit = applescript_literal(&manifest.to_string_lossy());
    let home_lit = applescript_literal(&user_home.to_string_lossy());
    format!(
        "do shell script (quoted form of {exe_lit}) & \" __elevated-move --manifest \" & (quoted form of {man_lit}) & \" --user-home \" & (quoted form of {home_lit}) & \" --json\" with administrator privileges"
    )
}

/// 把字符串包成 AppleScript 字符串字面量（转义 `\` 与 `"`）。
fn applescript_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevation_applescript_uses_quoted_form() {
        let exe = Path::new("/Applications/My Thin/thin");
        let manifest = Path::new("/Users/me/.thin/elevate-1.json");
        let home = Path::new("/Users/me");
        let script = elevation_applescript(exe, manifest, home);
        assert!(script.contains("with administrator privileges"), "{script}");
        assert!(script.contains("__elevated-move"), "{script}");
        assert!(script.contains("quoted form of"), "{script}");
        assert!(script.contains("/Applications/My Thin/thin"), "{script}");
        assert!(script.contains("--user-home"), "{script}");
        // 通过 applescript_literal 验证路径内引号 / 反斜杠被转义，避免注入。
        assert_eq!(applescript_literal("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}
