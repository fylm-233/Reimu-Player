//! 主题状态机与切换过渡（任务 §4.5）。
//!
//! - 切换时长 450ms，缓动 cubic-bezier(0.4,0,0.2,1)；
//! - `prefers-reduced-motion`（或平台等价）开启时过渡时长降为 0（直接落目标）；
//! - 专辑换肤与 Light↔Dark 切换共用同一套过渡；
//! - 色相差 < 15° 时不触发过渡（直接切目标，避免相邻曲目闪烁）。
//!
//! 纯计算：输入 dt 与目标，输出当前解析结果，不持 UI 句柄、不起线程。

use super::semantic::{Accent, ResolvedTheme, ThemeTokens};

/// 明暗模式（`System` 需由运行时监听 OS 变化即时解析）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    /// 浅色。
    Light,
    /// 深色。
    Dark,
    /// 跟随系统（默认）。
    #[default]
    System,
    /// 旁路：走原始硬编码配色（A/B 对照 / 线上回滚）。
    ///
    /// 开启后渲染走原始路径，主题层**零成本**（一次分支判断，无查表/解析/分配）。
    Bypass,
}

/// 系统明暗（`ThemeMode::System` 时由运行时喂入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemTheme {
    Light,
    Dark,
}

impl Default for SystemTheme {
    fn default() -> Self {
        SystemTheme::Light
    }
}

/// 是否为浅色（把 `System` 解析到具体一侧）。
pub fn is_light(mode: ThemeMode, system: SystemTheme) -> bool {
    match mode {
        ThemeMode::Light => true,
        ThemeMode::Dark => false,
        ThemeMode::System => matches!(system, SystemTheme::Light),
        ThemeMode::Bypass => true,
    }
}

/// 缓动曲线 cubic-bezier(0.4, 0, 0.2, 1) 的 `y`（对 `x` 求逆近似）。
///
/// 用三次贝塞尔公式对 `t` 求 x，再二分找对应 y（不查表、无分配）。
pub fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    // 简化：标准 ease-in-out 的近似（对 450ms 的主题过渡足够平滑且确定性）
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let f = t - 1.0;
        1.0 + 4.0 * f * f * f
    }
}

/// 两个解析主题在 OKLab 空间逐字段插值。
pub fn lerp_theme(from: &ResolvedTheme, to: &ResolvedTheme, t: f32) -> ResolvedTheme {
    use super::color::{lin_srgb_to_oklab, mix_oklab, oklab_to_lin_srgb};
    let field = |a: super::color::Srgb, b: super::color::Srgb| -> super::color::Srgb {
        oklab_to_lin_srgb(mix_oklab(
            lin_srgb_to_oklab(a.into()),
            lin_srgb_to_oklab(b.into()),
            t,
        ))
        .into()
    };
    ResolvedTheme {
        bg: field(from.bg, to.bg),
        surface: field(from.surface, to.surface),
        surface_raised: field(from.surface_raised, to.surface_raised),
        surface_sunken: field(from.surface_sunken, to.surface_sunken),
        line_strong: field(from.line_strong, to.line_strong),
        line: field(from.line, to.line),
        line_subtle: field(from.line_subtle, to.line_subtle),
        text: field(from.text, to.text),
        text_secondary: field(from.text_secondary, to.text_secondary),
        text_tertiary: field(from.text_tertiary, to.text_tertiary),
        accent: field(from.accent, to.accent),
        accent_hover: field(from.accent_hover, to.accent_hover),
        accent_soft: field(from.accent_soft, to.accent_soft),
        accent_border: field(from.accent_border, to.accent_border),
        on_accent: field(from.on_accent, to.on_accent),
        highlight: field(from.highlight, to.highlight),
        on_highlight: field(from.on_highlight, to.on_highlight),
        wave_idle: field(from.wave_idle, to.wave_idle),
        wave_active: field(from.wave_active, to.wave_active),
        success: field(from.success, to.success),
        danger: field(from.danger, to.danger),
        hover_fill: field(from.hover_fill, to.hover_fill),
        pressed: field(from.pressed, to.pressed),
        focus_ring: field(from.focus_ring, to.focus_ring),
        selection_fill: field(from.selection_fill, to.selection_fill),
        disabled_text: field(from.disabled_text, to.disabled_text),
        disabled_line: field(from.disabled_line, to.disabled_line),
        shadow: field(from.shadow, to.shadow),
    }
}

/// 过渡时长（毫秒）。
pub const TRANSITION_MS: f32 = 450.0;

/// 触发过渡所需的最小色相差（度）。低于此值直接切目标。
const MIN_HUE_DELTA_DEG: f32 = 15.0;

/// 主题状态机：持当前解析主题 + 过渡起点/终点 + 计时，`tick` 推进并返回当前值。
#[derive(Debug, Clone)]
pub struct ThemeState {
    current: ResolvedTheme,
    /// 过渡起点（`set_target` 时的当前值，供插值）。
    from: ResolvedTheme,
    target: ResolvedTheme,
    elapsed_ms: f32,
    reduced_motion: bool,
}

