//! 封面印刷网纹（`设计2.md` §3.8 签名细节）的离屏合成算法。
//!
//! 按规范 §3.8「离屏渲染算法」实现为**纯字节层**操作（RGBA8），
//! 不依赖 egui 类型，因此可以完整单元测试；GUI 层只把结果上传为纹理
//! （规范 §9「把 §3.8 的网纹在资源加载阶段烘成纹理，UI 层只负责贴图」）。
//!
//! 图层顺序（自底向上，与规范一致）：
//! 1. 封面图（全局滤镜 contrast 1.06 / saturate 0.9 在步骤 4 统一施加）；
//! 2. 黑网点：2.5px 网格，半径 `r = (1 − l) × cell × 0.42`，
//!    颜色 `rgba(0,0,0,.62)` multiply，层不透明度 0.55；
//! 3. 套印偏色：5px 网格两层错位（品红 (0,0)、青 (2.5,2.5)），
//!    半径 `cell × 0.28`，screen 合成，层不透明度 0.5；
//! 4. 全局：contrast(1.06)、saturate(0.9)。
//!
//! 点距 2.5px 是「轻微网纹」与「廉价半调」的分界（规范 §3.8 参数敏感度）；
//! 不透明度 0.55 / 0.5 同样取自规范，不得擅改。

/// 封面纹理边长（px）。96 是「磁带标签 / 歌单条」两处显示尺寸的共同倍数，
/// 保证点阵网点密度一致（§3.8 2.5px 网点）。
///
/// 定义在这里而不是 app：**后台线程**（`bridge`）需要它来预烘纹理，
/// UI 层只负责上传，避免把解码 + 网纹压在 UI 线程上造成掉帧。
pub const COVER_PX: usize = 96;

/// 网点网格边长（逻辑像素）。规范 §3.8：2.5px。
const CELL: f32 = 2.5;

/// 黑网点颜色 alpha（rgba(0,0,0,.62)）。
const DOT_ALPHA: f32 = 0.62;

/// 黑网点层不透明度（multiply）。规范 §3.8：0.55。
const DOT_LAYER_OPACITY: f32 = 0.55;

/// 套印品红 rgba(255,0,120,.5)。
const MISREG_M: [f32; 4] = [255.0, 0.0, 120.0, 0.5];

/// 套印青 rgba(0,180,255,.45)。
const MISREG_C: [f32; 4] = [0.0, 180.0, 255.0, 0.45];

/// 套印层不透明度（screen）。规范 §3.8：0.5。
const MISREG_LAYER_OPACITY: f32 = 0.5;

/// 全局对比度（contrast 1.06）。
const CONTRAST: f32 = 1.06;

/// 全局饱和度（saturate 0.9）。
const SATURATE: f32 = 0.9;

/// 对 RGBA8 缓冲区**原地**施加 §3.8 网纹。
///
/// `rgba.len()` 必须等于 `width * height * 4`；尺寸为 0 时不做任何事。
/// 算法确定性：同一输入必得同一输出（单元测试覆盖）。
pub fn apply_halftone(rgba: &mut [u8], width: usize, height: usize) {
    assert_eq!(
        rgba.len(),
        width * height * 4,
        "RGBA 缓冲区长度与尺寸不一致"
    );
    if width == 0 || height == 0 {
        return;
    }

    // 步骤 2：黑网点（multiply）。逐 cell 求平均亮度，再画圆点。
    let cell = CELL;
    let mut cy = 0.0;
    while cy < height as f32 {
        let mut cx = 0.0;
        while cx < width as f32 {
            let l = cell_average_luma(rgba, width, height, cx, cy, cell);
            // r = (1 − l) × cell × 0.42
            let r = (1.0 - l) * cell * 0.42;
            if r > 0.05 {
                // multiply + 层不透明度 0.55：黑点等效 alpha = 0.62 × 0.55
                let a = DOT_ALPHA * DOT_LAYER_OPACITY;
                blend_disk(
                    rgba,
                    width,
                    height,
                    cx + cell * 0.5,
                    cy + cell * 0.5,
                    r,
                    0.0,
                    0.0,
                    0.0,
                    a,
                    Blend::Multiply,
                );
            }
            cx += cell;
        }
        cy += cell;
    }

    // 步骤 3：套印偏色（screen）。5px 网格、两层错位。
    let misreg_cell = cell * 2.0; // 5px
    for (color, offset) in [(MISREG_M, 0.0), (MISREG_C, cell)] {
        let r = cell * 0.28;
        let mut cy = offset;
        while cy < height as f32 {
            let mut cx = offset;
            while cx < width as f32 {
                let a = color[3] * MISREG_LAYER_OPACITY;
                blend_disk(
                    rgba,
                    width,
                    height,
                    cx + misreg_cell * 0.5,
                    cy + misreg_cell * 0.5,
                    r,
                    color[0],
                    color[1],
                    color[2],
                    a,
                    Blend::Screen,
                );
                cx += misreg_cell;
            }
            cy += misreg_cell;
        }
    }

    // 步骤 4：全局 contrast(1.06) / saturate(0.9)。
    for px in rgba.chunks_exact_mut(4) {
        let r = px[0] as f32;
        let g = px[1] as f32;
        let b = px[2] as f32;
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        // saturate 0.9：向亮度收敛
        let r = luma + (r - luma) * SATURATE;
        let g = luma + (g - luma) * SATURATE;
        let b = luma + (b - luma) * SATURATE;
        // contrast 1.06：围绕中灰 128 拉伸，随后钳位
        px[0] = clamp_u8((r - 128.0) * CONTRAST + 128.0);
        px[1] = clamp_u8((g - 128.0) * CONTRAST + 128.0);
        px[2] = clamp_u8((b - 128.0) * CONTRAST + 128.0);
    }
}

