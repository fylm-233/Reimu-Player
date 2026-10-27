//! 语义层：把中性/强调色板映射为**每个字段同时具备 Light 与 Dark** 的
//! `ThemeTokens`（任务 §3.5–3.6）。
//!
//! 组件层只准引用这里的语义 token，不得自取色板或写十六进制。所有派生色
//! 都经 [`super::color::gamut_clip`] 裁剪，保证落到 sRGB 色域。

use super::color::{contrast_ratio, gamut_clip, mix_colors, oklch_to_srgb_raw, Srgb};
use super::ramp::{self, neutral, ANCHORS};

/// 强调色锚点（由取色器给出：锚点编号 + 收敛后的彩度上限）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Accent {
    /// 12 锚点之一的下标（0..11）。
    pub anchor: usize,
    /// 收敛后的彩度上限（已 clamp 到 `[0.10, Cmax(anchor)]`）。
    pub c_max: f32,
}

impl Default for Accent {
    fn default() -> Self {
        Accent {
            anchor: ramp::DEFAULT_ANCHOR,
            c_max: ANCHORS[ramp::DEFAULT_ANCHOR].2,
        }
    }
}

impl Accent {
    /// 强调色的 `accent-*` 派生色（层级 600 / 400 / …）。
    pub fn level(self, lvl: usize) -> super::color::Oklch {
        let (_, h, _) = ANCHORS[self.anchor];
        let (l, coeff) = ramp::RAMP[lvl];
        super::color::Oklch::new(l, self.c_max * coeff, h)
    }
}

/// 一对比（Light/Dark 各一值）——缺一即编译失败。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DualTone {
    pub light: Srgb,
    pub dark: Srgb,
}

impl DualTone {
    /// 由 OKLCH 对构造（内部做色域裁剪）。
    pub fn from_oklch(light: super::color::Oklch, dark: super::color::Oklch) -> Self {
        DualTone {
            light: oklch_to_srgb_raw(gamut_clip(light)),
            dark: oklch_to_srgb_raw(gamut_clip(dark)),
        }
    }

    /// 由**同一**显示色构造（如品牌柠檬黄 Light/Dark 相近但非完全相同）。
    pub fn same(c: Srgb) -> Self {
        DualTone { light: c, dark: c }
    }

    /// 按模式取值。
    pub fn resolve(self, light: bool) -> Srgb {
        if light {
            self.light
        } else {
            self.dark
        }
    }
}

/// 语义 token（组件层唯一合法颜色来源）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeTokens {
    pub bg: DualTone,
    pub surface: DualTone,
    pub surface_raised: DualTone,
    pub surface_sunken: DualTone,
    pub line_strong: DualTone,
    pub line: DualTone,
    pub line_subtle: DualTone,
    pub text: DualTone,
    pub text_secondary: DualTone,
    pub text_tertiary: DualTone,
    pub accent: DualTone,
    pub accent_hover: DualTone,
    pub accent_soft: DualTone,
    pub accent_border: DualTone,
    pub on_accent: DualTone,
    pub highlight: DualTone,
    pub on_highlight: DualTone,
    pub wave_idle: DualTone,
    pub wave_active: DualTone,
    pub success: DualTone,
    pub danger: DualTone,
    pub hover_fill: DualTone,
    pub pressed: DualTone,
    pub focus_ring: DualTone,
    pub selection_fill: DualTone,
    pub disabled_text: DualTone,
    pub disabled_line: DualTone,
    pub shadow: DualTone,
}

/// 展开到具体某一模式的纯色集（一次解析，避免每处重复 `resolve`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedTheme {
    pub bg: Srgb,
    pub surface: Srgb,
    pub surface_raised: Srgb,
    pub surface_sunken: Srgb,
    pub line_strong: Srgb,
    pub line: Srgb,
    pub line_subtle: Srgb,
    pub text: Srgb,
    pub text_secondary: Srgb,
    pub text_tertiary: Srgb,
    pub accent: Srgb,
    pub accent_hover: Srgb,
    pub accent_soft: Srgb,
    pub accent_border: Srgb,
    pub on_accent: Srgb,
    pub highlight: Srgb,
    pub on_highlight: Srgb,
    pub wave_idle: Srgb,
    pub wave_active: Srgb,
    pub success: Srgb,
    pub danger: Srgb,
    pub hover_fill: Srgb,
    pub pressed: Srgb,
    pub focus_ring: Srgb,
    pub selection_fill: Srgb,
    pub disabled_text: Srgb,
    pub disabled_line: Srgb,
    pub shadow: Srgb,
}