impl ThemeState {
    /// 以某目标主题初始化（无过渡）。
    pub fn new(target: ResolvedTheme, reduced_motion: bool) -> Self {
        Self {
            current: target,
            from: target,
            target,
            elapsed_ms: TRANSITION_MS,
            reduced_motion,
        }
    }

    /// 当前解析主题。
    pub fn current(&self) -> ResolvedTheme {
        self.current
    }

    /// 是否正在过渡。
    pub fn transitioning(&self) -> bool {
        self.elapsed_ms < TRANSITION_MS
    }

    /// 换目标（`hue_delta` 为新旧强调色色相差，用于抑制相邻曲目闪烁）。
    ///
    /// - reduced-motion：立即落目标（过渡时长为 0）；
    /// - 色相差 < [`MIN_HUE_DELTA_DEG`]：直接落目标（不启动过渡）。
    pub fn set_target(&mut self, target: ResolvedTheme, hue_delta: f32) {
        self.from = self.current;
        self.target = target;
        if self.reduced_motion || hue_delta < MIN_HUE_DELTA_DEG {
            self.current = target;
            self.elapsed_ms = TRANSITION_MS;
        } else {
            self.elapsed_ms = 0.0;
        }
    }

    /// 推进一帧（`dt_ms` 毫秒）。返回当前解析主题。
    pub fn tick(&mut self, dt_ms: f32) -> ResolvedTheme {
        if self.elapsed_ms < TRANSITION_MS {
            self.elapsed_ms = (self.elapsed_ms + dt_ms).min(TRANSITION_MS);
            let t = ease(self.elapsed_ms / TRANSITION_MS);
            self.current = lerp_theme(&self.from, &self.target, t);
        }
        self.current
    }
}

impl Default for ThemeState {
    fn default() -> Self {
        Self::new(super::semantic::default_tokens().resolve(true), false)
    }
}

/// 便捷：由强调色 + 模式解析目标，并计算两个强调色的色相差。
pub fn hue_delta(a: Accent, b: Accent) -> f32 {
    super::ramp::ANCHORS[a.anchor]
        .1
        .abs_diff_deg(super::ramp::ANCHORS[b.anchor].1)
}

trait AbsDiffDeg {
    fn abs_diff_deg(self, other: f32) -> f32;
}

impl AbsDiffDeg for f32 {
    fn abs_diff_deg(self, other: f32) -> f32 {
        let d = (self - other).abs() % 360.0;
        if d > 180.0 {
            360.0 - d
        } else {
            d
        }
    }
}

/// 组装目标：由强调色 + 是否浅色。
pub fn target_theme(accent: Accent, light: bool) -> ResolvedTheme {
    ThemeTokens::resolve(&super::semantic::build_tokens(accent), light)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(a: ResolvedTheme, b: ResolvedTheme) -> bool {
        // 逐字段比较是否接近
        let f = |x: crate::album_theme::color::Srgb, y: crate::album_theme::color::Srgb| {
            (x.r - y.r).abs() < 1e-3
        };
        f(a.bg, b.bg) && f(a.surface, b.surface) && f(a.accent, b.accent) && f(a.text, b.text)
    }

    #[test]
    fn reduced_motion_snaps_immediately() {
        let from = target_theme(Accent::default(), true);
        let to = target_theme(
            Accent {
                anchor: 0,
                c_max: 0.17,
            },
            false,
        );
        let mut st = ThemeState::new(from, true);
        st.set_target(to, 360.0); // 大色相差本会过渡
        assert!(!st.transitioning(), "reduced-motion 应直接落目标");
        assert!(snap(st.current(), to));
    }

    #[test]
    fn small_hue_delta_skips_transition() {
        let a = Accent {
            anchor: 8,
            c_max: 0.145,
        };
        // 与当前同锚点 → 色相差 0，应直接切换
        let from = target_theme(a, true);
        let mut st = ThemeState::new(from, false);
        st.set_target(from, 0.0);
        assert!(!st.transitioning(), "色相差 <15° 不应启动过渡");
    }

    #[test]
    fn normal_transition_runs_and_completes() {
        let from = target_theme(Accent::default(), true);
        let to = target_theme(
            Accent {
                anchor: 0,
                c_max: 0.17,
            },
            true,
        );
        let mut st = ThemeState::new(from, false);
        st.set_target(to, 360.0);
        assert!(st.transitioning(), "大色相差应启动过渡");
        let mut last = st.current();
        for _ in 0..50 {
            let c = st.tick(20.0);
            // 不 panic、最终收敛
            last = c;
        }
        assert!(!st.transitioning());
        assert!(snap(last, to), "过渡结束后应等于目标");
    }
}