/// 混合模式（§3.8 只用到 multiply / screen）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Blend {
    Multiply,
    Screen,
}

/// 逐 cell 平均亮度 `l ∈ [0,1]`（只取 RGB，按 Rec.709 加权）。
fn cell_average_luma(rgba: &[u8], width: usize, height: usize, cx: f32, cy: f32, cell: f32) -> f32 {
    let x0 = cx.floor().max(0.0) as usize;
    let y0 = cy.floor().max(0.0) as usize;
    let x1 = ((cx + cell).ceil() as usize).min(width);
    let y1 = ((cy + cell).ceil() as usize).min(height);
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = (y * width + x) * 4;
            let r = rgba[i] as f32 / 255.0;
            let g = rgba[i + 1] as f32 / 255.0;
            let b = rgba[i + 2] as f32 / 255.0;
            sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
            n += 1.0;
        }
    }
    if n == 0.0 {
        0.0
    } else {
        (sum / n).clamp(0.0, 1.0)
    }
}

/// 以 (cx, cy) 为圆心画半径 r 的色点，按 alpha 与指定混合模式合成。
#[allow(clippy::too_many_arguments)]
fn blend_disk(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    cx: f32,
    cy: f32,
    r: f32,
    sr: f32,
    sg: f32,
    sb: f32,
    alpha: f32,
    mode: Blend,
) {
    let x0 = (cx - r).floor().max(0.0) as usize;
    let y0 = (cy - r).floor().max(0.0) as usize;
    let x1 = ((cx + r).ceil() as usize).min(width);
    let y1 = ((cy + r).ceil() as usize).min(height);
    for y in y0..y1 {
        for x in x0..x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            // 1px 软边缘面积覆盖率：半径很小时（浅色区微网点）仍按覆盖
            // 面积部分生效，避免点心落不到像素中心而整点丢失。
            let d = (dx * dx + dy * dy).sqrt();
            let coverage = (r - d + 0.5).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let a = alpha * coverage;
            let i = (y * width + x) * 4;
            for (k, src) in [sr, sg, sb].into_iter().enumerate() {
                let dst = rgba[i + k] as f32;
                let blended = match mode {
                    Blend::Multiply => dst * src / 255.0,
                    Blend::Screen => 255.0 - (255.0 - dst) * (255.0 - src) / 255.0,
                };
                rgba[i + k] = clamp_u8(dst + (blended - dst) * a);
            }
        }
    }
}

