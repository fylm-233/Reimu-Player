//! 二维码点阵生成（登录用）。
//!
//! 职责单一：把一段文本（网易云的登录链接）编码成**可直接上传为纹理的
//! [`egui::ColorImage`]**。不做网络、不碰 Cookie、不写盘。
//!
//! 为什么要自己画而不显示图片：二维码内容是登录凭据的一部分，
//! 交给任何第三方图片服务都等于把凭据外发（`NETEASE_INTEGRATION_SPEC.md`
//! 的凭据约束）。本地编码 + 本地纹理，凭据不出进程。
//!
//! 绘制风格与 System 6 界面一致：**纯黑白、零圆角、最近邻放大**（不做平滑），
//! 保证手机摄像头在 125% 缩放下也能正确识别。

use eframe::egui;

/// 二维码静区（quiet zone）模块数。规范要求 ≥4，取 4。
const QUIET: usize = 4;

/// 把文本编码成二维码纹理图像。
///
/// - `module_px`：每个模块的像素边长（整数，避免半像素导致扫描失败）；
/// - 返回 `None` 表示内容过长或无法编码（调用方应提示"二维码生成失败"，
///   而不是显示一个假的二维码）。
pub fn qr_image(text: &str, module_px: usize) -> Option<egui::ColorImage> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let width = code.width();
    // to_colors(): Color::Dark / Color::Light；转成 bool 便于索引
    let modules: Vec<bool> = code
        .to_colors()
        .into_iter()
        .map(|c| c == qrcode::Color::Dark)
        .collect();
    let module_px = module_px.max(1);
    let side = (width + QUIET * 2) * module_px;
    let mut pixels = vec![egui::Color32::WHITE; side * side];

    for y in 0..width {
        for x in 0..width {
            if !modules[y * width + x] {
                continue;
            }
            // 静区偏移 + 模块内填充
            let px0 = (x + QUIET) * module_px;
            let py0 = (y + QUIET) * module_px;
            for dy in 0..module_px {
                let row = (py0 + dy) * side;
                for dx in 0..module_px {
                    pixels[row + px0 + dx] = egui::Color32::BLACK;
                }
            }
        }
    }

    Some(egui::ColorImage {
        size: [side, side],
        pixels,
        source_size: egui::Vec2::new(side as f32, side as f32),
    })
}

/// 取二维码模块数（用于断言/布局；编码失败返回 `None`）。
pub fn qr_modules(text: &str) -> Option<usize> {
    qrcode::QrCode::new(text.as_bytes()).ok().map(|c| c.width())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_url_to_square_image() {
        let img = qr_image("https://music.163.com/login?codekey=abc", 4).expect("可编码");
        assert_eq!(img.size[0], img.size[1], "二维码必须是正方形");
        assert_eq!(img.pixels.len(), img.size[0] * img.size[1]);
        // 静区必须全白：第一行第一个像素是白
        assert_eq!(img.pixels[0], egui::Color32::WHITE);
        // 中心区域必须有深色模块（否则说明模块没画上去）
        let dark = img
            .pixels
            .iter()
            .filter(|p| **p == egui::Color32::BLACK)
            .count();
        assert!(dark > 0, "二维码必须含深色模块");
    }

    #[test]
    fn image_side_is_module_aligned() {
        let modules = qr_modules("https://music.163.com/login?codekey=abc").expect("可编码");
        let img = qr_image("https://music.163.com/login?codekey=abc", 3).expect("可编码");
        assert_eq!(img.size[0], (modules + QUIET * 2) * 3);
    }

    #[test]
    fn oversized_content_is_none_not_panic() {
        // 超出二维码容量：返回 None（调用方提示失败），不得 panic
        let huge = "x".repeat(10_000);
        assert!(qr_image(&huge, 4).is_none());
    }

    #[test]
    fn zero_module_size_still_produces_image() {
        // 0 会被夹到 1，不允许除零/空图
        let img = qr_image("https://music.163.com", 0).expect("可编码");
        assert!(img.size[0] > 0);
    }
}
