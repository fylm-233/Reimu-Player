//! 专辑封面取色（任务 §4.4）。
//!
//! 流程：缩到 64×64 → 中位切分量化（k ≤ 6）→ 过滤近灰/低占比簇 →
//! 在 L∈[0.45,0.75] 里取 C 最高者 → 色相吸附 12 锚点（带 ±5° 滞回）→
//! `c_max = clamp(C, 0.10, Cmax(anchor))`。纯色 / 极简黑白 → 回默认靛蓝。
//!
//! 本模块只做**纯计算**：输入是像素视图，输出是 [`super::semantic::Accent`]；
//! 不读文件、不发网络、不启动线程（解码/缩放由 `gui` 的运行时在后台线程完成）。

use super::color::{lin_srgb_to_oklab, Oklch};
use super::ramp::ANCHORS;
use super::semantic::Accent;

/// 只读 RGBA8 像素视图。
#[derive(Debug, Clone, Copy)]
pub struct CoverView<'a> {
    pub rgba: &'a [u8],
    pub width: usize,
    pub height: usize,
}

/// 采样后的目标边长（先缩小再量化，既省算力又天然抗噪）。
pub const SAMPLE_SIDE: usize = 64;

/// 量化最大簇数。
const MAX_CLUSTERS: usize = 6;

/// 过滤阈值：彩度低于此值视为近黑/近白/近灰。
const MIN_CHROMA: f32 = 0.04;

/// 过滤阈值：簇占比低于此值丢弃。
const MIN_SHARE: f32 = 0.08;

/// 选主色时允许的明度区间（下界 / 上界）。
const L_RANGE: (f32, f32) = (0.45, 0.75);

/// 色相吸附滞回（度）：新锚点须比当前锚点至少近这么多才切换。
const HYSTERESIS_DEG: f32 = 5.0;

/// 彩度收敛下限（c_max 的最小值）。
const MIN_C_MAX: f32 = 0.10;

/// 取色结果：要么给出新强调色，要么 `None`（保持当前主题不变）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Extraction {
    /// 采纳新强调色。
    Accent(Accent),
    /// 无有效候选（近灰/低占比/纯色），保持当前主题。
    Keep,
}

impl Extraction {
    /// 供无状态调用方使用：`Keep` 时回默认靛蓝（首张封面 / 兜底）。
    pub fn accent_or_default(self) -> Accent {
        match self {
            Extraction::Accent(a) => a,
            Extraction::Keep => Accent::default(),
        }
    }
}

/// 一个量化簇：平均色（0..=255）+ 权重（像素占比 0..=1）。
#[derive(Debug, Clone, Copy)]
struct Cluster {
    r: f32,
    g: f32,
    b: f32,
    weight: f32,
}

/// 对 RGBA8 视图取色（`current` 为当前锚点，用于滞回；`None` 表示无当前主题）。
pub fn extract(cover: CoverView<'_>, current: Option<usize>) -> Extraction {
    let samples = downsample(cover);
    if samples.is_empty() {
        return Extraction::Keep;
    }
    let clusters = median_cut(&samples);
    let candidates: Vec<(Oklch, f32)> = clusters
        .iter()
        .filter_map(|c| {
            if c.weight < MIN_SHARE {
                return None;
            }
            let srgb = super::color::Srgb::new(c.r / 255.0, c.g / 255.0, c.b / 255.0);
            let oklch: Oklch = lin_srgb_to_oklab(srgb.into()).into();
            if oklch.c < MIN_CHROMA {
                return None;
            }
            Some((oklch, c.weight))
        })
        .collect();

    if candidates.is_empty() {
        return Extraction::Keep;
    }

    // 选主色：L∈[0.45,0.75] 里取 C 最高；区间内无候选则回退全局 C 最高
    let pick = candidates
        .iter()
        .filter(|(o, _)| o.l >= L_RANGE.0 && o.l <= L_RANGE.1)
        .max_by(|a, b| a.0.c.partial_cmp(&b.0.c).unwrap())
        .copied()
        .or_else(|| {
            candidates
                .iter()
                .max_by(|a, b| a.0.c.partial_cmp(&b.0.c).unwrap())
                .copied()
        });
    let Some((chosen, _)) = pick else {
        return Extraction::Keep;
    };

    // 色相吸附到最近锚点（带滞回）
    let anchor = snap_anchor(chosen.h, current);
    let c_max = chosen.c.clamp(MIN_C_MAX, ANCHORS[anchor].2);
    Extraction::Accent(Accent { anchor, c_max })
}