fn clamp_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// 按曲目 ID 确定性生成一张抽象几何封面（§5 内置封面风格的程序化等价物），
/// 并叠加 §3.8 网纹。**不联网取图**：测试程序封面全部本地生成。
///
/// 同一 `seed`（建议取歌曲 ID 的数值散列）必得同一图像；
/// 图像风格按 `seed % 5` 在规范 §5 的五种内置封面构成中轮转。
pub fn procedural_cover(seed: u64, width: usize, height: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; width * height * 4];
    let style = (seed % 5) as u8;
    for y in 0..height {
        for x in 0..width {
            let u = x as f32 / width.max(1) as f32;
            let v = y as f32 / height.max(1) as f32;
            let (r, g, b) = match style {
                // 1 品红→紫→深蓝竖向渐变 + 横线 + 黄色落日
                0 => {
                    let base = lerp3([255.0, 0.0, 120.0], [20.0, 10.0, 90.0], v);
                    let line = if (y % 14) < 7 { 18.0 } else { 0.0 };
                    let sun = sun_spot(u, v, 0.5, 0.32, 0.17, [255.0, 220.0, 60.0]);
                    (
                        base[0] + line + sun[0],
                        base[1] + line + sun[1],
                        base[2] + line + sun[2],
                    )
                }
                // 2 深蓝斜向渐变 + 白色月盘 + 同心细纹
                1 => {
                    let t = (u + v) * 0.5;
                    let base = lerp3([12.0, 22.0, 70.0], [70.0, 110.0, 190.0], t);
                    let moon = sun_spot(u, v, 0.62, 0.3, 0.11, [245.0, 245.0, 240.0]);
                    let rings = if (t * 40.0) as i32 % 2 == 0 { 8.0 } else { 0.0 };
                    (
                        base[0] + moon[0] + rings,
                        base[1] + moon[1] + rings,
                        base[2] + moon[2] + rings,
                    )
                }
                // 3 酸绿/墨色斜条 + 纸白圆环
                2 => {
                    let stripes = if ((x + y) / 7) % 2 == 0 {
                        [215.0, 255.0, 53.0]
                    } else {
                        [23.0, 23.0, 23.0]
                    };
                    let d = ((u - 0.5).powi(2) + (v - 0.5).powi(2)).sqrt();
                    let ring = if (0.2..0.33).contains(&d) {
                        [242.0, 240.0, 231.0]
                    } else {
                        stripes
                    };
                    (ring[0], ring[1], ring[2])
                }
                // 4 米白→淡紫渐变 + 橙色日盘 + 横线
                3 => {
                    let base = lerp3([242.0, 240.0, 231.0], [200.0, 180.0, 235.0], v);
                    let line = if (y % 12) < 6 { -14.0 } else { 0.0 };
                    let sun = sun_spot(u, v, 0.25, 0.25, 0.12, [255.0, 150.0, 50.0]);
                    (
                        base[0] + line + sun[0],
                        base[1] + line + sun[1],
                        base[2] + line + sun[2],
                    )
                }
                // 5 粉/黑方格 + 白色高光点
                _ => {
                    let checker = if ((x / 12) + (y / 12)) % 2 == 0 {
                        [255.0, 112.0, 184.0]
                    } else {
                        [17.0, 17.0, 17.0]
                    };
                    let hi = sun_spot(u, v, 0.72, 0.72, 0.08, [255.0, 255.0, 255.0]);
                    (checker[0] + hi[0], checker[1] + hi[1], checker[2] + hi[2])
                }
            };
            let i = (y * width + x) * 4;
            rgba[i] = clamp_u8(r);
            rgba[i + 1] = clamp_u8(g);
            rgba[i + 2] = clamp_u8(b);
            rgba[i + 3] = 255;
        }
    }
    apply_halftone(&mut rgba, width, height);
    rgba
}

