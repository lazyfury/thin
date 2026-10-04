//! 显示宽度感知的文本工具。
//!
//! TUI 里混排中英文时，`format!("{:<10}")` 按 **字符数** 补齐，而中日韩全角字符
//! 实际占 2 列，导致列对不齐。这里统一用 `unicode-width` 的**显示列宽**来补/截。

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 字符串在终端里占用的列数
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// 右侧补空格到指定显示宽度；已超宽则原样返回。
pub fn pad_end(s: &str, w: usize) -> String {
    let cur = width(s);
    if cur >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - cur))
    }
}

/// 按显示宽度截断，超宽时以 `…` 结尾（`…` 自身占 1 列）。
pub fn truncate(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    let budget = w.saturating_sub(1);
    let mut out = String::new();
    let mut cur = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur + cw > budget {
            break;
        }
        out.push(ch);
        cur += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pads_by_display_width_not_chars() {
        // “已保护” 3 个字符 = 6 列
        assert_eq!(width("已保护"), 6);
        assert_eq!(pad_end("已保护", 8), "已保护  ");
        assert_eq!(pad_end("safe", 8), "safe    ");
        // 两种语言补到同一显示宽度
        assert_eq!(width(&pad_end("安全", 8)), 8);
        assert_eq!(width(&pad_end("不可再生", 8)), 8);
    }

    #[test]
    fn truncate_respects_wide_chars() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("ab", 4), "ab");
        // 4 个全角字符 = 8 列，截到 6 列只放得下 2 个 + …
        assert_eq!(truncate("中文标题", 6), "中文…");
    }
}
