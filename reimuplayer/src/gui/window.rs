//! 自绘窗口外观与窗口管理（需求 1：替代系统默认边框）。
//!
//! 窗口以 `with_decorations(false)` 启动，边框/标题栏由 [`super::app`]
//! 绘制（2px ink 边框 + 8px 硬投影 + 条纹标题栏，视觉参考 `code (3).html`
//! `.window-title`）。本模块负责**交互**一侧：
//!
//! - **标题栏**：拖动（[`egui::ViewportCommand::StartDrag`]）、双击最大化/还原、
//!   System 6 closebox 风格三钮（关闭 / 最小化 / 最大化）；
//! - **边缘缩放**：窗口四周 6px 热区，8 向 [`egui::ViewportCommand::BeginResize`]；
//! - **关闭动画**：[`CloseAnim`] 桌面色淡出（≈160ms）后才发真正关闭命令，
//!   避免窗口"啪"地消失；最小化/最大化走系统合成器动画。
//!
//! 所有函数只发 [`egui::ViewportCommand`]，不直接碰 winit，
//! 保持跨平台（Windows / macOS / Linux）。
//!
//! 命中约定：egui 里同一 layer 上后注册的 widget 优先拿到交互，
//! 因此本模块**先注册标题栏拖动区、后注册三钮**（与 egui demo
//! `custom_window_frame` 的顺序一致），按钮区另用几何判断从拖动/双击里排除，
//! 两种命中模型下都成立。

use eframe::egui::{self, CursorIcon, Pos2, Rect, Sense, Ui, Vec2};

use super::theme::{fg_on, pal};

/// 标题栏高度（`code (3).html` .window-title ≈28px）。
pub const TITLEBAR_HEIGHT: f32 = 28.0;

/// 边缘缩放热区宽度。
pub const RESIZE_BORDER: f32 = 6.0;

/// 关闭淡出动画时长（秒）。
pub const CLOSE_SECS: f32 = 0.16;

/// closebox 风格小方块边长（13px，内白高光 2px）。
pub const BUTTON_BOX: f32 = 13.0;

/// closebox 内白高光环宽。
const BUTTON_RING: f32 = 2.0;

/// 三钮距标题栏左缘的距离。
const BUTTON_MARGIN_X: f32 = 8.0;

/// 三钮间距。
const BUTTON_GAP: f32 = 4.0;

/// 标题栏右端版本号文本预留宽度（"v 2.001" @ 9px + 右边距）。
const VERSION_RESERVE: f32 = 46.0;

/// 标题栏文字的**右边界**（版本号左侧）。
///
/// 阶段 11 之前这里还有一块 80px 的"会话按钮"（登录 chip）。登录入口移进
/// 设置面板后，标题栏右侧只剩版本号——居中公式的右界因此收回到版本号左侧。
pub fn title_text_right(bar: Rect) -> f32 {
    bar.max.x - VERSION_RESERVE
}

/// 版本号文本锚点（标题栏右端）。
pub fn version_anchor(bar: Rect) -> Pos2 {
    Pos2::new(bar.max.x - 8.0, bar.center().y)
}

// ---------------------------------------------------------------------------
// 窗口动作
// ---------------------------------------------------------------------------

/// 标题栏交互产生的窗口管理动作（App 侧转成 [`egui::ViewportCommand`]）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowAction {
    /// 关闭（先播 [`CloseAnim`] 淡出）。
    Close,
    /// 最小化（系统动画）。
    Minimize,
    /// 最大化 ⇄ 还原（系统动画）。
    ToggleMaximize,
}

// ---------------------------------------------------------------------------
// 关闭淡出动画
// ---------------------------------------------------------------------------

/// 关闭淡出动画状态（0..=1，纯逻辑可单测）。
#[derive(Clone, Copy, Debug, Default)]
pub struct CloseAnim(Option<f32>);

impl CloseAnim {
    /// 启动淡出（幂等：已在动画中则忽略）。
    pub fn start(&mut self) {
        if self.0.is_none() {
            self.0 = Some(0.0);
        }
    }

    /// 淡出是否已启动（`start()` 后保持为真，遮罩一直画到窗口真正关闭）。
    pub fn active(&self) -> bool {
        self.0.is_some()
    }

    /// 推进一帧；动画完成的**那一帧**返回 `true`，之后不再重复回报，
    /// 调用方据此发一次 [`egui::ViewportCommand::Close`]。
    ///
    /// 累计进度钳在 0..=1；`dt <= 0` 视为不推进。
    pub fn tick(&mut self, dt: f32) -> bool {
        match self.0.as_mut() {
            None => false,
            Some(t) if *t >= 1.0 => false, // 已完成：只在完成那一帧报一次
            Some(t) => {
                *t = (*t + dt.max(0.0) / CLOSE_SECS).min(1.0);
                *t >= 1.0
            }
        }
    }

