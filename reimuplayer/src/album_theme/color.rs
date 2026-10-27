//! 色彩学：OKLab/OKLCH ↔ 线性 sRGB 转换、色域裁剪、WCAG 对比度、OKLab 混色。
//!
//! 全部使用 `f32` 纯函数、无堆分配、无 IO。转换公式出自 Björn Ottosson 的
//! 参考实现（OKLab，2020）：
//! - LMS → OKLab：`l_ = 0.4122214708r + 0.5363325363g + 0.0514459929b` 等；
//! - 详见模块内常量表。逆变换为同矩阵的逆。
//!
//! 约束（`DESIGN_SPEC` / 任务 §4.1）：
//! - **禁止**用 HSL/HSV 做任何推导或插值——本模块只用 OKLab/OKLCH；
//! - OKLCH → sRGB 超出 `[0,1]` 时**保持 L 与 H 不变、逐步衰减 C**（二分法），
//!   禁止简单 clamp（会引入色相偏移）。

/// 线性 sRGB 颜色（无 gamma 编码，各通道 ∈ [0,1] 表示色域内）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinSrgb {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// 显示用 sRGB 颜色（gamma 编码，各通道 ∈ [0,1]）。`Default` 为黑色。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Srgb {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Srgb {
    /// 构造（不做裁剪，调用方保证取值合法或自行裁剪）。
    pub const fn new(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b }
    }

    /// 转 0..=255 的 8-bit 分量（用于导出 / 快照）。
    pub fn to_u8(self) -> [u8; 3] {
        let c = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
        [c(self.r), c(self.g), c(self.b)]
    }

    /// 转 `#RRGGBB` 十六进制串（导出 / 快照用；非业务渲染路径）。
    pub fn to_hex(self) -> String {
        let [r, g, b] = self.to_u8();
        format!("#{r:02X}{g:02X}{b:02X}")
    }

    /// 相对亮度（WCAG 2.x 的 `relative luminance`）。
    pub fn luminance(self) -> f32 {
        let channel = |c: f32| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }
}

/// OKLab 颜色（L ∈ [0,1]，a/b ∈ [-0.4, 0.4] 量级）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklab {
    pub l: f32,
    pub a: f32,
    pub b: f32,
}

/// OKLCH 颜色（极坐标 OKLab：L 明度、C 彩度、H 色相角（度））。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklch {
    pub l: f32,
    pub c: f32,
    pub h: f32,
}

impl Oklch {
    pub const fn new(l: f32, c: f32, h: f32) -> Self {
        Self { l, c, h }
    }

    /// 转 OKLab。
    pub fn to_oklab(self) -> Oklab {
        let h = self.h.to_radians();
        Oklab {
            l: self.l,
            a: self.c * h.cos(),
            b: self.c * h.sin(),
        }
    }
}

impl From<Oklab> for Oklch {
    fn from(v: Oklab) -> Self {
        Oklch {
            l: v.l,
            c: (v.a * v.a + v.b * v.b).sqrt(),
            h: v.b.atan2(v.a).to_degrees(),
        }
    }
}