/// `on_accent`：按 WCAG 对比度在 `n-0`（近白）与 `n-1000`（最深暖黑）之间
/// **选更高对比**者——这是对规范「L>0.72→n900 否则 n0」的修正：按简单阈值
/// 选，12 锚点中有多个在浅色下不足正文 4.5:1，改为对比度优先可全数达标。
fn on_accent_of(accent: Srgb) -> Srgb {
    let white = oklch_to_srgb_raw(gamut_clip(neutral(ramp::n::N0)));
    let black = oklch_to_srgb_raw(gamut_clip(neutral(ramp::n::N1000)));
    if contrast_ratio(accent, white) >= contrast_ratio(accent, black) {
        white
    } else {
        black
    }
}

/// 由强调色锚点构建整套语义 token。
pub fn build_tokens(accent: Accent) -> ThemeTokens {
    use ramp::level as L;
    use ramp::n as N;

    let d = |lvl_light: usize, lvl_dark: usize| {
        DualTone::from_oklch(accent.level(lvl_light), accent.level(lvl_dark))
    };
    let nd = |lvl_light: usize, lvl_dark: usize| {
        DualTone::from_oklch(neutral(lvl_light), neutral(lvl_dark))
    };

    // 品牌柠檬黄：Light (0.86,0.16,125) / Dark (0.88,0.16,125)
    let highlight = DualTone::from_oklch(
        super::color::Oklch::new(0.86, 0.16, 125.0),
        super::color::Oklch::new(0.88, 0.16, 125.0),
    );
    let on_highlight = DualTone::from_oklch(neutral(N::N900), neutral(N::N900));
    let success = DualTone::from_oklch(
        super::color::Oklch::new(0.55, 0.13, 155.0),
        super::color::Oklch::new(0.72, 0.13, 155.0),
    );
    let danger = DualTone::from_oklch(
        super::color::Oklch::new(0.55, 0.19, 25.0),
        super::color::Oklch::new(0.70, 0.16, 25.0),
    );

    let accent_tone = d(L::L600, L::L400);
    let surface_light = oklch_to_srgb_raw(gamut_clip(neutral(N::N25)));
    let surface_dark = oklch_to_srgb_raw(gamut_clip(neutral(N::N900)));

    ThemeTokens {
        bg: nd(N::N50, N::N950),
        surface: DualTone {
            light: surface_light,
            dark: surface_dark,
        },
        surface_raised: nd(N::N0, N::N800),
        surface_sunken: nd(N::N100, N::N1000),
        line_strong: nd(N::N900, N::N200),
        // 规范原值 n300/n700 对 surface 只有 ~1.73:1（不达标），改用 n500 双模式
        line: nd(N::N500, N::N500),
        line_subtle: nd(N::N200, N::N800),
        text: nd(N::N900, N::N25),
        text_secondary: nd(N::N600, N::N300),
        text_tertiary: nd(N::N400, N::N500),
        accent: accent_tone,
        accent_hover: d(L::L700, L::L300),
        accent_soft: d(L::L100, L::L900),
        accent_border: d(L::L300, L::L800),
        on_accent: DualTone {
            light: on_accent_of(accent_tone.light),
            dark: on_accent_of(accent_tone.dark),
        },
        highlight,
        on_highlight,
        wave_idle: nd(N::N300, N::N700),
        wave_active: d(L::L600, L::L400),
        success,
        danger,
        // 交互态：hover = mix(accent, surface, 8%/14%)；pressed = accent700/500
        hover_fill: DualTone {
            light: mix_colors(accent_tone.light, surface_light, 0.08),
            dark: mix_colors(accent_tone.dark, surface_dark, 0.14),
        },
        pressed: d(L::L700, L::L500),
        focus_ring: d(L::L600, L::L400),
        // 选中文字/底 = accent 以 28%/38% 透明度叠在 surface 上
        selection_fill: DualTone {
            light: mix_colors(surface_light, accent_tone.light, 0.28),
            dark: mix_colors(surface_dark, accent_tone.dark, 0.38),
        },
        disabled_text: nd(N::N400, N::N500),
        disabled_line: nd(N::N200, N::N800),
        // 硬阴影 = line_strong（0 2px 0）
        shadow: nd(N::N900, N::N200),
    }
}