    /// 当前淡出进度（0=不透明窗口，1=全桌面色遮罩）。
    pub fn alpha(&self) -> f32 {
        self.0.unwrap_or(0.0).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// 标题栏三钮（System 6 closebox 风格）
// ---------------------------------------------------------------------------

/// 三钮布局：closebox 紧贴左侧（视觉参考只有一钮，产品需最小化/最大化，
/// 同风格方块顺排右侧）。
#[derive(Clone, Copy, Debug)]
pub struct TitleButtons {
    pub close: Rect,
    pub minimize: Rect,
    pub maximize: Rect,
}

impl TitleButtons {
    /// 全部按钮矩形（供标题栏拖动排除命中）。
    pub fn all(&self) -> [Rect; 3] {
        [self.close, self.minimize, self.maximize]
    }

    /// 指针是否落在任一按钮上（命中按钮时拖动/双击必须让位）。
    pub fn contains(&self, pos: Option<Pos2>) -> bool {
        pos.is_some_and(|p| self.all().iter().any(|r| r.contains(p)))
    }
}

/// 标题栏内三钮布局（左侧起：close → minimize → maximize，间距 4px，
/// 垂直居中，与 HTML closebox 同位）。
pub fn layout_title_buttons(bar: Rect) -> TitleButtons {
    let y = bar.center().y - BUTTON_BOX / 2.0;
    let x0 = bar.min.x + BUTTON_MARGIN_X;
    let rect_at = |i: usize| {
        Rect::from_min_size(
            Pos2::new(x0 + i as f32 * (BUTTON_BOX + BUTTON_GAP), y),
            Vec2::splat(BUTTON_BOX),
        )
    };
    TitleButtons {
        close: rect_at(0),
        minimize: rect_at(1),
        maximize: rect_at(2),
    }
}

/// 绘制单个 closebox 风格按钮（13px 方块 + 内衬环 + 符号）。
///
/// 内衬与符号用 `label_light / fg_on(label_light)` 成对取色：浅色主题里是
/// 「白面黑叉」，深色主题里自动反转为「深面白叉」，叉号不会消失在白面上。
///
/// `symbol`：`"close"` / `"min"` / `"max"` / `"restore"`。
pub fn draw_title_button(painter: &egui::Painter, rect: Rect, symbol: &str, hovered: bool) {
    painter.rect(
        rect,
        egui::CornerRadius::ZERO,
        if hovered {
            pal().shelf_hover
        } else {
            pal().paper
        },
        egui::Stroke::new(1.0, pal().line),
        egui::StrokeKind::Inside,
    );
    let inner = Rect::from_min_size(
        rect.min + Vec2::splat(BUTTON_RING),
        Vec2::splat(BUTTON_BOX - 2.0 * BUTTON_RING),
    );
    painter.rect_filled(inner, egui::CornerRadius::ZERO, pal().label_light);

    let c = inner.center();
    let symbol_color = fg_on(pal().label_light);
    let ink = |w: f32| egui::Stroke::new(w, symbol_color);
    match symbol {
        "close" => {
            // 8px 对角线叉（自绘，避免字形度量抖动）
            let d = 3.0;
            for [a, b] in [
                [Pos2::new(c.x - d, c.y - d), Pos2::new(c.x + d, c.y + d)],
                [Pos2::new(c.x + d, c.y - d), Pos2::new(c.x - d, c.y + d)],
            ] {
                painter.line_segment([a, b], ink(1.2));
            }
        }
        "min" => {
            // 底部一条横线
            painter.line_segment(
                [
                    Pos2::new(c.x - 3.0, c.y + 2.0),
                    Pos2::new(c.x + 3.0, c.y + 2.0),
                ],
                ink(1.6),
            );
        }
        "max" => {
            // 6px 方框
            painter.rect_stroke(
                Rect::from_center_size(c, Vec2::splat(6.0)),
                egui::CornerRadius::ZERO,
                ink(1.2),
                egui::StrokeKind::Inside,
            );
        }
        "restore" => {
            // 两个错位小方框（后景 + 前景）：**先填后描**，再画前景，
            // 让前景底色压住后景重叠的那条边，形成"叠放"关系。
            for r in [
                Rect::from_center_size(c + Vec2::splat(1.5), Vec2::splat(5.0)),
                Rect::from_center_size(c - Vec2::splat(1.5), Vec2::splat(5.0)),
            ] {
                painter.rect_filled(r, egui::CornerRadius::ZERO, pal().label_light);
                painter.rect_stroke(
                    r,
                    egui::CornerRadius::ZERO,
                    ink(1.0),
                    egui::StrokeKind::Inside,
                );
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 标题栏交互
// ---------------------------------------------------------------------------

/// 标题栏交互：三钮点击 + 空白区拖动（[`egui::ViewportCommand::StartDrag`]）
/// + 双击最大化/还原。返回待执行动作；拖动命令即时发出。
///
/// `exclude` 是标题栏内的**自定义控件**矩形（如右侧会话按钮）：命中这些区域时
/// 不得触发窗口拖动/双击，否则点"登录"会顺手把窗口拖走。
///
/// 按钮命中优先：指针在按钮上时不触发窗口拖动/双击。
pub fn titlebar_interact(
    ui: &mut Ui,
    bar: Rect,
    buttons: &TitleButtons,
    maximized: bool,
    exclude: &[Rect],
) -> Vec<WindowAction> {
    let mut actions = Vec::new();

    // 1) 标题栏空白区：拖动 / 双击。
    //    先注册拖动区（三钮在下方注册，命中优先）；再用几何判断把按钮区排除，
    //    兜底防止"点按钮却开始拖窗口/切最大化"。
    let over_button = buttons.contains(ui.input(|input| input.pointer.hover_pos()))
        || ui
            .input(|input| input.pointer.hover_pos())
            .is_some_and(|p| exclude.iter().any(|r| r.contains(p)));
    let drag = ui.interact(
        bar,
        ui.scope_id().with("titlebar-drag"),
        Sense::click_and_drag(),
    );
    if !over_button {
        if drag.double_clicked() {
            actions.push(WindowAction::ToggleMaximize);
        } else if drag.is_pointer_button_down_on()
            && ui.input(|input| input.pointer.primary_pressed())
        {
            // 只在主键按下的那一帧发一次，避免拖动过程中每帧重复 StartDrag
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
    }

    // 2) 三钮：后注册，交互优先于标题栏拖动区。
    let painter = ui.painter().clone();
    for (i, (rect, symbol, action)) in [
        (buttons.close, "close", WindowAction::Close),
        (buttons.minimize, "min", WindowAction::Minimize),
        (
            buttons.maximize,
            if maximized { "restore" } else { "max" },
            WindowAction::ToggleMaximize,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let id = ui.scope_id().with(("title-btn", i));
        let resp = ui.interact(rect, id, Sense::click());
        draw_title_button(&painter, rect, symbol, resp.hovered());
        if resp.clicked() {
            actions.push(action);
        }
    }

    actions
}

// ---------------------------------------------------------------------------
// 边缘缩放
// ---------------------------------------------------------------------------

/// 8 向缩放热区（纯逻辑，单测覆盖）：外缘 6px 条带 + 四角 12px 方块。
///
/// 窗口比 `2 * RESIZE_BORDER * 2` 还小时中间段长度钳到 0，
/// 不产生负尺寸矩形（egui 中负矩形 `is_positive() == false`，热区会静默失效）。
pub fn edge_zones(outer: Rect) -> [(Rect, egui::ResizeDirection); 8] {
    let b = RESIZE_BORDER;
    let corner = b * 2.0;
    let (min, max) = (outer.min, outer.max);
    let mid_w = (outer.width() - 2.0 * corner).max(0.0);
    let mid_h = (outer.height() - 2.0 * corner).max(0.0);
    let rect =
        |x: f32, y: f32, w: f32, h: f32| Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h));
    [
        (
            rect(min.x, min.y, corner, corner),
            egui::ResizeDirection::NorthWest,
        ),
        (
            rect(max.x - corner, min.y, corner, corner),
            egui::ResizeDirection::NorthEast,
        ),
        (
            rect(min.x, max.y - corner, corner, corner),
            egui::ResizeDirection::SouthWest,
        ),
        (
            rect(max.x - corner, max.y - corner, corner, corner),
            egui::ResizeDirection::SouthEast,
        ),
        (
            rect(min.x + corner, min.y, mid_w, b),
            egui::ResizeDirection::North,
        ),
        (
            rect(min.x + corner, max.y - b, mid_w, b),
            egui::ResizeDirection::South,
        ),
        (
            rect(min.x, min.y + corner, b, mid_h),
            egui::ResizeDirection::West,
        ),
        (
            rect(max.x - b, min.y + corner, b, mid_h),
            egui::ResizeDirection::East,
        ),
    ]
}

/// 各方向的悬停光标（手柄反馈）。
fn cursor_for(dir: egui::ResizeDirection) -> CursorIcon {
    use egui::ResizeDirection as D;
    match dir {
        D::North | D::South => CursorIcon::ResizeVertical,
        D::East | D::West => CursorIcon::ResizeHorizontal,
        D::NorthEast | D::SouthWest => CursorIcon::ResizeNeSw,
        D::NorthWest | D::SouthEast => CursorIcon::ResizeNwSe,
    }
}

/// 注册边缘缩放热区：悬停改光标，主键按下即 [`egui::ViewportCommand::BeginResize`]。
///
/// `outer` 为窗口内容全幅（viewport rect）；最大化时由调用方跳过。
pub fn edge_resize(ui: &mut Ui, outer: Rect) {
    for (i, (rect, dir)) in edge_zones(outer).into_iter().enumerate() {
        let id = ui.scope_id().with(("win-edge", i));
        let resp = ui.interact(rect, id, Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(cursor_for(dir));
        }
        if resp.is_pointer_button_down_on() && ui.input(|input| input.pointer.primary_pressed()) {
            // 按下即交回系统开始缩放；每帧只发一次，避免重复打断合成器手势
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
        }
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_anim_ticks_once_to_completion() {
        let mut anim = CloseAnim::default();
        assert!(!anim.active());
        assert!(!anim.tick(0.016)); // 未启动不推进
        anim.start();
        anim.start(); // 幂等
        assert!(anim.active());

        assert!(!anim.tick(CLOSE_SECS / 2.0));
        assert!(anim.alpha() > 0.0 && anim.alpha() < 1.0);

        assert!(anim.tick(CLOSE_SECS)); // 完成的那一帧报一次
        assert_eq!(anim.alpha(), 1.0);
        assert!(!anim.tick(CLOSE_SECS)); // 之后不再重复报完成
        assert!(!anim.tick(CLOSE_SECS));
    }

    #[test]
    fn close_anim_ignores_bad_dt() {
        let mut anim = CloseAnim::default();
        anim.start();
        assert!(!anim.tick(-1.0)); // 负 dt 不推进
        assert_eq!(anim.alpha(), 0.0);
        assert!(anim.tick(10.0)); // 超大 dt 一步到位并钳到 1.0
        assert_eq!(anim.alpha(), 1.0);
    }

    #[test]
    fn title_buttons_layout_is_left_aligned_stack() {
        let bar = Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(400.0, 28.0));
        let b = layout_title_buttons(bar);
        assert_eq!(b.close.min.x, 108.0); // bar.min.x + BUTTON_MARGIN_X
        assert_eq!(b.close.size(), Vec2::splat(BUTTON_BOX));
        // 顺排不重叠，间距为 BUTTON_GAP
        assert_eq!(b.minimize.min.x - b.close.max.x, BUTTON_GAP);
        assert_eq!(b.maximize.min.x - b.minimize.max.x, BUTTON_GAP);
        // 垂直居中
        assert_eq!(b.close.center().y, bar.center().y);
        assert!(b.contains(Some(b.close.center())));
        assert!(b.contains(Some(b.maximize.center())));
        assert!(!b.contains(Some(bar.max - Vec2::splat(1.0))));
        assert!(!b.contains(None));
    }

    #[test]
    fn edge_zones_have_one_zone_per_direction() {
        let outer = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(800.0, 600.0));
        let zones = edge_zones(outer);
        // 用 Debug 文本判等，避免依赖 ResizeDirection 的 trait 实现
        for name in [
            "North",
            "NorthEast",
            "East",
            "SouthEast",
            "South",
            "SouthWest",
            "West",
            "NorthWest",
        ] {
            let n = zones
                .iter()
                .filter(|(_, d)| format!("{d:?}") == name)
                .count();
            assert_eq!(n, 1, "{name} 应恰好一个热区，实际 {n} 个");
        }
        for (r, d) in zones {
            assert!(
                r.width() >= 0.0 && r.height() >= 0.0,
                "{d:?} 出现负尺寸 {r:?}"
            );
            assert!(outer.contains_rect(r), "{d:?} 热区越界 {r:?}");
        }
    }

    #[test]
    fn edge_zones_cover_the_whole_resize_border() {
        let outer = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let zones = edge_zones(outer);
        let covered = |p: Pos2| zones.iter().any(|(r, _)| r.contains(p));
        for i in 0..800 {
            let x = i as f32 + 0.5;
            assert!(covered(Pos2::new(x, 0.5)), "顶边 x={x} 未覆盖");
            assert!(covered(Pos2::new(x, 599.5)), "底边 x={x} 未覆盖");
        }
        for i in 0..600 {
            let y = i as f32 + 0.5;
            assert!(covered(Pos2::new(0.5, y)), "左边 y={y} 未覆盖");
            assert!(covered(Pos2::new(799.5, y)), "右边 y={y} 未覆盖");
        }
    }

    #[test]
    fn edge_zones_stay_non_negative_on_tiny_window() {
        let outer = Rect::from_min_size(Pos2::ZERO, Vec2::new(4.0, 4.0));
        for (r, d) in edge_zones(outer) {
            assert!(
                r.width() >= 0.0 && r.height() >= 0.0,
                "{d:?} 出现负尺寸 {r:?}"
            );
        }
    }
}
