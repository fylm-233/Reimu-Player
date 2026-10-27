//! 色板：中性暖调色（H=95）+ 12 色相锚点 + 11 级共享明度/彩度曲线。
//!
//! 唯一色值源 = OKLCH（任务 §3.1–3.4）。这里只放**推导前的常量表**，
//! 具体语义 token 见 [`super::semantic`]；全部派生色在运行时经
//! [`super::color::gamut_clip`] 裁剪到 sRGB 色域。

use super::color::Oklch;

/// 中性色层级（H 恒为 95 的暖灰）。
pub const NEUTRAL_H: f32 = 95.0;

/// 中性色板：14 级（`n-0` .. `n-1000`），每级 `(L, C)`。
pub const NEUTRALS: [(f32, f32); 14] = [
    (0.995, 0.004), // n-0
    (0.980, 0.008), // n-25
    (0.962, 0.012), // n-50
    (0.935, 0.014), // n-100
    (0.885, 0.013), // n-200
    (0.805, 0.012), // n-300
    (0.690, 0.011), // n-400
    (0.575, 0.010), // n-500
    (0.470, 0.009), // n-600
    (0.375, 0.009), // n-700
    (0.290, 0.009), // n-800
    (0.215, 0.010), // n-900
    (0.155, 0.012), // n-950
    (0.105, 0.014), // n-1000
];

/// 中性层级下标（与 [`NEUTRALS`] 一一对应，命名便于引用）。
#[allow(dead_code)]
pub mod n {
    pub const N0: usize = 0;
    pub const N25: usize = 1;
    pub const N50: usize = 2;
    pub const N100: usize = 3;
    pub const N200: usize = 4;
    pub const N300: usize = 5;
    pub const N400: usize = 6;
    pub const N500: usize = 7;
    pub const N600: usize = 8;
    pub const N700: usize = 9;
    pub const N800: usize = 10;
    pub const N900: usize = 11;
    pub const N950: usize = 12;
    pub const N1000: usize = 13;
}

/// 取某级中性色 OKLCH。
pub fn neutral(level: usize) -> Oklch {
    let (l, c) = NEUTRALS[level];
    Oklch::new(l, c, NEUTRAL_H)
}

/// 色相锚点（12 个）：名称 + `H` + `Cmax`。
pub const ANCHORS: [(&str, f32, f32); 12] = [
    ("赤红", 25.0, 0.170),
    ("珊瑚", 45.0, 0.155),
    ("琥珀", 70.0, 0.135),
    ("芥黄", 95.0, 0.125),
    ("青柠", 135.0, 0.145),
    ("森绿", 160.0, 0.135),
    ("湖绿", 200.0, 0.115),
    ("天青", 235.0, 0.105),
    ("靛蓝", 265.0, 0.145),
    ("紫罗兰", 305.0, 0.155),
    ("品红", 335.0, 0.170),
    ("玫红", 355.0, 0.175),
];

/// 共享明度/彩度曲线：11 级（50..950），每级 `(L, C 系数)`。
///
/// `#[allow(clippy::approx_constant)]`：900 级的明度 `0.318` 恰好接近 `1/π`，
/// clippy 会误判成"该用常量"。这里它是**明度值**，不是任何数学常数。
#[allow(clippy::approx_constant)]
pub const RAMP: [(f32, f32); 11] = [
    (0.972, 0.12), // 50
    (0.938, 0.22), // 100
    (0.878, 0.40), // 200
    (0.802, 0.60), // 300
    (0.722, 0.80), // 400
    (0.648, 0.94), // 500
    (0.572, 1.00), // 600
    (0.488, 0.92), // 700
    (0.402, 0.74), // 800
    (0.318, 0.52), // 900
    (0.238, 0.34), // 950
];

/// 派生色层级下标。
#[allow(dead_code)]
pub mod level {
    pub const L50: usize = 0;
    pub const L100: usize = 1;
    pub const L200: usize = 2;
    pub const L300: usize = 3;
    pub const L400: usize = 4;
    pub const L500: usize = 5;
    pub const L600: usize = 6;
    pub const L700: usize = 7;
    pub const L800: usize = 8;
    pub const L900: usize = 9;
    pub const L950: usize = 10;
}

/// 由锚点 + 层级推导 `accent-*` 色。
pub fn accent(anchor: usize, level: usize) -> Oklch {
    let (_, h, c_max) = ANCHORS[anchor];
    let (l, coeff) = RAMP[level];
    Oklch::new(l, c_max * coeff, h)
}

/// 默认锚点（兜底靛蓝）：H=265，Cmax=0.145。
pub const DEFAULT_ANCHOR: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twelve_anchors_x_eleven_levels_all_generate() {
        // 全色系覆盖：12 锚点 × 11 级 = 132 个派生色全部可生成且能入色域
        let mut count = 0;
        for a in 0..ANCHORS.len() {
            for l in 0..RAMP.len() {
                let c = accent(a, l);
                assert!(c.l >= 0.0 && c.l <= 1.0);
                let clipped = super::super::color::gamut_clip(c);
                assert!(
                    super::super::color::in_gamut(clipped),
                    "锚点 {a} 级 {l} 越界"
                );
                count += 1;
            }
        }
        assert_eq!(count, 132);
    }

    #[test]
    fn neutrals_are_warm_hue_95() {
        for (i, _) in NEUTRALS.iter().enumerate() {
            let c = neutral(i);
            assert_eq!(c.h, NEUTRAL_H);
        }
    }
}