/// 把任意尺寸视图缩到 `SAMPLE_SIDE×SAMPLE_SIDE`（盒式平均，返回 RGB 样本）。
fn downsample(cover: CoverView<'_>) -> Vec<[f32; 3]> {
    if cover.rgba.len() < cover.width * cover.height * 4 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(SAMPLE_SIDE * SAMPLE_SIDE);
    for gy in 0..SAMPLE_SIDE {
        for gx in 0..SAMPLE_SIDE {
            // 源矩形
            let x0 = gx * cover.width / SAMPLE_SIDE;
            let x1 = ((gx + 1) * cover.width / SAMPLE_SIDE)
                .max(x0 + 1)
                .min(cover.width);
            let y0 = gy * cover.height / SAMPLE_SIDE;
            let y1 = ((gy + 1) * cover.height / SAMPLE_SIDE)
                .max(y0 + 1)
                .min(cover.height);
            let mut r = 0u64;
            let mut g = 0u64;
            let mut b = 0u64;
            let mut n = 0u64;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * cover.width + x) * 4;
                    r += cover.rgba[i] as u64;
                    g += cover.rgba[i + 1] as u64;
                    b += cover.rgba[i + 2] as u64;
                    n += 1;
                }
            }
            if n > 0 {
                out.push([(r / n) as f32, (g / n) as f32, (b / n) as f32]);
            }
        }
    }
    out
}

/// 中位切分量化：把样本切到 ≤ `MAX_CLUSTERS` 个盒，返回每盒平均色 + 权重。
fn median_cut(samples: &[[f32; 3]]) -> Vec<Cluster> {
    // 每个盒用样本下标区间表示（samples 按当前切分排序）
    let mut order: Vec<usize> = (0..samples.len()).collect();
    // boxes: (start, len, level) —— level 用于控制递归深度
    let mut boxes: Vec<(usize, usize, usize)> = vec![(0, samples.len(), 0)];

    while boxes.len() < MAX_CLUSTERS {
        // 找最大盒
        let mut biggest: Option<(usize, &(usize, usize, usize))> = None;
        for (i, b) in boxes.iter().enumerate() {
            if biggest.map(|(bi, _)| b.1 > boxes[bi].1).unwrap_or(true) {
                biggest = Some((i, b));
            }
        }
        let Some((bi, b)) = biggest else { break };
        let (start, len, level) = *b;
        if len < 2 {
            break;
        }
        // 找该盒在 RGB 上的最大范围通道
        let mut minc = [f32::MAX; 3];
        let mut maxc = [f32::MIN; 3];
        for &si in &order[start..start + len] {
            let p = samples[si];
            for c in 0..3 {
                minc[c] = minc[c].min(p[c]);
                maxc[c] = maxc[c].max(p[c]);
            }
        }
        let mut channel = 0;
        let mut range = -1.0f32;
        for c in 0..3 {
            let r = maxc[c] - minc[c];
            if r > range {
                range = r;
                channel = c;
            }
        }
        if range <= 0.0 {
            break;
        }
        // 按该通道排序并从中位数切开
        let slice = &mut order[start..start + len];
        slice.sort_by(|&a, &b| {
            samples[a][channel]
                .partial_cmp(&samples[b][channel])
                .unwrap()
        });
        let mid = len / 2;
        boxes[bi] = (start, mid, level + 1);
        boxes.push((start + mid, len - mid, level + 1));
    }

    // 汇总每盒平均色与权重
    boxes
        .iter()
        .map(|&(start, len, _)| {
            let mut r = 0f64;
            let mut g = 0f64;
            let mut b = 0f64;
            for &si in &order[start..start + len] {
                let p = samples[si];
                r += p[0] as f64;
                g += p[1] as f64;
                b += p[2] as f64;
            }
            let n = len as f64;
            Cluster {
                r: (r / n) as f32,
                g: (g / n) as f32,
                b: (b / n) as f32,
                weight: len as f32 / samples.len() as f32,
            }
        })
        .collect()
}