/// 真实封面 → 点阵网纹 RGBA（边长 `size`）。
///
/// 流程：解码（PNG/JPEG）→ 等比裁切到正方形 → 缩到 `size×size`（三角形滤波，
/// 避免最近邻把细网点糊掉）→ [`apply_halftone`]。
///
/// 返回 `None` 表示字节不是可解码图片（调用方回退程序化封面，
/// **不要**用空图冒充真实封面）。
///
/// 为什么在这里而不是 UI 层：解码 + 网纹是纯字节运算，放这里才能被单测覆盖；
/// UI 层只负责把返回值上传为纹理。
pub fn halftone_cover_bytes(bytes: &[u8], size: usize) -> Option<Vec<u8>> {
    let size = size.max(8);
    let img = image::load_from_memory(bytes).ok()?;
    // 先居中裁成正方形，再缩放：直接 resize 会把封面拉变形
    let (w, h) = (img.width(), img.height());
    let side = w.min(h);
    let square = img.crop_imm((w - side) / 2, (h - side) / 2, side, side);
    let scaled = square.resize_exact(
        size as u32,
        size as u32,
        image::imageops::FilterType::Triangle,
    );
    let mut rgba = scaled.to_rgba8().into_raw();
    apply_halftone(&mut rgba, size, size);
    Some(rgba)
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// 圆形色斑（日盘/月盘/高光），返回应加到基色上的贡献。
fn sun_spot(u: f32, v: f32, cu: f32, cv: f32, radius: f32, color: [f32; 3]) -> [f32; 3] {
    let d = ((u - cu).powi(2) + (v - cv).powi(2)).sqrt();
    if d <= radius {
        let fade = 1.0 - (d / radius) * 0.35;
        [
            (color[0] - 128.0) * fade * 0.6,
            (color[1] - 128.0) * fade * 0.6,
            (color[2] - 128.0) * fade * 0.6,
        ]
    } else {
        [0.0, 0.0, 0.0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halftone_is_deterministic() {
        let mut a = vec![128u8; 32 * 32 * 4];
        let mut b = vec![128u8; 32 * 32 * 4];
        for px in a.chunks_exact_mut(4) {
            px[3] = 255;
        }
        for px in b.chunks_exact_mut(4) {
            px[3] = 255;
        }
        apply_halftone(&mut a, 32, 32);
        apply_halftone(&mut b, 32, 32);
        assert_eq!(a, b, "同一输入必须产生同一输出（规范：重绘不变形）");
    }

    #[test]
    fn halftone_darkens_dot_centers_on_flat_gray() {
        let w = 16;
        let h = 16;
        let mut rgba = vec![0u8; w * h * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[0] = 200;
            px[1] = 200;
            px[2] = 200;
            px[3] = 255;
        }
        apply_halftone(&mut rgba, w, h);
        // 黑网点 multiply 必然压暗部分像素
        let darkest = rgba
            .chunks_exact(4)
            .map(|px| px[0])
            .min()
            .expect("非空图像");
        assert!(darkest < 200, "网点应压暗像素，实际最暗 {}", darkest);
    }

    #[test]
    fn halftone_preserves_alpha_channel() {
        let w = 8;
        let h = 8;
        let mut rgba = vec![100u8; w * h * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
        apply_halftone(&mut rgba, w, h);
        assert!(rgba.chunks_exact(4).all(|px| px[3] == 255));
    }

    #[test]
    fn halftone_zero_dimension_is_noop() {
        let mut rgba: Vec<u8> = Vec::new();
        apply_halftone(&mut rgba, 0, 0);
        assert!(rgba.is_empty());
    }

    #[test]
    #[should_panic(expected = "RGBA 缓冲区长度与尺寸不一致")]
    fn halftone_rejects_mismatched_buffer() {
        let mut rgba = vec![0u8; 10];
        apply_halftone(&mut rgba, 4, 4);
    }

    #[test]
    fn procedural_cover_is_deterministic_per_seed() {
        let a = procedural_cover(347230, 24, 24);
        let b = procedural_cover(347230, 24, 24);
        assert_eq!(a, b);
        assert_eq!(a.len(), 24 * 24 * 4);
    }

    #[test]
    fn procedural_cover_varies_by_seed() {
        let a = procedural_cover(1, 24, 24);
        let b = procedural_cover(2, 24, 24);
        assert_ne!(a, b, "不同曲目 ID 应得到不同封面");
    }

    #[test]
    fn halftone_cover_bytes_decodes_and_marks() {
        // 造一张 8×8 的红色 PNG，解码后必须得到 size×size 的 RGBA
        let mut buf = std::io::Cursor::new(Vec::new());
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([200, 40, 40, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .expect("编码 PNG");
        let out = halftone_cover_bytes(buf.get_ref(), 24).expect("可解码");
        assert_eq!(out.len(), 24 * 24 * 4);
        assert!(out.chunks_exact(4).all(|px| px[3] == 255));
        // 网纹会压暗部分像素：与纯色原图不完全相同
        assert!(
            out.chunks_exact(4).any(|px| px[0] < 200),
            "点阵必须真的压暗了像素"
        );
    }

    #[test]
    fn halftone_cover_bytes_rejects_non_image() {
        assert!(halftone_cover_bytes(b"not an image", 24).is_none());
        assert!(halftone_cover_bytes(&[], 24).is_none());
    }

    #[test]
    fn procedural_cover_is_opaque() {
        let rgba = procedural_cover(7, 12, 12);
        assert!(rgba.chunks_exact(4).all(|px| px[3] == 255));
    }
}
