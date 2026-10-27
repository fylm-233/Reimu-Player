//! 布局纯函数层（GUI_ALIGNMENT_PLAN §4.1）——零 IO、零状态，可完整单测。
//!
//! 职责：把 `code (3).html` 的 CSS 几何语义（fr 列比 / 居中行 / art 高度 /
//! 物理像素吸附）翻译成 egui 可直接消费的数值。
//! 所有 px 均为设计基准（1100px 宽）下的逻辑像素。

use eframe::egui::{pos2, Rect, Vec2};

/// 两栏分割：按 CSS `grid-template-columns: a b` 的 fr 值分配。
///
/// 返回 `(left_w, right_w)`，**不含**列间距——调用方在两列之间留 `gap`。
///
/// - Player 页传 `(1.02, 1.0)`（`.main-grid`）；
/// - Search / Hot 页传 `(1.55, 1.0)`（`.main-grid.wide`）；
/// - Playlist 页传 `(0.86, 1.14)`（`.pl-layout`）。
pub fn columns(total: f32, left_fr: f32, right_fr: f32) -> (f32, f32) {
    let sum = (left_fr + right_fr).max(f32::EPSILON);
    let l = total * left_fr / sum;
    (l, total - l)
}

/// 页面导航整行矩形（居中）。返回每个按钮的 `Rect`。
///
/// `widths` 为各按钮宽度序列（含边框/内距的完整宽），`gap` 为按钮间距。
/// 真居中：`(总宽 - Σ宽 - Σgap) / 2` 为起始偏移，不依赖任何默认 item_spacing。
pub fn nav_row(total: f32, widths: &[f32], gap: f32) -> Vec<Rect> {
    let n = widths.len();
    if n == 0 {
        return Vec::new();
    }
    let sum_w: f32 = widths.iter().sum();
    let sum_gap = gap * (n as f32 - 1.0);
    let x0 = ((total - sum_w - sum_gap) * 0.5).max(0.0);
    let mut rects = Vec::with_capacity(n);
    let mut x = x0;
    for w in widths {
        rects.push(Rect::from_min_size(pos2(x, 0.0), Vec2::new(*w, 0.0)));
        x += w + gap;
    }
    rects
}

/// art 区高度：`clamp(245, 0.34*avail)`，普通窗口上限 320、全屏上限 360。
pub fn art_height(avail: f32, maximized: bool) -> f32 {
    let cap = if maximized { 360.0 } else { 320.0 };
    (avail * 0.34).clamp(245.0, cap)
}

/// 物理像素吸附：把逻辑宽度吸附到物理像素网格（≥1 物理像素）。
pub fn snap_w(w: f32, ppp: f32) -> f32 {
    let ppp = if ppp <= 0.0 { 1.0 } else { ppp };
    ((w * ppp).round().max(1.0)) / ppp
}

/// 矩形边界吸附（只吸边界值，不吸宽高——共享边界的两块会落到同一像素，无缝）。
pub fn snap_rect(r: Rect, ppp: f32) -> Rect {
    let ppp = if ppp <= 0.0 { 1.0 } else { ppp };
    let s = |v: f32| (v * ppp).round() / ppp;
    Rect::from_min_max(pos2(s(r.min.x), s(r.min.y)), pos2(s(r.max.x), s(r.max.y)))
}