/// 色相吸附到最近锚点（带 ±HYSTERESIS_DEG 滞回，防止相邻曲目来回闪烁）。
fn snap_anchor(h: f32, current: Option<usize>) -> usize {
    let mut nearest = 0usize;
    let mut best = f32::MAX;
    for (i, (_, ah, _)) in ANCHORS.iter().enumerate() {
        let d = angular_distance(h, *ah);
        if d < best {
            best = d;
            nearest = i;
        }
    }
    match current {
        None => nearest,
        Some(cur) if cur == nearest => nearest,
        Some(cur) => {
            let d_cur = angular_distance(h, ANCHORS[cur].1);
            // 新锚点须比当前至少近 HYSTERESIS_DEG 才切换（越过中点后再推进 5°）
            if best + HYSTERESIS_DEG <= d_cur {
                nearest
            } else {
                cur
            }
        }
    }
}

/// 两个色相角的最小角度差（度，0..=180）。
fn angular_distance(a: f32, b: f32) -> f32 {
    let d = (a - b).abs() % 360.0;
    if d > 180.0 {
        360.0 - d
    } else {
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造纯色封面。
    fn solid(rgb: [u8; 3]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(SAMPLE_SIDE * SAMPLE_SIDE * 4);
        for _ in 0..SAMPLE_SIDE * SAMPLE_SIDE {
            buf.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        buf
    }

    fn view<'a>(buf: &'a [u8]) -> CoverView<'a> {
        CoverView {
            rgba: buf,
            width: SAMPLE_SIDE,
            height: SAMPLE_SIDE,
        }
    }

    #[test]
    fn twelve_hues_snap_to_expected_anchors() {
        // 至少 12 张不同色调的封面，断言锚点编号符合预期（验收 §6）。
        // 用每个锚点自身的 accent-600 色生成封面：色相必然吸附回该锚点。
        for anchor in 0..ANCHORS.len() {
            let c = super::super::ramp::accent(anchor, super::super::ramp::level::L600);
            let srgb = super::super::color::oklch_to_srgb_raw(super::super::color::gamut_clip(c));
            let rgb = srgb.to_u8();
            let buf = solid(rgb);
            match extract(view(&buf), None) {
                Extraction::Accent(a) => assert_eq!(
                    a.anchor, anchor,
                    "锚点 {anchor} 的强调色应吸附回自身，得 {}",
                    a.anchor
                ),
                Extraction::Keep => panic!("锚点 {anchor} 不应回退 Keep"),
            }
        }
    }

    #[test]
    fn black_white_and_grey_fall_back_to_keep() {
        for rgb in [
            [0u8, 0, 0],
            [255, 255, 255],
            [128, 128, 128],
            [240, 240, 238],
        ] {
            let buf = solid(rgb);
            assert_eq!(
                extract(view(&buf), None),
                Extraction::Keep,
                "近灰/纯色封面应回退默认主题"
            );
        }
    }

    #[test]
    fn keep_resolves_to_default_indigo() {
        let buf = solid([0, 0, 0]);
        let a = extract(view(&buf), None).accent_or_default();
        assert_eq!(a.anchor, super::super::ramp::DEFAULT_ANCHOR);
    }

    #[test]
    fn hysteresis_holds_near_boundary_until_crossed() {
        // 当前锚点 = 8（靛蓝 H=265）。取一个正好在靛蓝与紫罗兰(305)中点(285)附近、
        // 略偏紫罗兰但未越过滞回带的色，应保持 8；越过则切换。
        let cur = Some(8usize);
        // 285° 距 265=20、距 305=20，平局 → 保持当前（不加滞回时可能抖动）
        let a = snap_anchor(285.0, cur);
        assert_eq!(a, 8, "平局应保持当前锚点");

        // 明显偏紫罗兰（295°）：距 265=30、距 305=10，差 20 > 5 → 切换
        let b = snap_anchor(295.0, cur);
        assert_eq!(b, 9);
    }

    #[test]
    fn chroma_is_clamped_to_anchor_max() {
        // 高彩度红：c_max 被 clamp 到锚点 Cmax（0.170）
        let buf = solid([255, 0, 0]);
        if let Extraction::Accent(a) = extract(view(&buf), None) {
            assert!(a.c_max <= ANCHORS[a.anchor].2 + 1e-5);
            assert!(a.c_max >= MIN_C_MAX - 1e-5);
        }
    }
}
