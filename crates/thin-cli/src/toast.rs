//! 底部瞬时提示（toast）——统一 TUI 的反馈语言。
//!
//! 交互约定：
//! - `Info`：操作成功等正向反馈，约 3.6s 自动消失；
//! - `Warn`：需要留意但不阻塞，约 6s 自动消失；
//! - `Error`：失败或被安全门拒绝，**保留在底部直到用户按 Esc 关闭**，不会自行消失。
//!
//! 颜色统一：info 灰底黑字、warn 黄底黑字、error 红底白字。
//! Esc 的行为：有提示时先关闭提示，再按一次才退出（见各 TUI 的按键处理）。

use ratatui::style::{Color, Modifier, Style};

/// 每 tick ≈ 80ms（事件循环轮询间隔）
const INFO_TTL: usize = 45; // ≈3.6s
const WARN_TTL: usize = 75; // ≈6.0s

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Info,
    Warn,
    Error,
}

pub struct Toast {
    kind: Kind,
    text: String,
    at: usize,
    ttl: usize,
}

impl Toast {
    fn new(kind: Kind, text: impl Into<String>, tick: usize, ttl: usize) -> Self {
        Self {
            kind,
            text: text.into(),
            at: tick,
            ttl,
        }
    }

    /// 正向反馈：会自动消失
    pub fn info(text: impl Into<String>, tick: usize) -> Self {
        Self::new(Kind::Info, text, tick, INFO_TTL)
    }

    /// 提示/拒绝执行：稍后自动消失
    pub fn warn(text: impl Into<String>, tick: usize) -> Self {
        Self::new(Kind::Warn, text, tick, WARN_TTL)
    }

    /// 错误：必须由用户显式关闭（Esc）
    pub fn error(text: impl Into<String>, tick: usize) -> Self {
        Self::new(Kind::Error, text, tick, usize::MAX)
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// 是否已过期（错误永不自动过期）
    pub fn expired(&self, tick: usize) -> bool {
        self.ttl != usize::MAX && tick.wrapping_sub(self.at) > self.ttl
    }
}

/// 状态栏配色：整条底栏带背景色，错误最醒目。
pub fn style(kind: Kind) -> Style {
    match kind {
        Kind::Info => Style::default().fg(Color::Black).bg(Color::Gray),
        Kind::Warn => Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        Kind::Error => Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD),
    }
}

/// 无提示时快捷键提示栏的样式（与 info 同色系，避免闪烁）
pub fn bar_style() -> Style {
    Style::default().fg(Color::Black).bg(Color::Gray)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_and_warn_expire_but_error_does_not() {
        let i = Toast::info("ok", 0);
        assert!(!i.expired(INFO_TTL));
        assert!(i.expired(INFO_TTL + 1));

        let e = Toast::error("bad", 0);
        assert!(!e.expired(1_000_000));
        assert_eq!(e.kind(), Kind::Error);
    }
}
