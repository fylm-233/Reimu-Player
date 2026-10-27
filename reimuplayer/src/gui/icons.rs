//! 开源图标字库接入（Font Awesome 6 Free Solid，SIL OFL 1.1）。
//!
//! 需求（窗口 UI 交付·图标与资源）：设计稿的行内符号（▶ ⌕ ★ ≣ ↻ …）由
//! 统一的开源图标字库承接，保证全应用图标风格一致。选用 **Font Awesome 6
//! Free Solid**（`fa-solid-900.ttf`，OFL 1.1，约 410 KB）：字形为实心块面，
//! 与 System 6 / Y2K 磁带机的硬边视觉同调；码点经 fontTools 对字库 cmap
//! 逐一核验（见 `docs/WINDOW_UI_DESIGN.md`）。
//!
//! 接入方式：字库以 `FontFamily::Name("icons")` 装入 egui；图标即
//! **私用区（PUA）字符**，[`widgets::spec_job`] 检测到 PUA 字符自动切换到
//! 图标字族（见 [`is_icon_text`]），因此所有既有绘制函数无签名变化即可
//! 渲染图标。

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{self, Color32, FontId};

/// 图标字族名（[`install`] 注册）。
pub const ICONS_FAMILY: &str = "icons";

/// Font Awesome 6 Free Solid 字库（OFL 1.1，随二进制嵌入）。
pub const ICONS_FONT_DATA: &[u8] = include_bytes!("../../assets/fonts/fa-solid-900.ttf");

// ---------------------------------------------------------------------------
// 字形常量（FA6 Free Solid 码点，已对字库 cmap 核验）
// ---------------------------------------------------------------------------

/// 播放 ▶。
pub const PLAY: &str = "\u{f04b}";
/// 暂停 ❚❚。
pub const PAUSE: &str = "\u{f04c}";
/// 上一首 |◀。
pub const PREV: &str = "\u{f048}";
/// 下一首 ▶|。
pub const NEXT: &str = "\u{f051}";
/// 随机 ⤨。
pub const SHUFFLE: &str = "\u{f074}";
/// 循环 ↻。
pub const REPEAT: &str = "\u{f363}";
/// FA6 `repeat-1`——**不要在界面上使用**。
///
/// 随包的 `fa-solid-900.ttf` 只在 cmap format 4 子表里列了这个码点，光栅器实际
/// 走的 format 12 子表里它是 NOTDEF，画出来是一个空心豆腐块（实测见
/// `outputs/glyph_key_2.png`）。保留常量只为让
/// [`crate::gui::app`] 里模式键字形选择的回归测试能断言它不再回到键面上；
/// 单曲循环用 [`REPEAT`] + 等宽字体 `1` 角标。
pub const REPEAT_ONE: &str = "\u{f365}";
/// 顺序播放（箭头）。
pub const SEQUENTIAL: &str = "\u{f061}";
/// 搜索 ⌕。
pub const SEARCH: &str = "\u{f002}";
/// 收藏/热榜 ★。
pub const STAR: &str = "\u{f005}";
/// 歌单 ≣。
pub const LIST: &str = "\u{f03a}";
/// 唱片（推荐页）。
pub const DISC: &str = "\u{f51f}";
/// 音符。
pub const MUSIC: &str = "\u{f001}";
/// 设置齿轮。
pub const GEAR: &str = "\u{f013}";
/// 打开文件夹（曲库扫描）。
pub const FOLDER: &str = "\u{f07c}";
/// 音量。
pub const VOLUME: &str = "\u{f028}";
/// 退带。
pub const EJECT: &str = "\u{f052}";
/// 最小化（window-minimize）。
pub const WIN_MIN: &str = "\u{f2d1}";
/// 最大化（window-maximize）。
pub const WIN_MAX: &str = "\u{f2d0}";
/// 还原（window-restore）。
pub const WIN_RESTORE: &str = "\u{f2d2}";
/// 关闭 ×。
pub const CLOSE: &str = "\u{f00d}";
/// 信息。
pub const INFO: &str = "\u{f05a}";
/// 时钟。
pub const CLOCK: &str = "\u{f017}";
/// 均衡器（bars）。
pub const BARS: &str = "\u{f0c9}";
/// 实心红心（收藏态）。图标字库只有 solid 面，未收藏态用低透明度同字形区分。
pub const HEART: &str = "\u{f004}";
/// 心动模式（`heart-pulse`，FA6 心形带心电线；FA5 旧名 `heartbeat`）。
///
/// 与「喜欢」的 [`HEART`] **刻意不同形**：两个键都在控制行附近出现，同形只换
/// 颜色会让人分不清"收藏"和"智能播放"。
///
/// 码点靠**渲染实测**确定，不靠 cmap：先前用 `f580`（以为是 heart-pulse），
/// 探针帧里画出来的是一张**笑脸**——cmap 只保证"这个码点有字形"，不保证它是
/// 哪个图标。`f21e` 实测是心形带心电线，才是要的这颗心。
pub const HEARTBEAT: &str = "\u{f21e}";
/// 上锁（`lock`，FA6 `lock`）。
///
/// 桌面歌词右上角用**两个不同字形**表达锁定状态：锁定时是合上的挂锁，
/// 解锁时是开锁。曾经两态共用一个手绘锁形、只改颜色，用户实测反馈
/// 「分不清现在是锁着还是没锁」——形状差异比颜色差异可靠得多。
pub const LOCK: &str = "\u{f023}";
/// 解锁（`unlock`，FA6 `unlock`：锁梁朝上翻开）。
pub const UNLOCK: &str = "\u{f09c}";