impl ThemeTokens {
    /// 展开到某一模式。
    pub fn resolve(&self, light: bool) -> ResolvedTheme {
        let r = |t: &DualTone| t.resolve(light);
        ResolvedTheme {
            bg: r(&self.bg),
            surface: r(&self.surface),
            surface_raised: r(&self.surface_raised),
            surface_sunken: r(&self.surface_sunken),
            line_strong: r(&self.line_strong),
            line: r(&self.line),
            line_subtle: r(&self.line_subtle),
            text: r(&self.text),
            text_secondary: r(&self.text_secondary),
            text_tertiary: r(&self.text_tertiary),
            accent: r(&self.accent),
            accent_hover: r(&self.accent_hover),
            accent_soft: r(&self.accent_soft),
            accent_border: r(&self.accent_border),
            on_accent: r(&self.on_accent),
            highlight: r(&self.highlight),
            on_highlight: r(&self.on_highlight),
            wave_idle: r(&self.wave_idle),
            wave_active: r(&self.wave_active),
            success: r(&self.success),
            danger: r(&self.danger),
            hover_fill: r(&self.hover_fill),
            pressed: r(&self.pressed),
            focus_ring: r(&self.focus_ring),
            selection_fill: r(&self.selection_fill),
            disabled_text: r(&self.disabled_text),
            disabled_line: r(&self.disabled_line),
            shadow: r(&self.shadow),
        }
    }
}

/// 默认令牌（靛蓝锚点 + 默认彩度上限）。
pub fn default_tokens() -> ThemeTokens {
    build_tokens(Accent::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn every_field_has_both_light_and_dark() {
        // 所有 token 都是 DualTone：缺一即编译失败（这里只做运行时抽查）
        let t = default_tokens();
        let fields: [(&str, DualTone); 28] = [
            ("bg", t.bg),
            ("surface", t.surface),
            ("surface_raised", t.surface_raised),
            ("surface_sunken", t.surface_sunken),
            ("line_strong", t.line_strong),
            ("line", t.line),
            ("line_subtle", t.line_subtle),
            ("text", t.text),
            ("text_secondary", t.text_secondary),
            ("text_tertiary", t.text_tertiary),
            ("accent", t.accent),
            ("accent_hover", t.accent_hover),
            ("accent_soft", t.accent_soft),
            ("accent_border", t.accent_border),
            ("on_accent", t.on_accent),
            ("highlight", t.highlight),
            ("on_highlight", t.on_highlight),
            ("wave_idle", t.wave_idle),
            ("wave_active", t.wave_active),
            ("success", t.success),
            ("danger", t.danger),
            ("hover_fill", t.hover_fill),
            ("pressed", t.pressed),
            ("focus_ring", t.focus_ring),
            ("selection_fill", t.selection_fill),
            ("disabled_text", t.disabled_text),
            ("disabled_line", t.disabled_line),
            ("shadow", t.shadow),
        ];
        for (name, tone) in fields {
            let _ = name;
            assert!(tone.light.r >= 0.0 && tone.light.r <= 1.0);
            assert!(tone.dark.r >= 0.0 && tone.dark.r <= 1.0);
        }
    }

    #[test]
    fn contrast_thresholds_hold_for_all_twelve_anchors() {
        // 验收 §6：text/surface ≥4.5、text_secondary/surface ≥4.5、
        // on_accent/accent ≥4.5、on_highlight/highlight ≥3、line/surface ≥3
        for anchor in 0..ANCHORS.len() {
            let t = build_tokens(Accent {
                anchor,
                c_max: ANCHORS[anchor].2,
            });
            for light in [true, false] {
                let r = t.resolve(light);
                assert!(
                    contrast_ratio(r.text, r.surface) >= 4.5,
                    "锚点 {anchor} light={light}: text/surface 不足 4.5"
                );
                assert!(
                    contrast_ratio(r.text_secondary, r.surface) >= 4.5,
                    "锚点 {anchor} light={light}: text_secondary/surface 不足 4.5"
                );
                assert!(
                    contrast_ratio(r.on_accent, r.accent) >= 4.5,
                    "锚点 {anchor} light={light}: on_accent/accent 不足 4.5"
                );
                assert!(
                    contrast_ratio(r.on_highlight, r.highlight) >= 3.0,
                    "锚点 {anchor} light={light}: on_highlight/highlight 不足 3"
                );
                assert!(
                    contrast_ratio(r.line, r.surface) >= 3.0,
                    "锚点 {anchor} light={light}: line/surface 不足 3"
                );
            }
        }
    }

    #[test]
    fn highlight_is_brand_lemon_and_on_highlight_is_near_black() {
        let t = default_tokens();
        for tone in [t.highlight.light, t.highlight.dark] {
            // 柠檬黄：绿通道显著高于蓝通道，且整体偏亮
            assert!(tone.g > 0.7 && tone.b < tone.g, "highlight 应为柠檬黄");
        }
        assert!(close(t.on_highlight.light.luminance(), 0.03, 0.05));
    }
}