/// 描边吸附：奇数物理像素宽的线吸附到像素中心，边缘不发灰。
///
/// 返回 `(吸附后的矩形, 吸附后的线宽)`。
pub fn snap_stroke(r: Rect, w: f32, ppp: f32) -> (Rect, f32) {
    let ppp = if ppp <= 0.0 { 1.0 } else { ppp };
    let w_px = (w * ppp).round().max(1.0);
    let w = w_px / ppp;
    // 奇数物理像素宽：矩形边界落在半像素处（像素中心对齐线中心）
    let half = if (w_px as i64) % 2 == 1 {
        0.5 / ppp
    } else {
        0.0
    };
    let s = |v: f32| (v * ppp).round() / ppp;
    let r = Rect::from_min_max(
        pos2(s(r.min.x) + half, s(r.min.y) + half),
        pos2(s(r.max.x) - half, s(r.max.y) - half),
    );
    (r, w)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 0.01;

    #[test]
    fn columns_player_ratio_matches_css() {
        // .main-grid{grid-template-columns:1.02fr 1fr}
        let (l, r) = columns(1080.0, 1.02, 1.0);
        assert!((l / 1080.0 - 0.50495).abs() < EPS);
        assert!((r / 1080.0 - 0.49505).abs() < EPS);
        assert!((l + r - 1080.0).abs() < EPS);
    }

    #[test]
    fn columns_playlist_ratio_matches_css() {
        // .pl-layout{grid-template-columns:.86fr 1.14fr}
        let (l, r) = columns(1080.0, 0.86, 1.14);
        assert!((l / 1080.0 - 0.43).abs() < EPS);
        assert!((r / 1080.0 - 0.57).abs() < EPS);
    }

    #[test]
    fn columns_right_starts_after_left_plus_gap() {
        let (l, _) = columns(1000.0, 1.02, 1.0);
        // 布局契约：right.min.x > left.max.x（R1 修复的验收条件）
        let left = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(l, 800.0));
        let right = Rect::from_min_size(
            pos2(left.max.x + 10.0, 0.0),
            Vec2::new(1000.0 - l - 10.0, 800.0),
        );
        assert!(right.min.x > left.max.x);
        assert!(right.max.x <= 1000.0 + 0.5);
    }

    #[test]
    fn nav_row_is_centered() {
        let rects = nav_row(800.0, &[100.0, 100.0, 100.0], 9.0);
        let start = rects[0].min.x;
        let end = rects[2].max.x;
        // 居中：左空白 ≈ 右空白
        assert!((start - (800.0 - end)).abs() < 0.5);
        // 间距精确 9
        assert!((rects[1].min.x - rects[0].max.x - 9.0).abs() < EPS);
    }

    #[test]
    fn nav_row_narrow_total_clamps_to_zero() {
        let rects = nav_row(50.0, &[100.0, 100.0], 9.0);
        assert_eq!(rects[0].min.x, 0.0);
    }

    #[test]
    fn art_height_clamps() {
        assert_eq!(art_height(500.0, false), 245.0); // 0.34*500=170 < 245
        assert_eq!(art_height(800.0, false), 272.0); // 0.34*800
        assert_eq!(art_height(2000.0, false), 320.0); // 普通上限
        assert_eq!(art_height(2000.0, true), 360.0); // 全屏上限
    }

    #[test]
    fn snap_w_rounds_to_physical_grid() {
        assert!((snap_w(1.5, 1.25) - 1.6).abs() < EPS); // 1.875px → 2px / 1.25
        assert!((snap_w(0.2, 1.25) - 0.8).abs() < EPS); // 0.25px → 1px / 1.25
        assert!((snap_w(10.0, 1.0) - 10.0).abs() < EPS);
    }

    #[test]
    fn snap_rect_snaps_edges_not_size() {
        let r = Rect::from_min_max(pos2(0.4, 10.2), pos2(20.6, 30.9));
        let s = snap_rect(r, 1.0);
        assert_eq!(s.min, pos2(0.0, 10.0));
        assert_eq!(s.max, pos2(21.0, 31.0));
    }

    #[test]
    fn snap_stroke_odd_pixels_land_on_centers() {
        // 1.5pt @ 1.25ppp = 1.875px → 2px（偶），无半像素偏移
        let (r, w) = snap_stroke(
            Rect::from_min_max(pos2(0.0, 0.0), pos2(10.0, 10.0)),
            1.5,
            1.25,
        );
        assert!((w - 1.6).abs() < EPS);
        assert_eq!(r.min, pos2(0.0, 0.0));
        // 1pt @ 1.0ppp = 1px（奇）→ 半像素偏移
        let (r, w) = snap_stroke(
            Rect::from_min_max(pos2(0.0, 0.0), pos2(10.0, 10.0)),
            1.0,
            1.0,
        );
        assert!((w - 1.0).abs() < EPS);
        assert!((r.min.x - 0.5).abs() < EPS);
        assert!((r.max.x - 9.5).abs() < EPS);
    }

    #[test]
    fn snap_helpers_tolerate_zero_ppp() {
        assert!((snap_w(5.0, 0.0) - 5.0).abs() < EPS);
        let _ = snap_rect(Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), 0.0);
        let _ = snap_stroke(Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), 1.0, 0.0);
    }
}