/// 判定字符串是否为图标字形（首字符落在 Unicode 私用区）。
///
/// [`widgets::spec_job`] 据此自动把整段文本切到图标字族——
/// 图标常量都是单字符，判定首字符即可。
pub fn is_icon_text(s: &str) -> bool {
    s.chars()
        .next()
        .map(|c| ('\u{e000}'..='\u{f8ff}').contains(&c))
        .unwrap_or(false)
}

/// 把图标字库注册进字体定义（[`super::theme::setup_fonts`] 调用）。
pub fn install(defs: &mut egui::FontDefinitions) {
    defs.font_data.insert(
        "fa-solid".to_owned(),
        std::sync::Arc::new(egui::FontData::from_owned(ICONS_FONT_DATA.to_vec())),
    );
    defs.families
        .insert(font_family(), vec!["fa-solid".to_owned()]);
}

/// 图标字族（`FontFamily::Name` 载荷为 `Arc<str>`）。
pub fn font_family() -> egui::FontFamily {
    egui::FontFamily::Name(std::sync::Arc::from(ICONS_FAMILY))
}

/// 图标 [`LayoutJob`]（无字距；图标是面，不需要 tracking）。
pub fn icon_job(glyph: &str, size: f32, color: Color32) -> LayoutJob {
    let format = TextFormat {
        font_id: FontId::new(size, font_family()),
        color,
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.wrap.max_rows = 1;
    job.append(glyph, 0.0, format);
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_glyphs_are_pua_and_detected() {
        for g in [PLAY, PAUSE, SEARCH, GEAR, WIN_MAX, CLOSE, LOCK, UNLOCK] {
            assert!(is_icon_text(g), "{g:?} 应识别为图标字形");
        }
        assert!(!is_icon_text("▶"), "普通文本字形不触发图标字族");
        assert!(!is_icon_text("Reimu Player"));
        assert!(!is_icon_text(""));
    }

    /// 锁定/解锁必须是**两个不同字形**：同一图标只换颜色，用户分不清状态。
    #[test]
    fn lock_and_unlock_are_different_glyphs() {
        assert_ne!(LOCK, UNLOCK);
        assert_ne!(LOCK, PLAY);
        assert_ne!(UNLOCK, PLAY);
    }

    #[test]
    fn all_glyphs_are_single_char() {
        for g in [
            PLAY,
            PAUSE,
            PREV,
            NEXT,
            SHUFFLE,
            REPEAT,
            REPEAT_ONE,
            SEQUENTIAL,
            SEARCH,
            STAR,
            LIST,
            DISC,
            MUSIC,
            GEAR,
            FOLDER,
            VOLUME,
            EJECT,
            WIN_MIN,
            WIN_MAX,
            WIN_RESTORE,
            CLOSE,
            INFO,
            CLOCK,
            BARS,
            HEART,
            HEARTBEAT,
            LOCK,
            UNLOCK,
        ] {
            assert_eq!(g.chars().count(), 1, "{g:?} 必须是单字符");
        }
    }

    /// 心动模式与收藏必须**不同形**：同形只换颜色，用户分不清两个键各干什么。
    #[test]
    fn heartbeat_is_a_distinct_glyph_from_the_like_heart() {
        assert_ne!(HEARTBEAT, HEART);
        // `f580` 实测是一张笑脸（cmap 有字 ≠ 是对的图标），见 HEARTBEAT 的文档
        assert_ne!(HEARTBEAT, "\u{f580}", "笑脸不能当心形用");
        assert_eq!(HEARTBEAT, "\u{f21e}", "heart-pulse");
    }
}
