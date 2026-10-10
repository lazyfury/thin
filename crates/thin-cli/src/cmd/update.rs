//! 通过 cargo 从 git 安装/更新 thin 自身。

use crate::*;

/// 默认安装源（官方仓库）。
const DEFAULT_GIT: &str = "https://github.com/lazyfury/thin.git";
const PKG: &str = "thin-cli";

pub(crate) fn cmd_update(args: UpdateArgs) -> Result<()> {
    let git = if args.git.is_empty() {
        DEFAULT_GIT
    } else {
        &args.git
    };
    let mut cmd = std::process::Command::new("cargo");
    cmd.args(["install", "--git", git, PKG, "--force"]);

    if args.dry_run {
        println!("cargo install --git {git} {PKG} --force");
        return Ok(());
    }

    eprintln!(
        "正在通过 cargo 安装最新版 thin（首次需编译，可能较慢）…\n  cargo install --git {git} {PKG} --force"
    );
    let status = cmd
        .status()
        .context("无法启动 cargo；请确认已安装 Rust 工具链")?;
    if !status.success() {
        anyhow::bail!("cargo install 失败（{status}）");
    }
    println!("更新完成。若 `thin --version` 仍是旧版，请确认 ~/.cargo/bin 在 PATH 前面。");
    Ok(())
}