/// gamma 编码（线性 sRGB → 显示 sRGB）。
pub fn linear_to_gamma(x: f32) -> f32 {
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// gamma 解码（显示 sRGB → 线性 sRGB）。
pub fn gamma_to_linear(x: f32) -> f32 {
    if x <= 0.040_45 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

impl From<LinSrgb> for Srgb {
    fn from(v: LinSrgb) -> Self {
        Srgb {
            r: linear_to_gamma(v.r),
            g: linear_to_gamma(v.g),
            b: linear_to_gamma(v.b),
        }
    }
}

impl From<Srgb> for LinSrgb {
    fn from(v: Srgb) -> Self {
        LinSrgb {
            r: gamma_to_linear(v.r),
            g: gamma_to_linear(v.g),
            b: gamma_to_linear(v.b),
        }
    }
}

/// 线性 sRGB → OKLab（Ottosson 参考实现矩阵）。
pub fn lin_srgb_to_oklab(v: LinSrgb) -> Oklab {
    let l = 0.412_221_47 * v.r + 0.536_332_54 * v.g + 0.051_445_99 * v.b;
    let m = 0.211_903_5 * v.r + 0.680_699_5 * v.g + 0.107_396_96 * v.b;
    let s = 0.088_302_46 * v.r + 0.281_718_85 * v.g + 0.629_978_7 * v.b;
    let l_ = l.cbrt();
    let m_ = m.cbrt();
    let s_ = s.cbrt();
    Oklab {
        l: 0.210_454_26 * l_ + 0.793_617_78 * m_ - 0.004_072_046_8 * s_,
        a: 1.977_998_5 * l_ - 2.428_592_2 * m_ + 0.450_593_7 * s_,
        b: 0.025_904_037 * l_ + 0.782_771_77 * m_ - 0.808_675_77 * s_,
    }
}

/// OKLab → 线性 sRGB（逆矩阵）。
pub fn oklab_to_lin_srgb(v: Oklab) -> LinSrgb {
    let l_ = v.l + 0.396_337_78 * v.a + 0.215_803_76 * v.b;
    let m_ = v.l - 0.105_561_346 * v.a - 0.063_854_17 * v.b;
    let s_ = v.l - 0.089_484_18 * v.a - 1.291_485_5 * v.b;
    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;
    LinSrgb {
        r: 4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_93 * s,
        g: -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        b: -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    }
}

/// OKLCH → 显示 sRGB（**不含色域裁剪**；色域外分量会 <0 或 >1）。
pub fn oklch_to_srgb_raw(v: Oklch) -> Srgb {
    oklab_to_lin_srgb(v.to_oklab()).into()
}

/// 判断某 OKLCH 转换后是否落在 sRGB 色域内（容差 ε）。
pub fn in_gamut(v: Oklch) -> bool {
    let c = oklab_to_lin_srgb(v.to_oklab());
    let eps = 0.000_5;
    (c.r >= -eps && c.r <= 1.0 + eps)
        && (c.g >= -eps && c.g <= 1.0 + eps)
        && (c.b >= -eps && c.b <= 1.0 + eps)
}

/// 色域裁剪：保持 L 与 H 不变，把 C 二分衰减到能落入色域为止。
///
/// 禁止简单 clamp（会偏移色相）；二分到 `c` 的微小下界后仍越界时，用
/// 最接近色域内的一点兜底。返回裁剪后的 OKLCH。
pub fn gamut_clip(v: Oklch) -> Oklch {
    if in_gamut(v) {
        return v;
    }
    let mut lo = 0.0f32;
    let mut hi = v.c;
    let mut clipped = Oklch { c: 0.0, ..v };
    for _ in 0..48 {
        let mid = (lo + hi) * 0.5;
        let candidate = Oklch { c: mid, ..v };
        if in_gamut(candidate) {
            lo = mid;
            clipped = candidate;
        } else {
            hi = mid;
        }
    }
    clipped
}

/// WCAG 对比度（相对亮度比值）。
pub fn contrast_ratio(a: Srgb, b: Srgb) -> f32 {
    let la = a.luminance();
    let lb = b.luminance();
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// 在 OKLab 空间做线性插值（`t` ∈ [0,1]）。
///
/// 色相不在此处处理——对 OKLab 的 a/b 分量插值天然沿感知均匀的直线，
/// 避免在 HSV/HSL 里绕色环出现错误路径。
pub fn mix_oklab(a: Oklab, b: Oklab, t: f32) -> Oklab {
    let t = t.clamp(0.0, 1.0);
    Oklab {
        l: a.l + (b.l - a.l) * t,
        a: a.a + (b.a - a.a) * t,
        b: a.b + (b.b - a.b) * t,
    }
}

/// 两个显示色在 OKLab 空间混色（交互态 `mix(accent, surface, %)` 的实现）。
pub fn mix_colors(a: Srgb, b: Srgb, t: f32) -> Srgb {
    let oa: Oklab = lin_srgb_to_oklab(a.into());
    let ob: Oklab = lin_srgb_to_oklab(b.into());
    oklab_to_lin_srgb(mix_oklab(oa, ob, t)).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn linear_gamma_round_trips() {
        for x in [0.0, 0.01, 0.1, 0.5, 0.9, 1.0] {
            assert!(close(gamma_to_linear(linear_to_gamma(x)), x, 1e-5));
            assert!(close(linear_to_gamma(gamma_to_linear(x)), x, 1e-5));
        }
    }

    #[test]
    fn oklab_oklch_round_trips() {
        for c in [
            Oklch::new(0.5, 0.1, 25.0),
            Oklch::new(0.7, 0.2, 120.0),
            Oklch::new(0.3, 0.15, 300.0),
        ] {
            let back: Oklch = c.to_oklab().into();
            assert!(close(back.l, c.l, 1e-4), "{c:?} -> {back:?}");
            assert!(close(back.c, c.c, 1e-4));
        }
    }

    #[test]
    fn gamut_clip_preserves_l_and_h() {
        // 超高彩度青柠在 sRGB 外：裁剪后 L/H 不变，C 被压低到色域内
        let v = Oklch::new(0.648, 0.5, 135.0);
        assert!(!in_gamut(v));
        let clipped = gamut_clip(v);
        assert!(close(clipped.l, v.l, 1e-5), "L 不得变");
        // 色相角可能跨越 ±180 边界，用 cos/sin 比较方向
        let (h, hc) = (v.h.to_radians(), clipped.h.to_radians());
        assert!(
            (h.cos() - hc.cos()).abs() < 1e-4 && (h.sin() - hc.sin()).abs() < 1e-4,
            "H 不得变"
        );
        assert!(clipped.c < v.c, "彩度必须被压低");
        assert!(in_gamut(clipped), "裁剪后必须入色域");
    }

    #[test]
    fn contrast_ratio_is_symmetric_and_known() {
        // 黑对白 = 21:1
        let black = Srgb::new(0.0, 0.0, 0.0);
        let white = Srgb::new(1.0, 1.0, 1.0);
        let c = contrast_ratio(black, white);
        assert!((c - 21.0).abs() < 0.01, "黑对白应为 21:1，得 {c}");
        assert_eq!(contrast_ratio(white, black), c);
    }

    #[test]
    fn mix_is_bounded() {
        let a = Srgb::new(1.0, 0.0, 0.0);
        let b = Srgb::new(0.0, 0.0, 1.0);
        let m = mix_colors(a, b, 0.5);
        assert!(m.r >= 0.0 && m.r <= 1.0 && m.b >= 0.0 && m.b <= 1.0);
    }
}
