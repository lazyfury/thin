//! 硬盘占用矩形图（treemap）。
//!
//! 采用二分 treemap（binary treemap）：按体积把条目递归二分，沿较长边切分。
//! 比 slice-and-dice 更方正，实现也足够简单稳健。

use ratatui::layout::Rect;

/// 计算矩形划分，返回 (矩形, 原索引)，按体积从大到小填充
pub fn layout(rect: Rect, values: &[u64]) -> Vec<(Rect, usize)> {
    let mut items: Vec<(u64, usize)> = values
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, v)| (v, i))
        .collect();
    items.sort_by_key(|x| std::cmp::Reverse(x.0));
    let mut out = Vec::new();
    split(rect, &items, &mut out);
    out
}

fn split(rect: Rect, items: &[(u64, usize)], out: &mut Vec<(Rect, usize)>) {
    if items.is_empty() || rect.width == 0 || rect.height == 0 {
        return;
    }
    if items.len() == 1 {
        out.push((rect, items[0].1));
        return;
    }
    let total: u64 = items.iter().map(|(v, _)| *v).sum();
    if total == 0 {
        out.push((rect, items[0].1));
        return;
    }

    let can_h = rect.width >= 2;
    let can_v = rect.height >= 2;
    if !can_h && !can_v {
        out.push((rect, items[0].1));
        return;
    }

    // 找接近一半的分割点
    let mut acc = 0u64;
    let mut split_at = 0usize;
    for (i, (v, _)) in items.iter().enumerate() {
        if i > 0 && acc + v > total / 2 {
            break;
        }
        acc += v;
        split_at = i + 1;
    }
    split_at = split_at.clamp(1, items.len() - 1);

    let (a, b) = items.split_at(split_at);
    let a_total: u64 = a.iter().map(|(v, _)| *v).sum();
    let ratio = a_total as f64 / total as f64;

    let horizontal = can_h && (rect.width >= rect.height || !can_v);
    if horizontal {
        let w = ((rect.width as f64 * ratio).round() as u16).clamp(1, rect.width - 1);
        let left = Rect { width: w, ..rect };
        let right = Rect {
            x: rect.x + w,
            width: rect.width - w,
            ..rect
        };
        split(left, a, out);
        split(right, b, out);
    } else {
        let h = ((rect.height as f64 * ratio).round() as u16).clamp(1, rect.height - 1);
        let top = Rect { height: h, ..rect };
        let bottom = Rect {
            y: rect.y + h,
            height: rect.height - h,
            ..rect
        };
        split(top, a, out);
        split(bottom, b, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_covers_area_without_overlap_too_much() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        let cells = layout(rect, &[100, 50, 25, 12, 3]);
        assert_eq!(cells.len(), 5);
        // 面积之和不超过容器（允许因取整略少）
        let total: u32 = cells
            .iter()
            .map(|(r, _)| r.width as u32 * r.height as u32)
            .sum();
        assert!(total <= 40 * 20);
        assert!(total > 40 * 20 / 2);
    }
}
